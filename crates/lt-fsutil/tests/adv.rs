//! Reviewer's lock-race repro (N7), adapted. The original interleaving was
//! A holds → B opens → A unlocks → B flocks → A *unlinks the lock file* →
//! C creates a new file and locks it too. lt-import now locks the destination
//! directory itself (`DirLock::try_lock_dir`), so there is nothing to unlink;
//! the same interleaving must leave exactly one holder.

#[test]
fn lock_race_leaves_exactly_one_holder() {
    let t = tempfile::tempdir().unwrap();
    let dest = t.path();
    let a = lt_fsutil::DirLock::try_lock_dir(dest, "import")
        .unwrap()
        .unwrap(); // run A holds it
    let b_fd = std::fs::File::open(dest).unwrap(); // B: open() inside try_lock_dir
    drop(a); // A finishes
    rustix::fs::flock(&b_fd, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap(); // B: flock() succeeds
    // (A used to remove its lock file here; a directory cannot be swapped out
    // from under the lock while it is the destination.)
    let c = lt_fsutil::DirLock::try_lock_dir(dest, "import").unwrap(); // C (or A's next import)
    assert!(
        c.is_none(),
        "B and C both hold the import lock on one destination"
    );
}
