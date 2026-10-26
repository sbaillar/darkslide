use super::*;

fn looks(text: &str, builtin: bool) -> Result<(Vec<LookItem>, Vec<String>), String> {
    let n = normalize(Kind::Looks, "com.example.p", builtin, text.as_bytes())?;
    Ok((serde_json::from_str(&n.json).unwrap(), n.skipped))
}

#[test]
fn third_party_looks_are_namespaced_with_derived_uuids() {
    let text = r#"[{"id":"warm","name":"Warm","summary":"s","exact":["exposure.saturation"],
        "values":{"exposure.saturation":-100,"fx.grain.amount":12}}]"#;
    let (items, skipped) = looks(text, false).unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(items[0].id, "com.example.p/warm");
    let uuid = items[0].uuid.clone().unwrap();
    assert_eq!(&uuid[14..15], "8");
    let again = normalize(Kind::Looks, "com.example.p", false, text.as_bytes()).unwrap();
    let other = normalize(Kind::Looks, "com.example.q", false, text.as_bytes()).unwrap();
    assert!(again.json.contains(&uuid) && !other.json.contains(&uuid));
}

#[test]
fn bundled_looks_keep_their_ids_and_uuids() {
    let ok =
        r#"[{"id":"look.a","uuid":"00000000-0000-0000-0000-00000000b001","name":"A","values":{}}]"#;
    let (items, _) = looks(ok, true).unwrap();
    assert_eq!(items[0].id, "look.a");
    assert_eq!(
        items[0].uuid.as_deref(),
        Some("00000000-0000-0000-0000-00000000B001")
    );
    let (_, skipped) = looks(r#"[{"id":"look.a","name":"A"}]"#, true).unwrap();
    assert!(skipped[0].contains("needs its `uuid`"));
    let (_, skipped) = looks(
        r#"[{"id":"a","uuid":"00000000-0000-0000-0000-00000000b001","name":"A"}]"#,
        true,
    )
    .unwrap();
    assert!(skipped[0].contains("starts with `look.`"));
    let (_, skipped) = looks(r#"[{"id":"look.a","uuid":"nope","name":"A"}]"#, true).unwrap();
    assert!(skipped[0].contains("not a UUID"));
}

#[test]
fn bad_look_items_are_left_out_with_a_reason() {
    let cases = [
        (r#"[{"id":"","name":"A"}]"#, "needs a `name`"),
        (
            r#"[{"id":"a","name":"A","values":{"bogus.x":1}}]"#,
            "needs a newer Luxtable",
        ),
        (
            r#"[{"id":"a","name":"A","values":{"exposure.ev":40}}]"#,
            "outside",
        ),
        (
            r#"[{"id":"a","name":"A","values":{"crop.aspect":2}}]"#,
            "can't set",
        ),
        (
            r#"[{"id":"a","name":"A","values":{"wb.asShot":1}}]"#,
            "can't set",
        ),
        (
            r#"[{"id":"a","name":"A","exact":["hdr.blacks"]}]"#,
            "has no value",
        ),
        (
            r#"[{"id":"a","name":"A","curves":{"RGB":[[0,0]]}}]"#,
            "no curves",
        ),
        (
            r#"[{"id":"a","name":"A","uuid":"00000000-0000-0000-0000-00000000b001"}]"#,
            "leave it out",
        ),
        (
            r#"[{"id":"a","name":"A"},{"id":"a","name":"B"}]"#,
            "used twice",
        ),
    ];
    for (text, want) in cases {
        let (_, skipped) = looks(text, false).unwrap();
        assert!(
            skipped.iter().any(|s| s.contains(want)),
            "{text}: {skipped:?}"
        );
    }
    assert!(looks("not json", false).unwrap_err().contains("line 1"));
    let many = format!("[{}]", vec![r#"{"id":"x","name":"x"}"#; 201].join(","));
    assert!(looks(&many, false).unwrap_err().contains("at most"));
    let big = vec![b' '; MAX_FILE_BYTES + 1];
    assert!(
        normalize(Kind::Looks, "p.q", false, &big)
            .unwrap_err()
            .contains("1 MB")
    );
}

#[test]
fn styles_are_explicit_and_checked() {
    let ok = r#"[{"name":"Warm","section":"Film","groups":["exposure","colorBalance","curve"],"relative":["exposure"],
        "absolute":["exposure.saturation"],"values":{"exposure.contrast":-8,"lens.enabled":1},"curves":{"RGB":[[0.1,0.2]]}}]"#;
    let n = normalize(Kind::Styles, "p.q", false, ok.as_bytes()).unwrap();
    assert!(n.skipped.is_empty(), "{:?}", n.skipped);
    let out: Vec<StyleItem> = serde_json::from_str(&n.json).unwrap();
    assert!(out[0].uuid.is_some());
    let cases = [
        (
            r#"[{"name":"","section":"F","groups":["exposure"]}]"#,
            "needs a `name`",
        ),
        (
            r#"[{"name":"A","section":"F","groups":["nope"]}]"#,
            "unknown group",
        ),
        (r#"[{"name":"A","section":"F","groups":[]}]"#, "is empty"),
        (
            r#"[{"name":"A","section":"F","groups":["hdr"],"relative":["exposure"]}]"#,
            "relative",
        ),
        (
            r#"[{"name":"A","section":"F","groups":["lens"],"values":{"lens.enabled":0.5}}]"#,
            "0 or 1",
        ),
        (
            r#"[{"name":"A","section":"F","groups":["hdr"],"absolute":["x.y"]}]"#,
            "newer Luxtable",
        ),
        (
            r#"[{"name":"A","section":"F","groups":["curve"],"curves":{"CMYK":[[0,0]]}}]"#,
            "curve channel",
        ),
        (
            r#"[{"name":"A","section":"F","groups":["curve"],"curves":{"RGB":[[0,2]]}}]"#,
            "each in 0–1",
        ),
        (
            r#"[{"name":"A","section":"F","groups":["hdr"],"uuid":"00000000-0000-0000-0000-00000000a001"}]"#,
            "leave it out",
        ),
        (
            r#"[{"name":"A","section":"F","groups":["hdr"]},{"name":"A","section":"F","groups":["hdr"]}]"#,
            "two styles",
        ),
    ];
    for (text, want) in cases {
        let n = normalize(Kind::Styles, "p.q", false, text.as_bytes()).unwrap();
        assert!(
            n.skipped.iter().any(|s| s.contains(want)),
            "{text}: {:?}",
            n.skipped
        );
    }
    let bundled = r#"[{"uuid":"00000000-0000-0000-0000-00000000a004","name":"B","section":"F","groups":["hdr"]}]"#;
    assert!(
        normalize(Kind::Styles, "luxtable.x", true, bundled.as_bytes())
            .unwrap()
            .json
            .contains("A004")
    );
}

#[test]
fn phrase_packs_need_a_language() {
    let n = normalize(
        Kind::VoicePhrases,
        "p.q",
        false,
        br#"{"language":"en","phrases":{"keeper":"key-pick","":"x"}}"#,
    )
    .unwrap();
    assert!(n.json.contains("keeper") && n.skipped.len() == 1);
    assert!(normalize(Kind::VoicePhrases, "p.q", false, br#"{"phrases":{}}"#).is_err());
    assert!(
        normalize(
            Kind::VoicePhrases,
            "p.q",
            false,
            br#"{"language":"e n","phrases":{}}"#
        )
        .is_err()
    );
    assert!(normalize(Kind::ExportPostProcess, "p.q", false, b"[]").is_err());
}

#[test]
fn unknown_item_fields_leave_that_item_out() {
    let text = r#"[{"id":"ok","name":"Ok"},{"id":"new","name":"New","flags":{"x":1}}]"#;
    let (items, skipped) = looks(text, false).unwrap();
    assert_eq!(items.len(), 1);
    assert!(
        skipped[0].contains("look `new`") && skipped[0].contains("unknown field"),
        "{skipped:?}"
    );
    let n = normalize(
        Kind::Styles,
        "p.q",
        false,
        br#"[{"name":"A","section":"F","groups":["hdr"],"blend":1}, 7]"#,
    )
    .unwrap();
    assert_eq!(n.skipped.len(), 2, "{:?}", n.skipped);
    assert!(n.skipped[1].contains("style 2"));
}
