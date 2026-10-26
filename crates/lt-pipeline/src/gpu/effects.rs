//! Spatial effect passes of DEVELOP: guided filters, lens blur, glow, box and separable blurs.
use super::{GuidedParams, Inner, PipelineError, params};

impl Inner {
    /// Self-guided filter coefficients over `src.w`, into pooled buffer `out`.
    pub(super) fn guided_self(
        &mut self,
        enc: &mut wgpu::CommandEncoder,
        src: &wgpu::Buffer,
        out: &'static str,
        down: &str,
        w: u32,
        h: u32,
        gp: GuidedParams,
    ) -> Result<wgpu::Buffer, PipelineError> {
        let s = gp.s as u32;
        let (ox, oy) = (gp.off[0] as u32, gp.off[1] as u32);
        let (lw, lh) = ((w + ox).div_ceil(s), (h + oy).div_ceil(s));
        let bytes = u64::from(lw) * u64::from(lh) * 16;
        let a = self.ctx.buf("g_a", bytes)?;
        let b = self.ctx.buf("g_b", bytes)?;
        let o = self.ctx.buf(out, bytes)?;
        let ud = self.ctx.uniform(&params::block(&[
            [w as f32, h as f32, s as f32, 0.0],
            [lw as f32, lh as f32, ox as f32, oy as f32],
        ]));
        self.ctx.dispatch(enc, down, &[&ud, src, &a], lw, lh);
        self.box2(enc, &a, &b, &a, lw, lh, gp.r);
        let uc = self
            .ctx
            .uniform(&params::block(&[[lw as f32, lh as f32, gp.eps, 0.0]]));
        self.ctx.dispatch(enc, "gcoef_self", &[&uc, &a, &b], lw, lh);
        self.box2(enc, &b, &a, &o, lw, lh, gp.r);
        Ok(o)
    }

    /// Haze-estimate coefficients (mirrors `local::guided_haze`): block means of the
    /// guide and the block-mean dark channel relative to `air`, a 3×3-block min, a
    /// cross guided filter; into pooled buffer `coef_dhz`.
    pub(super) fn guided_haze(
        &mut self,
        enc: &mut wgpu::CommandEncoder,
        src: &wgpu::Buffer,
        w: u32,
        h: u32,
        gp: GuidedParams,
        air: [f32; 3],
    ) -> Result<wgpu::Buffer, PipelineError> {
        let s = gp.s as u32;
        let (ox, oy) = (gp.off[0] as u32, gp.off[1] as u32);
        let (lw, lh) = ((w + ox).div_ceil(s), (h + oy).div_ceil(s));
        let bytes = u64::from(lw) * u64::from(lh) * 16;
        let a = self.ctx.buf("g_a", bytes)?;
        let b = self.ctx.buf("g_b", bytes)?;
        let o = self.ctx.buf("coef_dhz", bytes)?;
        let ud = self.ctx.uniform(&params::block(&[
            [w as f32, h as f32, s as f32, 0.0],
            [lw as f32, lh as f32, ox as f32, oy as f32],
            [air[0], air[1], air[2], 0.0],
        ]));
        self.ctx
            .dispatch(enc, "gdown_haze", &[&ud, src, &a], lw, lh);
        let uc = self
            .ctx
            .uniform(&params::block(&[[lw as f32, lh as f32, gp.eps, 0.0]]));
        self.ctx.dispatch(enc, "haze_patch", &[&uc, &a, &b], lw, lh);
        self.box2(enc, &b, &a, &b, lw, lh, gp.r);
        self.ctx.dispatch(enc, "gcoef_haze", &[&uc, &b, &a], lw, lh);
        self.box2(enc, &a, &b, &o, lw, lh, gp.r);
        Ok(o)
    }

    /// Lens blur of the scene-linear image `lin` in place (see [`crate::effects`]):
    /// circle of confusion, gather grid, occlusion-aware gather, composite.
    pub(super) fn lens_blur(
        &mut self,
        cmd: &mut wgpu::CommandEncoder,
        dev: &wgpu::Buffer,
        lin: &wgpu::Buffer,
        lb: &crate::effects::LensBlurPlan,
        w: u32,
        h: u32,
    ) -> Result<(), PipelineError> {
        let n = u64::from(w) * u64::from(h);
        let blk = params::bokeh_block(lb, w, h);
        let depth = match &lb.source {
            crate::effects::CocSource::Depth { map, .. } => {
                self.ctx.storage_init("depth", &params::depth_words(map))?
            }
            crate::effects::CocSource::Plane { .. } => self.ctx.dummy(),
        };
        let coc = self.ctx.buf("bk_coc", n * 4)?;
        let u = self.ctx.uniform(&blk);
        self.ctx
            .dispatch(cmd, "bokeh_coc", &[dev, &u, &depth, &coc], w, h);
        let (lw, lh) = lb.grid(w as usize, h as usize);
        let (lw, lh) = (lw as u32, lh as u32);
        let cells = u64::from(lw) * u64::from(lh);
        let lo = self.ctx.buf("bk_lo", cells * 16)?;
        let loz = self.ctx.buf("bk_loz", cells * 4)?;
        let gathered = self.ctx.buf("bk_out", cells * 16)?;
        let ud = self.ctx.uniform(&params::rows_from(&blk, 6));
        self.ctx
            .dispatch(cmd, "bokeh_down", &[&ud, lin, &coc, &lo, &loz], lw, lh);
        let ug = self.ctx.uniform(&params::rows_from(&blk, 4));
        self.ctx.dispatch(
            cmd,
            "bokeh_gather",
            &[dev, &ug, &lo, &loz, &gathered],
            lw,
            lh,
        );
        self.ctx
            .dispatch(cmd, "bokeh_apply", &[&ud, lin, &coc, &gathered], w, h);
        Ok(())
    }

