//! Lens corrections from the lensfun database (vendored subset: every calibrated Sony
//! E-mount lens, Sony and third party, see `data/lensfun-sony-e.xml`, CC BY-SA 3.0).
//!
//! Models (lensfun conventions):
//! - distortion `ptlens`: `r_d = r_u·(a·r_u³ + b·r_u² + c·r_u + 1 − a − b − c)` and `poly3`
//!   (`a = c = 0`, `b = k1`), with `r = 1` at half the short side;
//! - lateral CA `poly3`/`linear`: red/blue radii scaled by `v + c·r + b·r²`;
//! - vignetting `pa`: `1 + k1·r² + k2·r⁴ + k3·r⁶` with `r = 1` at the corner.
//!
//! Coefficients are interpolated linearly in focal length (and aperture for vignetting).

use std::sync::OnceLock;

const DB: &str = include_str!("../data/lensfun-sony-e.xml");

/// Lens makers in the database (and aliases), dropped from names for matching.
const MAKERS: [(&str, &str); 7] = [
    ("sony", "Sony"),
    ("sigma", "Sigma"),
    ("tamron", "Tamron"),
    ("samyang", "Samyang"),
    ("rokinon", "Samyang"),
    ("zeiss", "Zeiss"),
    ("tokina", "Tokina"),
];

