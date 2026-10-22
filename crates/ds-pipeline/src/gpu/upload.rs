//! RAW mosaic uploads: staging into pooled, parallel-copied buffers, and the per-image RAW slots.
use super::{
    Account, Arc, AssertUnwindSafe, CfaPattern, GpuRenderer, ImageKey, Inner, Ordering,
    PipelineError, Priority, RAW_SLOTS, RawImage, RawSlot, Shared, catch_unwind, context,
};

impl GpuRenderer {
    /// Upload `raw` unless resident; the copy into the upload runs outside any turn.
    pub(super) fn stage_raw(
        &self,
        key: ImageKey,
        raw: &RawImage,
        p: Priority,
    ) -> Result<(), PipelineError> {
        if raw.cfa == CfaPattern::LinearRgb {
            return Ok(());
        }
        // One upload per image at a time: a second thread staging the same image waits,
        // then finds it resident instead of copying another 50-120 MB.
        let _claim = self.claim_upload(key);
        let bytes: &[u8] = bytemuck::cast_slice(&raw.data);
        let size = (bytes.len() as u64).max(16).next_multiple_of(4);
        let (device, queue, max, epoch, mappable, lost, maps, pooled) = {
            let mut g = self.lock();
            // The upload runs before any turn: a loss must be noticed (and the device
            // rebuilt) here too, or a destroyed device fails every upload for good.
            self.recover_if_lost(&mut g)?;
            if p == Priority::Interactive {
                g.pinned = Some(key);
            }
            if g.raws.iter().any(|r| r.key == key) {
                return Ok(());
            }
            let pooled = g.pooled_raw(size);
            if pooled.is_none() {
                g.make_room((raw.data.len() * 2) as u64, key);
            }
            (
                g.ctx.device.clone(),
                g.ctx.queue.clone(),
                g.ctx.max_buffer,
                g.ctx.epoch,
                g.ctx.mappable,
                Arc::clone(&g.ctx.lost),
                Arc::clone(&g.ctx.maps),
                pooled,
            )
        };
        if size > max {
            return Err(PipelineError::Gpu(format!(
                "raw needs a {size}-byte buffer; this GPU allows {max}"
            )));
        }
        let buf = if mappable {
            // Unified memory: write straight into a mappable storage buffer (a pooled one
            // is already paged in), copying in parallel with no lock held.
            let buf = if let Some(slot) = pooled {
                context::map_write(&device, &lost, &slot.buf, size)?;
                slot.buf
            } else {
                catch_unwind(AssertUnwindSafe(|| {
                    device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("raw"),
                        size,
                        usage: context::STORAGE | wgpu::BufferUsages::MAP_WRITE,
                        mapped_at_creation: true,
                    })
                }))
                .map_err(|_| PipelineError::Gpu("RAW upload failed".into()))?
            };
            {
                // A destroyed device frees mapped memory: not while we copy.
                let destroyed = maps
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if *destroyed {
                    lost.store(true, Ordering::SeqCst);
                    return Err(PipelineError::Gpu("device lost".into()));
                }
                let mut view = buf
                    .slice(..size)
                    .get_mapped_range_mut()
                    .map_err(|e| PipelineError::Gpu(format!("{e:?}")))?;
                // Write-only (possibly write-combined) memory: 1 MiB chunks copied in
                // parallel (stores only, never reads of the mapping), the padding zeroed.
                let (mut head, mut tail) = view.slice(..).split_at(bytes.len());
                let base = head.as_raw_element_ptr().as_ptr() as usize;
                crate::par::run_for(p, || {
                    use rayon::prelude::*;
                    bytes.par_chunks(1 << 20).enumerate().for_each(|(k, src)| {
                        // SAFETY: `head` is the mapped range's first `bytes.len()` bytes,
                        // held mutably (and the device can't be destroyed: `destroyed`
                        // read guard) until after this call; chunk `k` writes exactly
                        // its own disjoint `src.len()` bytes at offset `k << 20`.
                        #[allow(unsafe_code)]
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                src.as_ptr(),
                                (base + (k << 20)) as *mut u8,
                                src.len(),
                            );
                        }
                    });
                });
                tail.fill(0);
            }
            buf.unmap();
            buf
        } else {
            catch_unwind(AssertUnwindSafe(|| {
                let b = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("raw"),
                    size,
                    usage: context::STORAGE,
                    mapped_at_creation: false,
                });
                if bytes.len().is_multiple_of(4) {
                    queue.write_buffer(&b, 0, bytes);
                } else {
                    let mut padded = bytes.to_vec();
                    padded.resize(bytes.len().next_multiple_of(4), 0);
                    queue.write_buffer(&b, 0, &padded);
                }
                b
            }))
            .map_err(|_| PipelineError::Gpu("RAW upload failed".into()))?
        };
        self.shared.uploads.fetch_add(1, Ordering::Relaxed);
        let mut g = self.lock();
        // A buffer of a device that was replaced meanwhile is dropped; the render then
        // uploads again inside its turn (or fails with the loss).
        if g.ctx.epoch == epoch {
            g.insert_raw(key, buf);
        }
        Ok(())
    }
}

