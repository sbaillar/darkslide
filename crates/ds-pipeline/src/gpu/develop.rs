//! DEVELOP: recording the stage graph into one command buffer, and reading frames back.
use super::{
    Adjustments, Arc, AssertUnwindSafe, BaseView, CompiledMask, Finish, GpuRenderer, GuidedParams,
    INLINE_SHARPEN_TAPS, ImageKey, Inner, Mutex, OutSlot, OutputFormat, Pending, PipelineError,
    Plan, RasterCache, RawImage, RenderRequest, Size, Tables, catch_unwind, context, lock,
    lost_during, params,
};

impl GpuRenderer {
    /// Develop `plan` in one turn and read the result back outside it, writing the output
    /// pixels at (x0, y0) of an `out` image `dst_w` pixels wide.
    pub(super) fn develop_into(
        &self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        req: &RenderRequest,
        plan: &Plan,
        masks: &[CompiledMask],
        format: OutputFormat,
        out: &mut [u8],
        at: [u32; 3],
    ) -> Result<Option<crate::Histogram>, PipelineError> {
        self.read_into(
            key,
            req,
            plan.out_size(),
            format,
            out,
            at,
            None,
            |inner, slot| {
                inner.develop(
                    key,
                    raw,
                    adj,
                    plan,
                    masks,
                    &self.shared.rasters,
                    format,
                    Finish::Output {
                        slot,
                        histogram: req.histogram,
                    },
                )
            },
        )
    }

    /// One turn that records work ending in a readback of a `size` output into a slot
    /// (`record`), then the wait and copy into `out` at (x0, y0) of an image `at[2]`
    /// pixels wide, outside the turn. `epoch`: fail if the device changed since.
    pub(super) fn read_into(
        &self,
        key: ImageKey,
        req: &RenderRequest,
        size: Size,
        format: OutputFormat,
        out: &mut [u8],
        at: [u32; 3],
        epoch: Option<u64>,
        record: impl FnOnce(&mut Inner, OutSlot) -> Result<Pending, PipelineError>,
    ) -> Result<Option<crate::Histogram>, PipelineError> {
        let px_bytes = out_bytes(format, size);
        // The histogram counters follow the pixels in the output slot.
        let bytes = px_bytes
            + if req.histogram {
                crate::Histogram::GPU_WORDS as u64 * 4
            } else {
                0
            };
        let slot = {
            let mut outs = lock(&self.shared.outs);
            outs.iter()
                .position(|o| o.fits(bytes))
                .map(|i| outs.swap_remove(i))
        };
        let pending = self.turn(req, |inner| {
            if epoch.is_some_and(|e| e != inner.ctx.epoch) {
                return Err(lost_during("export"));
            }
            let slot = match slot {
                Some(s) if s.epoch == inner.ctx.epoch => s,
                _ => {
                    inner.make_room(bytes, key);
                    inner.ctx.out_slot(bytes)?
                }
            };
            record(inner, slot)
        })?;
        let Pending::Frame(rb) = pending else {
            return Err(PipelineError::Gpu("no readback".into()));
        };
        let mut hist = None;
        let slot = catch_unwind(AssertUnwindSafe(|| {
            rb.finish(req.cancel.as_ref(), |b| {
                let (px, h) = b.split_at(px_bytes as usize);
                copy_out(px, format, size, out, at);
                if req.histogram {
                    let words: Vec<u32> = h
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| u32::from_ne_bytes(*c))
                        .collect();
                    hist = Some(crate::Histogram::from_gpu(&words));
                }
            })
        }))
        .unwrap_or_else(|_| Err(PipelineError::Gpu("GPU readback failed".into())))?;
        let mut outs = lock(&self.shared.outs);
        // A couple of idle slots cover double buffering; none while over budget.
        if !crate::cache::over_budget_with(0) {
            outs.push(slot);
        }
        while outs.len() > 3 || (!outs.is_empty() && crate::cache::over_budget_with(0)) {
            outs.remove(0);
        }
        Ok(hist)
    }

    /// Wait for a stored stage (outside any turn); fails on cancel or device loss.
    pub(super) fn wait_stored(pending: Pending, req: &RenderRequest) -> Result<(), PipelineError> {
        match pending {
            Pending::Stored(done, device, lost) => {
                context::wait_for(&device, &lost, &done, req.cancel.as_ref())
            }
            Pending::Frame(_) => Err(PipelineError::Gpu("unexpected readback".into())),
        }
    }
}

