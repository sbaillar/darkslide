//! BASE levels on the GPU: demosaic builds (banded, so background work yields) and the BASE cache.
use super::{
    Arc, BaseJob, CfaPattern, DemosaicMode, GpuBase, GpuRenderer, ImageKey, Inner, PipelineError,
    Plan, RawImage, RenderRequest, build_base, build_rect, context, f16, level_need, lost_during,
    params, plan,
};

impl GpuRenderer {
    /// Build the BASE for `plan` (if not cached) in bands, one turn and one wait each.
    /// `exact`: build exactly what `plan` needs (a stripe of a tiled render), replacing
    /// the image's entry for that level first so two never coexist.
    pub(super) fn build_base_banded(
        &self,
        key: ImageKey,
        raw: &RawImage,
        plan: &Plan,
        req: &RenderRequest,
        exact: bool,
    ) -> Result<(), PipelineError> {
        let job = self.turn(req, |inner| match inner.cached_base(key, plan) {
            Some(_) => Ok(None),
            None if exact && plan.base_key.mode == DemosaicMode::Rcd && plan.base_key.down == 0 => {
                inner.bases.evict_level(key, plan.base_key);
                let need = level_need(plan, plan.level_size);
                inner.begin_base_rect(key, raw, plan, need).map(Some)
            }
            None => inner.begin_base(key, raw, plan).map(Some),
        })?;
        if let Some(mut job) = job {
            while !job.done() {
                req.check_cancel()?;
                let (done, device, lost) = self.turn(req, |inner| {
                    inner.base_step(&mut job, raw)?;
                    let (tx, rx) = std::sync::mpsc::channel();
                    inner.ctx.queue.on_submitted_work_done(move || {
                        let _ = tx.send(());
                    });
                    Ok((rx, inner.ctx.device.clone(), Arc::clone(&inner.ctx.lost)))
                })?;
                context::wait_for(&device, &lost, &done, req.cancel.as_ref())?;
            }
            self.turn(req, |inner| inner.finish_base(job, raw).map(|_| ()))?;
        }
        Ok(())
    }
}

/// Rows of an RCD band over a `width`-pixel rectangle: ~2 Mpx, one short GPU submission
/// (~4 ms at f32).
pub(super) fn rcd_band(width: u32) -> u32 {
    (2_000_000 / width.max(1)).max(64) & !1
}

