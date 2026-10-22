use ds_template::*;
#[test]
fn h6_every_strftime_spec_parses_or_rejects_and_never_panics_in_render() {
    let dt = chrono::NaiveDate::from_ymd_opt(2026, 9, 25)
        .unwrap()
        .and_hms_opt(1, 2, 3)
        .unwrap();
    let specs = [
        "%Z", "%z", "%:z", "%::z", "%:::z", "%#z", "%+", "%s", "%.3f", "%f", "%P", "%U", "%G",
        "%%", "%", "%Q", "%E", "%-d", "%_H", "%0e", "%.9f", "%3f", "%v", "%c", "%x", "%X", "%r",
        "%n", "%t", "%j", "%k", "%l",
    ];
    for s in specs {
        for t in [format!("{{date:{s}}}"), format!("{{time:{s}}}")] {
            let parsed = std::panic::catch_unwind(|| Template::parse(&t));
            let Ok(parsed) = parsed else {
                panic!("parse panicked on {t}")
            };
            if let Ok(tpl) = parsed {
                let v = Vars {
                    name: "A",
                    seq: 1,
                    datetime: Some(dt),
                    camera: None,
                    ext: "ARW",
                };
                let r = std::panic::catch_unwind(|| {
                    (
                        tpl.render_file_name(&v),
                        tpl.render_subfolder(&v).map(|_| ()),
                    )
                });
                assert!(r.is_ok(), "render panicked on {t}");
                let (name, _) = r.unwrap();
                eprintln!("{t:14} -> ok {name:?}");
                let v2 = Vars {
                    datetime: None,
                    ..v
                };
                assert!(
                    std::panic::catch_unwind(|| tpl.render_file_name(&v2)).is_ok(),
                    "no-date render panicked {t}"
                );
            } else {
                eprintln!("{t:14} -> rejected");
            }
        }
    }
}
