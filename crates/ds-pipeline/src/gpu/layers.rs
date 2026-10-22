//! Local-adjustment layers: mask rasters on the GPU (upload, feathering, brush strokes).
use super::{
    Account, CompiledMask, Inner, LayerRasters, Mutex, PipelineError, RasterCache, RasterEntry,
    mask, params,
};

/// Rows changed by brush steps (their union).
pub(super) fn brush_rows(ops: &[mask::BrushOp], h: usize) -> (usize, usize) {
    ops.iter()
        .map(|o| o.rows(h))
        .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
        .unwrap_or((0, 0))
}

impl Inner {
    /// Output-space planes of a layer's mask (see [`mask::PlaneSrc`]), packed `n` floats
    /// each into one buffer, as [`mask::build_planes`] builds them.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn mask_planes(
        &mut self,
        cmd: &mut wgpu::CommandEncoder,
        cm: &CompiledMask,
        graded: &wgpu::Buffer,
        rasters: &wgpu::Buffer,
        layout: &[params::RasterSlot],
        dev: &wgpu::Buffer,
        w: u32,
        h: u32,
        k: &dyn Fn(&str) -> String,
    ) -> Result<wgpu::Buffer, PipelineError> {
        let n = u64::from(w) * u64::from(h);
        let out = self
            .ctx
            .new_buffer("mask_planes", n * 4 * cm.planes.len() as u64)?;
        let src = self.ctx.buf("mp_src", n * 4)?;
        let dst = self.ctx.buf("mp_dst", n * 4)?;
        for (pi, ps) in cm.planes.iter().enumerate() {
            let sigma = match *ps {
                mask::PlaneSrc::ColorRange { range, gp } => {
                    let u = self.ctx.uniform(&params::block(&[[
                        range.center,
                        range.half,
                        range.smooth,
                        0.0,
                    ]]));
                    let guide = self.ctx.buf("mp_guide", n * 4)?;
                    self.ctx
                        .dispatch(cmd, &k("cr_plane"), &[dev, &u, graded, &dst, &guide], w, h);
                    // Edge-aware refinement (local::guided_cross on mask::cr_guide).
                    let s = gp.s as u32;
                    let (ox, oy) = (gp.off[0] as u32, gp.off[1] as u32);
                    let (lw, lh) = ((w + ox).div_ceil(s), (h + oy).div_ceil(s));
                    let bytes = u64::from(lw) * u64::from(lh) * 16;
                    let m = self.ctx.buf("g_a", bytes)?;
                    let t = self.ctx.buf("g_b", bytes)?;
                    let ud = self.ctx.uniform(&params::block(&[
                        [w as f32, h as f32, s as f32, 0.0],
                        [lw as f32, lh as f32, ox as f32, oy as f32],
                    ]));
                    self.ctx
                        .dispatch(cmd, "gdown_cr", &[&ud, &guide, &dst, &m], lw, lh);
                    self.box2(cmd, &m, &t, &m, lw, lh, gp.r);
                    let uc = self
                        .ctx
                        .uniform(&params::block(&[[lw as f32, lh as f32, gp.eps, 0.0]]));
                    // The haze coefficient kernel is the same cross guided filter.
                    self.ctx.dispatch(cmd, "gcoef_haze", &[&uc, &m, &t], lw, lh);
                    self.box2(cmd, &t, &m, &t, lw, lh, gp.r);
                    self.ctx
                        .dispatch(cmd, "cr_apply", &[&ud, &guide, &t, &src], w, h);
                    0.0
                }
                mask::PlaneSrc::Auto {
                    raster,
                    core,
                    gp,
                    sigma,
                } => {
                    let (r, off) = (&layout[raster].raster, &layout[raster].read);
                    let (c, coff) = (&layout[core].raster, &layout[core].read);
                    let u = self.ctx.uniform(&params::block(&[
                        [
                            r.plane.w as f32,
                            r.plane.h as f32,
                            f32::from_bits(*off),
                            0.0,
                        ],
                        [
                            c.plane.w as f32,
                            c.plane.h as f32,
                            f32::from_bits(*coff),
                            0.0,
                        ],
                    ]));
                    let cov = self.ctx.buf("mp_cov", n * 4)?;
                    let corep = self.ctx.buf("mp_core", n * 4)?;
                    self.ctx
                        .dispatch(cmd, "auto_src", &[dev, &u, rasters, &cov, &corep], w, h);
                    let s = gp.s as u32;
                    let (ox, oy) = (gp.off[0] as u32, gp.off[1] as u32);
                    let (lw, lh) = ((w + ox).div_ceil(s), (h + oy).div_ceil(s));
                    let bytes = u64::from(lw) * u64::from(lh) * 16;
                    let m = self.ctx.buf("g_a", bytes)?;
                    let t = self.ctx.buf("g_b", bytes)?;
                    let ud = self.ctx.uniform(&params::block(&[
                        [w as f32, h as f32, s as f32, mask::AUTO_TOL],
                        [lw as f32, lh as f32, ox as f32, oy as f32],
                        [mask::AUTO_TOL_AB, 0.0, 0.0, 0.0],
                    ]));
                    self.ctx
                        .dispatch(cmd, &k("gdown_x"), &[&ud, graded, &corep, &m], lw, lh);
                    self.box2(cmd, &m, &t, &m, lw, lh, gp.r);
                    self.ctx
                        .dispatch(cmd, &k("auto_apply"), &[&ud, graded, &cov, &m, &src], w, h);
                    sigma
                }
            };
            self.feather_plane(cmd, &src, &dst, sigma, w, h)?;
            cmd.copy_buffer_to_buffer(&dst, 0, &out, pi as u64 * n * 4, n * 4);
        }
        Ok(out)
    }

    /// Make a layer's E-frame rasters resident, uploading only what changed, and feather
    /// those that carry a feather on the GPU (into the slot right after the source; only
    /// the band of changed rows ± the blur's reach when the source changed incrementally).
    pub(super) fn layer_rasters(
        &mut self,
        cmd: &mut wgpu::CommandEncoder,
        rasters: &Mutex<RasterCache>,
        image: u64,
        layer: u64,
        layout: &[params::RasterSlot],
    ) -> Result<Option<wgpu::Buffer>, PipelineError> {
        if layout.is_empty() {
            return Ok(None);
        }
        let want: Vec<RasterEntry> = layout
            .iter()
            .map(|sl| RasterEntry {
                id: sl.raster.id,
                generation: sl.raster.generation,
                w: sl.raster.plane.w,
                h: sl.raster.plane.h,
                src: sl.src,
                read: sl.read,
                feathered: None,
            })
            .collect();
        let same_shape = self.layer_rasters.get(&(image, layer)).is_some_and(|e| {
            e.layout.len() == want.len()
                && e.layout.iter().zip(&want).all(|(a, b)| {
                    (a.id, a.w, a.h, a.src, a.read) == (b.id, b.w, b.h, b.src, b.read)
                })
        });
        // Row bands to (re-)feather per raster: None = nothing, Some(None) = all rows.
        let mut blur: Vec<Option<Option<(usize, usize)>>> = vec![None; layout.len()];
        // GPU brush steps to run (raster index, steps).
        let mut brush_ops: Vec<(usize, Vec<mask::BrushOp>)> = Vec::new();
        let buf = if same_shape {
            let entry = self
                .layer_rasters
                .get_mut(&(image, layer))
                .expect("checked above");
            for (k, (sl, have)) in layout.iter().zip(entry.layout.iter_mut()).enumerate() {
                let r = &sl.raster;
                let w = r.plane.w;
                let key = (r.generation, r.feather.to_bits());
                if r.gpu_brush {
                    // Stamped on the GPU: apply the recorded steps (all of them after a
                    // rebuild of the raster).
                    let ops = if have.generation == r.generation {
                        crate::par::hold(rasters).take_ops(r.id)
                    } else {
                        have.generation = r.generation;
                        crate::par::hold(rasters).replay_ops(r.id)
                    };
                    if !ops.is_empty() {
                        let band = brush_rows(&ops, r.plane.h);
                        brush_ops.push((k, ops));
                        blur[k] = Some(Some(band));
                    }
                } else if have.generation != r.generation {
                    self.ctx.queue.write_buffer(
                        &entry.buf,
                        u64::from(sl.src) * 4,
                        bytemuck::cast_slice(&r.plane.data),
                    );
                    have.generation = r.generation;
                    let _ = crate::par::hold(rasters).take_dirty(r.id);
                    blur[k] = Some(None);
                } else if let Some((y0, y1)) = crate::par::hold(rasters).take_dirty(r.id) {
                    let rows = &r.plane.data[y0 * w..y1 * w];
                    self.ctx.queue.write_buffer(
                        &entry.buf,
                        (u64::from(sl.src) + (y0 * w) as u64) * 4,
                        bytemuck::cast_slice(rows),
                    );
                    blur[k] = Some(Some((y0, y1)));
                }
                if r.feather > 0.0 && have.feathered != Some(key) {
                    blur[k] = Some(None);
                }
                have.feathered = (r.feather > 0.0).then_some(key);
            }
            entry.buf.clone()
        } else {
            let floats: u64 = layout
                .iter()
                .map(|sl| {
                    let n = (sl.raster.plane.w * sl.raster.plane.h) as u64;
                    let own = if sl.raster.gpu_brush { 3 * n } else { n };
                    own + if sl.raster.feather > 0.0 { n } else { 0 }
                })
                .sum();
            let buf = self.ctx.new_buffer("rasters", floats * 4)?;
            let mut entries = want;
            for (k, sl) in layout.iter().enumerate() {
                if sl.raster.gpu_brush {
                    // A new GPU copy: rebuild it from the whole stroke list.
                    brush_ops.push((k, crate::par::hold(rasters).replay_ops(sl.raster.id)));
                } else {
                    self.ctx.queue.write_buffer(
                        &buf,
                        u64::from(sl.src) * 4,
                        bytemuck::cast_slice(&sl.raster.plane.data),
                    );
                }
                let _ = crate::par::hold(rasters).take_dirty(sl.raster.id);
                blur[k] = Some(None);
                entries[k].feathered = (sl.raster.feather > 0.0)
                    .then_some((sl.raster.generation, sl.raster.feather.to_bits()));
            }
            self.layer_rasters.insert(
                (image, layer),
                LayerRasters {
                    buf: buf.clone(),
                    layout: entries,
                    _acct: Account::new(crate::cache::Use::GpuRaster, floats * 4),
                },
            );
            buf
        };
        for (k, ops) in &brush_ops {
            self.brush_ops(cmd, &buf, &layout[*k], ops)?;
        }
        for (sl, band) in layout.iter().zip(blur) {
            if let (true, Some(band)) = (sl.raster.feather > 0.0, band) {
                self.feather_raster(cmd, &buf, sl, band)?;
            }
        }
        Ok(Some(buf))
    }

    /// Run recorded brush steps on raster `sl`'s combined/base/stroke planes.
    pub(super) fn brush_ops(
        &mut self,
        cmd: &mut wgpu::CommandEncoder,
        buf: &wgpu::Buffer,
        sl: &params::RasterSlot,
        ops: &[mask::BrushOp],
    ) -> Result<(), PipelineError> {
        let (w, h) = (sl.raster.plane.w, sl.raster.plane.h);
        let n = (w * h) as u32;
        let (comb, base, stroke) = (sl.src, sl.src + n, sl.src + 2 * n);
        let bits = f32::from_bits;
        for op in ops {
            match op {
                mask::BrushOp::Clear => {
                    cmd.clear_buffer(buf, u64::from(comb) * 4, Some(u64::from(3 * n) * 4));
                }
                mask::BrushOp::Fold(b) => {
                    let (bw, bh) = ((b[2] - b[0]) as u32, (b[3] - b[1]) as u32);
                    let u = self.ctx.uniform(&params::block(&[
                        [w as f32, b[0] as f32, b[1] as f32, bw as f32],
                        [bh as f32, bits(comb), bits(base), bits(stroke)],
                    ]));
                    self.ctx.dispatch(cmd, "brush_fold", &[&u, buf], bw, bh);
                }
                mask::BrushOp::Stamp {
                    segs,
                    boxes,
                    bbox,
                    radius,
                    hard,
                    flow,
                    erase,
                } => {
                    let (bw, bh) = (bbox[2] - bbox[0], bbox[3] - bbox[1]);
                    let (tx, ty) = (bw.div_ceil(32), bh.div_ceil(32));
                    // Bin the segments into the box's 32×32 tiles.
                    let mut lists: Vec<Vec<u32>> = vec![Vec::new(); tx * ty];
                    for (si, sb) in boxes.iter().enumerate() {
                        let (x0, y0) = ((sb[0] - bbox[0]) / 32, (sb[1] - bbox[1]) / 32);
                        let (x1, y1) = (
                            (sb[2] - bbox[0]).div_ceil(32).min(tx),
                            (sb[3] - bbox[1]).div_ceil(32).min(ty),
                        );
                        for tyi in y0..y1 {
                            for txi in x0..x1 {
                                lists[tyi * tx + txi].push(si as u32);
                            }
                        }
                    }
                    let mut table: Vec<[u32; 2]> = Vec::with_capacity(lists.len());
                    let mut idx: Vec<u32> = Vec::new();
                    for l in &lists {
                        table.push([idx.len() as u32, l.len() as u32]);
                        idx.extend_from_slice(l);
                    }
                    if idx.is_empty() {
                        idx.push(0);
                    }
                    let seg_buf = self
                        .ctx
                        .storage_init("brush_segs", bytemuck::cast_slice(segs))?;
                    let tile_buf = self
                        .ctx
                        .storage_init("brush_tiles", bytemuck::cast_slice(&table))?;
                    let idx_buf = self
                        .ctx
                        .storage_init("brush_idx", bytemuck::cast_slice(&idx))?;
                    let u = self.ctx.uniform(&params::block(&[
                        [w as f32, bbox[0] as f32, bbox[1] as f32, bw as f32],
                        [bh as f32, tx as f32, *radius, *hard],
                        [
                            *flow,
                            if *erase { 1.0 } else { 0.0 },
                            bits(comb),
                            bits(base),
                        ],
                        [bits(stroke), 0.0, 0.0, 0.0],
                    ]));
                    self.ctx.dispatch(
                        cmd,
                        "brush_stamp",
                        &[&u, &seg_buf, &tile_buf, &idx_buf, buf],
                        bw as u32,
                        bh as u32,
                    );
                }
            }
        }
        Ok(())
    }

    /// Feather raster `sl` from its source slot into its read slot, all rows or the band
    /// `rows` widened by the blur's reach (aligned to its block grid, so the band equals
    /// the same rows of a full blur).
    pub(super) fn feather_raster(
        &mut self,
        cmd: &mut wgpu::CommandEncoder,
        buf: &wgpu::Buffer,
        sl: &params::RasterSlot,
        rows: Option<(usize, usize)>,
    ) -> Result<(), PipelineError> {
        let (w, h) = (sl.raster.plane.w as u32, sl.raster.plane.h as u32);
        let sigma = sl.raster.feather;
        let (y0, y1) = match rows {
            None => (0, h),
            Some((a, b)) => {
                let f = mask::feather_block(sigma) as u32;
                let reach = mask::feather_reach(sigma) as u32;
                let lo = (a as u32).saturating_sub(reach) / f * f;
                let hi = ((b as u32 + reach).div_ceil(f) * f).min(h);
                (lo, hi)
            }
        };
        // The band's source rows with context (block-aligned), blurred as a plane.
        let f = mask::feather_block(sigma) as u32;
        let ctx_rows = mask::feather_reach(sigma) as u32;
        let s0 = y0.saturating_sub(ctx_rows) / f * f;
        let s1 = ((y1 + ctx_rows).div_ceil(f) * f).min(h);
        let bh = s1 - s0;
        let n = u64::from(w) * u64::from(bh);
        let src = self.ctx.buf("fr_src", n * 4)?;
        let dst = self.ctx.buf("fr_dst", n * 4)?;
        cmd.copy_buffer_to_buffer(
            buf,
            (u64::from(sl.src) + u64::from(s0) * u64::from(w)) * 4,
            &src,
            0,
            n * 4,
        );
        self.feather_plane(cmd, &src, &dst, sigma, w, bh)?;
        cmd.copy_buffer_to_buffer(
            &dst,
            u64::from(y0 - s0) * u64::from(w) * 4,
            buf,
            (u64::from(sl.read) + u64::from(y0) * u64::from(w)) * 4,
            u64::from(y1 - y0) * u64::from(w) * 4,
        );
        Ok(())
    }

    /// A Gaussian feather of the scalar plane `src` (w × h) into `dst`, as
    /// [`mask::feather`] computes it (full resolution, or on a block grid and upsampled).
    pub(super) fn feather_plane(
        &mut self,
        cmd: &mut wgpu::CommandEncoder,
        src: &wgpu::Buffer,
        dst: &wgpu::Buffer,
        sigma: f32,
        w: u32,
        h: u32,
    ) -> Result<(), PipelineError> {
        let n = u64::from(w) * u64::from(h);
        match params::Feather::new(sigma, w, h) {
            params::Feather::None => cmd.copy_buffer_to_buffer(src, 0, dst, 0, n * 4),
            params::Feather::Full { kernel } => {
                let tmp = self.ctx.buf("mp_tmp", n * 4)?;
                self.sep(cmd, src, &tmp, dst, w, h, &kernel, 1);
            }
            params::Feather::Low { f, lw, lh, kernel } => {
                let lb = u64::from(lw) * u64::from(lh) * 4;
                let lo = self.ctx.buf("mp_low", lb)?;
                let tmp = self.ctx.buf("mp_ltmp", lb)?;
                let blurred = self.ctx.buf("mp_lblur", lb)?;
                let u = self.ctx.uniform(&params::block(&[
                    [w as f32, h as f32, f as f32, 0.0],
                    [lw as f32, lh as f32, 0.0, 0.0],
                ]));
                self.ctx.dispatch(cmd, "pdown", &[&u, src, &lo], lw, lh);
                self.sep(cmd, &lo, &tmp, &blurred, lw, lh, &kernel, 1);
                self.ctx
                    .dispatch(cmd, "plane_up", &[&u, &blurred, dst], w, h);
            }
        }
        Ok(())
    }
}
