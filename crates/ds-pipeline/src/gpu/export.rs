//! Full-resolution exports: tiled develop into a stripe, resized and sharpened on the GPU.
use super::develop::{out_bytes, output_kernel};
use super::{
    Adjustments, Arc, CompiledMask, Finish, GpuRenderer, ImageKey, Inner, OutSlot, OutputFormat,
    Pending, PipelineError, Plan, RawImage, RenderRequest, TILE, lost_during, params,
};

impl GpuRenderer {
    /// Exports that resize (and/or output-sharpen): develop at full resolution in tiles
    /// (one turn each) into a full-width stripe, resize each stripe horizontally, then
    /// the vertical resize, output sharpening and output pass. Every turn is short and
    /// waited for before the next, so interactive frames and cancels never wait long.
    pub(super) fn export_resized(
        &self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        req: &RenderRequest,
        plan: &Plan,
        masks: &[CompiledMask],
        format: OutputFormat,
        out: &mut [u8],
    ) -> Result<crate::RenderedFrame, PipelineError> {
        let size = plan.out_size();
        let mut dev_plan = plan.clone();
        dev_plan.resize = None;
        dev_plan.out_sharpen = None;
        let (wd, hd) = (dev_plan.geo.out.width, dev_plan.geo.out.height);
        let ((stripe, _sa), (tmp, _ta), epoch) = self.turn(req, |inner| {
            inner.make_room(
                (u64::from(wd) * u64::from(TILE.min(hd)) + u64::from(size.width) * u64::from(hd))
                    * 8,
                key,
            );
            let s = inner
                .ctx
                .owned_buffer("rs_stripe", u64::from(wd) * u64::from(TILE.min(hd)) * 8)?;
            let t = inner
                .ctx
                .owned_buffer("rs_tmp", u64::from(size.width) * u64::from(hd) * 8)?;
            Ok((s, t, inner.ctx.epoch))
        })?;
        let same_device = |inner: &Inner| {
            if inner.ctx.epoch == epoch {
                Ok(())
            } else {
                Err(lost_during("export"))
            }
        };
        for ty in (0..hd).step_by(TILE as usize) {
            let rows = TILE.min(hd - ty);
            // Only this stripe's BASE (plus its context) is resident.
            let stripe_plan =
                crate::par::run_for(req.priority, || dev_plan.tile(raw, adj, [0, ty, wd, rows]));
            self.build_base_banded(key, raw, &stripe_plan, req, true)?;
            for tx in (0..wd).step_by(TILE as usize) {
                req.check_cancel()?;
                let tile = crate::par::run_for(req.priority, || {
                    dev_plan.tile(raw, adj, [tx, ty, TILE.min(wd - tx), rows])
                });
                let pending = self.turn(req, |inner| {
                    same_device(inner)?;
                    inner.develop(
                        key,
                        raw,
                        adj,
                        &tile,
                        masks,
                        &self.shared.rasters,
                        format,
                        Finish::Store {
                            dst: &stripe,
                            at: [tx, 0, wd],
                        },
                    )
                })?;
                Self::wait_stored(pending, req)?;
            }
            req.check_cancel()?;
            let pending = self.turn(req, |inner| {
                same_device(inner)?;
                Ok(inner.resize_stripe(&stripe, wd, rows, &tmp, size.width, ty))
            })?;
            Self::wait_stored(pending, req)?;
        }
        req.check_cancel()?;
        let histogram = self.read_into(
            key,
            req,
            size,
            format,
            out,
            [0, 0, size.width],
            Some(epoch),
            |inner, slot| inner.finish_export(plan, &tmp, hd, format, slot, req.histogram),
        )?;
        Ok(crate::RenderedFrame { size, histogram })
    }
}

impl Inner {
    /// Export resize, horizontal pass for one developed stripe (`rows` rows of a `wd`
    /// wide stripe) into rows `row0..` of the `tw` wide linear intermediate.
    pub(super) fn resize_stripe(
        &mut self,
        stripe: &wgpu::Buffer,
        wd: u32,
        rows: u32,
        tmp: &wgpu::Buffer,
        tw: u32,
        row0: u32,
    ) -> Pending {
        let mut cmd = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let u = self.ctx.uniform(&params::block(&[[
            wd as f32,
            rows as f32,
            tw as f32,
            row0 as f32,
        ]]));
        self.ctx
            .dispatch(&mut cmd, "resize_h", &[&u, stripe, tmp], tw, rows);
        self.ctx.queue.submit([cmd.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        self.ctx.queue.on_submitted_work_done(move || {
            let _ = tx.send(());
        });
        Pending::Stored(rx, self.ctx.device.clone(), Arc::clone(&self.ctx.lost))
    }

    /// Export resize, vertical pass + output sharpening + output transform (+ histogram)
    /// into `slot`.
    pub(super) fn finish_export(
        &mut self,
        plan: &Plan,
        tmp: &wgpu::Buffer,
        hd: u32,
        format: OutputFormat,
        slot: OutSlot,
        histogram: bool,
    ) -> Result<Pending, PipelineError> {
        self.ctx.begin_frame();
        let to = plan.out_size();
        let (tw, th) = (to.width, to.height);
        let n = to.pixels();
        let mut cmd = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let ob = self.ctx.buf("rs_enc", n * 8)?;
        let yp = self.ctx.buf("rs_luma", n * 4)?;
        let uv = self
            .ctx
            .uniform(&params::block(&[[hd as f32, tw as f32, th as f32, 0.0]]));
        self.ctx
            .dispatch(&mut cmd, "resize_v", &[&uv, tmp, &ob, &yp], tw, th);
        let blur = match &plan.out_sharpen {
            Some(os) => {
                let t = self.ctx.buf("rs_t", n * 4)?;
                let o = self.ctx.buf("rs_blur", n * 4)?;
                self.sep(&mut cmd, &yp, &t, &o, tw, th, &os.kernel, 1);
                o
            }
            None => self.ctx.dummy(),
        };
        // The stripes hold output-space, gamut-mapped values already: the last pass only
        // encodes (no matrix, no second gamut compression).
        let dev = self.ctx.uniform(&params::out_block(
            to,
            &crate::color::IDENTITY,
            plan.out_sharpen.as_ref(),
            histogram,
            true,
        ));
        let hist = if histogram {
            let hb = self
                .ctx
                .buf("hist", crate::Histogram::GPU_WORDS as u64 * 4)?;
            cmd.clear_buffer(&hb, 0, None);
            hb
        } else {
            self.ctx.dummy_rw()
        };
        let px_bytes = out_bytes(format, to);
        let target = self.output_target(format, to, &slot.gpu)?;
        // The resized image is 16-bit fixed point: the `_h` output kernels read it.
        self.ctx.dispatch(
            &mut cmd,
            &format!("{}_h", output_kernel(format, histogram)),
            &[&dev, &ob, &blur, &target, &hist],
            tw,
            th,
        );
        self.pack_output(&mut cmd, format, to, &target, &slot.gpu);
        let bytes = if histogram {
            let hb = crate::Histogram::GPU_WORDS as u64 * 4;
            cmd.copy_buffer_to_buffer(&hist, 0, &slot.gpu, px_bytes, hb);
            px_bytes + hb
        } else {
            px_bytes
        };
        Ok(Pending::Frame(self.ctx.submit_readback(cmd, slot, bytes)))
    }
}
