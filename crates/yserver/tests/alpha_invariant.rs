//! L1 alpha-invariant tests — Phase A of the X11 composite plan.
//!
//! Each test paints a known pattern through a specific code path and
//! asserts the per-pixel α byte the mirror holds afterwards. The
//! rule under test:
//!
//!   - Depth-24 destinations (server-owned α): α must end at 0xFF
//!     for any painted pixel, regardless of what value the client
//!     supplied in the alpha-byte slot.
//!   - Depth-32 ARGB destinations (client-meaningful α): the
//!     painted pixel's α byte must round-trip exactly.
//!
//! Tests are `#[ignore = "needs live Vulkan ICD"]` and run with
//! `cargo test ... -- --ignored`. Each commit in the L1 series adds
//! one or more tests here covering the path it just wired.

#![cfg(target_os = "linux")]

mod common;

use common::server_fixture::ServerFixture;

// ---- A.3: PolyFillRectangle / FillRectangles fast path -----------
//
// Note: the plan's depth-24 "untouched pixel stays at α=0" assertion
// depends on `DrawableImage::initialize_clear` actually landing in
// the mirror's storage; under lavapipe the post-clear readback
// reads uninitialised memory regardless of the clear pattern (both
// `cmd_clear_color_image` and `cmd_clear_attachments` inside a
// render pass exhibit it). Investigating the initialize-clear vs
// readback discrepancy is filed as a follow-up; the A.3 fix
// (depth-32 α preservation) is observable on the painted region
// alone, which is what this test gates on.

#[test]
#[ignore = "needs live Vulkan ICD"]
fn fill_rectangle_writes_opaque_pixel_on_depth24() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    fix.fill_rectangle_simple(win, 10, 10, 20, 20, 0x00_00_ff_00);
    let m = fix.capture_window_mirror(win);
    let p = m.pixel(15, 15);
    // Painted region: green channel set, α forced to 0xFF
    // (server-owned invariant — depth-24 ignores client α bits).
    assert_eq!((p.b, p.g, p.r, p.a), (0x00, 0xff, 0x00, 0xff));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn fill_rectangle_argb32_preserves_client_alpha() {
    let mut fix = ServerFixture::start();
    let pix = fix.create_pixmap(64, 64, 32);
    // Under the ARGB visual: A=0x80, R=0, G=0, B=0xff.
    fix.fill_rectangle_simple(pix, 0, 0, 64, 64, 0x80_00_00_ff);
    let m = fix.capture_pixmap_mirror(pix);
    let p = m.pixel(32, 32);
    assert_eq!(
        (p.b, p.g, p.r, p.a),
        (0xff, 0x00, 0x00, 0x80),
        "depth-32 fill must round-trip client α"
    );
}

// ---- A.4: Poly* (stroke) ops share the A.3 call site ------------

#[test]
#[ignore = "needs live Vulkan ICD"]
fn poly_rectangle_writes_opaque_pixel_on_depth24() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    let gc = fix.create_gc(win, 0x00_00_ff_00);
    fix.poly_rectangle(win, gc, 10, 10, 20, 20);
    // The outline is 1px wide; sample a top-edge pixel.
    let m = fix.capture_window_mirror(win);
    let p = m.pixel(15, 10);
    assert_eq!((p.b, p.g, p.r, p.a), (0x00, 0xff, 0x00, 0xff));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn poly_line_writes_opaque_pixel_on_depth24() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    let gc = fix.create_gc(win, 0x00_00_ff_00);
    // Horizontal segment along y=20 from x=10 to x=30.
    fix.poly_line(win, gc, &[(10, 20), (30, 20)]);
    let m = fix.capture_window_mirror(win);
    let p = m.pixel(20, 20);
    assert_eq!((p.b, p.g, p.r, p.a), (0x00, 0xff, 0x00, 0xff));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn poly_segment_writes_opaque_pixel_on_depth24() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    let gc = fix.create_gc(win, 0x00_00_ff_00);
    fix.poly_segment(win, gc, &[(10, 25, 30, 25)]);
    let m = fix.capture_window_mirror(win);
    let p = m.pixel(20, 25);
    assert_eq!((p.b, p.g, p.r, p.a), (0x00, 0xff, 0x00, 0xff));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn poly_point_writes_opaque_pixel_on_depth24() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    let gc = fix.create_gc(win, 0x00_00_ff_00);
    fix.poly_point(win, gc, &[(20, 20)]);
    let m = fix.capture_window_mirror(win);
    let p = m.pixel(20, 20);
    assert_eq!((p.b, p.g, p.r, p.a), (0x00, 0xff, 0x00, 0xff));
}

// ---- A.5: Poly[Fill]Arc share the A.3 call site -----------------

