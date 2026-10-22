//! Copy & Apply: transfer selected adjustment groups from one variant to others.
//!
//! Improvements over Lightroom/Capture One:
//! - per-group **relative** paste (exposure/WB/tone deltas keep each target's own correction);
//! - **aspect-aware crop** (a landscape 3:2 crop pasted on a portrait frame becomes 2:3);
//! - per-layer selection (paste one layer, not all).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::adjust::{Adjustments, Crop, WhiteBalance};
use crate::geom::{NormRect, Size};

/// Copyable groups, one per tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjustGroup {
    /// The base look (camera profile).
    Profile,
    WhiteBalance,
    Exposure,
    Presence,
    Hdr,
    Levels,
    Curve,
    ColorBalance,
    ColorEditor,
    Detail,
    Noise,
    Vignette,
    /// Lens blur, glow, grain and halation.
    Effects,
    Lens,
    Crop,
    Layers,
    /// Heal/clone spots and content-aware removals (pasted at the same normalized
    /// positions). Per-photo retouching: the app never preselects it.
    Spots,
}

impl AdjustGroup {
    pub const ALL: [AdjustGroup; 17] = [
        Self::Profile,
        Self::WhiteBalance,
        Self::Exposure,
        Self::Presence,
        Self::Hdr,
        Self::Levels,
        Self::Curve,
        Self::ColorBalance,
        Self::ColorEditor,
        Self::Detail,
        Self::Noise,
        Self::Vignette,
        Self::Effects,
        Self::Lens,
        Self::Crop,
        Self::Layers,
        Self::Spots,
    ];

    /// Groups where a relative (delta) paste is meaningful.
    pub fn supports_relative(self) -> bool {
        matches!(
            self,
            Self::WhiteBalance | Self::Exposure | Self::Presence | Self::Hdr
        )
    }

    /// Groups that differ between `a` and `b`; drives the "only changed" default in the UI.
    pub fn changed(a: &Adjustments, b: &Adjustments) -> GroupSet {
        Self::ALL.into_iter().filter(|g| !g.equal(a, b)).collect()
    }

    fn equal(self, a: &Adjustments, b: &Adjustments) -> bool {
        match self {
            Self::Profile => a.profile == b.profile,
            Self::WhiteBalance => a.white_balance == b.white_balance,
            Self::Exposure => a.exposure == b.exposure,
            Self::Presence => a.presence == b.presence,
            Self::Hdr => a.hdr == b.hdr,
            Self::Levels => a.levels == b.levels,
            Self::Curve => a.curve == b.curve,
            Self::ColorBalance => a.color_balance == b.color_balance,
            Self::ColorEditor => a.color_editor == b.color_editor,
            Self::Detail => a.detail == b.detail,
            Self::Noise => a.noise == b.noise,
            Self::Vignette => a.vignette == b.vignette,
            Self::Effects => a.effects == b.effects,
            Self::Lens => a.lens == b.lens,
            Self::Crop => a.crop == b.crop,
            Self::Layers => a.layers == b.layers,
            Self::Spots => a.spots == b.spots && a.removals == b.removals,
        }
    }
}

pub type GroupSet = BTreeSet<AdjustGroup>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasteMode {
    /// Replace the target's values with the source's.
    #[default]
    Absolute,
    /// Add the source's change relative to `base` onto the target.
    Relative,
}

/// What to paste and how.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PasteOptions {
    pub groups: GroupSet,
    /// Groups pasted relatively; ignored for groups that don't support it.
    pub relative: GroupSet,
    /// When pasting `Layers`, restrict to these layer ids (empty = all layers).
    pub layer_ids: Vec<u64>,
    /// Replace target layers instead of appending.
    pub replace_layers: bool,
}

impl PasteOptions {
    pub fn all() -> Self {
        Self {
            groups: AdjustGroup::ALL.into_iter().collect(),
            ..Self::default()
        }
    }

    fn mode(&self, g: AdjustGroup) -> PasteMode {
        if g.supports_relative() && self.relative.contains(&g) {
            PasteMode::Relative
        } else {
            PasteMode::Absolute
        }
    }
}

/// A copied set of adjustments plus the baseline needed for relative paste.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clipboard {
    pub source: Adjustments,
    /// What the source looked like before editing (its defaults / auto-applied preset).
    /// Relative paste transfers `source - base`.
    pub base: Adjustments,
    /// Oriented (display) size of the source, for aspect-aware crop.
    pub source_size: Size,
    /// The source image's as-shot white balance as (Kelvin, tint), so a
    /// relative WB paste can treat `AsShot` as a number.
    #[serde(default)]
    pub source_as_shot: Option<(f32, f32)>,
}

/// What [`Clipboard::apply_to`] needs to know about one target image.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PasteTarget {
    /// Oriented (EXIF-applied) size of the target.
    pub size: Size,
    /// The target's as-shot white balance as (Kelvin, tint), if known.
    pub as_shot: Option<(f32, f32)>,
}

/// Something the paste could not do exactly as asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasteWarning {
    /// Relative WB needed an as-shot value that was not provided, so the
    /// source's white balance was pasted absolutely instead.
    WhiteBalanceAbsoluteFallback,
}

/// Result of a paste: the new (sanitized) adjustments plus any warnings.
#[derive(Debug, Clone, PartialEq)]
pub struct Pasted {
    pub adjustments: Adjustments,
    pub warnings: Vec<PasteWarning>,
}

impl Clipboard {
    pub fn new(source: Adjustments, base: Adjustments, source_size: Size) -> Self {
        Self {
            source,
            base,
            source_size,
            source_as_shot: None,
        }
    }