/// Name words that a camera's lens string and lensfun's name for the same lens may or
/// may not carry: the mount ("E", "FE"), "AF", Tamron's "Di III"/"RXD" and "OSD",
/// Samyang's coatings, and Sigma's release-year codes ("019"). Everything else (focal
/// length, aperture, series, "II"/"G2" generations, model codes) must agree.
fn generic_token(t: &str) -> bool {
    matches!(
        t,
        "e" | "fe"
            | "af"
            | "di"
            | "iii"
            | "rxd"
            | "osd"
            | "umc"
            | "as"
            | "if"
            | "ed"
            | "ncs"
            | "cs"
    ) || (t.len() == 3 && t.starts_with('0') && t.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Dist {
    focal: f32,
    abc: [f32; 3],
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Tca {
    focal: f32,
    /// (v, c, b) for red and blue.
    red: [f32; 3],
    blue: [f32; 3],
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Vig {
    focal: f32,
    aperture: f32,
    k: [f32; 3],
}

/// One calibrated lens.
#[derive(Debug, Clone, PartialEq)]
pub struct LensProfile {
    pub maker: String,
    pub model: String,
    /// Crop factor of the camera the lens was calibrated on (coefficients are in that
    /// sensor's units).
    pub crop_factor: f32,
    /// Aspect ratio (long / short side) of that sensor; lensfun's default is 3:2.
    pub aspect: f32,
    /// The lens's names in the database: the camera's lens strings and English names
    /// (`model` is the first untranslated one). Other translations match but aren't
    /// listed.
    pub aliases: Vec<String>,
    /// The same, normalized ([`normalize_name`]) and as words ([`tokens`]).
    keys: Vec<String>,
    names: Vec<Vec<String>>,
    dist: Vec<Dist>,
    tca: Vec<Tca>,
    vig: Vec<Vig>,
}

/// Interpolated corrections for one capture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LensModel {
    /// ptlens a, b, c (zero = no distortion).
    pub dist: [f32; 3],
    /// (v, c, b) radial scale polynomials for red and blue.
    pub tca_red: [f32; 3],
    pub tca_blue: [f32; 3],
    /// pa k1, k2, k3.
    pub vig: [f32; 3],
}

#[derive(Debug, Clone, Copy)]
struct VigRow {
    vig: Vig,
    distance: f32,
}

impl LensModel {
    pub const IDENTITY: LensModel = LensModel {
        dist: [0.0; 3],
        tca_red: [1.0, 0.0, 0.0],
        tca_blue: [1.0, 0.0, 0.0],
        vig: [0.0; 3],
    };

    /// Distorted radius factor `r_d / r_u` at undistorted radius `r` (short-side units).
    #[inline]
    pub fn dist_factor(&self, r: f32) -> f32 {
        let [a, b, c] = self.dist;
        a * r * r * r + b * r * r + c * r + 1.0 - a - b - c
    }

    /// TCA radius factor for a channel polynomial at radius `r`.
    #[inline]
    pub fn tca_factor(p: [f32; 3], r: f32) -> f32 {
        p[0] + p[1] * r + p[2] * r * r
    }

    /// Vignetting falloff `C(r)` (corner = 1); the correction divides by it.
    #[inline]
    pub fn vig_falloff(&self, r: f32) -> f32 {
        let r2 = r * r;
        1.0 + r2 * (self.vig[0] + r2 * (self.vig[1] + r2 * self.vig[2]))
    }

    /// Scale each correction by a strength (0 = off, 1 = the full profile). The light
    /// falloff slider defaults to 0.5 (`LensCorrection::default`), like in-camera shading
    /// compensation: fully flattened corners look unnatural. Non-finite strengths fall
    /// back to the defaults (1 for distortion, 0.5 for falloff).
    #[must_use]
    pub fn with_strength(mut self, distortion: f32, ca: bool, falloff: f32) -> Self {
        let d = if distortion.is_finite() {
            distortion.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let l = if falloff.is_finite() {
            falloff.clamp(0.0, 1.0)
        } else {
            0.5
        };
        self.dist = self.dist.map(|v| v * d);
        self.vig = self.vig.map(|v| v * l);
        if !ca {
            self.tca_red = Self::IDENTITY.tca_red;
            self.tca_blue = Self::IDENTITY.tca_blue;
        }
        self
    }

    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }
}

/// Normalize a lens name for matching: lowercase alphanumerics, "f/2.8" ≡ "F2.8",
/// without the maker and parenthesized suffixes such as "(SEL35F14GM)".
pub fn normalize_name(s: &str) -> String {
    let mut depth = 0u32;
    let mut l = s.to_lowercase();
    for (m, _) in MAKERS {
        l = l.replace(m, "");
    }
    l.chars()
        .filter(|&c| {
            match c {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                _ => return depth == 0 && c.is_ascii_alphanumeric(),
            }
            false
        })
        .collect()
}

/// A lens name as words for matching: lowercase, without parenthesized suffixes and maker
/// names, "f/2.8" ≡ "F2.8", "56 mm" ≡ "56mm", and an aperture range with equal ends
/// ("F2.8-2.8", as Tamron zooms report it) ≡ "f2.8". Returns the words and the maker the
/// name mentions, if any.
pub fn tokens(s: &str) -> (Vec<String>, Option<&'static str>) {
    let mut depth = 0u32;
    let clean: String = s
        .to_lowercase()
        .replace("f/", "f")
        .chars()
        .map(|c| {
            match c {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                _ if depth == 0 && (c.is_ascii_alphanumeric() || c == '.' || c == '-') => {
                    return c;
                }
                _ => {}
            }
            ' '
        })
        .collect();
    let mut maker = None;
    let mut out: Vec<String> = Vec::new();
    for w in clean.split_whitespace() {
        let w = w.trim_matches(['.', '-']);
        if w.is_empty() {
            continue;
        }
        if let Some((_, m)) = MAKERS.iter().find(|(k, _)| *k == w) {
            maker = Some(*m);
            continue;
        }
        if w == "mm"
            && let Some(prev) = out.last_mut()
            && prev
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'-' || b == b'.')
        {
            prev.push_str("mm");
            continue;
        }
        let w = match w.strip_prefix('f').and_then(|r| r.split_once('-')) {
            Some((a, b)) if a == b && !a.is_empty() => format!("f{a}"),
            _ => w.to_owned(),
        };
        out.push(w);
    }
    (out, maker)
}

fn attr(line: &str, name: &str) -> Option<f32> {
    let pat = format!(" {name}=\"");
    let start = line.find(&pat)? + pat.len();
    let end = line[start..].find('"')? + start;
    line[start..end].parse().ok()
}

fn text(line: &str, tag: &str) -> Option<String> {
    let open = line.find(&format!("<{tag}"))?;
    let gt = line[open..].find('>')? + open + 1;
    let close = line[gt..].find(&format!("</{tag}>"))? + gt;
    Some(line[gt..close].trim().to_string())
}

/// Parse the vendored database (line-oriented; the file is machine-formatted).
fn parse(db: &str) -> Vec<LensProfile> {
    let mut out = Vec::new();
    let mut cur: Option<LensProfile> = None;
    let mut vig: Vec<VigRow> = Vec::new();
    for line in db.lines() {
        let l = line.trim();
        if l.starts_with("<lens>") {
            cur = Some(LensProfile {
                maker: String::new(),
                model: String::new(),
                crop_factor: 1.0,
                aspect: 1.5,
                aliases: Vec::new(),
                keys: Vec::new(),
                names: Vec::new(),
                dist: Vec::new(),
                tca: Vec::new(),
                vig: Vec::new(),
            });
            vig.clear();
        } else if l.starts_with("</lens>") {
            if let Some(mut p) = cur.take() {
                p.dist.sort_by(|a, b| a.focal.total_cmp(&b.focal));
                p.tca.sort_by(|a, b| a.focal.total_cmp(&b.focal));
                // Vignetting is calibrated at several focus distances; keep the farthest
                // per focal length and aperture (focus at infinity, lensfun's default).
                p.vig = vig
                    .iter()
                    .filter(|r| {
                        !vig.iter().any(|o| {
                            o.vig.focal == r.vig.focal
                                && o.vig.aperture == r.vig.aperture
                                && o.distance > r.distance
                        })
                    })
                    .map(|r| r.vig)
                    .collect();
                out.push(p);
            }
        } else if let Some(p) = cur.as_mut() {
            if let Some(m) = text(l, "maker") {
                if !l.contains("lang=") {
                    p.maker = m;
                }
            } else if let Some(c) = text(l, "cropfactor") {
                p.crop_factor = c.parse().ok().filter(|c: &f32| *c > 0.0).unwrap_or(1.0);
            } else if let Some(a) = text(l, "aspect-ratio") {
                p.aspect = parse_aspect(&a).unwrap_or(1.5);
            } else if l.starts_with("<model") {
                // Every name (the camera's lens string and translations) is matched; the
                // first untranslated one is the display name.
                if let Some(m) = text(l, "model") {
                    p.keys.push(normalize_name(&m));
                    p.names.push(tokens(&m).0);
                    if !l.contains("lang=") || l.contains("lang=\"en\"") {
                        p.aliases.push(m.clone());
                    }
                    if p.model.is_empty() && !l.contains("lang=") {
                        p.model = m;
                    }
                }
            } else if l.starts_with("<distortion") {
                let Some(focal) = attr(l, "focal") else {
                    continue;
                };
                let abc = if l.contains("model=\"ptlens\"") {
                    [
                        attr(l, "a").unwrap_or(0.0),
                        attr(l, "b").unwrap_or(0.0),
                        attr(l, "c").unwrap_or(0.0),
                    ]
                } else if l.contains("model=\"poly3\"") {
                    [0.0, attr(l, "k1").unwrap_or(0.0), 0.0]
                } else {
                    continue;
                };
                p.dist.push(Dist { focal, abc });
            } else if l.starts_with("<tca") {
                let Some(focal) = attr(l, "focal") else {
                    continue;
                };
                let (red, blue) = if l.contains("model=\"linear\"") {
                    (
                        [attr(l, "kr").unwrap_or(1.0), 0.0, 0.0],
                        [attr(l, "kb").unwrap_or(1.0), 0.0, 0.0],
                    )
                } else {
                    (
                        [
                            attr(l, "vr").unwrap_or(1.0),
                            attr(l, "cr").unwrap_or(0.0),
                            attr(l, "br").unwrap_or(0.0),
                        ],
                        [
                            attr(l, "vb").unwrap_or(1.0),
                            attr(l, "cb").unwrap_or(0.0),
                            attr(l, "bb").unwrap_or(0.0),
                        ],
                    )
                };
                p.tca.push(Tca { focal, red, blue });
            } else if l.starts_with("<vignetting")
                && l.contains("model=\"pa\"")
                && let (Some(focal), Some(aperture)) = (attr(l, "focal"), attr(l, "aperture"))
            {
                let k = [
                    attr(l, "k1").unwrap_or(0.0),
                    attr(l, "k2").unwrap_or(0.0),
                    attr(l, "k3").unwrap_or(0.0),
                ];
                vig.push(VigRow {
                    vig: Vig { focal, aperture, k },
                    distance: attr(l, "distance").unwrap_or(f32::INFINITY),
                });
            }
        }
    }
    out
}

/// An aspect ratio as lensfun writes it: "4:3" or a number (long / short side).
fn parse_aspect(s: &str) -> Option<f32> {
    let r = match s.split_once(':') {
        Some((a, b)) => a.trim().parse::<f32>().ok()? / b.trim().parse::<f32>().ok()?,
        None => s.trim().parse().ok()?,
    };
    (r.is_finite() && r > 0.0).then(|| r.max(1.0 / r))
}

/// All vendored lens profiles.
pub fn profiles() -> &'static [LensProfile] {
    static P: OnceLock<Vec<LensProfile>> = OnceLock::new();
    P.get_or_init(|| parse(DB))
}

/// Find a profile by EXIF lens model (or a profile override name) for a body with crop
/// factor `crop` (Sony FE full frame = 1.0), in order:
/// 1. a name equal to one of the profile's ([`normalize_name`]: makers, case,
///    punctuation and parenthesized suffixes don't count);
/// 2. a name whose words begin a profile's name (e.g. "FE 24-70mm F2.8" → "… GM"): the
///    shortest such name;
/// 3. a name with the same words as a profile's apart from [`generic_token`]s (e.g.
///    "Tamron 28-75mm F/2.8 Di III RXD" ≡ "E 28-75mm F2.8-2.8"), if exactly one lens
///    model matches.
///
/// Steps 2 and 3 keep to the maker the name mentions, if any. Among entries of the
/// found model, the calibration closest to `crop` wins.
pub fn find(name: &str, crop: f32) -> Option<&'static LensProfile> {
    let key = normalize_name(name);
    if key.len() < 4 {
        return None;
    }
    let exact: Vec<&LensProfile> = profiles()
        .iter()
        .filter(|p| p.keys.contains(&key))
        .collect();
    let pool = if exact.is_empty() {
        by_words(name)?
    } else {
        exact
    };
    let crop = if crop.is_finite() && crop > 0.0 {
        crop
    } else {
        1.0
    };
    pool.into_iter().min_by(|a, b| {
        (a.crop_factor - crop)
            .abs()
            .total_cmp(&(b.crop_factor - crop).abs())
    })
}