/// Marks `key` as being uploaded until dropped.
pub(super) struct UploadClaim<'a>(&'a Shared, ImageKey);

impl Drop for UploadClaim<'_> {
    fn drop(&mut self) {
        lock_plain(&self.0.uploading.0).remove(&self.1);
        self.0.uploading.1.notify_all();
    }
}

fn lock_plain<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl GpuRenderer {
    /// Wait until no other thread is uploading `key`, then claim it.
    fn claim_upload(&self, key: ImageKey) -> UploadClaim<'_> {
        let (set, cv) = &self.shared.uploading;
        let mut s = lock_plain(set);
        while s.contains(&key) {
            s = cv
                .wait(s)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        s.insert(key);
        UploadClaim(&self.shared, key)
    }
}

impl Inner {
    /// Keep an uploaded RAW (most recently used last), evicting the oldest unpinned ones.
    pub(super) fn insert_raw(&mut self, key: ImageKey, buf: wgpu::Buffer) {
        self.raws.retain(|r| r.key != key);
        let bytes = buf.size();
        self.raws.push(RawSlot {
            key,
            buf,
            _acct: Account::new(crate::cache::Use::Raw, bytes),
        });
        while self.raws.len() > RAW_SLOTS {
            let Some(i) = self.raws.iter().position(|r| Some(r.key) != self.pinned) else {
                break;
            };
            let old = self.raws.remove(i);
            self.retire_raw(old);
        }
    }

    /// Keep a retired RAW upload for reuse (mappable buffers, at most two).
    pub(super) fn retire_raw(&mut self, slot: RawSlot) {
        if slot.buf.usage().contains(wgpu::BufferUsages::MAP_WRITE) && self.raw_pool.len() < 2 {
            self.raw_pool.push(slot);
        }
    }

    /// A pooled upload buffer of `size` bytes (or up to 1/8 more), if any.
    pub(super) fn pooled_raw(&mut self, size: u64) -> Option<RawSlot> {
        let i = self
            .raw_pool
            .iter()
            .position(|r| r.buf.size() >= size && r.buf.size() <= size + size / 8)?;
        Some(self.raw_pool.swap_remove(i))
    }

    pub(super) fn raw_buffer(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
    ) -> Result<wgpu::Buffer, PipelineError> {
        if let Some(pos) = self.raws.iter().position(|r| r.key == key) {
            let e = self.raws.remove(pos);
            let b = e.buf.clone();
            self.raws.push(e);
            return Ok(b);
        }
        let b = self
            .ctx
            .storage_init("raw", bytemuck::cast_slice(&raw.data))?;
        self.insert_raw(key, b.clone());
        Ok(b)
    }
}
