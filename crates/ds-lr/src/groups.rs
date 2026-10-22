//! The develop categories as slices of an [`Adjustments`]: what an import
//! compares, writes and journals one category at a time. A slice is a JSON
//! object `{json pointer: value}`; the masks category is special (only the
//! layers an import made, tracked by id in its state record, are the import's:
//! a layer the user made is never touched, whatever it is called).

use ds_core::Adjustments;
use ds_core::adjust::Layer;
use serde_json::{Map, Value};

use crate::model::Category;

const LAYERS: &str = "/layers";

pub(crate) fn pointers(cat: Category) -> &'static [&'static str] {
    match cat {
        Category::Crop => &["/crop"],
        Category::Tone => &[
            "/exposure/exposure_ev",
            "/exposure/contrast",
            "/hdr",
            "/curve",
        ],
        Category::WhiteBalance => &["/white_balance"],
        Category::Color => &[
            "/exposure/saturation",
            "/presence/vibrance",
            "/color_editor",
            "/color_balance",
        ],
        Category::Detail => &[
            "/presence/texture",
            "/presence/dehaze",
            "/detail/clarity",
            "/detail/sharpen_amount",
            "/detail/sharpen_radius",
            "/detail/sharpen_threshold",
            "/noise/luminance",
            "/noise/color",
            "/noise/detail",
        ],
        Category::Effects => &["/vignette", "/effects/grain"],
        Category::Lens => &[
            "/lens/enabled",
            "/lens/distortion",
            "/lens/light_falloff",
            "/lens/chromatic_aberration",
        ],
        Category::Masks => &[LAYERS],
        _ => &[],
    }
}

fn to_value(a: &Adjustments) -> Value {
    serde_json::to_value(a).unwrap_or(Value::Null)
}

/// The category's part of `a`. `owned` are the ids of the layers an import
/// made (the masks category's part).
pub(crate) fn slice(cat: Category, a: &Adjustments, owned: &[u64]) -> Value {
    let mut out = Map::new();
    if cat == Category::Masks {
        // Ids are assigned when written: they don't take part in comparisons.
        let layers: Vec<Value> = a
            .layers
            .iter()
            .filter(|l| owned.contains(&l.id))
            .map(|l| {
                let mut v = serde_json::to_value(l).unwrap_or(Value::Null);
                if let Some(o) = v.as_object_mut() {
                    o.remove("id");
                }
                v
            })
            .collect();
        out.insert(LAYERS.to_owned(), Value::Array(layers));
        return Value::Object(out);
    }
    let whole = to_value(a);
    for p in pointers(cat) {
        if let Some(v) = whole.pointer(p) {
            out.insert((*p).to_owned(), v.clone());
        }
    }
    Value::Object(out)
}

