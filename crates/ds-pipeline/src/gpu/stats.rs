//! Auto tools and prefetch: scene statistics from the GPU (block means, histograms).
use super::{
    Adjustments, Arc, CfaPattern, Exposure, GpuRenderer, Hdr, ImageKey, Levels, OutputFormat,
    PipelineError, Priority, RawImage, RenderRequest, Renderer, WhiteBalance, build_rect, context,
    level_need, params, plan,
};

impl GpuRenderer {
    /// Prefetch the BASE a (region) request will need, in the background, so a pan that
    /// leaves the cached rectangle does not stall on a synchronous rebuild. The new
    /// rectangle also covers the one currently cached (the view in progress keeps
    /// working); it replaces it once built. Returns whether anything was built.
    pub fn prefetch(
        &self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        req: &RenderRequest,
    ) -> Result<bool, PipelineError> {
        let req = RenderRequest {
            priority: Priority::Background,
            ..req.clone()
        };
        let removed = crate::removal::resolve(key, raw, adj, &req)?;
        let (key, raw) = match &removed {
            Some((k, r)) => (*k, r.as_ref()),
            None => (key, raw),
        };
        let _bg = self
            .shared
            .gate
            .background(req.priority, req.cancel.as_ref())?;
        let plan = crate::par::run_for(req.priority, || plan::build(raw, adj, &req))?;
        self.stage_raw(key, raw, Priority::Background)?;
        let job = self.turn(&req, |inner| {
            if inner.cached_base(key, &plan).is_some() {
                return Ok(None);
            }
            let full = plan.level_size;
            let need = level_need(&plan, full);
            let mut rect = build_rect(plan.base_key, need, full);
            if let Some(have) = inner.bases.rect_of(key, plan.base_key) {
                // Union with what the current view uses.
                let x0 = rect[0].min(have[0]);
                let y0 = rect[1].min(have[1]);
                let x1 = (rect[0] + rect[2]).max(have[0] + have[2]);
                let y1 = (rect[1] + rect[3]).max(have[1] + have[3]);
                rect = [x0, y0, x1 - x0, y1 - y0];
                if u64::from(rect[2]) * u64::from(rect[3]) * 10
                    > u64::from(full[0]) * u64::from(full[1]) * 6
                {
                    rect = [0, 0, full[0], full[1]];
                }
            }
            inner.begin_base_rect(key, raw, &plan, rect).map(Some)
        })?;
        let Some(mut job) = job else {
            return Ok(false);
        };
        while !job.done() {
            req.check_cancel()?;
            let (done, device, lost) = self.turn(&req, |inner| {
                inner.base_step(&mut job, raw)?;
                let (tx, rx) = std::sync::mpsc::channel();
                inner.ctx.queue.on_submitted_work_done(move || {
                    let _ = tx.send(());
                });
                Ok((rx, inner.ctx.device.clone(), Arc::clone(&inner.ctx.lost)))
            })?;
            context::wait_for(&device, &lost, &done, req.cancel.as_ref())?;
        }
        self.turn(&req, |inner| inner.finish_base(job, raw).map(|_| ()))?;
        Ok(true)
    }

    /// Per-image statistics a new photo needs, off the critical path: the sensor noise
    /// (CPU) is measured while the RAW uploads, and the dehaze light comes from GPU block
    /// means of the uploaded mosaic. Both are cached per image.
    pub(super) fn prepare_statistics(
        &self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        req: &RenderRequest,
    ) -> Result<(), PipelineError> {
        if raw.cfa == CfaPattern::LinearRgb || raw.validate().is_err() {
            return Ok(());
        }
        let p = req.priority;
        // The upload is admitted against the memory budget like a render (it runs before
        // the render's own admission): while it copies, other renders see its bytes.
        let resident = self.lock().raws.iter().any(|r| r.key == key);
        let _admitted = if resident {
            None
        } else {
            Some(self.admit(key, (raw.data.len() * 2) as u64, req)?)
        };
        if plan::raw_noise_known(raw) {
            self.stage_raw(key, raw, p)?;
        } else {
            std::thread::scope(|s| {
                let noise = s.spawn(|| crate::par::run_for(p, || plan::raw_noise(raw)));
                let staged = self.stage_raw(key, raw, p);
                let _ = noise.join();
                staged
            })?;
        }
        let dehaze = adj.presence.dehaze != 0.0
            || adj
                .layers
                .iter()
                .any(|l| l.enabled && l.adjustments.presence.dehaze != 0.0);
        if dehaze
            && !plan::airlight_known(raw)
            && let Ok(blocks) = self.camera_blocks(key, raw)
        {
            plan::seed_airlight(raw, blocks);
        }
        Ok(())
    }