    /// Glow and halation on the develop image `encb` (and its luma plane) in place.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn glow_halation(
        &mut self,
        cmd: &mut wgpu::CommandEncoder,
        encb: &wgpu::Buffer,
        ypl: &wgpu::Buffer,
        fx: &crate::effects::FxPlan,
        w: u32,
        h: u32,
        k: &dyn Fn(&str) -> String,
    ) -> Result<(), PipelineError> {
        let mut plane = |ov: Option<&crate::effects::BlurOverlay>,
                         mode: f32,
                         names: [&'static str; 2]|
         -> Result<wgpu::Buffer, PipelineError> {
            let Some(ov) = ov else {
                return Ok(self.ctx.dummy());
            };
            let (lw, lh) = ov.grid(w as usize, h as usize);
            let bytes = (lw * lh) as u64 * 16;
            let low = self.ctx.buf(names[0], bytes)?;
            let tmp = self.ctx.buf("fx_tmp", bytes)?;
            let blurred = self.ctx.buf(names[1], bytes)?;
            let u = self.ctx.uniform(&params::overlay_block(ov, w, h, mode));
            self.ctx
                .dispatch(cmd, &k("fx_down"), &[&u, encb, &low], lw as u32, lh as u32);
            let uh = self
                .ctx
                .uniform(&params::blur4_block(lw, lh, &ov.kernel, false));
            let uv = self
                .ctx
                .uniform(&params::blur4_block(lw, lh, &ov.kernel, true));
            self.ctx
                .dispatch(cmd, "fx_blur", &[&uh, &low, &tmp], lw as u32, lh as u32);
            self.ctx
                .dispatch(cmd, "fx_blur", &[&uv, &tmp, &blurred], lw as u32, lh as u32);
            Ok(blurred)
        };
        let glow = plane(fx.glow.as_ref(), 0.0, ["fx_glow_lo", "fx_glow"])?;
        let hal = plane(fx.halation.as_ref(), 1.0, ["fx_hal_lo", "fx_hal"])?;
        let u = self.ctx.uniform(&params::fx_apply_block(fx, w, h));
        self.ctx
            .dispatch(cmd, &k("fx_apply"), &[&u, encb, &glow, &hal, ypl], w, h);
        Ok(())
    }

    /// Separable box blur `src → tmp → dst` on a vec4 grid.
    pub(super) fn box2(
        &self,
        enc: &mut wgpu::CommandEncoder,
        src: &wgpu::Buffer,
        tmp: &wgpu::Buffer,
        dst: &wgpu::Buffer,
        w: u32,
        h: u32,
        r: usize,
    ) {
        let uh = self
            .ctx
            .uniform(&params::block(&[[w as f32, h as f32, r as f32, 0.0]]));
        let uv = self
            .ctx
            .uniform(&params::block(&[[w as f32, h as f32, r as f32, 1.0]]));
        self.ctx.dispatch(enc, "gbox", &[&uh, src, tmp], w, h);
        self.ctx.dispatch(enc, "gbox", &[&uv, tmp, dst], w, h);
    }

    /// Separable symmetric convolution of a scalar plane `src → tmp → dst`.
    pub(super) fn sep(
        &self,
        enc: &mut wgpu::CommandEncoder,
        src: &wgpu::Buffer,
        tmp: &wgpu::Buffer,
        dst: &wgpu::Buffer,
        w: u32,
        h: u32,
        kernel: &[f32],
        step: u32,
    ) {
        let uh = self
            .ctx
            .uniform(&params::sep_block(w, h, kernel, step, false));
        let uv = self
            .ctx
            .uniform(&params::sep_block(w, h, kernel, step, true));
        self.ctx.dispatch(enc, "sep", &[&uh, src, tmp], w, h);
        self.ctx.dispatch(enc, "sep", &[&uv, tmp, dst], w, h);
    }
}