    /// Produce the target's new adjustments. `target_size` is the target's
    /// oriented size. Same as [`Clipboard::apply_to`] without as-shot context.
    pub fn apply(
        &self,
        target: &Adjustments,
        target_size: Size,
        opts: &PasteOptions,
    ) -> Adjustments {
        self.apply_to(
            target,
            &PasteTarget {
                size: target_size,
                as_shot: None,
            },
            opts,
        )
        .adjustments
    }

    /// Paste onto `target`. The output is always [sanitized](Adjustments::sanitize),
    /// so non-finite values in the clipboard can never reach a sidecar.
    pub fn apply_to(&self, target: &Adjustments, t: &PasteTarget, opts: &PasteOptions) -> Pasted {
        let mut out = target.clone();
        let mut warnings = Vec::new();
        for &g in &opts.groups {
            let rel = opts.mode(g) == PasteMode::Relative;
            match g {
                AdjustGroup::WhiteBalance => {
                    out.white_balance = if rel {
                        self.relative_wb(target.white_balance, t.as_shot, &mut warnings)
                    } else {
                        self.source.white_balance
                    };
                }
                AdjustGroup::Exposure => {
                    let (s, b, t) = (&self.source.exposure, &self.base.exposure, &target.exposure);
                    out.exposure = if rel {
                        crate::adjust::Exposure {
                            exposure_ev: shift(t.exposure_ev, s.exposure_ev, b.exposure_ev),
                            contrast: shift(t.contrast, s.contrast, b.contrast),
                            brightness: shift(t.brightness, s.brightness, b.brightness),
                            saturation: shift(t.saturation, s.saturation, b.saturation),
                        }
                    } else {
                        *s
                    };
                }
                AdjustGroup::Hdr => {
                    let (s, b, t) = (&self.source.hdr, &self.base.hdr, &target.hdr);
                    out.hdr = if rel {
                        crate::adjust::Hdr {
                            highlights: shift(t.highlights, s.highlights, b.highlights),
                            shadows: shift(t.shadows, s.shadows, b.shadows),
                            whites: shift(t.whites, s.whites, b.whites),
                            blacks: shift(t.blacks, s.blacks, b.blacks),
                        }
                    } else {
                        *s
                    };
                }
                AdjustGroup::Presence => {
                    let (s, b, t) = (&self.source.presence, &self.base.presence, &target.presence);
                    out.presence = if rel {
                        crate::adjust::Presence {
                            vibrance: shift(t.vibrance, s.vibrance, b.vibrance),
                            texture: shift(t.texture, s.texture, b.texture),
                            dehaze: shift(t.dehaze, s.dehaze, b.dehaze),
                        }
                    } else {
                        *s
                    };
                }
                AdjustGroup::Profile => out.profile.clone_from(&self.source.profile),
                AdjustGroup::Spots => {
                    out.spots.clone_from(&self.source.spots);
                    out.removals.clone_from(&self.source.removals);
                }
                AdjustGroup::Levels => out.levels = self.source.levels,
                AdjustGroup::Curve => out.curve = self.source.curve.clone(),
                AdjustGroup::ColorBalance => out.color_balance = self.source.color_balance,
                AdjustGroup::ColorEditor => out.color_editor = self.source.color_editor.clone(),
                AdjustGroup::Detail => out.detail = self.source.detail,
                AdjustGroup::Noise => out.noise = self.source.noise,
                AdjustGroup::Vignette => out.vignette = self.source.vignette,
                AdjustGroup::Effects => out.effects = self.source.effects.clone(),
                AdjustGroup::Lens => out.lens = self.source.lens.clone(),
                AdjustGroup::Crop => {
                    out.crop = adapt_crop(&self.source.crop, self.source_size, t.size);
                }
                AdjustGroup::Layers => {
                    let picked = self
                        .source
                        .layers
                        .iter()
                        .filter(|l| opts.layer_ids.is_empty() || opts.layer_ids.contains(&l.id))
                        .cloned();
                    if opts.replace_layers {
                        out.layers = picked.collect();
                    } else {
                        let next_id = out.layers.iter().map(|l| l.id).max().map_or(0, |m| m + 1);
                        out.layers.extend(picked.enumerate().map(|(i, mut l)| {
                            l.id = next_id + i as u64;
                            l
                        }));
                    }
                }
            }
        }
        out.sanitize();
        Pasted {
            adjustments: out,
            warnings,
        }
    }

    /// Relative WB: Kelvin transfers as a ratio, tint as a difference. `AsShot`
    /// on any side is read through the as-shot context; if a needed value is
    /// missing (or the base is degenerate), fall back to the source's absolute
    /// value and warn.
    fn relative_wb(
        &self,
        target: WhiteBalance,
        target_as_shot: Option<(f32, f32)>,
        warnings: &mut Vec<PasteWarning>,
    ) -> WhiteBalance {
        let (base, src) = (self.base.white_balance, self.source.white_balance);
        if base == src {
            return target;
        }
        let resolved = (
            resolve_wb(base, self.source_as_shot),
            resolve_wb(src, self.source_as_shot),
            resolve_wb(target, target_as_shot),
        );
        if let (Some((bk, bt)), Some((sk, st)), Some((tk, tt))) = resolved {
            let temp_k = tk * sk / bk;
            let tint = tt + st - bt;
            if bk.is_finite() && bk > 0.0 && temp_k.is_finite() && tint.is_finite() {
                return WhiteBalance::Custom {
                    temp_k: temp_k.clamp(2000.0, 50000.0),
                    tint: tint.clamp(-150.0, 150.0),
                };
            }
        }
        warnings.push(PasteWarning::WhiteBalanceAbsoluteFallback);
        src
    }
}

/// `target + (source - base)`, ignoring a non-finite delta (keeps the target).
/// Range clamping happens in the final sanitize.
fn shift(target: f32, source: f32, base: f32) -> f32 {
    let delta = source - base;
    if delta.is_finite() {
        target + delta
    } else {
        target
    }
}