#[test]
#[ignore = "needs live Vulkan ICD"]
fn poly_fill_arc_writes_opaque_pixel_on_depth24() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    let gc = fix.create_gc(win, 0x00_00_ff_00);
    // Full circle (angle2 = 360 * 64) centered at (32, 32), radius 12.
    fix.poly_fill_arc(win, gc, 20, 20, 24, 24);
    let m = fix.capture_window_mirror(win);
    let p = m.pixel(32, 32);
    assert_eq!((p.b, p.g, p.r, p.a), (0x00, 0xff, 0x00, 0xff));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn poly_arc_writes_opaque_pixel_on_depth24() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    let gc = fix.create_gc(win, 0x00_00_ff_00);
    // Same full circle; stroke pass paints a horizontal cap at y=20.
    fix.poly_arc(win, gc, 20, 20, 24, 24);
    let m = fix.capture_window_mirror(win);
    let p = m.pixel(32, 20);
    assert_eq!((p.b, p.g, p.r, p.a), (0x00, 0xff, 0x00, 0xff));
}

// ---- A.15b: GCfunction Copy + Xor crossover ---------------------

#[test]
#[ignore = "needs live Vulkan ICD"]
fn gc_function_copy_and_xor_both_keep_alpha_opaque_on_depth24() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    // First fill via the Copy (fast) path — A.3-covered.
    let copy_gc = fix.create_gc(win, 0x00_ff_ff_ff); // white
    fix.fill_rectangle(win, copy_gc, 0, 0, 64, 64);
    // Then overlay via the non-Copy (logic-fill) path — A.6b-covered.
    let xor_gc = fix.create_gc_with_function(win, 6, 0x00_00_ff_00); // XOR, green fg
    fix.fill_rectangle(win, xor_gc, 10, 10, 20, 20);
    let m = fix.capture_window_mirror(win);
    // Copy-painted region (outside XOR rect): α=ff.
    assert_eq!(m.pixel(40, 40).a, 0xff, "Copy fill α=ff");
    // XOR-painted region (inside XOR rect): α=ff (alpha masked out
    // of the logic op; baseline α was already 0xff).
    assert_eq!(m.pixel(15, 15).a, 0xff, "XOR fill α=ff");
}

// ---- A.10d: CopyPlane writes opaque α via M1 --------------------

#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_plane_writes_opaque_alpha_on_depth24() {
    let mut fix = ServerFixture::start();
    let src = fix.create_window_with_bg_pixel(8, 8, 24, 0x00_00_00_01); // bit-0 set
    fix.map_window(src);
    let dst = fix.create_window_with_bg_pixel(8, 8, 24, 0x00_00_00_00);
    fix.map_window(dst);
    let gc = fix.create_gc(src, 0x00_00_ff_00); // green fg
    fix.copy_plane(src, dst, gc, 0, 0, 0, 0, 8, 8, 1);
    let m = fix.capture_window_mirror(dst);
    // CopyPlane fans out into background+foreground rect fills via
    // try_vk_fill_with_function (M1, A.3-covered) — both end up
    // with α=0xff regardless of src bits.
    assert_eq!(m.pixel(4, 4).a, 0xff);
}

// ---- A.10c: CopyArea α round-trip -------------------------------