pub(super) fn output_kernel(format: OutputFormat, histogram: bool) -> &'static str {
    match (format, histogram) {
        (OutputFormat::Rgba8, false) => "output8",
        (OutputFormat::Rgb16, false) => "output16",
        (OutputFormat::Rgba16fLinearP3, false) => "outputf16",
        (OutputFormat::Rgba8, true) => "output8_hist",
        (OutputFormat::Rgb16, true) => "output16_hist",
        (OutputFormat::Rgba16fLinearP3, true) => "outputf16_hist",
    }
}

/// Bytes per pixel the output kernels write (RGBA8, RGBA16 or RGBA16F).
pub(super) fn gpu_bpp(format: OutputFormat) -> u64 {
    match format {
        OutputFormat::Rgba8 => 4,
        OutputFormat::Rgb16 | OutputFormat::Rgba16fLinearP3 => 8,
    }
}

/// Bytes per row of the read-back image: the format's own pixels (RGB16 is packed on the
/// GPU, see `pack16.wgsl`), rows padded to 4 bytes.
pub(super) fn out_pitch(format: OutputFormat, width: u32) -> u64 {
    (u64::from(width) * format.bytes_per_pixel() as u64).next_multiple_of(4)
}

/// Bytes of a read-back `size` image.
pub(super) fn out_bytes(format: OutputFormat, size: Size) -> u64 {
    out_pitch(format, size.width) * u64::from(size.height)
}

/// Copy a read-back `size` block (rows of [`out_pitch`]) into `out` at (`at[0]`, `at[1]`)
/// of an image `at[2]` pixels wide: one memcpy per row.
pub(super) fn copy_out(src: &[u8], format: OutputFormat, size: Size, out: &mut [u8], at: [u32; 3]) {
    let db = format.bytes_per_pixel();
    let (w, dst_w) = (size.width as usize, at[2] as usize);
    let pitch = out_pitch(format, size.width) as usize;
    for (y, row) in src.chunks_exact(pitch).enumerate() {
        let d0 = ((at[1] as usize + y) * dst_w + at[0] as usize) * db;
        out[d0..d0 + w * db].copy_from_slice(&row[..w * db]);
    }
}

impl Inner {
    /// Record and submit DEVELOP for `plan` into `slot`; the caller waits for the readback.
    pub(super) fn develop(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        plan: &Plan,
        masks: &[CompiledMask],
        rasters: &Mutex<RasterCache>,
        format: OutputFormat,
        finish: Finish<'_>,
    ) -> Result<Pending, PipelineError> {
        let clamp = format != OutputFormat::Rgba16fLinearP3;
        self.ctx.begin_frame();
        let base = self.base(key, raw, plan)?;
        let base = base.get();
        let (w, h) = (plan.geo.out.width, plan.geo.out.height);
        let n = u64::from(w) * u64::from(h);
        let tables = Tables::build(plan);
        let tab_buf = self.tables_buffer(&tables.data)?;
        let range_buf = self
            .ctx
            .storage_init("ranges", bytemuck::cast_slice(&tables.ranges))?;
        let lin = if plan.needs_lin() {
            self.ctx.buf("lin", n * 16)?
        } else {
            self.ctx.dummy()
        };
        // Develop image: packed f16 (8 bytes) for previews, f32 for Full.
        let (enc_bpp, ek) = if base.precise { (16, "") } else { (8, "_h") };
        let k = |name: &str| format!("{name}{ek}");
        let encb = self.ctx.buf("enc", n * enc_bpp)?;
        let ypl = self.ctx.buf("luma", n * 4)?;
        // Without clarity/structure or layers nothing needs the pre-detail image, so the
        // grade pass writes straight into `enc` and the copy pass is skipped.
        let direct = !plan.needs_detail() && plan.layers.is_empty();
        let graded = if direct {
            encb.clone()
        } else {
            self.ctx.buf("graded", n * enc_bpp)?
        };
        let dummy = self.ctx.dummy();
        let mut cmd = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());