/// `base` with the category's part replaced by `part` (a [`slice`]), and the
/// ids of the layers the masks part wrote. A part that doesn't fit leaves
/// `base` as it was.
pub(crate) fn splice(
    cat: Category,
    base: &Adjustments,
    part: &Value,
    owned: &[u64],
) -> (Adjustments, Vec<u64>) {
    let Some(part) = part.as_object() else {
        return (base.clone(), owned.to_vec());
    };
    if cat == Category::Masks {
        let mut a = base.clone();
        let new: Vec<Layer> = part
            .get(LAYERS)
            .and_then(Value::as_array)
            .map(|l| {
                l.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        a.layers.retain(|l| !owned.contains(&l.id));
        let first = a.layers.iter().map(|l| l.id).max().map_or(1, |m| m + 1);
        let mut ids = Vec::new();
        for (id, mut l) in (first..).zip(new) {
            l.id = id;
            ids.push(id);
            a.layers.push(l);
        }
        return (a.sanitized(), ids);
    }
    let mut whole = to_value(base);
    for (p, v) in part {
        if let Some(slot) = whole.pointer_mut(p) {
            *slot = v.clone();
        }
    }
    let out = serde_json::from_value(whole).unwrap_or_else(|_| base.clone());
    (out, owned.to_vec())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp, clippy::field_reassign_with_default)]
    use super::*;
    use ds_core::adjust::{Mask, WhiteBalance};

    fn layer(name: &str, id: u64) -> Layer {
        Layer {
            id,
            name: name.into(),
            ..Layer::default()
        }
    }

    #[test]
    fn slices_hold_only_their_pointers() {
        let mut a = Adjustments::default();
        a.exposure.exposure_ev = 1.0;
        a.exposure.saturation = 20.0;
        let tone = slice(Category::Tone, &a, &[]);
        assert_eq!(tone["/exposure/exposure_ev"], 1.0);
        assert!(tone.get("/exposure/saturation").is_none());
        assert_eq!(
            slice(Category::Color, &a, &[])["/exposure/saturation"],
            20.0
        );
        assert_eq!(slice(Category::Rating, &a, &[]), Value::Object(Map::new()));
        assert_eq!(pointers(Category::Keywords).len(), 0);
    }

    #[test]
    fn splice_replaces_only_the_category() {
        let mut base = Adjustments::default();
        base.exposure.saturation = 30.0;
        base.crop.angle_deg = 2.0;
        let mut src = Adjustments::default();
        src.exposure.exposure_ev = 0.7;
        src.exposure.contrast = 10.0;
        src.hdr.shadows = 40.0;
        src.white_balance = WhiteBalance::Custom {
            temp_k: 4000.0,
            tint: 3.0,
        };
        let out = splice(
            Category::Tone,
            &base,
            &slice(Category::Tone, &src, &[]),
            &[],
        )
        .0;
        assert_eq!(out.exposure.exposure_ev, 0.7);
        assert_eq!(out.hdr.shadows, 40.0);
        assert_eq!(out.exposure.saturation, 30.0, "colour is another category");
        assert_eq!(out.crop.angle_deg, 2.0);
        assert_eq!(out.white_balance, WhiteBalance::AsShot);
        // A part that doesn't fit changes nothing.
        assert_eq!(splice(Category::Tone, &base, &Value::Null, &[]).0, base);
        let bad = serde_json::json!({"/hdr": "text"});
        assert_eq!(splice(Category::Tone, &base, &bad, &[]).0, base);
        // Round trip.
        assert_eq!(
            slice(
                Category::Tone,
                &splice(
                    Category::Tone,
                    &base,
                    &slice(Category::Tone, &src, &[]),
                    &[]
                )
                .0,
                &[]
            ),
            slice(Category::Tone, &src, &[])
        );
    }

    #[test]
    fn masks_touch_only_layers_the_import_owns() {
        let mut base = Adjustments::default();
        base.layers = vec![
            layer("My brush", 5),
            layer("Lightroom look", 6),
            layer("Lightroom: Linear 1", 7),
            layer("Lightroom: Mine too", 8),
        ];
        let owned = [7];
        let mut src = Adjustments::default();
        src.layers = vec![Layer {
            mask: Mask {
                invert: true,
                ..Mask::default()
            },
            ..layer("Lightroom: Linear 1", 99)
        }];
        let part = slice(Category::Masks, &src, &[99]);
        assert!(part["/layers"][0].get("id").is_none());
        let (out, ids) = splice(Category::Masks, &base, &part, &owned);
        let names: Vec<&str> = out.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "My brush",
                "Lightroom look",
                "Lightroom: Mine too",
                "Lightroom: Linear 1"
            ],
            "only the owned layer was replaced, even next to look-alikes"
        );
        assert_eq!(ids, [9], "fresh id after every other layer");
        assert!(out.layers[3].mask.invert);
        // Ids don't matter to comparisons.
        assert_eq!(slice(Category::Masks, &out, &ids), part);
        // Only the owned layers are in the slice.
        assert_eq!(
            slice(Category::Masks, &base, &owned)["/layers"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // No part at all: the owned layers go away.
        let (cleared, none) = splice(Category::Masks, &base, &serde_json::json!({}), &owned);
        assert_eq!(cleared.layers.len(), 3);
        assert_eq!(none.len(), 0);
        assert_eq!(splice(Category::Masks, &base, &Value::Null, &owned).0, base);
    }
}