fn resolve_wb(wb: WhiteBalance, as_shot: Option<(f32, f32)>) -> Option<(f32, f32)> {
    match wb {
        WhiteBalance::Custom { temp_k, tint } => Some((temp_k, tint)),
        WhiteBalance::AsShot => as_shot,
    }
}

/// `size` as displayed after `quarter_turns` extra rotations.
fn turned(size: Size, quarter_turns: u8) -> Size {
    if quarter_turns % 2 == 1 {
        Size::new(size.height, size.width)
    } else {
        size
    }
}

/// Map a crop from the source frame onto the target frame.
///
/// Coordinates: `Crop::rect` is normalized in the *oriented* image (EXIF
/// applied, before the crop's own `quarter_turns` and straighten). The pasted
/// crop keeps the source's `quarter_turns`, so orientation is judged on the
/// displayed frames (oriented size turned by those quarter turns) on both sides.
///
/// - Unlocked crop, same displayed orientation → rect copied as-is (normalized,
///   even if the sensor aspect differs).
/// - Unlocked crop, different orientation → the rect is transposed about its
///   center (a landscape-image crop becomes the portrait equivalent).
/// - Aspect-locked crop whose orientation **matches** its source frame (the
///   usual 3:2 crop of a 3:2 frame) → it follows the target: 3:2 on landscape
///   targets, 2:3 on portrait targets.
/// - Aspect-locked crop whose orientation **differs** from its frame (a
///   deliberate 4:5 portrait crop out of a landscape frame) → its own
///   orientation is kept on every target.
pub fn adapt_crop(src: &Crop, src_size: Size, dst_size: Size) -> Crop {
    let mut out = *src;
    if src.rect.is_full() && src.aspect.is_none() {
        return out;
    }
    let src_disp = turned(src_size, src.quarter_turns);
    let dst_disp = turned(dst_size, src.quarter_turns);
    // Rect orientation in displayed pixels (a transpose of the turn cancels out
    // when comparing with the frame, so compare in oriented pixels directly).
    let rect_portrait = |r: NormRect, s: Size| {
        f64::from(r.h) * f64::from(s.height) > f64::from(r.w) * f64::from(s.width)
    };
    let src_crop_portrait = rect_portrait(src.rect, src_size) != (src.quarter_turns % 2 == 1);
    let follows_frame = src_crop_portrait == src_disp.is_portrait();
    if src_disp.is_portrait() != dst_disp.is_portrait() && (src.aspect.is_none() || follows_frame) {
        out.rect = transpose(src.rect);
    }
    if let Some(a) = src.aspect {
        // Desired orientation of the crop in displayed pixels.
        let want_disp_portrait = if follows_frame {
            dst_disp.is_portrait()
        } else {
            src_crop_portrait
        };
        // ...and in oriented pixels, where the rect lives.
        let want_portrait = want_disp_portrait != (src.quarter_turns % 2 == 1);
        if rect_portrait(out.rect, dst_size) != want_portrait {
            out.rect = transpose(out.rect);
        }
        out.rect = fit_aspect(out.rect, a.long, a.short, dst_size, want_portrait);
    }
    out.rect = out.rect.clamped();
    out
}

/// Swap width and height about the rect's center.
fn transpose(r: NormRect) -> NormRect {
    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    NormRect {
        x: cy - r.h / 2.0,
        y: cx - r.w / 2.0,
        w: r.h,
        h: r.w,
    }
}