/// Steps 2 and 3 of [`find`].
fn by_words(name: &str) -> Option<Vec<&'static LensProfile>> {
    let (words, maker) = tokens(name);
    let candidates = || {
        profiles()
            .iter()
            .filter(move |p| maker.is_none_or(|m| p.maker == m))
    };
    let prefix_len = |p: &LensProfile| {
        p.names
            .iter()
            .filter(|n| n.starts_with(&words))
            .map(|n| n.iter().map(String::len).sum::<usize>())
            .min()
    };
    if let Some(shortest) = candidates().filter_map(prefix_len).min() {
        return Some(
            candidates()
                .filter(|p| prefix_len(p) == Some(shortest))
                .collect(),
        );
    }
    let core = |ws: &[String]| {
        let mut v: Vec<String> = ws.iter().filter(|w| !generic_token(w)).cloned().collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let want = core(&words);
    if want.len() < 2 {
        return None;
    }
    let found: Vec<&LensProfile> = candidates()
        .filter(|p| p.names.iter().any(|n| core(n) == want))
        .collect();
    let first = found.first()?;
    // Two different lenses fit: better no correction than a wrong one.
    if found
        .iter()
        .any(|p| p.maker != first.maker || p.model != first.model)
    {
        return None;
    }
    Some(found)
}

