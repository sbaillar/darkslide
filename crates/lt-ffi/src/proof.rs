//! Soft proofing (`lt_export::proof`): the viewer's proof table for an ICC profile.
//!
//! The app lists the installed `ColorSync` profiles itself and passes a profile's
//! path; [`proof_lut`] returns the 3D table its viewer shader samples (see
//! `lt_export::proof` for the layout and the gamut measure).

use std::path::Path;

use lt_export::proof::{self, IccProfile, ProofIntent, ProofSettings};

use crate::error::DsError;
use crate::jobs::guard;

/// Rendering intent of a soft proof or an output-profile export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ProofRenderingIntent {
    Perceptual,
    RelativeColorimetric,
}

impl From<ProofRenderingIntent> for ProofIntent {
    fn from(i: ProofRenderingIntent) -> Self {
        match i {
            ProofRenderingIntent::Perceptual => Self::Perceptual,
            ProofRenderingIntent::RelativeColorimetric => Self::RelativeColorimetric,
        }
    }
}

impl crate::export::ExportOutputProfile {
    pub(crate) fn settings(&self) -> ProofSettings {
        ProofSettings {
            intent: self.intent.into(),
            black_point_compensation: self.black_point_compensation,
        }
    }
}

/// The viewer's proof table: `size`³ RGBA `f32` texels, red fastest. Input is
/// Rec.2020 (sRGB-encoded); RGB is the proofed colour in linear Rec.2020; alpha is the
/// signed distance to the profile's gamut surface (> 0: outside).
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ProofLutData {
    pub size: u32,
    pub rgba: Vec<f32>,
    /// The profile's description, e.g. "Adobe RGB (1998)".
    pub profile_name: String,
    /// The profile has rendering-intent tables: perceptual and relative colorimetric
    /// differ (false for matrix/TRC profiles such as sRGB or Adobe RGB).
    pub has_intents: bool,
    /// Gray profiles: the viewer tests chroma (max − min of the encoded Rec.2020 colour)
    /// against this instead of alpha (a line-shaped gamut no grid field can hold).
    pub neutral_tolerance: Option<f32>,
}

/// Build the proof table for the profile at `profile_path` (≈ 10–40 ms: call off the
/// main thread). A file that isn't a usable RGB, CMYK or gray profile is
/// `InvalidArgument` with the reason.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn proof_lut(
    profile_path: String,
    intent: ProofRenderingIntent,
    black_point_compensation: bool,
) -> Result<ProofLutData, DsError> {
    guard(|| {
        let profile = IccProfile::read(Path::new(&profile_path))
            .map_err(|e| DsError::invalid(e.to_string()))?;
        let settings = ProofSettings {
            intent: intent.into(),
            black_point_compensation,
        };
        let lut = proof::proof_lut(&profile, settings, proof::LUT_SIZE)
            .map_err(|e| DsError::invalid(e.to_string()))?;
        Ok(ProofLutData {
            size: lut.size as u32,
            rgba: lut.rgba,
            profile_name: profile.name().to_owned(),
            has_intents: !profile.matrix_only(),
            neutral_tolerance: lut.neutral_tolerance,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lt_export::OutputSpace;

    #[test]
    fn proof_lut_reads_a_profile_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("srgb.icc");
        std::fs::write(&f, lt_export::icc::profile(OutputSpace::Srgb)).unwrap();
        let path = f.to_string_lossy().into_owned();
        let lut = proof_lut(path.clone(), ProofRenderingIntent::Perceptual, true).unwrap();
        assert_eq!(lut.size as usize, proof::LUT_SIZE);
        assert_eq!(lut.rgba.len(), proof::LUT_SIZE.pow(3) * 4);
        assert_eq!(lut.profile_name, "sRGB (built-in)");
        assert!(!lut.has_intents, "sRGB is a matrix profile");
        assert_eq!(lut.neutral_tolerance, None);
        // The last texel is white: white, in gamut. The Rec.2020 green corner is out.
        let n = proof::LUT_SIZE;
        let white = &lut.rgba[(n * n * n - 1) * 4..];
        assert!(white[..3].iter().all(|v| (v - 1.0).abs() < 2e-3) && white[3] < 0.0);
        let green = (n - 1) * n * 4;
        assert!(lut.rgba[green + 3] > 0.0);
        let rel = proof_lut(path, ProofRenderingIntent::RelativeColorimetric, false).unwrap();
        assert_eq!(rel.size, lut.size);
    }

    #[test]
    fn unusable_profiles_are_invalid_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("none.icc").to_string_lossy().into_owned();
        assert!(matches!(
            proof_lut(missing, ProofRenderingIntent::Perceptual, false),
            Err(DsError::InvalidArgument { .. })
        ));
        let junk = dir.path().join("junk.icc");
        std::fs::write(&junk, b"junk").unwrap();
        let e = proof_lut(
            junk.to_string_lossy().into_owned(),
            ProofRenderingIntent::Perceptual,
            false,
        )
        .unwrap_err();
        assert!(e.to_string().contains("not a readable ICC profile"), "{e}");
    }
}