        let low = |gp: GuidedParams| {
            let s = gp.s as u32;
            (
                (w + gp.off[0] as u32).div_ceil(s),
                (h + gp.off[1] as u32).div_ceil(s),
            )
        };
        let dims = params::Dims {
            base: (base.w, base.h),
            hdr: low(plan.hdr_gp),
            clarity: low(plan.clarity_gp),
            structure: low(plan.structure_gp),
            nr: low(plan.nr.gp),
            texture: low(plan.texture_gp),
            dehaze: low(plan.dehaze_gp),
        };
        let bv = BaseView {
            scale: plan.base_key.scale(),
            level: plan.level_size,
            origin: [base.x0, base.y0],
        };
        let out_matrix = match format {
            OutputFormat::Rgba16fLinearP3 => {
                crate::color::working_to_output(crate::cpu::f16_space(plan.output))
            }
            _ => plan.output_matrix,
        };
        let dev = self
            .ctx
            .uniform(&params::dev_block(plan, &dims, &tables, &bv, &out_matrix));
        let fmt = if base.precise { "_f" } else { "_h" };
        // Heal/clone spots: their ring statistics first (read by the scene pass).
        let (spot_buf, spot_stats) = if plan.spots.is_empty() {
            (dummy.clone(), dummy.clone())
        } else {
            let sb = self.ctx.storage_init(
                "spots",
                bytemuck::cast_slice(&params::spots(&plan.spots, w, h)),
            )?;
            let n = plan.spots.len() as u32;
            let st = self.ctx.buf("spot_stats", u64::from(n) * 16 * 16)?;
            self.ctx.dispatch(
                &mut cmd,
                &format!("spot_stats{fmt}"),
                &[&dev, &base.buf, &sb, &st],
                n * 16,
                1,
            );
            (sb, st)
        };
        let (hdr_coef, dhz) = if plan.needs_lin() {
            self.ctx.dispatch(
                &mut cmd,
                &format!("scene{fmt}"),
                &[&dev, &base.buf, &lin, &spot_buf, &spot_stats],
                w,
                h,
            );
            if let Some(lb) = &plan.lens_blur {
                self.lens_blur(&mut cmd, &dev, &lin, lb, w, h)?;
            }
            let coef = if plan.needs_hdr() {
                self.guided_self(&mut cmd, &lin, "coef_hdr", "gdown_w", w, h, plan.hdr_gp)?
            } else {
                dummy.clone()
            };
            let dhz = if plan.needs_dehaze() {
                self.guided_haze(&mut cmd, &lin, w, h, plan.dehaze_gp, plan.airlight)?
            } else {
                dummy.clone()
            };
            self.ctx.dispatch(
                &mut cmd,
                &k("grade"),
                &[&dev, &lin, &coef, &tab_buf, &range_buf, &graded, &ypl, &dhz],
                w,
                h,
            );
            (coef, dhz)
        } else {
            // Nothing needs the scene-linear image: fuse geometry/WB and grading.
            self.ctx.dispatch(
                &mut cmd,
                &format!("scene_grade{fmt}"),
                &[
                    &dev,
                    &base.buf,
                    &dummy,
                    &tab_buf,
                    &range_buf,
                    &graded,
                    &ypl,
                    &dummy,
                    &spot_buf,
                    &spot_stats,
                ],
                w,
                h,
            );
            (dummy.clone(), dummy.clone())
        };
        // Each detail plane only when something uses it.
        let down = k("gdown_w");
        let mut plane = |need: bool, name: &'static str, gp: GuidedParams| {
            if need {
                self.guided_self(&mut cmd, &graded, name, &down, w, h, gp)
            } else {
                Ok(dummy.clone())
            }
        };
        let clar = plane(plan.needs_clarity_plane(), "coef_clar", plan.clarity_gp)?;
        let strc = plane(plan.needs_structure_plane(), "coef_strc", plan.structure_gp)?;
        let tex = plane(plan.needs_texture_plane(), "coef_tex", plan.texture_gp)?;
        if !direct {
            self.ctx.dispatch(
                &mut cmd,
                &k("local_contrast"),
                &[&dev, &graded, &clar, &strc, &encb, &ypl, &tex],
                w,
                h,
            );
        }

        // Layers.
        if !plan.layers.is_empty() {
            let shapes = params::shapes(masks);
            let shape_buf = self
                .ctx
                .storage_init("shapes", bytemuck::cast_slice(&shapes.data))?;
            let mplane = self.ctx.buf("mask", n * 4)?;
            for (li, (lp, cm)) in plan.layers.iter().zip(masks).enumerate() {
                let layer_id = adj.layers[lp.index].id;
                let raster_buf = self
                    .layer_rasters(&mut cmd, rasters, key.0, layer_id, &shapes.layouts[li])?
                    .unwrap_or_else(|| dummy.clone());
                let planes = if cm.planes.is_empty() {
                    dummy.clone()
                } else {
                    self.mask_planes(
                        &mut cmd,
                        cm,
                        &graded,
                        &raster_buf,
                        &shapes.layouts[li],
                        &dev,
                        w,
                        h,
                        &k,
                    )?
                };
                let feather = params::Feather::new(cm.feather_px, w, h);
                let lay = self.ctx.uniform(&params::layer_block(
                    lp,
                    cm,
                    &tables,
                    li,
                    shapes.offsets[li],
                    &feather,
                ));
                self.ctx.dispatch(
                    &mut cmd,
                    &k("mask_eval"),
                    &[
                        &dev,
                        &lay,
                        &shape_buf,
                        &raster_buf,
                        &graded,
                        &mplane,
                        &planes,
                    ],
                    w,
                    h,
                );
                let src = match feather {
                    params::Feather::None => mplane.clone(),
                    params::Feather::Full { ref kernel } => {
                        let tmp = self.ctx.buf("mask_tmp", n * 4)?;
                        let outp = self.ctx.buf("mask_f", n * 4)?;
                        self.sep(&mut cmd, &mplane, &tmp, &outp, w, h, kernel, 1);
                        outp
                    }
                    params::Feather::Low {
                        f,
                        lw,
                        lh,
                        ref kernel,
                    } => {
                        let lb = u64::from(lw) * u64::from(lh) * 4;
                        let lo = self.ctx.buf("mask_low", lb)?;
                        let tmp = self.ctx.buf("mask_tmp", lb)?;
                        let outp = self.ctx.buf("mask_f", lb)?;
                        let u = self.ctx.uniform(&params::block(&[
                            [w as f32, h as f32, f as f32, 0.0],
                            [lw as f32, lh as f32, 0.0, 0.0],
                        ]));
                        self.ctx
                            .dispatch(&mut cmd, "pdown", &[&u, &mplane, &lo], lw, lh);
                        self.sep(&mut cmd, &lo, &tmp, &outp, lw, lh, kernel, 1);
                        outp
                    }
                };
                self.ctx.dispatch(
                    &mut cmd,
                    &k("layer_apply"),
                    &[
                        &dev, &lay, &src, &graded, &encb, &tab_buf, &range_buf, &hdr_coef, &lin,
                        &clar, &strc, &ypl, &tex, &dhz,
                    ],
                    w,
                    h,
                );
            }
        }

        if plan.fx.is_active() {
            self.glow_halation(&mut cmd, &encb, &ypl, &plan.fx, w, h, &k)?;
        }

        // Noise reduction.
        let nr = &plan.nr;
        if nr.luma_active() || nr.chroma_active() {
            let (b1, b2) = if nr.luma_active() {
                let b1 = self.ctx.buf("p_b1", n * 4)?;
                let b2 = self.ctx.buf("p_b2", n * 4)?;
                let u1 = self
                    .ctx
                    .uniform(&params::block(&[[w as f32, h as f32, 1.0, 0.0]]));
                let u2 = self
                    .ctx
                    .uniform(&params::block(&[[w as f32, h as f32, 2.0, 0.0]]));
                self.ctx
                    .dispatch(&mut cmd, "blur3", &[&u1, &ypl, &b1], w, h);
                self.ctx.dispatch(&mut cmd, "blur3", &[&u2, &b1, &b2], w, h);
                (b1, b2)
            } else {
                (dummy.clone(), dummy.clone())
            };
            let coef = if nr.chroma_active() {
                let (lw, lh) = dims.nr;
                let bytes = u64::from(lw) * u64::from(lh) * 16;
                let m0 = self.ctx.buf("nr_m0", bytes)?;
                let m1 = self.ctx.buf("nr_m1", bytes)?;
                let t = self.ctx.buf("g_a", bytes)?;
                let c = self.ctx.buf("nr_coef", bytes)?;
                let ud = self.ctx.uniform(&params::block(&[
                    [w as f32, h as f32, nr.gp.s as f32, 0.0],
                    [
                        lw as f32,
                        lh as f32,
                        nr.gp.off[0] as f32,
                        nr.gp.off[1] as f32,
                    ],
                ]));
                self.ctx
                    .dispatch(&mut cmd, &k("gdown_nr"), &[&ud, &encb, &m0, &m1], lw, lh);
                self.box2(&mut cmd, &m0, &t, &m0, lw, lh, nr.gp.r);
                let eps = (nr.chroma_sigma * nr.chroma_sigma).max(1e-10);
                let uc = self
                    .ctx
                    .uniform(&params::block(&[[lw as f32, lh as f32, eps, 0.0]]));
                self.ctx
                    .dispatch(&mut cmd, "gcoef_cross", &[&uc, &m0, &m1, &c], lw, lh);
                self.box2(&mut cmd, &c, &t, &c, lw, lh, nr.gp.r);
                c
            } else {
                dummy.clone()
            };
            self.ctx.dispatch(
                &mut cmd,
                &k("nr_apply"),
                &[&dev, &encb, &b1, &b2, &coef, &ypl],
                w,
                h,
            );
        }

        let store = matches!(finish, Finish::Store { .. });
        // Sharpening blur, then output (or store).
        let blur = if let Some(sp) = &plan.sharpen {
            if sp.kernel.len() <= INLINE_SHARPEN_TAPS && !store {
                // Small kernel: the output kernel blurs the luma plane itself.
                ypl.clone()
            } else {
                let tmp = self.ctx.buf("p_tmp", n * 4)?;
                let outp = self.ctx.buf("p_sharp", n * 4)?;
                self.sep(&mut cmd, &ypl, &tmp, &outp, w, h, &sp.kernel, 1);
                outp
            }
        } else {
            dummy.clone()
        };
        let (slot, histogram) = match finish {
            Finish::Store { dst, at } => {
                let win = plan.out_window();
                let u = self.ctx.uniform(&params::block(&[[
                    at[0] as f32,
                    at[1] as f32,
                    at[2] as f32,
                    if clamp { 1.0 } else { 0.0 },
                ]]));
                self.ctx.dispatch(
                    &mut cmd,
                    &k("store_enc"),
                    &[&dev, &u, &encb, &blur, dst],
                    win[2],
                    win[3],
                );
                self.ctx.queue.submit([cmd.finish()]);
                let (tx, rx) = std::sync::mpsc::channel();
                self.ctx.queue.on_submitted_work_done(move || {
                    let _ = tx.send(());
                });
                return Ok(Pending::Stored(
                    rx,
                    self.ctx.device.clone(),
                    Arc::clone(&self.ctx.lost),
                ));
            }
            Finish::Output { slot, histogram } => (slot, histogram),
        };
        let kernel = output_kernel(format, histogram);
        let win = plan.out_size();
        let px_bytes = out_bytes(format, win);
        let hist = if histogram {
            let hb = self
                .ctx
                .buf("hist", crate::Histogram::GPU_WORDS as u64 * 4)?;
            cmd.clear_buffer(&hb, 0, None);
            hb
        } else {
            self.ctx.dummy_rw()
        };
        let dev = if histogram {
            let mut blk = params::dev_block(plan, &dims, &tables, &bv, &out_matrix);
            params::set_histogram(&mut blk);
            self.ctx.uniform(&blk)
        } else {
            dev
        };
        let target = self.output_target(format, win, &slot.gpu)?;
        self.ctx.dispatch(
            &mut cmd,
            &k(kernel),
            &[&dev, &encb, &blur, &target, &hist],
            win.width,
            win.height,
        );
        self.pack_output(&mut cmd, format, win, &target, &slot.gpu);
        let bytes = if histogram {
            let hb = crate::Histogram::GPU_WORDS as u64 * 4;
            cmd.copy_buffer_to_buffer(&hist, 0, &slot.gpu, px_bytes, hb);
            px_bytes + hb
        } else {
            px_bytes
        };
        let rb = self.ctx.submit_readback(cmd, slot, bytes);
        // Frame buffers not used by this render go first when the pool is over its cap.
        let cap = (crate::cache::budget_bytes() / 8).max(256 << 20);
        self.ctx.trim_stale(cap);
        Ok(Pending::Frame(rb))
    }

    /// Where the output kernel writes: the readback slot itself, or (RGB16) an RGBA16
    /// buffer that [`Self::pack_output`] packs into the slot.
    pub(super) fn output_target(
        &mut self,
        format: OutputFormat,
        size: Size,
        slot: &wgpu::Buffer,
    ) -> Result<wgpu::Buffer, PipelineError> {
        if format == OutputFormat::Rgb16 {
            self.ctx.buf("out_rgba16", size.pixels() * 8)
        } else {
            Ok(slot.clone())
        }
    }

    /// RGB16 output: pack the RGBA16 `target` into the slot's padded RGB16 rows.
    pub(super) fn pack_output(
        &self,
        cmd: &mut wgpu::CommandEncoder,
        format: OutputFormat,
        size: Size,
        target: &wgpu::Buffer,
        slot: &wgpu::Buffer,
    ) {
        if format != OutputFormat::Rgb16 {
            return;
        }
        let pitch_words = out_pitch(format, size.width) / 4;
        let u = self.ctx.uniform(&params::block(&[[
            size.width as f32,
            size.height as f32,
            pitch_words as f32,
            0.0,
        ]]));
        self.ctx.dispatch(
            cmd,
            "pack16",
            &[&u, target, slot],
            size.width.div_ceil(2),
            size.height,
        );
    }
}
