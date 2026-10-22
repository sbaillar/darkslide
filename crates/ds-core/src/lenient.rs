//! Lenient float deserializers.
//!
//! JSON has no NaN/Infinity: `serde_json` writes non-finite floats as `null`.
//! Older builds could persist such values, and a strict reader would then reject
//! the whole sidecar. These helpers read `null` as `NaN`; [`Adjustments`]
//! deserialization then runs [`Adjustments::sanitize`], which replaces every
//! non-finite value with that field's default.
//!
//! [`Adjustments`]: crate::Adjustments
//! [`Adjustments::sanitize`]: crate::Adjustments::sanitize

use serde::{Deserialize, Deserializer};

fn or_nan(v: Option<f32>) -> f32 {
    v.unwrap_or(f32::NAN)
}

pub(crate) fn f32<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    Option::<f32>::deserialize(d).map(or_nan)
}

pub(crate) fn pair<'de, D: Deserializer<'de>>(d: D) -> Result<[f32; 2], D::Error> {
    <[Option<f32>; 2]>::deserialize(d).map(|p| p.map(or_nan))
}

pub(crate) fn pairs<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<[f32; 2]>, D::Error> {
    Vec::<[Option<f32>; 2]>::deserialize(d).map(|v| v.into_iter().map(|p| p.map(or_nan)).collect())
}
