//! `Renderer` for the GPU: one frame from request to pixels.

use super::{
    Adjustments, CompiledMask, GpuRenderer, ImageKey, OutputFormat, PipelineError, Priority,
    RawImage, RenderRequest, Renderer, TILE, TILE_PIXELS, lock, mask, plan,
};

/// Trims frame buffers above 256 MB when an export-sized render ends, however it ends.
struct TrimOnDrop<'a>(&'a GpuRenderer);

impl Drop for TrimOnDrop<'_> {
    fn drop(&mut self) {
        self.0.lock().ctx.trim_over(256 << 20);
    }
}

impl Renderer for GpuRenderer {
    fn render_frame_into(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        req: &RenderRequest,
        format: OutputFormat,
        out: &mut [u8],
    ) -> Result<crate::RenderedFrame, PipelineError> {
        let p = req.priority;
        // Content-aware removals: render the RAW with their fills composited in (computed
        // before taking a GPU slot: an inpaint must not hold up the class's queue).
        let removed = crate::removal::resolve(key, raw, adj, req)?;
        let (key, raw) = match &removed {
            Some((k, r)) => (*k, r.as_ref()),
            None => (key, raw),
        };
        let _bg = self.shared.gate.background(p, req.cancel.as_ref())?;
        req.check_cancel()?;
        self.prepare_statistics(key, raw, adj, req)?;
        // CPU preparation, outside any GPU turn.
        let mut plan = crate::par::run_for(p, || plan::build(raw, adj, req))?;
        if format == OutputFormat::Rgba16fLinearP3 {
            // Extended-range output keeps an f32 BASE (no f16 banding on EDR panels).
            plan.base_key.precise = true;
        }
        let size = plan.out_size();
        crate::check_out(size, format, out)?;
        // On this thread: the raster cache is locked only around lookups (compile's
        // parallel stamping runs on the pipeline pool with no lock held).
        let masks: Vec<CompiledMask> = plan
            .layers
            .iter()
            .map(|lp| {
                let l = &adj.layers[lp.index];
                mask::compile(
                    &l.mask,
                    l.id,
                    &plan.geo,
                    req.masks.as_ref(),
                    key.0,
                    &self.shared.rasters,
                )
            })
            .collect();
        let export = plan.resize.is_some() || plan.out_sharpen.is_some();
        let tiled_like = export
            || ((req.quality == crate::Quality::Full || p >= Priority::Background)
                && plan.window.is_none()
                && plan.upscale.is_none()
                && size.pixels() > TILE_PIXELS);
        let mut need = self.estimate(key, raw, &plan, format, tiled_like);
        if export {
            // The stripe and the resize intermediate.
            let (wd, hd) = (plan.geo.out.width, plan.geo.out.height);
            need += (u64::from(wd) * u64::from(TILE.min(hd))
                + u64::from(size.width) * u64::from(hd))
                * 8;
        }
        let _admitted = self.admit(key, need, req)?;
        self.stage_raw(key, raw, p)?;
        req.check_cancel()?;
        if plan.resize.is_some() || plan.out_sharpen.is_some() {
            return self.export_resized(key, raw, adj, req, &plan, &masks, format, out);
        }
        let tiled = (req.quality == crate::Quality::Full || p >= Priority::Background)
            && plan.window.is_none()
            && plan.upscale.is_none()
            && size.pixels() > TILE_PIXELS;
        if !tiled {
            // Interactive: BASE (if needed) and DEVELOP in one turn.
            let histogram = self.develop_into(
                key,
                raw,
                adj,
                req,
                &plan,
                &masks,
                format,
                out,
                [0, 0, size.width],
            )?;
            return Ok(crate::RenderedFrame { size, histogram });
        }
        // After a full-resolution export (finished, cancelled or failed), drop the
        // export-sized frame buffers; an interactive view reuses its buffers every frame
        // and must not pay this.
        let _trim = (req.quality == crate::Quality::Full).then(|| TrimOnDrop(self));
        // Per stripe: its BASE in bands (one turn each, waited for), then one turn per tile.
        let mut histogram: Option<crate::Histogram> = None;
        for ty in (0..size.height).step_by(TILE as usize) {
            let rows = TILE.min(size.height - ty);
            let stripe_plan =
                crate::par::run_for(p, || plan.tile(raw, adj, [0, ty, size.width, rows]));
            self.build_base_banded(key, raw, &stripe_plan, req, true)?;
            for tx in (0..size.width).step_by(TILE as usize) {
                req.check_cancel()?;
                let r = [
                    tx,
                    ty,
                    TILE.min(size.width - tx),
                    TILE.min(size.height - ty),
                ];
                let tile = crate::par::run_for(p, || plan.tile(raw, adj, r));
                let h = self.develop_into(
                    key,
                    raw,
                    adj,
                    req,
                    &tile,
                    &masks,
                    format,
                    out,
                    [tx, ty, size.width],
                )?;
                histogram = match (histogram, h) {
                    (Some(a), Some(b)) => Some(a.merge(&b)),
                    (a, b) => a.or(b),
                };
            }
        }
        Ok(crate::RenderedFrame { size, histogram })
    }

    fn evict(&mut self, key: ImageKey) {
        for k in std::iter::once(key).chain(crate::removal::forget(key)) {
            self.lock().evict(k);
            lock(&self.shared.rasters).evict(k.0);
        }
    }
}