/// The RCD work buffers (pooled name, bytes) of a band of `n` pixels, margin included.
pub(super) fn rcd_bufs(n: u64) -> [(&'static str, u64); 4] {
    [
        ("rcd_cfa", n * 4),
        ("rcd_dirs", n * 16),
        ("rcd_green", n * 4),
        ("rcd_rb", n * 8),
    ]
}

impl Inner {
    pub(super) fn new_base_buffer(
        &self,
        w: u32,
        h: u32,
        precise: bool,
    ) -> Result<wgpu::Buffer, PipelineError> {
        let bpp = if precise { 16 } else { 8 };
        self.ctx
            .new_buffer("base", u64::from(w) * u64::from(h) * bpp)
    }

    /// The resident BASE for `plan`, if cached.
    pub(super) fn cached_base(
        &mut self,
        key: ImageKey,
        plan: &Plan,
    ) -> Option<Arc<crate::cache::Tracked<GpuBase>>> {
        let need = level_need(plan, plan.level_size);
        let epoch = self.ctx.epoch;
        self.bases
            .get(key, plan.base_key, need)
            .filter(|b| b.get().epoch == epoch)
    }

    /// Resident BASE for this plan, building it (in one go) on a miss.
    pub(super) fn base(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
        plan: &Plan,
    ) -> Result<Arc<crate::cache::Tracked<GpuBase>>, PipelineError> {
        if let Some(b) = self.cached_base(key, plan) {
            return Ok(b);
        }
        let mut job = self.begin_base(key, raw, plan)?;
        while !job.done() {
            self.base_step(&mut job, raw)?;
        }
        self.finish_base(job, raw)
    }

    /// Start building the BASE for `plan`: allocate it (a linear-RGB input is converted
    /// right away on the CPU); the demosaic runs in [`Self::base_step`] bands.
    pub(super) fn begin_base(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
        plan: &Plan,
    ) -> Result<BaseJob, PipelineError> {
        let need = level_need(plan, plan.level_size);
        let rect = build_rect(plan.base_key, need, plan.level_size);
        self.begin_base_rect(key, raw, plan, rect)
    }

    /// [`Self::begin_base`] for an explicit level rectangle.
    pub(super) fn begin_base_rect(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
        plan: &Plan,
        rect: [u32; 4],
    ) -> Result<BaseJob, PipelineError> {
        let bk = plan.base_key;
        if raw.cfa == CfaPattern::LinearRgb {
            // Already-demosaiced input (linear DNG): a per-pixel map on the CPU, on the
            // pipeline pool. Never on the caller's pool while this lock is held: its
            // workers may be renders blocked on this lock or on this render's gate slot
            // (the lt-ffi suite hung that way; `tests/gate_deadlock.rs`).
            let img = crate::par::run_holding_renderer(|| build_base(raw, plan, rect));
            let data: Vec<u8> = if bk.precise {
                bytemuck::cast_slice(
                    &img.px
                        .iter()
                        .map(|p| [p[0], p[1], p[2], 0.0])
                        .collect::<Vec<[f32; 4]>>(),
                )
                .to_vec()
            } else {
                let packed: Vec<u16> = img
                    .px
                    .iter()
                    .flat_map(|p| [p[0], p[1], p[2], 0.0].map(|v| f16::from_f32(v).to_bits()))
                    .collect();
                bytemuck::cast_slice(&packed).to_vec()
            };
            return Ok(BaseJob {
                key,
                bk,
                base_params: plan.base_params,
                rect: [img.x0, img.y0, img.w, img.h],
                buf: self.ctx.storage_init("base", &data)?,
                next_row: img.h,
                band: img.h.max(1),
                linear: true,
                epoch: self.ctx.epoch,
            });
        }
        // The demosaic works at level 0: a sub-rectangle only for full-resolution RCD
        // levels (`build_rect`), the whole mosaic whenever pyramid levels follow.
        let rect = match bk.mode {
            DemosaicMode::Superpixel => [0, 0, raw.size.width / 2, raw.size.height / 2],
            DemosaicMode::Rcd if bk.down == 0 => rect,
            DemosaicMode::Rcd => [0, 0, raw.size.width, raw.size.height],
        };
        let (w, h) = (rect[2], rect[3]);
        self.make_room(
            u64::from(w) * u64::from(h) * if bk.precise { 16 } else { 8 },
            key,
        );
        let buf = self.new_base_buffer(w, h, bk.precise)?;
        // RCD bands of ~2 Mpx: each is one short GPU submission (~4 ms at f32).
        let band = match bk.mode {
            DemosaicMode::Superpixel => h.max(1),
            DemosaicMode::Rcd => rcd_band(rect[2]).min(h.max(2)),
        };
        Ok(BaseJob {
            key,
            bk,
            base_params: plan.base_params,
            rect,
            buf,
            next_row: 0,
            band,
            linear: false,
            epoch: self.ctx.epoch,
        })
    }

    /// Record and submit the next band of the demosaic.
    pub(super) fn base_step(
        &mut self,
        job: &mut BaseJob,
        raw: &RawImage,
    ) -> Result<(), PipelineError> {
        // A job begun on a device that was replaced since: its BASE buffer belongs to
        // the lost device, so nothing may be recorded against it on the new one.
        if job.epoch != self.ctx.epoch {
            return Err(lost_during("BASE build"));
        }
        let rawbuf = self.raw_buffer(job.key, raw)?;
        let bk = job.bk;
        let fmt = if bk.precise { "_f" } else { "_h" };
        let k = |n: &str| format!("{n}{fmt}");
        let mut enc = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let [rx, ry, rw, rh] = job.rect;
        match bk.mode {
            DemosaicMode::Superpixel => {
                let u = self.ctx.uniform(&params::base_block(
                    raw,
                    &job.base_params,
                    rw,
                    rh,
                    [0; 4],
                    [0; 4],
                ));
                self.ctx
                    .dispatch(&mut enc, &k("base_super"), &[&u, &rawbuf, &job.buf], rw, rh);
                job.next_row = rh;
            }
            DemosaicMode::Rcd => {
                // This band's rows plus a margin so RCD neighbourhoods are real data.
                let (y0, bh) = (job.next_row, job.band.min(rh - job.next_row));
                let band = [rx, ry + y0, rw, bh];
                let (sw, sh) = (raw.size.width, raw.size.height);
                let m = plan::BBOX_MARGIN;
                let gx0 = band[0].saturating_sub(m) & !1;
                let gy0 = band[1].saturating_sub(m) & !1;
                let gw = (band[0] + band[2] + m).min(sw) - gx0;
                let gh = (band[1] + band[3] + m).min(sh) - gy0;
                let n = u64::from(gw) * u64::from(gh);
                // Work buffers beyond the reservation (a first band, or another render
                // trimmed them): room first.
                let bufs = rcd_bufs(n);
                self.make_room(self.ctx.shortfall(&bufs), job.key);
                let [cfa, dirs, green, rb] = bufs.map(|(name, bytes)| self.ctx.buf(name, bytes));
                let (cfa, dirs, green, rb) = (cfa?, dirs?, green?, rb?);
                let u = self.ctx.uniform(&params::base_block(
                    raw,
                    &job.base_params,
                    gw,
                    gh,
                    [gx0, gy0, band[0] - gx0, band[1] - gy0],
                    [band[2], band[3], y0, rw],
                ));
                self.ctx
                    .dispatch(&mut enc, "rcd_cfa", &[&u, &cfa, &rawbuf], gw, gh);
                self.ctx
                    .dispatch(&mut enc, "rcd_dir", &[&u, &cfa, &dirs], gw, gh);
                self.ctx
                    .dispatch(&mut enc, "rcd_green", &[&u, &cfa, &dirs, &green], gw, gh);
                self.ctx
                    .dispatch(&mut enc, "rcd_rb", &[&u, &cfa, &dirs, &green, &rb], gw, gh);
                self.ctx.dispatch(
                    &mut enc,
                    &k("rcd_final"),
                    &[&u, &cfa, &dirs, &green, &rb, &job.buf],
                    band[2],
                    band[3],
                );
                job.next_row += bh;
            }
        }
        self.ctx.queue.submit([enc.finish()]);
        Ok(())
    }

    /// Pyramid levels, then cache the finished BASE.
    pub(super) fn finish_base(
        &mut self,
        job: BaseJob,
        raw: &RawImage,
    ) -> Result<Arc<crate::cache::Tracked<GpuBase>>, PipelineError> {
        if job.epoch != self.ctx.epoch {
            return Err(lost_during("BASE build"));
        }
        let bk = job.bk;
        let [x0, y0, mut w, mut h] = job.rect;
        let mut cur = job.buf;
        if !job.linear {
            let fmt = if bk.precise { "_f" } else { "_h" };
            let mut enc = self
                .ctx
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            for _ in 0..bk.down {
                let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
                let Ok(next) = self.new_base_buffer(nw, nh, bk.precise) else {
                    break;
                };
                let u = self.ctx.uniform(&params::block(&[[
                    nw as f32, nh as f32, w as f32, h as f32,
                ]]));
                self.ctx.dispatch(
                    &mut enc,
                    &format!("base_down{fmt}"),
                    &[&u, &cur, &next],
                    nw,
                    nh,
                );
                cur = next;
                (w, h) = (nw, nh);
            }
            self.ctx.queue.submit([enc.finish()]);
            // RCD work buffers of a large (full-frame) build are not worth keeping around;
            // 1:1 region builds keep theirs for the next pan.
            if bk.mode == DemosaicMode::Rcd
                && u64::from(job.rect[2]) * u64::from(job.rect[3]) * 4
                    >= u64::from(raw.size.width) * u64::from(raw.size.height)
            {
                self.ctx.drop_pooled("rcd_");
            }
        }
        let base = GpuBase {
            buf: cur,
            w,
            h,
            x0: if bk.mode == DemosaicMode::Rcd { x0 } else { 0 },
            y0: if bk.mode == DemosaicMode::Rcd { y0 } else { 0 },
            precise: bk.precise,
            epoch: job.epoch,
        };
        Ok(self.bases.insert(job.key, bk, base))
    }
}