    /// Camera-space block means for the auto tools, computed on the GPU from the
    /// uploaded mosaic (see `auto::camera_blocks`).
    pub(super) fn camera_blocks(
        &self,
        key: ImageKey,
        raw: &RawImage,
    ) -> Result<Vec<[f32; 3]>, PipelineError> {
        if raw.cfa == CfaPattern::LinearRgb {
            return Ok(crate::auto::camera_blocks(raw, 2));
        }
        let req = RenderRequest {
            priority: Priority::Tool,
            ..RenderRequest::default()
        };
        self.stage_raw(key, raw, Priority::Tool)?;
        let block = crate::auto::block_size(raw) as u32;
        let (bw, bh) = (raw.size.width / block, raw.size.height / block);
        let bytes = u64::from(bw) * u64::from(bh) * 16;
        if bytes == 0 {
            return Ok(Vec::new());
        }
        let rb = self.turn(&req, |inner| {
            let rawbuf = inner.raw_buffer(key, raw)?;
            let slot = inner.ctx.out_slot(bytes)?;
            let si = crate::demosaic::site_info(raw);
            let u = inner.ctx.uniform(&params::block(&[
                [
                    raw.size.width as f32,
                    raw.size.height as f32,
                    block as f32,
                    bw as f32,
                ],
                si.color.map(|c| c as f32),
                si.black,
                si.inv_range,
            ]));
            let mut cmd = inner
                .ctx
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            inner
                .ctx
                .dispatch(&mut cmd, "blocks", &[&u, &rawbuf, &slot.gpu], bw, bh);
            Ok(inner.ctx.submit_readback(cmd, slot, bytes))
        })?;
        let mut blocks = Vec::new();
        rb.finish(None, |b| {
            for c in b.as_chunks::<16>().0 {
                let f = |i: usize| f32::from_ne_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]);
                if f(12) > 0.5 {
                    blocks.push([f(0), f(4), f(8)]);
                }
            }
        })?;
        Ok(blocks)
    }

    /// Camera-space block means of `raw` (see `auto::camera_blocks`) computed on the GPU
    /// from the resident mosaic, for [`crate::optimize::analyze`]; the CPU computes them
    /// if the GPU fails. `key` must identify `raw`, as for [`Self::auto_white_balance`].
    pub fn block_means(&self, key: ImageKey, raw: &RawImage) -> Vec<[f32; 3]> {
        if raw.validate().is_err() {
            return Vec::new();
        }
        self.camera_blocks(key, raw)
            .unwrap_or_else(|_| crate::par::run_low(|| crate::auto::block_means(raw)))
    }

    /// [`crate::auto::auto_white_balance`] with its statistics computed on the GPU (falls
    /// back to the CPU if the GPU fails).
    ///
    /// The statistics come from the mosaic resident under `key` (uploaded on first use,
    /// like a render's): `key` must identify `raw`, as for [`Renderer::render`]. Two
    /// different raws under one key get the first one's answer until the key is evicted.
    pub fn auto_white_balance(&self, key: ImageKey, raw: &RawImage) -> WhiteBalance {
        if raw.validate().is_err() {
            return crate::auto::auto_white_balance(raw);
        }
        match self.camera_blocks(key, raw) {
            Ok(b) => crate::auto::wb_from_blocks(raw, &b),
            Err(_) => crate::auto::auto_white_balance(raw),
        }
    }

    /// [`crate::auto::auto_levels`] with its statistics (camera blocks, and the probe
    /// render with its histogram) computed on the GPU.
    pub fn auto_levels(
        &self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
    ) -> (Exposure, Hdr, Levels) {
        if raw.validate().is_err() {
            return crate::auto::auto_levels(raw, adj);
        }
        let Ok(blocks) = self.camera_blocks(key, raw) else {
            return crate::auto::auto_levels(raw, adj);
        };
        let mut me = self.clone();
        crate::auto::levels_from(raw, adj, &blocks, |probe, req| {
            let req = RenderRequest {
                histogram: true,
                priority: Priority::Tool,
                ..req.clone()
            };
            let size = crate::output_size(raw, probe, &req).ok()?;
            let mut out = vec![0u8; size.pixels() as usize * 4];
            me.render_frame_into(key, raw, probe, &req, OutputFormat::Rgba8, &mut out)
                .ok()?
                .histogram
        })
    }
}