/// Shrink `rect` (about its center) so that in pixels of `size` it has the
/// long:short ratio, portrait or landscape as requested.
fn fit_aspect(rect: NormRect, long: u32, short: u32, size: Size, portrait: bool) -> NormRect {
    if long == 0 || short == 0 || size.width == 0 || size.height == 0 {
        return rect;
    }
    let (iw, ih) = (f64::from(size.width), f64::from(size.height));
    let (mut pw, mut ph) = (f64::from(rect.w) * iw, f64::from(rect.h) * ih);
    let ratio = if portrait {
        f64::from(short) / f64::from(long)
    } else {
        f64::from(long) / f64::from(short)
    };
    if pw / ph > ratio {
        pw = ph * ratio;
    } else {
        ph = pw / ratio;
    }
    let (w, h) = ((pw / iw) as f32, (ph / ih) as f32);
    let (cx, cy) = (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
    NormRect {
        x: cx - w / 2.0,
        y: cy - h / 2.0,
        w,
        h,
    }
}

#[cfg(test)]
mod tests {
    // Exact float equality is intended: defaults and clamps produce exact values.
    #![allow(
        clippy::float_cmp,
        clippy::too_many_lines,
        clippy::field_reassign_with_default
    )]

    use super::*;
    use crate::adjust::{Aspect, Exposure, Hdr, Layer, Levels};

    const LAND: Size = Size::new(6000, 4000);
    const PORT: Size = Size::new(4000, 6000);
    const SQUARE: Size = Size::new(4000, 4000);

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    fn rect_approx(a: NormRect, b: NormRect) -> bool {
        approx(a.x, b.x) && approx(a.y, b.y) && approx(a.w, b.w) && approx(a.h, b.h)
    }

    /// Pixel aspect (w/h) of `r` on an image of `size`.
    fn px_ratio(r: NormRect, size: Size) -> f64 {
        (f64::from(r.w) * f64::from(size.width)) / (f64::from(r.h) * f64::from(size.height))
    }

    fn opts(groups: &[AdjustGroup]) -> PasteOptions {
        PasteOptions {
            groups: groups.iter().copied().collect(),
            ..PasteOptions::default()
        }
    }

    fn rel(groups: &[AdjustGroup]) -> PasteOptions {
        PasteOptions {
            relative: groups.iter().copied().collect(),
            ..opts(groups)
        }
    }

    fn layer(id: u64, name: &str) -> Layer {
        Layer {
            id,
            name: name.into(),
            ..Layer::default()
        }
    }

    fn clip(source: Adjustments, base: Adjustments) -> Clipboard {
        Clipboard {
            source,
            base,
            source_size: LAND,
            source_as_shot: None,
        }
    }

    fn wb(temp_k: f32, tint: f32) -> WhiteBalance {
        WhiteBalance::Custom { temp_k, tint }
    }

    // ------------------------------------------------------------ groups ---

    #[test]
    fn all_groups_and_relative_support() {
        assert_eq!(PasteOptions::all().groups.len(), AdjustGroup::ALL.len());
        let rel: Vec<_> = AdjustGroup::ALL
            .into_iter()
            .filter(|g| g.supports_relative())
            .collect();
        assert_eq!(
            rel,
            [
                AdjustGroup::WhiteBalance,
                AdjustGroup::Exposure,
                AdjustGroup::Presence,
                AdjustGroup::Hdr
            ]
        );
    }

    #[test]
    fn changed_reports_each_group() {
        let a = Adjustments::default();
        assert!(AdjustGroup::changed(&a, &a).is_empty());
        let mut b = a.clone();
        b.white_balance = wb(5000.0, 0.0);
        b.exposure.exposure_ev = 1.0;
        b.hdr.shadows = 10.0;
        b.levels.gamma = 1.2;
        b.curve.rgb.push([0.5, 0.6]);
        b.color_balance.master.sat = 0.1;
        b.color_editor
            .advanced
            .push(crate::adjust::ColorRange::default());
        b.detail.clarity = 5.0;
        b.noise.color = 0.0;
        b.vignette.amount_ev = -1.0;
        b.lens.enabled = false;
        b.crop.angle_deg = 2.0;
        b.layers.push(layer(0, "x"));
        b.presence.dehaze = 10.0;
        b.spots.push(crate::adjust::Spot::default());
        b.effects.grain.amount = 20.0;
        b.profile = crate::adjust::Profile::Vivid;
        let all: GroupSet = AdjustGroup::ALL.into_iter().collect();
        assert_eq!(AdjustGroup::changed(&a, &b), all);
        // The profile pastes absolutely, and only with its group.
        let c = clip(b.clone(), a.clone());
        let only = |g: AdjustGroup| PasteOptions {
            groups: [g].into_iter().collect(),
            ..PasteOptions::default()
        };
        let t = PasteTarget {
            size: LAND,
            as_shot: None,
        };
        assert_eq!(
            c.apply_to(&a, &t, &only(AdjustGroup::Profile))
                .adjustments
                .profile,
            crate::adjust::Profile::Vivid
        );
        assert_eq!(
            c.apply_to(&a, &t, &only(AdjustGroup::Exposure))
                .adjustments
                .profile,
            crate::adjust::Profile::Standard
        );

        // Effects paste whole; the applied look's record is the target's own.
        let mut fx = a.clone();
        fx.effects.lens_blur.amount = 40.0;
        fx.effects.lens_blur.focus = Some([0.3, 0.4]);
        fx.look = Some(crate::effects::AppliedLook::default());
        let effects: GroupSet = [AdjustGroup::Effects].into_iter().collect();
        assert_eq!(AdjustGroup::changed(&a, &fx), effects);
        let pasted = clip(fx.clone(), a.clone()).apply(
            &a,
            LAND,
            &PasteOptions {
                groups: effects,
                ..PasteOptions::default()
            },
        );
        assert_eq!(pasted.effects, fx.effects);
        assert_eq!(pasted.look, None);

        let mut c = a.clone();
        c.hdr.whites = 3.0;
        assert_eq!(
            AdjustGroup::changed(&a, &c),
            [AdjustGroup::Hdr].into_iter().collect()
        );

        // A removal alone is a Heal & Clone change, and pastes with that group only.
        let mut r = a.clone();
        r.removals.push(crate::adjust::Removal::default());
        let spots: GroupSet = [AdjustGroup::Spots].into_iter().collect();
        assert_eq!(AdjustGroup::changed(&a, &r), spots);
        let all_but_spots = PasteOptions {
            groups: all.difference(&spots).copied().collect(),
            ..PasteOptions::default()
        };
        let cl = clip(r.clone(), a.clone());
        assert_eq!(cl.apply(&a, LAND, &all_but_spots).removals.len(), 0);
        let only_spots = PasteOptions {
            groups: spots,
            ..PasteOptions::default()
        };
        assert_eq!(cl.apply(&a, LAND, &only_spots).removals, r.removals);
    }

    #[test]
    fn empty_groups_is_noop() {
        let mut src = Adjustments::default();
        src.exposure.exposure_ev = 2.0;
        let mut target = Adjustments::default();
        target.hdr.blacks = 7.0;
        let out = clip(src, Adjustments::default()).apply(&target, LAND, &PasteOptions::default());
        assert_eq!(out, target);
    }

    #[test]
    fn absolute_paste_copies_selected_groups_only() {
        let mut src = Adjustments::default();
        src.white_balance = wb(5600.0, 10.0);
        src.exposure = Exposure {
            exposure_ev: 1.5,
            contrast: 20.0,
            brightness: -5.0,
            saturation: 3.0,
        };
        src.hdr.highlights = -40.0;
        src.levels = Levels {
            in_black: 0.05,
            ..Levels::default()
        };
        src.curve.luma = vec![[0.0, 0.1], [1.0, 0.9]];
        src.color_balance.shadows.hue = 200.0;
        src.color_editor.basic[0].saturation = 30.0;
        src.detail.structure = 12.0;
        src.noise.luminance = 60.0;
        src.vignette.midpoint = 0.2;
        src.lens.profile = Some("Sony FE 24-70mm".into());

        let mut target = Adjustments::default();
        target.exposure.exposure_ev = -0.7;
        target.hdr.shadows = 25.0;

        let groups = [
            AdjustGroup::WhiteBalance,
            AdjustGroup::Exposure,
            AdjustGroup::Levels,
            AdjustGroup::Curve,
            AdjustGroup::ColorBalance,
            AdjustGroup::ColorEditor,
            AdjustGroup::Detail,
            AdjustGroup::Noise,
            AdjustGroup::Vignette,
            AdjustGroup::Lens,
        ];
        let out = clip(src.clone(), Adjustments::default()).apply(&target, LAND, &opts(&groups));
        assert_eq!(out.white_balance, src.white_balance);
        assert_eq!(out.exposure, src.exposure);
        assert_eq!(out.levels, src.levels);
        assert_eq!(out.curve, src.curve);
        assert_eq!(out.color_balance, src.color_balance);
        assert_eq!(out.color_editor, src.color_editor);
        assert_eq!(out.detail, src.detail);
        assert_eq!(out.noise, src.noise);
        assert_eq!(out.vignette, src.vignette);
        assert_eq!(out.lens, src.lens);
        // Not selected: untouched.
        assert_eq!(out.hdr, target.hdr);
        assert_eq!(out.crop, target.crop);
        assert_eq!(out.layers, target.layers);
    }

    #[test]
    fn relative_flag_ignored_for_unsupported_groups() {
        let mut src = Adjustments::default();
        src.levels.gamma = 2.0;
        let mut base = Adjustments::default();
        base.levels.gamma = 1.5;
        let mut target = Adjustments::default();
        target.levels.gamma = 0.8;
        let out = clip(src, base).apply(&target, LAND, &rel(&[AdjustGroup::Levels]));
        assert_eq!(out.levels.gamma, 2.0, "levels always paste absolutely");
    }

    #[test]
    fn relative_flag_without_group_does_nothing() {
        let mut src = Adjustments::default();
        src.exposure.exposure_ev = 1.0;
        let o = PasteOptions {
            relative: [AdjustGroup::Exposure].into_iter().collect(),
            ..PasteOptions::default()
        };
        let target = Adjustments::default();
        assert_eq!(
            clip(src, Adjustments::default()).apply(&target, LAND, &o),
            target
        );
    }

    // ---------------------------------------------------------- relative ---

    #[test]
    fn relative_exposure_adds_delta_and_clamps() {
        let mut base = Adjustments::default();
        base.exposure = Exposure {
            exposure_ev: 0.2,
            contrast: 10.0,
            brightness: 0.0,
            saturation: 0.0,
        };
        let mut src = base.clone();
        src.exposure = Exposure {
            exposure_ev: 0.5,
            contrast: 40.0,
            brightness: -10.0,
            saturation: 90.0,
        };
        let mut target = Adjustments::default();
        target.exposure = Exposure {
            exposure_ev: -1.0,
            contrast: 80.0,
            brightness: -95.0,
            saturation: 50.0,
        };
        let out = clip(src, base).apply(&target, LAND, &rel(&[AdjustGroup::Exposure]));
        assert!(approx(out.exposure.exposure_ev, -0.7));
        assert_eq!(out.exposure.contrast, 100.0, "80 + 30 clamps to 100");
        assert_eq!(out.exposure.brightness, -100.0, "-95 - 10 clamps to -100");
        assert_eq!(out.exposure.saturation, 100.0);

        let mut big = Adjustments::default();
        big.exposure.exposure_ev = 4.0;
        let mut t = Adjustments::default();
        t.exposure.exposure_ev = 3.0;
        let out = clip(big.clone(), Adjustments::default()).apply(
            &t,
            LAND,
            &rel(&[AdjustGroup::Exposure]),
        );
        assert_eq!(out.exposure.exposure_ev, 5.0);
        t.exposure.exposure_ev = -3.0;
        let mut neg = big;
        neg.exposure.exposure_ev = -4.0;
        let out = clip(neg, Adjustments::default()).apply(&t, LAND, &rel(&[AdjustGroup::Exposure]));
        assert_eq!(out.exposure.exposure_ev, -5.0);
    }

    #[test]
    fn relative_hdr_adds_delta_and_clamps() {
        let mut src = Adjustments::default();
        src.hdr = Hdr {
            highlights: -50.0,
            shadows: 30.0,
            whites: 5.0,
            blacks: -5.0,
        };
        let mut target = Adjustments::default();
        target.hdr = Hdr {
            highlights: -70.0,
            shadows: 10.0,
            whites: 98.0,
            blacks: -98.0,
        };
        let out = clip(src, Adjustments::default()).apply(&target, LAND, &rel(&[AdjustGroup::Hdr]));
        assert_eq!(
            out.hdr,
            Hdr {
                highlights: -100.0,
                shadows: 40.0,
                whites: 100.0,
                blacks: -100.0
            }
        );
    }

    #[test]
    fn absolute_hdr_replaces() {
        let mut src = Adjustments::default();
        src.hdr.highlights = -50.0;
        let mut target = Adjustments::default();
        target.hdr.shadows = 10.0;
        let out = clip(src.clone(), Adjustments::default()).apply(
            &target,
            LAND,
            &opts(&[AdjustGroup::Hdr]),
        );
        assert_eq!(out.hdr, src.hdr);
    }

    #[test]
    fn relative_wb_transfers_kelvin_ratio_and_tint_delta() {
        let src = Adjustments {
            white_balance: wb(6000.0, 5.0),
            ..Adjustments::default()
        };
        let base = Adjustments {
            white_balance: wb(5000.0, 0.0),
            ..Adjustments::default()
        };
        let target = Adjustments {
            white_balance: wb(4000.0, -10.0),
            ..Adjustments::default()
        };
        let out = clip(src, base).apply(&target, LAND, &rel(&[AdjustGroup::WhiteBalance]));
        let WhiteBalance::Custom { temp_k, tint } = out.white_balance else {
            panic!()
        };
        assert!(approx(temp_k, 4800.0));
        assert!(approx(tint, -5.0));
    }

    #[test]
    fn relative_wb_clamps() {
        let src = Adjustments {
            white_balance: wb(40000.0, 150.0),
            ..Adjustments::default()
        };
        let base = Adjustments {
            white_balance: wb(4000.0, 0.0),
            ..Adjustments::default()
        };
        let target = Adjustments {
            white_balance: wb(10000.0, 100.0),
            ..Adjustments::default()
        };
        let out = clip(src, base).apply(&target, LAND, &rel(&[AdjustGroup::WhiteBalance]));
        assert_eq!(out.white_balance, wb(50000.0, 150.0));
        let src = Adjustments {
            white_balance: wb(2000.0, -150.0),
            ..Adjustments::default()
        };
        let base = Adjustments {
            white_balance: wb(8000.0, 0.0),
            ..Adjustments::default()
        };
        let target = Adjustments {
            white_balance: wb(3000.0, -100.0),
            ..Adjustments::default()
        };
        let out = clip(src, base).apply(&target, LAND, &rel(&[AdjustGroup::WhiteBalance]));
        assert_eq!(out.white_balance, wb(2000.0, -150.0));
    }

    #[test]
    fn relative_wb_falls_back_to_source_when_any_side_as_shot() {
        let custom = wb(5500.0, 3.0);
        for (base, src, target) in [
            (WhiteBalance::AsShot, custom, wb(4000.0, 0.0)),
            (wb(5000.0, 0.0), custom, WhiteBalance::AsShot),
            (wb(5000.0, 0.0), WhiteBalance::AsShot, wb(4000.0, 0.0)),
        ] {
            let c = clip(
                Adjustments {
                    white_balance: src,
                    ..Adjustments::default()
                },
                Adjustments {
                    white_balance: base,
                    ..Adjustments::default()
                },
            );
            let t = Adjustments {
                white_balance: target,
                ..Adjustments::default()
            };
            assert_eq!(
                c.apply(&t, LAND, &rel(&[AdjustGroup::WhiteBalance]))
                    .white_balance,
                src
            );
        }
    }

    #[test]
    fn relative_wb_with_degenerate_base_falls_back_to_source() {
        // A zero/negative/non-finite base temperature used to divide by zero
        // (inf clamped to 50000 K, or NaN for 0/0).
        for bk in [0.0, -100.0, f32::NAN, f32::INFINITY] {
            let src = wb(5500.0, 3.0);
            let c = clip(
                Adjustments {
                    white_balance: src,
                    ..Adjustments::default()
                },
                Adjustments {
                    white_balance: wb(bk, 0.0),
                    ..Adjustments::default()
                },
            );
            let t = Adjustments {
                white_balance: wb(4000.0, 0.0),
                ..Adjustments::default()
            };
            assert_eq!(
                c.apply(&t, LAND, &rel(&[AdjustGroup::WhiteBalance]))
                    .white_balance,
                src,
                "base {bk}"
            );
        }
        // Source 0 K with target 0 K would be 0*0/bk = 0 -> clamps to 2000; NaN target -> source.
        let c = clip(
            Adjustments {
                white_balance: wb(5500.0, 0.0),
                ..Adjustments::default()
            },
            Adjustments {
                white_balance: wb(5000.0, 0.0),
                ..Adjustments::default()
            },
        );
        let t = Adjustments {
            white_balance: wb(f32::NAN, 0.0),
            ..Adjustments::default()
        };
        assert_eq!(
            c.apply(&t, LAND, &rel(&[AdjustGroup::WhiteBalance]))
                .white_balance,
            wb(5500.0, 0.0)
        );
    }

    // -------------------------------------------------------------- crop ---

    fn crop_paste(src: Crop, from: Size, to: Size) -> Crop {
        let c = Clipboard {
            source: Adjustments {
                crop: src,
                ..Adjustments::default()
            },
            base: Adjustments::default(),
            source_size: from,
            source_as_shot: None,
        };
        c.apply(&Adjustments::default(), to, &opts(&[AdjustGroup::Crop]))
            .crop
    }

    #[test]
    fn full_crop_without_lock_is_returned_unchanged() {
        let src = Crop {
            angle_deg: 3.0,
            quarter_turns: 1,
            flip_h: true,
            ..Crop::default()
        };
        for to in [LAND, PORT, SQUARE] {
            assert_eq!(crop_paste(src, LAND, to), src);
        }
    }

    #[test]
    fn same_orientation_copies_rect() {
        let rect = NormRect {
            x: 0.1,
            y: 0.2,
            w: 0.5,
            h: 0.6,
        };
        let src = Crop {
            rect,
            angle_deg: -2.0,
            flip_v: true,
            ..Crop::default()
        };
        let out = crop_paste(src, LAND, Size::new(3000, 2000));
        assert_eq!(out, src);
        assert_eq!(crop_paste(src, PORT, Size::new(2000, 3000)), src);
    }

    #[test]
    fn orientation_change_transposes_about_center() {
        let rect = NormRect {
            x: 0.1,
            y: 0.2,
            w: 0.6,
            h: 0.5,
        };
        let out = crop_paste(
            Crop {
                rect,
                ..Crop::default()
            },
            LAND,
            PORT,
        );
        // Center (0.4, 0.45) maps to (0.45, 0.4); extents swap.
        assert!(
            rect_approx(
                out.rect,
                NormRect {
                    x: 0.2,
                    y: 0.1,
                    w: 0.5,
                    h: 0.6
                }
            ),
            "{:?}",
            out.rect
        );
        // Pixel aspect on the portrait target is the inverse of the source's.
        assert!((px_ratio(out.rect, PORT) - 1.0 / px_ratio(rect, LAND)).abs() < 1e-3);
        // And back again.
        let back = crop_paste(out, PORT, LAND);
        assert!(rect_approx(back.rect, rect));
    }

    #[test]
    fn square_counts_as_landscape() {
        let rect = NormRect {
            x: 0.1,
            y: 0.2,
            w: 0.6,
            h: 0.5,
        };
        let c = Crop {
            rect,
            ..Crop::default()
        };
        assert_eq!(crop_paste(c, LAND, SQUARE).rect, rect);
        let t = crop_paste(c, SQUARE, PORT).rect;
        assert!(rect_approx(
            t,
            NormRect {
                x: 0.2,
                y: 0.1,
                w: 0.5,
                h: 0.6
            }
        ));
    }

    #[test]
    fn aspect_lock_landscape_to_portrait() {
        // 3:2 crop covering 80% of a 6000x4000 frame: 4800x3200 px.
        let rect = NormRect {
            x: 0.1,
            y: 0.1,
            w: 0.8,
            h: 0.8,
        };
        let src = Crop {
            rect,
            aspect: Some(Aspect { long: 3, short: 2 }),
            ..Crop::default()
        };
        assert!((px_ratio(rect, LAND) - 1.5).abs() < 1e-6);
        let out = crop_paste(src, LAND, PORT);
        assert!(
            (px_ratio(out.rect, PORT) - 2.0 / 3.0).abs() < 1e-3,
            "{:?}",
            out.rect
        );
        assert_eq!(out.aspect, src.aspect);
        // Same orientation: unchanged.
        assert!(rect_approx(crop_paste(src, LAND, LAND).rect, rect));
    }

    #[test]
    fn aspect_lock_refits_onto_different_frame_aspect() {
        // 3:2 lock, full-frame crop, pasted onto a 4:3 landscape (e.g. phone) frame.
        let src = Crop {
            aspect: Some(Aspect { long: 3, short: 2 }),
            ..Crop::default()
        };
        let to = Size::new(4000, 3000);
        let out = crop_paste(src, LAND, to);
        assert!(
            (px_ratio(out.rect, to) - 1.5).abs() < 1e-3,
            "{:?}",
            out.rect
        );
        assert!(approx(out.rect.w, 1.0));
        assert!(
            approx(out.rect.y + out.rect.h / 2.0, 0.5),
            "centered vertically"
        );
        // Onto a square frame.
        let out = crop_paste(src, LAND, SQUARE);
        assert!((px_ratio(out.rect, SQUARE) - 1.5).abs() < 1e-3);
        // Onto a portrait frame it becomes 2:3.
        let out = crop_paste(src, LAND, PORT);
        assert!((px_ratio(out.rect, PORT) - 2.0 / 3.0).abs() < 1e-3);
        // Taller-than-ratio rect gets its height trimmed instead of width.
        let tall = Crop {
            rect: NormRect {
                x: 0.0,
                y: 0.0,
                w: 0.5,
                h: 1.0,
            },
            ..src
        };
        let to = Size::new(6001, 6000);
        let out = crop_paste(tall, LAND, to);
        assert!(approx(out.rect.w, 0.5));
        assert!(
            (px_ratio(out.rect, to) - 2.0 / 3.0).abs() < 1e-3,
            "portrait rect stays portrait"
        );
    }

    #[test]
    fn aspect_lock_keeps_portrait_crop_on_landscape_frame() {
        // A 4:5 portrait crop (Instagram) inside a landscape frame: 2400x3000 px.
        // Aspect is stored landscape-first, and orientation follows the crop
        // rectangle, so pasting onto another landscape frame must stay 4:5 portrait.
        let rect = NormRect {
            x: 0.3,
            y: 0.125,
            w: 0.4,
            h: 0.75,
        };
        let src = Crop {
            rect,
            aspect: Some(Aspect { long: 5, short: 4 }),
            ..Crop::default()
        };
        assert!((px_ratio(rect, LAND) - 0.8).abs() < 1e-6);
        let out = crop_paste(src, LAND, LAND);
        assert!(
            (px_ratio(out.rect, LAND) - 0.8).abs() < 1e-3,
            "{:?}",
            out.rect
        );
        assert!(rect_approx(out.rect, rect));
        // A deliberate portrait crop keeps its orientation on a portrait frame too.
        let out = crop_paste(src, LAND, PORT);
        assert!(
            (px_ratio(out.rect, PORT) - 0.8).abs() < 1e-3,
            "{:?}",
            out.rect
        );
    }

    #[test]
    fn degenerate_aspect_is_ignored() {
        let rect = NormRect {
            x: 0.1,
            y: 0.1,
            w: 0.5,
            h: 0.5,
        };
        for a in [Aspect { long: 0, short: 2 }, Aspect { long: 3, short: 0 }] {
            let src = Crop {
                rect,
                aspect: Some(a),
                ..Crop::default()
            };
            assert_eq!(crop_paste(src, LAND, LAND).rect, rect);
        }
        let src = Crop {
            rect,
            aspect: Some(Aspect { long: 3, short: 2 }),
            ..Crop::default()
        };
        // Empty target size: rect is only transposed (0x10 counts as portrait), never refit.
        let out = adapt_crop(&src, LAND, Size::new(10, 0)).rect;
        assert!(rect_approx(out, rect), "{out:?}");
    }

    #[test]
    fn adapted_crop_always_inside_frame() {
        let rect = NormRect {
            x: 0.9,
            y: 0.0,
            w: 0.3,
            h: 0.2,
        };
        let out = crop_paste(
            Crop {
                rect,
                aspect: Some(Aspect { long: 16, short: 9 }),
                ..Crop::default()
            },
            LAND,
            PORT,
        );
        assert!(out.rect.x >= 0.0 && out.rect.x + out.rect.w <= 1.0 + 1e-6);
        assert!(out.rect.y >= 0.0 && out.rect.y + out.rect.h <= 1.0 + 1e-6);
    }

    // ------------------------------------------------------------ layers ---

    fn with_layers(ls: Vec<Layer>) -> Adjustments {
        Adjustments {
            layers: ls,
            ..Adjustments::default()
        }
    }

    #[test]
    fn layers_append_remaps_ids_after_target_max() {
        let c = clip(
            with_layers(vec![layer(0, "a"), layer(1, "b")]),
            Adjustments::default(),
        );
        let target = with_layers(vec![layer(0, "t0"), layer(5, "t5")]);
        let out = c.apply(&target, LAND, &opts(&[AdjustGroup::Layers]));
        let ids: Vec<_> = out.layers.iter().map(|l| (l.id, l.name.as_str())).collect();
        assert_eq!(ids, [(0, "t0"), (5, "t5"), (6, "a"), (7, "b")]);
    }

    #[test]
    fn layers_append_to_empty_target_starts_at_zero() {
        let c = clip(
            with_layers(vec![layer(9, "a"), layer(4, "b")]),
            Adjustments::default(),
        );
        let out = c.apply(&Adjustments::default(), LAND, &opts(&[AdjustGroup::Layers]));
        let ids: Vec<_> = out.layers.iter().map(|l| l.id).collect();
        assert_eq!(ids, [0, 1]);
    }

    #[test]
    fn layers_filter_by_id() {
        let c = clip(
            with_layers(vec![layer(1, "a"), layer(2, "b"), layer(3, "c")]),
            Adjustments::default(),
        );
        let target = with_layers(vec![layer(2, "t")]);
        let o = PasteOptions {
            layer_ids: vec![3, 1],
            ..opts(&[AdjustGroup::Layers])
        };
        let out = c.apply(&target, LAND, &o);
        let got: Vec<_> = out.layers.iter().map(|l| (l.id, l.name.as_str())).collect();
        // Source order preserved; ids remapped past the target's max (2).
        assert_eq!(got, [(2, "t"), (3, "a"), (4, "c")]);
        let unique: std::collections::HashSet<_> = out.layers.iter().map(|l| l.id).collect();
        assert_eq!(unique.len(), out.layers.len());
    }

    #[test]
    fn layers_replace_keeps_source_ids() {
        let c = clip(
            with_layers(vec![layer(1, "a"), layer(2, "b")]),
            Adjustments::default(),
        );
        let target = with_layers(vec![layer(0, "t")]);
        let o = PasteOptions {
            replace_layers: true,
            ..opts(&[AdjustGroup::Layers])
        };
        let out = c.apply(&target, LAND, &o);
        assert_eq!(out.layers, c.source.layers);
        let o = PasteOptions {
            replace_layers: true,
            layer_ids: vec![2],
            ..opts(&[AdjustGroup::Layers])
        };
        let out = c.apply(&target, LAND, &o);
        assert_eq!(out.layers, vec![layer(2, "b")]);
    }

    #[test]
    fn serde_round_trip_options_and_clipboard() {
        let o = PasteOptions {
            layer_ids: vec![4],
            replace_layers: true,
            ..rel(&[AdjustGroup::Exposure])
        };
        let json = serde_json::to_string(&o).unwrap();
        assert!(json.contains(r#""exposure""#));
        assert_eq!(serde_json::from_str::<PasteOptions>(&json).unwrap(), o);
        assert_eq!(
            serde_json::from_str::<PasteOptions>("{}").unwrap(),
            PasteOptions::default()
        );
        let c = clip(with_layers(vec![layer(1, "a")]), Adjustments::default());
        let back: Clipboard = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
        assert_eq!(PasteMode::default(), PasteMode::Absolute);
        assert_eq!(
            serde_json::to_string(&PasteMode::Relative).unwrap(),
            r#""relative""#
        );
    }

    proptest::proptest! {
        #[test]
        fn crop_paste_stays_in_frame_and_honors_lock(
            x in 0.0f32..0.9, y in 0.0f32..0.9, w in 0.05f32..1.0, h in 0.05f32..1.0,
            long in 1u32..20, short in 1u32..20,
            sw in 100u32..8000, sh in 100u32..8000, dw in 100u32..8000, dh in 100u32..8000,
        ) {
            let rect = NormRect { x, y, w: w.min(1.0 - x), h: h.min(1.0 - y) };
            let (long, short) = (long.max(short), long.min(short));
            let src = Crop { rect, aspect: Some(Aspect { long, short }), ..Crop::default() };
            let dst = Size::new(dw, dh);
            let out = adapt_crop(&src, Size::new(sw, sh), dst);
            let r = out.rect;
            proptest::prop_assert!(r.x >= 0.0 && r.y >= 0.0);
            proptest::prop_assert!(r.x + r.w <= 1.0 + 1e-5 && r.y + r.h <= 1.0 + 1e-5);
            // The lock holds (either orientation) unless clamped to the minimum extent.
            if r.w > 1e-3 && r.h > 1e-3 {
                let pr = px_ratio(r, dst);
                let want = f64::from(long) / f64::from(short);
                let ok = (pr - want).abs() / want < 1e-2 || (pr - 1.0 / want).abs() * want < 1e-2;
                proptest::prop_assert!(ok, "ratio {pr} want {want} or inverse; {r:?}");
            }
        }
    }
}