#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_area_preserves_alpha_on_depth32_argb() {
    let mut fix = ServerFixture::start();
    let src = fix.create_pixmap(8, 8, 32);
    let dst = fix.create_pixmap(8, 8, 32);
    let gc = fix.create_gc(src, 0);
    // Half α=0x80 (left), half α=0xC0 (right).
    let mut data = Vec::with_capacity(8 * 8 * 4);
    for _ in 0..8 {
        for x in 0..8 {
            let a = if x < 4 { 0x80 } else { 0xC0 };
            // wire order: r, g, b, a
            data.extend_from_slice(&[0x10, 0x20, 0x30, a]);
        }
    }
    fix.put_image_zpixmap(src, gc, 32, 0, 0, 8, 8, &data);
    fix.copy_area(src, dst, gc, 0, 0, 0, 0, 8, 8);
    let m = fix.capture_pixmap_mirror(dst);
    let left = m.pixel(2, 4);
    let right = m.pixel(6, 4);
    assert_eq!((left.b, left.g, left.r, left.a), (0x30, 0x20, 0x10, 0x80));
    assert_eq!(
        (right.b, right.g, right.r, right.a),
        (0x30, 0x20, 0x10, 0xC0)
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_area_preserves_alpha_on_depth24() {
    let mut fix = ServerFixture::start();
    let src = fix.create_window_with_bg_pixel(64, 64, 24, 0x00_ff_ff_ff); // white bg
    fix.map_window(src);
    fix.fill_rectangle_simple(src, 10, 10, 20, 20, 0x00_00_ff_00); // green block
    let dst = fix.create_window(64, 64, 24);
    fix.map_window(dst);
    let gc = fix.create_gc(src, 0);
    fix.copy_area(src, dst, gc, 0, 0, 0, 0, 64, 64);
    let m = fix.capture_window_mirror(dst);
    // Both green-painted region and white bg arrive with α=ff (A.3
    // ensured α=ff on src; CopyArea via cmd_copy_image preserves
    // bytes verbatim).
    assert_eq!(m.pixel(15, 15).a, 0xff, "copied green pixel keeps α=ff");
    assert_eq!(m.pixel(50, 50).a, 0xff, "copied white pixel keeps α=ff");
}

// ---- A.10a: ClearArea solid bg α --------------------------------

#[test]
#[ignore = "needs live Vulkan ICD"]
fn clear_area_solid_bg_writes_opaque_alpha_on_depth24() {
    let mut fix = ServerFixture::start();
    // gray bg pixel under the root visual: R=G=B=0x80.
    let win = fix.create_window_with_bg_pixel(64, 64, 24, 0x00_80_80_80);
    fix.map_window(win);
    fix.clear_area(win, 10, 10, 30, 30, false);
    let m = fix.capture_window_mirror(win);
    let p = m.pixel(20, 20);
    assert_eq!((p.b, p.g, p.r, p.a), (0x80, 0x80, 0x80, 0xff));
}

// ---- A.8: PutImage ZPixmap depth-aware α -----------------------

#[test]
#[ignore = "needs live Vulkan ICD"]
fn put_image_zpixmap_writes_alpha_255_on_depth24() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(8, 8, 24);
    fix.map_window(win);
    let gc = fix.create_gc(win, 0);
    // X11 wire: 4 bytes per pixel `[r, g, b, a]`. Client passes
    // a=0 — under the L1 contract the backend must overwrite with
    // 0xFF for depth-24 destinations.
    let pixel = [0xff, 0x80, 0x40, 0x00]; // r=ff g=80 b=40 a=0
    let mut data = Vec::with_capacity(8 * 8 * 4);
    for _ in 0..(8 * 8) {
        data.extend_from_slice(&pixel);
    }
    fix.put_image_zpixmap(win, gc, 24, 0, 0, 8, 8, &data);
    let m = fix.capture_window_mirror(win);
    let p = m.pixel(4, 4);
    assert_eq!(
        (p.b, p.g, p.r, p.a),
        (0x40, 0x80, 0xff, 0xff),
        "depth-24 must override α=0xff regardless of client-supplied α byte"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn put_image_zpixmap_argb32_preserves_client_alpha() {
    let mut fix = ServerFixture::start();
    let pix = fix.create_pixmap(8, 8, 32);
    let gc = fix.create_gc(pix, 0);
    let pixel = [0xff, 0x00, 0x00, 0x80]; // r=ff g=0 b=0 α=0x80
    let mut data = Vec::with_capacity(8 * 8 * 4);
    for _ in 0..(8 * 8) {
        data.extend_from_slice(&pixel);
    }
    fix.put_image_zpixmap(pix, gc, 32, 0, 0, 8, 8, &data);
    let m = fix.capture_pixmap_mirror(pix);
    let p = m.pixel(4, 4);
    assert_eq!(
        (p.b, p.g, p.r, p.a),
        (0x00, 0x00, 0xff, 0x80),
        "depth-32 PutImage must round-trip client α"
    );
}

// ---- A.6b: logic-fill (non-Copy GcFunction) -------------------

#[test]
#[ignore = "needs live Vulkan ICD"]
fn logic_fill_xor_preserves_opaque_alpha_on_depth24() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    // Baseline-fill the window white (Copy path → α=ff via A.3).
    fix.fill_rectangle_simple(win, 0, 0, 64, 64, 0x00_ff_ff_ff);
    // XOR-fill a sub-rect with green. With the A.6b fix, the LogicOp
    // touches RGB only; the destination's α=ff is preserved.
    let xor_gc = fix.create_gc_with_function(win, 6, 0x00_00_ff_00);
    fix.fill_rectangle(win, xor_gc, 10, 10, 20, 20);
    let m = fix.capture_window_mirror(win);
    let p = m.pixel(15, 15);
    // RGB = white XOR green = (0xff^0, 0xff^0xff, 0xff^0) = (ff, 0, ff).
    // α stays at 0xff because the logic op is masked away from alpha.
    assert_eq!((p.b, p.g, p.r, p.a), (0xff, 0x00, 0xff, 0xff));
}

// ---- A.6a: FillPoly shares the A.3 call site --------------------

#[test]
#[ignore = "needs live Vulkan ICD"]
fn fill_poly_writes_opaque_pixel_on_depth24() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    let gc = fix.create_gc(win, 0x00_00_ff_00);
    // Triangle (20,20), (40,20), (30,40) — interior covers (30, 30).
    fix.fill_poly(win, gc, &[(20, 20), (40, 20), (30, 40)]);
    let m = fix.capture_window_mirror(win);
    let p = m.pixel(30, 30);
    assert_eq!((p.b, p.g, p.r, p.a), (0x00, 0xff, 0x00, 0xff));
}
