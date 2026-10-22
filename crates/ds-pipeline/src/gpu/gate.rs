//! Turn-taking on the GPU and memory admission.

use std::sync::{Condvar, Mutex, MutexGuard};

use super::lock;
use crate::{PipelineError, Priority};

/// Turn-taking on the GPU: one render records/submits at a time, the highest priority
/// class first, and one render per non-interactive class in flight (FIFO within a class).
pub(super) struct Gate {
    st: Mutex<GateState>,
    /// Waiters for a GPU turn.
    turn_cv: Condvar,
    /// Renders queued for their class's slot.
    slot_cv: Condvar,
}

#[derive(Default)]
pub(super) struct GateState {
    busy: bool,
    /// Turn waiters per class (see [`Priority::class`]).
    waiting: [usize; 4],
    /// A render of this class is in flight (classes 1..=3).
    slot: [bool; 4],
    /// Per class: the next ticket to hand out, the ticket that may take the slot next,
    /// and tickets whose waiter gave up (cancelled) before its turn.
    next_ticket: [u64; 4],
    serving: [u64; 4],
    abandoned: [Vec<u64>; 4],
}

impl GateState {
    /// The slot's next ticket after `t`, skipping abandoned ones.
    fn serve_after(&mut self, c: usize, t: u64) {
        let mut next = t + 1;
        while let Some(i) = self.abandoned[c].iter().position(|a| *a == next) {
            self.abandoned[c].swap_remove(i);
            next += 1;
        }
        self.serving[c] = next;
    }
}

pub(super) struct Turn<'a>(&'a Gate);
pub(super) struct BackgroundSlot<'a>(&'a Gate, usize);

/// Block on `cv` briefly (callers re-check their condition and cancellation). The
/// waiting thread runs nothing else meanwhile: a rayon worker that stole another render
/// here could wait for a slot its own outer frame holds. (The renders' parallel loops run
/// on the pipeline's own pool, see [`crate::par`], so waiting workers never starve them.)
fn park<'a, T>(cv: &Condvar, g: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cv.wait_timeout(g, std::time::Duration::from_millis(5))
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .0
}

impl Gate {
    pub(super) fn state(&self) -> MutexGuard<'_, GateState> {
        self.st
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Wait (cancellable) for the one in-flight place of `p`'s class. Interactive renders
    /// take none.
    pub(super) fn background(
        &self,
        p: Priority,
        cancel: Option<&crate::CancelToken>,
    ) -> Result<Option<BackgroundSlot<'_>>, PipelineError> {
        let c = p.class();
        if c == 0 {
            return Ok(None);
        }
        let mut s = self.state();
        let ticket = s.next_ticket[c];
        s.next_ticket[c] += 1;
        loop {
            if !s.slot[c] && s.serving[c] == ticket {
                s.slot[c] = true;
                s.serve_after(c, ticket);
                return Ok(Some(BackgroundSlot(self, c)));
            }
            if cancel.is_some_and(crate::CancelToken::is_cancelled) {
                if s.serving[c] == ticket {
                    s.serve_after(c, ticket);
                } else {
                    s.abandoned[c].push(ticket);
                }
                drop(s);
                self.slot_cv.notify_all();
                return Err(PipelineError::Cancelled);
            }
            s = park(&self.slot_cv, s);
        }
    }

    /// Wait for the GPU turn of `p`'s class (cancellable, like [`Gate::background`]).
    pub(super) fn turn(
        &self,
        p: Priority,
        cancel: Option<&crate::CancelToken>,
    ) -> Result<Turn<'_>, PipelineError> {
        let c = p.class();
        let mut s = self.state();
        s.waiting[c] += 1;
        while s.busy || s.waiting[..c].iter().any(|n| *n > 0) {
            if cancel.is_some_and(crate::CancelToken::is_cancelled) {
                s.waiting[c] -= 1;
                drop(s);
                // Lower classes may have been waiting on this one.
                self.turn_cv.notify_all();
                return Err(PipelineError::Cancelled);
            }
            s = park(&self.turn_cv, s);
        }
        s.waiting[c] -= 1;
        s.busy = true;
        Ok(Turn(self))
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.state().busy = false;
        self.0.turn_cv.notify_all();
    }
}

impl Drop for BackgroundSlot<'_> {
    fn drop(&mut self) {
        self.0.state().slot[self.1] = false;
        self.0.slot_cv.notify_all();
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            st: Mutex::new(GateState::default()),
            turn_cv: Condvar::new(),
            slot_cv: Condvar::new(),
        }
    }
}

/// Memory admission: a render starts only when its estimated working set fits the budget
/// next to what is already counted (or when nothing else is rendering), so the budget
/// holds as a ceiling instead of being exceeded by concurrent renders.
#[derive(Default)]
pub(super) struct MemGate {
    /// (renders admitted, bytes they reserved).
    pub(super) admitted: Mutex<(u32, u64)>,
    pub(super) cv: Condvar,
}

pub(super) struct Admission<'a>(pub(super) &'a MemGate, pub(super) u64);

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        let mut a = lock(&self.0.admitted);
        a.0 -= 1;
        a.1 -= self.1;
        drop(a);
        self.0.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::CancelToken;

    #[test]
    fn a_cancelled_waiter_leaves_the_queue_without_taking_a_turn() {
        let gate = Gate::default();
        let held = gate.turn(Priority::Bulk, None).expect("first turn");
        let token = CancelToken::new();
        std::thread::scope(|s| {
            let waiter = s.spawn(|| gate.turn(Priority::Bulk, Some(&token)).map(|_| ()));
            std::thread::sleep(Duration::from_millis(30));
            token.cancel();
            assert!(matches!(
                waiter.join().expect("waiter"),
                Err(PipelineError::Cancelled)
            ));
        });
        assert_eq!(gate.state().waiting, [0; 4], "the cancelled ticket is gone");
        drop(held);
        gate.turn(Priority::Bulk, None)
            .expect("the gate still works");
    }

    #[test]
    fn a_cancelled_higher_class_waiter_unblocks_lower_classes() {
        let gate = Gate::default();
        let held = gate.turn(Priority::Interactive, None).expect("held");
        let token = CancelToken::new();
        std::thread::scope(|s| {
            let tool = s.spawn(|| gate.turn(Priority::Tool, Some(&token)).map(|_| ()));
            std::thread::sleep(Duration::from_millis(30));
            let bulk = s.spawn(|| gate.turn(Priority::Bulk, None).map(|_| ()));
            std::thread::sleep(Duration::from_millis(30));
            token.cancel();
            assert!(tool.join().expect("tool").is_err());
            drop(held);
            assert!(bulk.join().expect("bulk").is_ok());
        });
    }
}
