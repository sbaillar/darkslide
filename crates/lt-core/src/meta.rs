//! Per-image organizational metadata (ratings, tags) and content identity.

use std::fmt;

use serde::{Deserialize, Serialize};

/// BLAKE3 digest of a file's bytes. Identity that survives moves and renames.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileHash(pub [u8; 32]);

impl FileHash {
    pub fn to_hex(&self) -> String {
        use fmt::Write;
        self.0.iter().fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        // `from_str_radix` accepts a leading `+`, so check digits explicitly.
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for FileHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileHash({})", &self.to_hex()[..12])
    }
}

/// Star rating, 0 (unrated) to 5.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(try_from = "u8", into = "u8")]
pub struct Rating(u8);

impl Rating {
    pub const MAX: u8 = 5;

    pub fn new(stars: u8) -> Option<Self> {
        (stars <= Self::MAX).then_some(Self(stars))
    }

    pub fn stars(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Rating {
    type Error = String;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        Self::new(v).ok_or_else(|| format!("rating {v} out of range 0..=5"))
    }
}

impl From<Rating> for u8 {
    fn from(r: Rating) -> u8 {
        r.0
    }
}

/// Color labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorTag {
    #[default]
    None,
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Pink,
    Purple,
}

/// Lightroom-style pick flag, orthogonal to rating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickState {
    #[default]
    Unflagged,
    Picked,
    Rejected,
}

/// A position set by hand (map geotagging), decimal degrees: negative latitude
/// = south, negative longitude = west; altitude in metres (negative = below sea
/// level). Lives in the sidecar; the original is never written.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeoPoint {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub altitude_m: Option<f64>,
}

impl GeoPoint {
    /// `None` when a coordinate is out of range or not finite; a non-finite
    /// altitude is dropped.
    pub fn new(latitude: f64, longitude: f64, altitude_m: Option<f64>) -> Option<Self> {
        let ok = latitude.is_finite()
            && longitude.is_finite()
            && latitude.abs() <= 90.0
            && longitude.abs() <= 180.0;
        ok.then(|| Self {
            latitude,
            longitude,
            altitude_m: altitude_m.filter(|a| a.is_finite()),
        })
    }

    /// Whether [`GeoPoint::new`] would accept it (a sidecar edited by hand may not).
    pub fn is_valid(&self) -> bool {
        Self::new(self.latitude, self.longitude, self.altitude_m).as_ref() == Some(self)
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

    fn sample_hash() -> FileHash {
        let mut b = [0u8; 32];
        for (i, v) in b.iter_mut().enumerate() {
            *v = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        FileHash(b)
    }

    #[test]
    fn hex_round_trip() {
        let h = sample_hash();
        let hex = h.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(FileHash::from_hex(&hex), Some(h));
        assert_eq!(FileHash::from_hex(&hex.to_uppercase()), Some(h));
        assert_eq!(FileHash([0; 32]).to_hex(), "0".repeat(64));
        assert_eq!(FileHash([0xff; 32]).to_hex(), "f".repeat(64));
    }

    #[test]
    fn from_hex_rejects_bad_input() {
        assert_eq!(FileHash::from_hex(""), None);
        assert_eq!(FileHash::from_hex(&"a".repeat(63)), None);
        assert_eq!(FileHash::from_hex(&"a".repeat(65)), None);
        assert_eq!(FileHash::from_hex(&format!("{}zz", "a".repeat(62))), None);
        assert_eq!(FileHash::from_hex(&format!("+1{}", "a".repeat(62))), None);
        // 64 bytes but multibyte chars: must not panic on a char boundary.
        let non_ascii = format!("é{}", "a".repeat(62));
        assert_eq!(non_ascii.len(), 64);
        assert_eq!(FileHash::from_hex(&non_ascii), None);
    }

    #[test]
    fn debug_shows_short_prefix() {
        let h = sample_hash();
        assert_eq!(format!("{h:?}"), format!("FileHash({})", &h.to_hex()[..12]));
    }

    #[test]
    fn hash_serde_round_trip() {
        let h = sample_hash();
        let back: FileHash = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn rating_bounds() {
        for s in 0..=5 {
            assert_eq!(Rating::new(s).unwrap().stars(), s);
            assert_eq!(u8::from(Rating::try_from(s).unwrap()), s);
        }
        assert_eq!(Rating::new(6), None);
        assert!(Rating::try_from(255).unwrap_err().contains("255"));
        assert_eq!(Rating::default().stars(), 0);
        assert!(Rating::new(2) < Rating::new(3));
    }

    #[test]
    fn rating_serde_is_plain_number_and_rejects_out_of_range() {
        assert_eq!(
            serde_json::to_string(&Rating::new(4).unwrap()).unwrap(),
            "4"
        );
        let r: Rating = serde_json::from_str("3").unwrap();
        assert_eq!(r.stars(), 3);
        assert!(serde_json::from_str::<Rating>("6").is_err());
        assert!(serde_json::from_str::<Rating>("-1").is_err());
    }

    #[test]
    fn tag_and_pick_serde_names() {
        assert_eq!(
            serde_json::to_string(&ColorTag::Purple).unwrap(),
            r#""purple""#
        );
        assert_eq!(
            serde_json::to_string(&PickState::Rejected).unwrap(),
            r#""rejected""#
        );
        assert_eq!(
            serde_json::from_str::<ColorTag>(r#""none""#).unwrap(),
            ColorTag::None
        );
        assert_eq!(
            serde_json::from_str::<PickState>(r#""picked""#).unwrap(),
            PickState::Picked
        );
        assert_eq!(ColorTag::default(), ColorTag::None);
        assert_eq!(PickState::default(), PickState::Unflagged);
        assert!(serde_json::from_str::<ColorTag>(r#""teal""#).is_err());
    }

    #[test]
    fn geo_point_validates_ranges() {
        let p = GeoPoint::new(-33.8568, 151.2153, Some(-4.5)).unwrap();
        assert_eq!(
            (p.latitude, p.longitude, p.altitude_m),
            (-33.8568, 151.2153, Some(-4.5))
        );
        assert!(p.is_valid());
        assert!(GeoPoint::new(90.0, -180.0, None).is_some());
        assert_eq!(GeoPoint::new(90.1, 0.0, None), None);
        assert_eq!(GeoPoint::new(0.0, -180.5, None), None);
        assert_eq!(GeoPoint::new(f64::NAN, 0.0, None), None);
        assert_eq!(GeoPoint::new(0.0, f64::INFINITY, None), None);
        assert_eq!(
            GeoPoint::new(1.0, 2.0, Some(f64::NAN)).unwrap().altitude_m,
            None
        );
        let bad = GeoPoint {
            latitude: 100.0,
            longitude: 0.0,
            altitude_m: None,
        };
        assert!(!bad.is_valid());
        // Altitude is optional in JSON.
        let j: GeoPoint = serde_json::from_str(r#"{"latitude":1.5,"longitude":-2.0}"#).unwrap();
        assert_eq!(j, GeoPoint::new(1.5, -2.0, None).unwrap());
    }
}
