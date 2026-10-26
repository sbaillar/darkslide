use super::*;
use serde_json::json;

const BORDER: &str = include_str!("../../../plugins/film-border.ltplugin/border.wat");

fn never() -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(|| false)
}

fn params(v: Value) -> Map<String, Value> {
    let Value::Object(m) = v else {
        panic!("not an object")
    };
    m
}

fn ctx() -> Value {
    json!({"output": {"space": "srgb"}})
}

fn border() -> PostProcessor {
    PostProcessor::new(
        "Film Border",
        WasmModule::compile(BORDER.as_bytes()).unwrap(),
        "lt_post_process",
        0,
    )
    .unwrap()
}

/// A module with the API 1 boilerplate around `body` (a `lt_post_process` and extras).
fn module(body: &str) -> String {
    format!(
        r#"(module
  (import "luxtable" "error" (func $error (param i32 i32)))
  (import "luxtable" "log" (func $log (param i32 i32 i32)))
  (import "luxtable" "progress" (func $progress (param i32) (result i32)))
  (import "luxtable" "param_i64" (func $param_i64 (param i32 i32) (result i64)))
  (import "luxtable" "param_bool" (func $param_bool (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "nope")
  (data (i32.const 8) "flag")
  (data (i32.const 16) "count")
  (global $heap (mut i32) (i32.const 1024))
  (func (export "lt_api") (result i32) (i32.const 1))
  (func (export "lt_alloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (local.get $p) (local.get $len)))
    (drop (memory.grow (i32.add (i32.shr_u (local.get $len) (i32.const 16)) (i32.const 1))))
    (local.get $p))
  {body})"#
    )
}

fn pp(body: &str) -> PostProcessor {
    pp_apron(body, 0)
}

fn pp_apron(body: &str, apron: u32) -> PostProcessor {
    PostProcessor::new(
        "Test",
        WasmModule::compile(module(body).as_bytes()).unwrap(),
        "lt_post_process",
        apron,
    )
    .unwrap()
}

const SIG: &str = r#"(func (export "lt_post_process") (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)"#;

fn run8(
    p: &PostProcessor,
    px: &mut [u8],
    w: u32,
    h: u32,
    prm: Value,
) -> Result<Vec<String>, PluginError> {
    p.run(
        Pixels::Rgba8(px),
        w,
        h,
        [0, 0, w, h],
        params(prm),
        &ctx(),
        &|| false,
    )
}

#[test]
fn film_border_inside_paints_rgba8_and_keeps_alpha() {
    let (w, h) = (100u32, 60u32);
    let mut px = vec![200u8; (w * h * 4) as usize];
    for a in px.iter_mut().skip(3).step_by(4) {
        *a = 77;
    }
    run8(
        &border(),
        &mut px,
        w,
        h,
        json!({"width": 0.1, "color": "#102030", "outside": false}),
    )
    .unwrap();
    let at = |x: u32, y: u32| &px[((y * w + x) * 4) as usize..((y * w + x) * 4 + 4) as usize];
    // 10% of the short edge (60) = 6 px on every side.
    assert_eq!(at(0, 0), [16, 32, 48, 77]);
    assert_eq!(at(5, 30), [16, 32, 48, 77]);
    assert_eq!(at(94, 30), [16, 32, 48, 77]);
    assert_eq!(at(50, 54), [16, 32, 48, 77]);
    assert_eq!(at(6, 6), [200, 200, 200, 77]);
    assert_eq!(at(93, 53), [200, 200, 200, 77]);
}

#[test]
fn film_border_outside_grows_the_canvas_and_paints_around_the_photo() {
    let b = border();
    let p = params(json!({"width": 0.1, "color": "#FF0000", "outside": true}));
    let l = b.layout(100, 60, &p, &ctx()).unwrap();
    assert_eq!(
        l,
        Layout {
            width: 112,
            height: 72,
            x: 6,
            y: 6
        }
    );
    // The host's enlarged canvas: black margin, the photo (grey) at (6, 6).
    let (w, h) = (l.width, l.height);
    let mut px = vec![0u16; (w * h * 3) as usize];
    for y in 6..66 {
        for x in 6..106 {
            let i = ((y * w + x) * 3) as usize;
            px[i..i + 3].copy_from_slice(&[1000, 1000, 1000]);
        }
    }
    b.run(
        Pixels::Rgb16(&mut px),
        w,
        h,
        [6, 6, 100, 60],
        p,
        &ctx(),
        &|| false,
    )
    .unwrap();
    let at = |x: u32, y: u32| &px[((y * w + x) * 3) as usize..((y * w + x) * 3 + 3) as usize];
    assert_eq!(at(0, 0), [65535, 0, 0]);
    assert_eq!(at(5, 30), [65535, 0, 0]);
    assert_eq!(at(106, 30), [65535, 0, 0]);
    assert_eq!(at(6, 6), [1000, 1000, 1000], "the photo stays whole");
    assert_eq!(at(105, 65), [1000, 1000, 1000]);
    let inside = b
        .layout(100, 60, &params(json!({"outside": false})), &ctx())
        .unwrap();
    assert_eq!(
        inside,
        Layout {
            width: 100,
            height: 60,
            x: 0,
            y: 0
        }
    );
}

#[test]
fn film_border_blends_alpha_in_rgb16() {
    let (w, h) = (40u32, 40u32);
    let mut px = vec![0u16; (w * h * 3) as usize];
    let p = params(json!({"width": 0.05, "color": "#FF000080", "outside": false}));
    border()
        .run(
            Pixels::Rgb16(&mut px),
            w,
            h,
            [0, 0, w, h],
            p,
            &ctx(),
            &|| false,
        )
        .unwrap();
    assert_eq!(&px[..3], &[32896, 0, 0]);
    let inside = ((2 * w + 2) * 3) as usize;
    assert_eq!(&px[inside..inside + 3], &[0, 0, 0]);
}

#[test]
fn big_images_go_through_in_strips_with_the_same_result() {
    // 1200 × 1200 RGBA8 is 5.5 MB: two strips.
    let (w, h) = (1200u32, 1200u32);
    let mut px = vec![9u8; (w * h * 4) as usize];
    run8(
        &border(),
        &mut px,
        w,
        h,
        json!({"width": 0.01, "color": "#000000", "outside": false}),
    )
    .unwrap();
    let at = |x: u32, y: u32| px[((y * w + x) * 4) as usize];
    for (x, y, v) in [
        (0, 0, 0),
        (600, 11, 0),
        (600, 12, 9),
        (600, 1187, 9),
        (600, 1188, 0),
        (1199, 600, 0),
        (600, 600, 9),
    ] {
        assert_eq!(at(x, y), v, "({x}, {y})");
    }
}

#[test]
fn strips_carry_their_apron() {
    // Returns 3 unless the strip's buffer starts `apron` rows above its own rows
    // (or at the top) and covers 2 rows below (or the bottom).
    let body = format!(
        "{SIG} (local.set 0 (i32.sub (local.get 9) (local.get 7)))
          (if (i32.and (i32.ne (local.get 0) (i32.const 2)) (i32.ne (local.get 9) (i32.const 0)))
            (then (return (i32.const 3))))
          (i32.const 0))"
    );
    let p = pp_apron(&body, 2);
    let (w, h) = (2048u32, 1200u32);
    let mut px = vec![0u8; (w * h * 4) as usize];
    run8(&p, &mut px, w, h, json!({})).unwrap();
}

#[test]
fn film_border_refuses_a_border_wider_than_the_photo() {
    let mut px = vec![9u8; 10 * 10 * 4];
    // `width` past the manifest's range reaches the module only from tests.
    let e = run8(
        &border(),
        &mut px,
        10,
        10,
        json!({"width": 0.6, "outside": false}),
    )
    .unwrap_err();
    assert!(
        e.to_string().contains("Film Border: the border is wider"),
        "{e}"
    );
}

#[test]
fn runaway_code_runs_out_of_fuel() {
    let spin = pp(&format!("{SIG} (loop $l (br $l)) (i32.const 0))"));
    let mut px = vec![0u8; 4];
    assert!(
        run8(&spin, &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("took too long")
    );
    let spin_alloc = module("").replace(
        "(local.set $p (global.get $heap))",
        "(loop $l (br $l)) (local.set $p (global.get $heap))",
    );
    let spin_alloc = format!(
        "{} {SIG} (i32.const 0)))",
        &spin_alloc[..spin_alloc.len() - 1]
    );
    let p = PostProcessor::new(
        "T",
        WasmModule::compile(spin_alloc.as_bytes()).unwrap(),
        "lt_post_process",
        0,
    )
    .unwrap();
    assert!(
        run8(&p, &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("took too long")
    );
}

#[test]
fn memory_is_capped() {
    let hog = pp(&format!(
        "{SIG} (if (i32.eq (memory.grow (i32.const 2048)) (i32.const -1)) (then (unreachable))) (i32.const 0))"
    ));
    let mut px = vec![0u8; 4];
    assert!(
        run8(&hog, &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("crashed")
    );
}

#[test]
fn instances_share_one_memory_budget() {
    let held: Vec<MemorySlot> = (0..TOTAL_MEMORY / MEMORY_LIMIT)
        .map(|_| MemorySlot::take(&|| false).unwrap())
        .collect();
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s2 = started.clone();
    let t = std::thread::spawn(move || {
        let _slot = MemorySlot::take(&|| false).unwrap();
        s2.store(true, Ordering::SeqCst);
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!started.load(Ordering::SeqCst), "a ninth instance waits");
    drop(held);
    t.join().unwrap();
    assert!(started.load(Ordering::SeqCst));
}

#[test]
fn a_cancelled_job_stops_waiting_for_a_slot() {
    let held: Vec<MemorySlot> = (0..TOTAL_MEMORY / MEMORY_LIMIT)
        .map(|_| MemorySlot::take(&|| false).unwrap())
        .collect();
    assert!(MemorySlot::take(&|| true).is_none());
    drop(held);
}

#[test]
fn traps_and_error_codes_are_reported() {
    let mut px = vec![1u8; 4];
    let trap = pp(&format!("{SIG} (unreachable))"));
    assert!(
        run8(&trap, &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("Test: crashed")
    );
    let code = pp(&format!("{SIG} (i32.const 7))"));
    assert!(
        run8(&code, &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("code 7")
    );
    let msg = pp(&format!(
        "{SIG} (call $error (i32.const 0) (i32.const 4)) (i32.const 3))"
    ));
    assert!(
        run8(&msg, &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .ends_with("nope")
    );
}

#[test]
fn three_failures_in_a_row_suspend_it() {
    let trap = pp(&format!("{SIG} (unreachable))"));
    let mut px = vec![0u8; 4];
    for _ in 0..MAX_FAILURES {
        assert!(!trap.is_suspended());
        let _ = run8(&trap, &mut px, 1, 1, json!({}));
    }
    assert!(trap.is_suspended());
    assert!(
        run8(&trap, &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("after a relaunch")
    );
    assert!(
        trap.layout(1, 1, &Map::new(), &ctx()).is_ok(),
        "no layout export: the canvas is unchanged"
    );
}

#[test]
fn a_success_resets_the_failure_count() {
    let flaky = pp(&format!(
        "{SIG} (call $param_bool (i32.const 8) (i32.const 4)))"
    ));
    let mut px = vec![0u8; 4];
    for _ in 0..5 {
        let _ = run8(&flaky, &mut px, 1, 1, json!({"flag": true}));
        let _ = run8(&flaky, &mut px, 1, 1, json!({"flag": false}));
    }
    assert!(!flaky.is_suspended());
}

#[test]
fn params_logs_and_cancel_reach_the_module() {
    let body = format!(
        "{SIG} (call $log (i32.const 0) (i32.const 16) (i32.const 5))
          (if (i64.ne (call $param_i64 (i32.const 16) (i32.const 5)) (i64.const 3)) (then (return (i32.const 9))))
          (if (i64.ne (call $param_i64 (i32.const 0) (i32.const 4)) (i64.const -9223372036854775808)) (then (return (i32.const 8))))
          (call $progress (i32.const 500)))"
    );
    let m = pp(&body);
    let mut px = vec![0u8; 4];
    assert_eq!(
        run8(&m, &mut px, 1, 1, json!({"count": 3})).unwrap(),
        ["count"]
    );
    let e = m.run(
        Pixels::Rgba8(&mut px),
        1,
        1,
        [0, 0, 1, 1],
        params(json!({"count": 3})),
        &ctx(),
        &|| true,
    );
    assert!(matches!(e.unwrap_err(), PluginError::Cancelled));
    assert!(!m.is_suspended());
}

#[test]
fn modules_are_checked_when_compiled() {
    let e = |text: &str| {
        WasmModule::compile(text.as_bytes())
            .unwrap_err()
            .to_string()
    };
    assert!(e("(module").contains("not a WebAssembly module"));
    let fs = r#"(module (import "wasi" "fd_write" (func (param i32))) (memory (export "memory") 1)
        (func (export "lt_api") (result i32) (i32.const 1)) (func (export "lt_alloc") (param i32) (result i32) (i32.const 0)))"#;
    assert!(e(fs).contains("wasi.fd_write"));
    // No import can reach the host's state: grants and trust aren't in the API.
    let grant = fs.replace(r#""wasi" "fd_write""#, r#""luxtable" "grant""#);
    assert!(e(&grant).contains("luxtable.grant"));
    let no_alloc = r#"(module (memory (export "memory") 1) (func (export "lt_api") (result i32) (i32.const 1)))"#;
    assert!(e(no_alloc).contains("lt_alloc"));
    assert!(e(&" ".repeat(MAX_MODULE_BYTES + 1)).contains("over 16 MB"));
    let m = WasmModule::compile(module("").as_bytes()).unwrap();
    assert!(!m.exports("lt_post_process"));
    assert!(
        PostProcessor::new("X", m, "lt_post_process", 0)
            .unwrap_err()
            .to_string()
            .contains("doesn't export")
    );
    assert!(
        format!("{:?}", WasmModule::compile(BORDER.as_bytes()).unwrap()).contains("WasmModule")
    );
}

#[test]
fn calls_are_checked_at_runtime() {
    let mut px = vec![0u8; 4];
    let wrong_sig = pp(r#"(func (export "lt_post_process") (result i32) (i32.const 0))"#);
    assert!(
        run8(&wrong_sig, &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("wrong signature")
    );
    let api2 = module(&format!("{SIG} (i32.const 0))")).replace(
        "(i32.const 1))\n  (func (export \"lt_alloc\")",
        "(i32.const 2))\n  (func (export \"lt_alloc\")",
    );
    let api2 = PostProcessor::new(
        "T",
        WasmModule::compile(api2.as_bytes()).unwrap(),
        "lt_post_process",
        0,
    )
    .unwrap();
    assert!(
        run8(&api2, &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("host API 2")
    );
    let fixed = |ret: i32| {
        let m = format!(
            r#"(module (memory (export "memory") 1) (func (export "lt_api") (result i32) (i32.const 1))
            (func (export "lt_alloc") (param i32) (result i32) (i32.const {ret})) {SIG} (i32.const 0)))"#
        );
        PostProcessor::new(
            "T",
            WasmModule::compile(m.as_bytes()).unwrap(),
            "lt_post_process",
            0,
        )
        .unwrap()
    };
    assert!(
        run8(&fixed(65534), &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("outside its memory")
    );
    assert!(
        run8(&fixed(-1), &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("outside its memory")
    );
    let start_trap = format!(
        r#"(module (memory (export "memory") 1) (func $s unreachable) (start $s) (func (export "lt_api") (result i32) (i32.const 1))
        (func (export "lt_alloc") (param i32) (result i32) (i32.const 64)) {SIG} (i32.const 0)))"#
    );
    let start_trap = PostProcessor::new(
        "T",
        WasmModule::compile(start_trap.as_bytes()).unwrap(),
        "lt_post_process",
        0,
    )
    .unwrap();
    assert!(
        run8(&start_trap, &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("couldn't start")
    );
    let ok = pp(&format!("{SIG} (i32.const 0))"));
    assert!(
        run8(&ok, &mut px, 2, 2, json!({}))
            .unwrap_err()
            .to_string()
            .contains("doesn't match")
    );
    assert!(run8(&ok, &mut [], 0, 0, json!({})).is_err());
}

#[test]
fn layouts_are_checked() {
    let layout = |w: i32, h: i32, x: i32, y: i32, rc: i32| {
        pp(&format!(
            r#"(func (export "lt_output_layout") (param i32 i32 i32 i32 i32) (result i32)
                (i32.store (local.get 4) (i32.const {w})) (i32.store offset=4 (local.get 4) (i32.const {h}))
                (i32.store offset=8 (local.get 4) (i32.const {x})) (i32.store offset=12 (local.get 4) (i32.const {y}))
                (i32.const {rc}))
              {SIG} (i32.const 0))"#
        ))
    };
    assert_eq!(
        layout(12, 12, 1, 1, 0)
            .layout(10, 10, &Map::new(), &ctx())
            .unwrap(),
        Layout {
            width: 12,
            height: 12,
            x: 1,
            y: 1
        }
    );
    let e = layout(12, 12, 5, 1, 0)
        .layout(10, 10, &Map::new(), &ctx())
        .unwrap_err();
    assert!(e.to_string().contains("doesn't hold the photo"), "{e}");
    assert!(
        layout(70000, 70000, 0, 0, 0)
            .layout(10, 10, &Map::new(), &ctx())
            .is_err()
    );
    assert!(
        layout(10, 10, 0, 0, 4)
            .layout(10, 10, &Map::new(), &ctx())
            .unwrap_err()
            .to_string()
            .contains("code 4")
    );
}

#[test]
fn host_state_debug_hides_closures() {
    let s = HostState {
        limits: StoreLimitsBuilder::new().build(),
        params: Map::new(),
        log: vec!["x".into()],
        error: None,
        cancelled: never(),
        photo: [0; 4],
    };
    assert!(format!("{s:?}").contains("\"x\""));
}

/// Review probe H5: one linear memory per module (the memory limit is per memory), and
/// tables are capped.
#[test]
fn a_second_memory_or_a_table_bomb_is_refused() {
    let two = module(&format!("(memory $m2 1) {SIG} (i32.const 0))"));
    assert!(
        WasmModule::compile(two.as_bytes()).is_err(),
        "multi-memory is off"
    );
    let bomb = pp(&format!(
        "(table $t 0 funcref) {SIG}
           (if (i32.eq (table.grow $t (ref.null func) (i32.const 0x04000000)) (i32.const -1)) (then (return (i32.const 3))))
           (i32.const 0))"
    ));
    let mut px = vec![0u8; 4];
    assert!(
        run8(&bomb, &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("code 3"),
        "the grow is refused"
    );
    // One table per instance.
    let two_tables = pp(&format!(
        "(table $a 1 funcref) (table $b 1 funcref) {SIG} (i32.const 0))"
    ));
    assert!(run8(&two_tables, &mut px, 1, 1, json!({})).is_err());
}

/// Review probe M7: `start` has its own small fuel budget, not the image's.
#[test]
fn start_runs_on_its_own_small_budget() {
    let spin = pp(&format!(
        "(func $spin (loop $l (br $l))) (start $spin) {SIG} (i32.const 0))"
    ));
    let mut px = vec![0u8; 4];
    let t0 = std::time::Instant::now();
    let e = run8(&spin, &mut px, 1, 1, json!({}))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("couldn't start") && e.contains("too long"),
        "{e}"
    );
    assert!(t0.elapsed() < std::time::Duration::from_secs(10));
}

/// N2: the export's Cancel reaches a module mid-image: `progress()` reports it.
#[test]
fn a_cancel_during_the_call_reaches_progress() {
    // Spins until progress() says cancelled, then returns 1.
    let waits = pp(&format!(
        "{SIG} (loop $l (br_if $l (i32.eqz (call $progress (i32.const 0))))) (i32.const 1))"
    ));
    let mut px = vec![0u8; 1000 * 1000 * 4]; // a big budget: only Cancel stops it
    let t0 = std::time::Instant::now();
    let cancel = move || t0.elapsed() > std::time::Duration::from_millis(5);
    let e = waits
        .run(
            Pixels::Rgba8(&mut px),
            1000,
            1000,
            [0, 0, 1000, 1000],
            Map::new(),
            &ctx(),
            &cancel,
        )
        .unwrap_err();
    assert!(matches!(e, PluginError::Cancelled), "{e}");
}

/// Round-2 probe: strips read every apron row from the input, never from rows already
/// written back. A "row above + 1" filter gives the same result as on the whole image.
#[test]
fn strip_seams_see_only_input_rows() {
    // Bottom-up over the strip's own rows: row y = (input row y − 1) + 1, row 0 = 1.
    let body = format!(
        "{SIG} (local $y i32) (local $x i32) (local $p i32) (local $v i32)
          (local.set $y (i32.sub (i32.add (local.get 9) (local.get 10)) (i32.const 1)))
          (block $done (loop $rows
            (br_if $done (i32.lt_s (local.get $y) (local.get 9)))
            (local.set $p (i32.add (local.get 2) (i32.mul (i32.sub (local.get $y) (local.get 7)) (local.get 5))))
            (local.set $x (i32.const 0))
            (block $rd (loop $cols
              (br_if $rd (i32.ge_u (local.get $x) (local.get 5)))
              (local.set $v (if (result i32) (i32.eqz (local.get $y)) (then (i32.const 0))
                (else (i32.load8_u (i32.sub (i32.add (local.get $p) (local.get $x)) (local.get 5))))))
              (i32.store8 (i32.add (local.get $p) (local.get $x)) (i32.add (local.get $v) (i32.const 1)))
              (local.set $x (i32.add (local.get $x) (i32.const 4)))
              (br $cols)))
            (local.set $y (i32.sub (local.get $y) (i32.const 1)))
            (br $rows)))
          (i32.const 0))"
    );
    let p = pp_apron(&body, 1);
    // 1024 px RGBA8 rows: 4 MB strips of 1024 rows, so seams at 1024 and 2048.
    let (w, h) = (1024u32, 2560u32);
    let mut px = vec![0u8; (w * h * 4) as usize];
    run8(&p, &mut px, w, h, json!({})).unwrap();
    let rows: std::collections::BTreeSet<u8> = (0..h).map(|y| px[(y * w * 4) as usize]).collect();
    assert_eq!(
        rows,
        [1].into_iter().collect(),
        "every row is 1, at the seams too"
    );
}

/// One instance can't use more than its 64 MB, however it asks.
#[test]
fn one_instance_stays_within_its_memory() {
    let grow = pp(&format!(
        "{SIG} (if (i32.eq (memory.grow (i32.const {})) (i32.const -1)) (then (return (i32.const 7)))) (i32.const 0))",
        (MEMORY_LIMIT >> 16) + 1
    ));
    let mut px = vec![0u8; 4];
    assert!(
        run8(&grow, &mut px, 1, 1, json!({}))
            .unwrap_err()
            .to_string()
            .contains("code 7")
    );
}

/// Round 3 probe: an apron taller than a strip (8192 px RGBA8: 128-row strips, apron
/// 200) still reads only input rows, from as many earlier strips as it spans.
#[test]
fn an_apron_taller_than_a_strip_sees_only_input_rows() {
    // Bottom-up: row y = (input row y − 200) + 1 for y ≥ 200.
    let body = format!(
        "{SIG} (local $y i32) (local $x i32) (local $p i32)
          (local.set $y (i32.sub (i32.add (local.get 9) (local.get 10)) (i32.const 1)))
          (block $done (loop $rows
            (br_if $done (i32.lt_s (local.get $y) (local.get 9)))
            (if (i32.ge_s (local.get $y) (i32.const 200)) (then
              (local.set $p (i32.add (local.get 2) (i32.mul (i32.sub (local.get $y) (local.get 7)) (local.get 5))))
              (local.set $x (i32.const 0))
              (block $rd (loop $cols
                (br_if $rd (i32.ge_u (local.get $x) (local.get 5)))
                (i32.store8 (i32.add (local.get $p) (local.get $x))
                  (i32.add (i32.const 1)
                    (i32.load8_u (i32.sub (i32.add (local.get $p) (local.get $x)) (i32.mul (i32.const 200) (local.get 5))))))
                (local.set $x (i32.add (local.get $x) (i32.const 4)))
                (br $cols)))))
            (local.set $y (i32.sub (local.get $y) (i32.const 1)))
            (br $rows)))
          (i32.const 0))"
    );
    let p = pp_apron(&body, 200);
    let (w, h) = (8192u32, 640u32);
    let mut px = vec![0u8; (w * h * 4) as usize];
    run8(&p, &mut px, w, h, json!({})).unwrap();
    let wrong: Vec<u32> = (200..h)
        .filter(|&y| px[(y * w * 4) as usize] != 1)
        .collect();
    assert!(
        wrong.is_empty(),
        "{} wrong rows, first {:?}",
        wrong.len(),
        wrong.first()
    );
}

/// A plug-in built before the rename imports from `darkslide` and exports `ds_api`,
/// `ds_alloc`, `ds_output_layout` and `ds_post_process`: it runs exactly as before.
#[test]
fn a_plugin_built_before_the_rename_still_runs() {
    let old = BORDER
        .replace("\"luxtable\"", "\"darkslide\"")
        .replace("(export \"lt_", "(export \"ds_");
    assert!(old.contains("\"darkslide\"") && old.contains("\"ds_post_process\""));
    assert!(!old.contains("\"lt_"));
    let old = PostProcessor::new(
        "Old Border",
        WasmModule::compile(old.as_bytes()).unwrap(),
        "lt_post_process",
        0,
    )
    .unwrap();
    let p = params(json!({"width": 0.1, "color": "#FF0000", "outside": true}));
    assert_eq!(
        old.layout(100, 60, &p, &ctx()).unwrap(),
        border().layout(100, 60, &p, &ctx()).unwrap()
    );
    let (w, h) = (40u32, 30u32);
    let prm = json!({"width": 0.1, "color": "#102030", "outside": false});
    let mut a = vec![200u8; (w * h * 4) as usize];
    let mut b = a.clone();
    run8(&old, &mut a, w, h, prm.clone()).unwrap();
    run8(&border(), &mut b, w, h, prm).unwrap();
    assert_eq!(a, b);
    // Anything else still has to be there under one name or the other.
    assert!(
        PostProcessor::new(
            "x",
            WasmModule::compile(BORDER.as_bytes()).unwrap(),
            "lt_missing",
            0
        )
        .is_err()
    );
}