/// Linear interpolation over a focal-sorted list.
fn interp<T: Copy>(
    items: &[T],
    focal: f32,
    fk: impl Fn(&T) -> f32,
    val: impl Fn(&T) -> Vec<f32>,
) -> Option<Vec<f32>> {
    let first = items.first()?;
    let last = items.last()?;
    if focal <= fk(first) {
        return Some(val(first));
    }
    if focal >= fk(last) {
        return Some(val(last));
    }
    let i = items.iter().position(|it| fk(it) >= focal)?;
    let (a, b) = (&items[i - 1], &items[i]);
    let t = (focal - fk(a)) / (fk(b) - fk(a)).max(1e-6);
    Some(
        val(a)
            .iter()
            .zip(val(b))
            .map(|(x, y)| x + (y - x) * t)
            .collect(),
    )
}

impl LensProfile {
    /// Another calibration of the same lens (the same maker and a name in common), e.g.
    /// Sigma's 30mm F2.8 DN measured on APS-C and on Micro Four Thirds.
    pub fn same_lens(&self, other: &LensProfile) -> bool {
        self.maker == other.maker && self.keys.iter().any(|k| other.keys.contains(k))
    }

    /// Corrections at `focal` mm and f-number `aperture` (vignetting uses the nearest
    /// calibrated apertures; unknown aperture = widest).
    pub fn model(&self, focal: f32, aperture: Option<f32>) -> LensModel {
        let mut m = LensModel::IDENTITY;
        if let Some(v) = interp(&self.dist, focal, |d| d.focal, |d| d.abc.to_vec()) {
            m.dist = [v[0], v[1], v[2]];
        }
        if let Some(v) = interp(&self.tca, focal, |t| t.focal, |t| [t.red, t.blue].concat()) {
            m.tca_red = [v[0], v[1], v[2]];
            m.tca_blue = [v[3], v[4], v[5]];
        }
        let mut apertures: Vec<f32> = self.vig.iter().map(|v| v.aperture).collect();
        apertures.sort_by(f32::total_cmp);
        apertures.dedup();
        if let Some(&widest) = apertures.first() {
            let want = aperture
                .filter(|a| a.is_finite() && *a > 0.0)
                .unwrap_or(widest);
            let at = |ap: f32| {
                let mut rows: Vec<Vig> = self
                    .vig
                    .iter()
                    .filter(|v| v.aperture == ap)
                    .copied()
                    .collect();
                rows.sort_by(|a, b| a.focal.total_cmp(&b.focal));
                interp(&rows, focal, |v| v.focal, |v| v.k.to_vec()).unwrap_or_else(|| vec![0.0; 3])
            };
            let hi = apertures.iter().position(|a| *a >= want);
            let k = match hi {
                Some(0) => at(apertures[0]),
                None => at(apertures[apertures.len() - 1]),
                Some(i) => {
                    let (a0, a1) = (apertures[i - 1], apertures[i]);
                    let t = (want - a0) / (a1 - a0);
                    at(a0)
                        .iter()
                        .zip(at(a1))
                        .map(|(x, y)| x + (y - x) * t)
                        .collect()
                }
            };
            m.vig = [k[0], k[1], k[2]];
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_parses_and_finds_the_users_lens() {
        assert!(profiles().len() >= 30);
        let gm = find("FE 24-70mm F2.8 GM", 1.0).expect("EXIF name matches lensfun's");
        assert_eq!(gm.model, "FE 24-70mm f/2.8 GM");
        assert_eq!(gm.maker, "Sony");
        assert_eq!(gm.crop_factor, 1.0);
        // The user's lens under a shorter name resolves to the GM (not GM II), full frame.
        let short = find("Sony FE 24-70mm F2.8", 1.0).expect("prefix match");
        assert_eq!(short.model, "FE 24-70mm f/2.8 GM");
        assert!(find("FE 24-70mm F2.8 GM II", 1.0).is_some_and(|p| p.model.ends_with("GM II")));
        // Parenthesized suffixes in the database are ignored.
        let gm35 = find("FE 35mm F1.4 GM", 1.0).expect("SEL35F14GM");
        assert!(gm35.model.starts_with("FE 35mm f/1.4 GM"));
        // Two calibrations of one lens: the one matching the body's crop factor wins.
        let za = find("FE 24-70mm F4 ZA OSS", 1.0).unwrap();
        assert_eq!(za.crop_factor, 1.0);
        let za_apsc = find("FE 24-70mm F4 ZA OSS", 1.5).unwrap();
        assert!((za_apsc.crop_factor - 1.534).abs() < 1e-3);
        assert!(find("FE 24-70mm F4 ZA OSS", f32::NAN).is_some_and(|p| p.crop_factor == 1.0));
        assert!(find("Some Unknown 50mm", 1.0).is_none());
        assert!(find("", 1.0).is_none());
        assert!(find("FE", 1.0).is_none());
        assert_eq!(normalize_name("Sony FE 24-70mm f/2.8 GM"), "fe2470mmf28gm");
        assert_eq!(
            normalize_name("FE 35mm f/1.4 GM (SEL35F14GM)"),
            "fe35mmf14gm"
        );
    }

    /// Every entry of the vendored file is taken, with every calibration row: nothing is
    /// silently dropped by the line parser, and every coefficient is usable.
    #[test]
    fn every_lens_in_the_database_parses_with_valid_calibrations() {
        let all = profiles();
        assert_eq!(all.len(), 142);
        assert_eq!(all.len(), DB.matches("<lens>").count());
        let count = |maker: &str| all.iter().filter(|p| p.maker == maker).count();
        assert_eq!(
            [
                count("Sony"),
                count("Sigma"),
                count("Tamron"),
                count("Samyang"),
                count("Zeiss"),
                count("Tokina")
            ],
            [61, 36, 18, 14, 12, 1]
        );
        // Every distortion and TCA row, and one vignetting row per (focal, aperture).
        let rows = |tag: &str| DB.lines().filter(|l| l.trim().starts_with(tag)).count();
        assert_eq!(
            all.iter().map(|p| p.dist.len()).sum::<usize>(),
            rows("<distortion")
        );
        assert_eq!(all.iter().map(|p| p.tca.len()).sum::<usize>(), rows("<tca"));
        assert!(all.iter().map(|p| p.vig.len()).sum::<usize>() > 1000);
        for p in all {
            let name = format!("{} {}", p.maker, p.model);
            assert!(!p.maker.is_empty() && !p.model.is_empty(), "{name}");
            assert!(p.crop_factor >= 1.0 && p.crop_factor <= 2.0, "{name}");
            assert!(p.aspect >= 1.3 && p.aspect <= 1.5, "{name}");
            assert!(
                !(p.dist.is_empty() && p.tca.is_empty() && p.vig.is_empty()),
                "{name}: no calibration"
            );
            for d in &p.dist {
                assert!(
                    d.focal > 0.0 && d.abc.iter().all(|v| v.abs() < 1.0),
                    "{name}"
                );
            }
            for t in &p.tca {
                assert!(t.focal > 0.0, "{name}");
                for c in [t.red, t.blue] {
                    assert!((c[0] - 1.0).abs() < 0.01 && c[1].abs() < 0.01, "{name}");
                    assert!(c[2].abs() < 0.01, "{name}");
                }
            }
            let mut seen = std::collections::HashSet::new();
            for v in &p.vig {
                assert!(v.focal > 0.0 && v.aperture > 0.0, "{name}");
                // A real falloff: positive inside the frame (the correction divides by it).
                // At the very corner three APS-C kit zooms wide open (E 16-55 G, E 18-135,
                // E PZ 16-50) reach 0.05 and below; the gain is capped at 20x there
                // (`Geometry::sensor_at`).
                let m = LensModel {
                    vig: v.k,
                    ..LensModel::IDENTITY
                };
                assert!(
                    (0..20).all(|i| m.vig_falloff(i as f32 / 20.0) > 0.05),
                    "{name}: {v:?}"
                );
                let key = (v.focal.to_bits(), v.aperture.to_bits());
                assert!(seen.insert(key), "{name}: one row per focal and aperture");
            }
            // The model at the middle of its range is a real correction.
            let focal = p.dist.first().or(p.dist.last()).map_or(35.0, |d| d.focal);
            let m = p.model(focal, None);
            assert!(!m.is_identity(), "{name}");
            let fields = [m.dist, m.tca_red, m.tca_blue, m.vig];
            assert!(fields.iter().flatten().all(|v| v.is_finite()), "{name}");
            // The picker's name brings back this lens (the calibration nearest the body).
            let back = find(&p.model, p.crop_factor).expect(&name);
            assert_eq!((&back.maker, &back.model), (&p.maker, &p.model), "{name}");
        }
    }

    /// Third-party lenses by the names cameras and tools write for them.
    #[test]
    fn third_party_lenses_match_their_exif_names() {
        let model = |n: &str| find(n, 1.0).map(|p| format!("{} {}", p.maker, p.model));
        // Sigma writes "<focal> <aperture> <series> | <line> <year>" (Sony's LensModel has
        // no maker); tools often prefix it.
        for n in [
            "24-70mm F2.8 DG DN | Art 019",
            "Sigma 24-70mm F2.8 DG DN | Art 019",
            "SIGMA 24-70mm F2.8 DG DN | Art 019",
        ] {
            assert_eq!(
                model(n).as_deref(),
                Some("Sigma 24-70mm F2.8 DG DN | Art 019"),
                "{n}"
            );
        }
        assert_eq!(
            model("Sigma 24-70mm F2.8 DG DN II | Art 024").as_deref(),
            Some("Sigma 24-70mm F2.8 DG DN II | Art 024")
        );
        // The DSLR (HSM) lens is a different design: no match rather than the DN one.
        assert_eq!(model("Sigma 24-70mm F2.8 DG OS HSM | Art 017"), None);
        // Tamron's LensModel is "E 28-75mm F2.8-2.8"; ExifTool's lens ID spells it out.
        for n in ["E 28-75mm F2.8-2.8", "Tamron 28-75mm F/2.8 Di III RXD"] {
            assert_eq!(
                model(n).as_deref(),
                Some("Tamron E 28-75mm F2.8-2.8"),
                "{n}"
            );
        }
        // ...but the G2 (VXD) is its own lens.
        assert!(
            model("Tamron 28-75mm F2.8 Di III VXD G2").is_some_and(|m| m.contains("G2")),
            "{:?}",
            model("Tamron 28-75mm F2.8 Di III VXD G2")
        );
        // The first 70-180 is sold as "Di III VXD"; the G2 adds "VC ... G2".
        assert_eq!(
            model("Tamron 70-180mm F/2.8 Di III VXD").as_deref(),
            Some("Tamron E 70-180mm F2.8 A056")
        );
        assert_eq!(
            model("Tamron 70-180mm F/2.8 Di III VC VXD G2").as_deref(),
            Some("Tamron E 70-180mm F2.8 A065")
        );
        assert_eq!(
            model("E 70-180mm F2.8 A056").as_deref(),
            Some("Tamron E 70-180mm F2.8 A056")
        );
        // Samyang (also sold as Rokinon) adds "FE"; lensfun has no AF 35mm F1.4, and it
        // must not borrow the F1.8's profile.
        assert_eq!(
            model("Samyang AF 35mm F1.8 FE").as_deref(),
            Some("Samyang Samyang AF 35mm f/1.8")
        );
        assert_eq!(
            model("Rokinon AF 14mm F2.8 FE").as_deref(),
            Some("Samyang Samyang AF 14mm f/2.8")
        );
        assert_eq!(model("Samyang AF 35mm F1.4 FE"), None);
        // Without a maker, "AF 35mm F1.8 FE" fits Samyang's and Sony's 35mm F1.8 alike.
        assert_eq!(model("AF 35mm F1.8 FE"), None);
        assert_eq!(model("FE 35mm F1.8").as_deref(), Some("Sony FE 35mm f/1.8"));
        // A maker in the name keeps prefix matches to that maker.
        assert_eq!(model("Tamron FE 24-70mm F2.8"), None);
        // Zeiss Batis: the camera string and lensfun's other name.
        assert_eq!(model("Batis 25mm f/2").as_deref(), Some("Zeiss E 25mm f/2"));
        // Name words: makers and parentheses out, equal aperture ends folded, "56 mm".
        assert_eq!(
            tokens("Sigma 56 mm F1.4 DC DN | C 018 (x)"),
            (
                ["56mm", "f1.4", "dc", "dn", "c", "018"]
                    .map(String::from)
                    .to_vec(),
                Some("Sigma")
            )
        );
        assert_eq!(tokens("E 17-28mm F2.8-2.8").0, ["e", "17-28mm", "f2.8"]);
        assert_eq!(tokens("f/3.5-5.6").0, ["f3.5-5.6"]);
        assert_eq!(parse_aspect("4:3"), Some(4.0 / 3.0));
        assert_eq!(parse_aspect("2:3"), Some(1.5));
        assert_eq!(parse_aspect("1.5"), Some(1.5));
        assert_eq!(parse_aspect("x"), None);
        assert_eq!(parse_aspect("0:1"), None);
    }

    /// Vignetting is calibrated at several focus distances; the farthest one is used.
    #[test]
    fn vignetting_uses_the_farthest_focus_distance() {
        let db = r#"<lens>
            <maker>Sony</maker>
            <model>E 16mm f/2.8</model>
            <aspect-ratio>4:3</aspect-ratio>
            <maker lang="de">Sonie</maker>
            <vignetting model="pa" focal="16" aperture="2.8" distance="0.25" k1="-1" k2="0" k3="0"/>
            <vignetting model="pa" focal="16" aperture="2.8" distance="1000" k1="-0.5" k2="0" k3="0"/>
            <vignetting model="pa" focal="16" aperture="4" k1="-0.2" k2="0" k3="0"/>
            <vignetting model="pa" focal="16" aperture="4" distance="10" k1="-0.9" k2="0" k3="0"/>
        </lens>"#;
        let p = &parse(db)[0];
        assert_eq!((p.maker.as_str(), p.aspect), ("Sony", 4.0 / 3.0));
        assert_eq!(p.model(16.0, Some(2.8)).vig[0], -0.5);
        let v = p.model(16.0, Some(4.0)).vig[0];
        assert!((v + 0.2).abs() < 1e-6, "no distance = infinity: {v}");
    }

    #[test]
    fn interpolates_calibrations() {
        let gm = find("FE 24-70mm F2.8 GM", 1.0).unwrap();
        let m24 = gm.model(24.0, Some(2.8));
        assert_eq!(m24.dist, [0.02824, -0.07818, 0.03129]);
        assert!((m24.tca_red[0] - 1.000_179_7).abs() < 1e-7);
        assert_eq!(m24.vig, [-0.5106, -0.3627, 0.1448]);
        // Between 24 and 30 mm: halfway.
        let m27 = gm.model(27.0, Some(2.8));
        assert!((m27.dist[0] - f32::midpoint(0.02824, 0.01769)).abs() < 1e-6);
        // Clamped outside the calibrated range; aperture interpolated.
        assert_eq!(gm.model(10.0, Some(2.8)).dist, m24.dist);
        let v = gm.model(24.0, Some(3.15)).vig;
        assert!(
            (v[0] - f32::midpoint(-0.5106, -0.5248)).abs() < 1e-4,
            "{v:?}"
        );
        assert_eq!(gm.model(24.0, None).vig, m24.vig);
        assert_eq!(
            gm.model(24.0, Some(100.0)).vig,
            gm.model(24.0, Some(1000.0)).vig
        );
    }

    #[test]
    fn model_math() {
        let m = LensModel {
            dist: [0.0, 0.1, 0.0],
            vig: [-0.5, 0.0, 0.0],
            ..LensModel::IDENTITY
        };
        assert!((m.dist_factor(1.0) - 1.0).abs() < 1e-6);
        assert!(m.dist_factor(2.0) > 1.0);
        assert!((m.vig_falloff(1.0) - 0.5).abs() < 1e-6);
        assert_eq!(LensModel::tca_factor([1.0, 0.0, 0.0], 3.0), 1.0);
        let off = m.with_strength(0.0, false, 0.0);
        assert!(off.is_identity());
        let half = m.with_strength(0.5, true, f32::NAN);
        assert_eq!(half.dist[1], 0.05);
        assert_eq!(half.vig[0], -0.5 * 0.5);
        assert_eq!(
            m.with_strength(1.0, true, 1.0).vig,
            m.vig,
            "full profile at 1"
        );
        assert!(
            !LensModel {
                dist: [0.0, 0.1, 0.0],
                ..LensModel::IDENTITY
            }
            .is_identity()
        );
    }
}
