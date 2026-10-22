;; Film Border: a border `width` × the photo's short edge wide in `color` (its alpha
;; blends it over the photo). With `outside` (the default) the canvas grows by the
;; border on every side and the photo stays whole; without it the border covers the
;; photo's edge. Host API 1, export_post_process contract 1.
;;
;; The host calls ds_output_layout once, then ds_post_process once per strip of rows:
;; pixels arrive in the output's encoding (format 0 = RGBA8, alpha left alone; format
;; 1 = RGB16), `stride` bytes per row, buffer rows buf_y0.., and only rows
;; core_y0..core_y0+core_rows are this call's to change.
(module
  (import "darkslide" "param_f64" (func $param_f64 (param i32 i32) (result f64)))
  (import "darkslide" "param_color" (func $param_color (param i32 i32) (result i64)))
  (import "darkslide" "param_bool" (func $param_bool (param i32 i32) (result i32)))
  (import "darkslide" "photo_rect" (func $photo_rect (param i32) (result i32)))
  (import "darkslide" "error" (func $error (param i32 i32)))

  (memory (export "memory") 1)
  (data (i32.const 0) "width")
  (data (i32.const 8) "color")
  (data (i32.const 16) "outside")
  (data (i32.const 32) "the border is wider than the photo")
  ;; 64..80: the photo's rect (x, y, w, h) from the host.
  ;; A bump allocator above the constants: the host allocates its buffers once.
  (global $heap (mut i32) (i32.const 1024))

  (func (export "ds_api") (result i32) (i32.const 1))

  (func (export "ds_alloc") (param $len i32) (result i32)
    (local $ptr i32) (local $end i32) (local $have i32)
    (local.set $ptr (i32.and (i32.add (global.get $heap) (i32.const 7)) (i32.const -8)))
    (local.set $end (i32.add (local.get $ptr) (local.get $len)))
    (if (i32.lt_u (local.get $end) (local.get $ptr)) (then (return (i32.const -1))))
    (local.set $have (i32.mul (memory.size) (i32.const 65536)))
    (if (i32.gt_u (local.get $end) (local.get $have))
      (then
        (if (i32.eq
              (memory.grow (i32.shr_u (i32.add (i32.sub (local.get $end) (local.get $have)) (i32.const 65535))
                                      (i32.const 16)))
              (i32.const -1))
          (then (return (i32.const -1))))))
    (global.set $heap (local.get $end))
    (local.get $ptr))

  (func (export "ds_free") (param i32 i32))

  ;; The border width in pixels for a w × h photo.
  (func $border (param $w i32) (param $h i32) (result i32)
    (i32.trunc_sat_f64_u
      (f64.mul (call $param_f64 (i32.const 0) (i32.const 5))
               (f64.convert_i32_u (select (local.get $w) (local.get $h) (i32.lt_u (local.get $w) (local.get $h)))))))

  ;; Outside: the canvas grows by the border on every side.
  (func (export "ds_output_layout") (param $ctx i32) (param $ctx_len i32) (param $w i32) (param $h i32)
        (param $out i32) (result i32)
    (local $b i32)
    (if (i32.eq (call $param_bool (i32.const 16) (i32.const 7)) (i32.const 0))
      (then (local.set $b (i32.const 0)))
      (else (local.set $b (call $border (local.get $w) (local.get $h)))))
    (i32.store (local.get $out) (i32.add (local.get $w) (i32.shl (local.get $b) (i32.const 1))))
    (i32.store offset=4 (local.get $out) (i32.add (local.get $h) (i32.shl (local.get $b) (i32.const 1))))
    (i32.store offset=8 (local.get $out) (local.get $b))
    (i32.store offset=12 (local.get $out) (local.get $b))
    (i32.const 0))

  ;; new over old with alpha a (0–255).
  (func $mix (param $old i32) (param $new i32) (param $a i32) (result i32)
    (i32.div_u
      (i32.add (i32.mul (local.get $new) (local.get $a))
               (i32.mul (local.get $old) (i32.sub (i32.const 255) (local.get $a))))
      (i32.const 255)))

  ;; Paint pixels x0..x1 of the row at `row`.
  (func $span (param $row i32) (param $x0 i32) (param $x1 i32) (param $fmt i32)
              (param $r i32) (param $g i32) (param $b i32) (param $a i32)
    (local $p i32) (local $end i32) (local $bpp i32)
    (local.set $bpp (select (i32.const 6) (i32.const 4) (local.get $fmt)))
    (local.set $p (i32.add (local.get $row) (i32.mul (local.get $x0) (local.get $bpp))))
    (local.set $end (i32.add (local.get $row) (i32.mul (local.get $x1) (local.get $bpp))))
    (block $done
      (loop $next
        (br_if $done (i32.ge_u (local.get $p) (local.get $end)))
        (if (local.get $fmt)
          (then
            (i32.store16 (local.get $p)
              (call $mix (i32.load16_u (local.get $p)) (i32.mul (local.get $r) (i32.const 257)) (local.get $a)))
            (i32.store16 offset=2 (local.get $p)
              (call $mix (i32.load16_u offset=2 (local.get $p)) (i32.mul (local.get $g) (i32.const 257)) (local.get $a)))
            (i32.store16 offset=4 (local.get $p)
              (call $mix (i32.load16_u offset=4 (local.get $p)) (i32.mul (local.get $b) (i32.const 257)) (local.get $a))))
          (else
            (i32.store8 (local.get $p) (call $mix (i32.load8_u (local.get $p)) (local.get $r) (local.get $a)))
            (i32.store8 offset=1 (local.get $p)
              (call $mix (i32.load8_u offset=1 (local.get $p)) (local.get $g) (local.get $a)))
            (i32.store8 offset=2 (local.get $p)
              (call $mix (i32.load8_u offset=2 (local.get $p)) (local.get $b) (local.get $a)))))
        (local.set $p (i32.add (local.get $p) (local.get $bpp)))
        (br $next))))

  (func (export "ds_post_process")
        (param $ctx i32) (param $ctx_len i32) (param $px i32) (param $w i32) (param $h i32)
        (param $stride i32) (param $fmt i32) (param $buf_y0 i32) (param $buf_rows i32)
        (param $core_y0 i32) (param $core_rows i32) (result i32)
    (local $c i64) (local $b i32) (local $y i32) (local $end i32) (local $row i32)
    (local $top i32) (local $bottom i32) (local $left i32) (local $right i32)
    (local $r i32) (local $g i32) (local $bl i32) (local $a i32)
    (drop (call $photo_rect (i32.const 64)))
    (if (i32.and (i32.eq (i32.load offset=72 (i32.const 0)) (local.get $w))
                 (i32.eq (i32.load offset=76 (i32.const 0)) (local.get $h)))
      (then
        ;; The photo is the whole canvas: the border covers its edge.
        (local.set $b (call $border (local.get $w) (local.get $h)))
        (if (i32.ge_u (i32.shl (local.get $b) (i32.const 1))
                      (select (local.get $w) (local.get $h) (i32.lt_u (local.get $w) (local.get $h))))
          (then (call $error (i32.const 32) (i32.const 34)) (return (i32.const 2))))
        (local.set $top (local.get $b))
        (local.set $bottom (i32.sub (local.get $h) (local.get $b)))
        (local.set $left (local.get $b))
        (local.set $right (i32.sub (local.get $w) (local.get $b))))
      (else
        ;; The canvas grew: paint everything around the photo.
        (local.set $left (i32.load offset=64 (i32.const 0)))
        (local.set $top (i32.load offset=68 (i32.const 0)))
        (local.set $right (i32.add (local.get $left) (i32.load offset=72 (i32.const 0))))
        (local.set $bottom (i32.add (local.get $top) (i32.load offset=76 (i32.const 0))))))
    (local.set $c (call $param_color (i32.const 8) (i32.const 5)))
    (local.set $r (i32.wrap_i64 (i64.and (i64.shr_u (local.get $c) (i64.const 24)) (i64.const 255))))
    (local.set $g (i32.wrap_i64 (i64.and (i64.shr_u (local.get $c) (i64.const 16)) (i64.const 255))))
    (local.set $bl (i32.wrap_i64 (i64.and (i64.shr_u (local.get $c) (i64.const 8)) (i64.const 255))))
    (local.set $a (i32.wrap_i64 (i64.and (local.get $c) (i64.const 255))))
    (local.set $y (local.get $core_y0))
    (local.set $end (i32.add (local.get $core_y0) (local.get $core_rows)))
    (block $done
      (loop $rows
        (br_if $done (i32.ge_u (local.get $y) (local.get $end)))
        (local.set $row (i32.add (local.get $px)
                                 (i32.mul (i32.sub (local.get $y) (local.get $buf_y0)) (local.get $stride))))
        (if (i32.or (i32.lt_u (local.get $y) (local.get $top)) (i32.ge_u (local.get $y) (local.get $bottom)))
          (then
            (call $span (local.get $row) (i32.const 0) (local.get $w) (local.get $fmt)
                        (local.get $r) (local.get $g) (local.get $bl) (local.get $a)))
          (else
            (call $span (local.get $row) (i32.const 0) (local.get $left) (local.get $fmt)
                        (local.get $r) (local.get $g) (local.get $bl) (local.get $a))
            (call $span (local.get $row) (local.get $right) (local.get $w) (local.get $fmt)
                        (local.get $r) (local.get $g) (local.get $bl) (local.get $a))))
        (local.set $y (i32.add (local.get $y) (i32.const 1)))
        (br $rows)))
    (i32.const 0))
)
