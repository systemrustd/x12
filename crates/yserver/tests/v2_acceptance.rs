//! v2 acceptance integration tests (Stage 2f).
//!
//! Drives `KmsBackendV2` directly via its `Backend` trait and
//! asserts pixel-correctness against a CPU oracle. Functionally
//! equivalent to the Stage 2 plan's "synthetic harness binary"
//! that would drive PutImage / CopyArea / PolyFillRectangle /
//! GetImage through the X11 protocol — but skipping the X11
//! protocol layer because the correctness gate is at the
//! Backend-trait surface, not at the protocol-encoding layer.
//!
//! These tests are gated on a live Vulkan ICD (lavapipe is fine):
//!
//! ```text
//! VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
//!   cargo test -p yserver --test v2_acceptance -- --ignored
//! ```
//!
//! User-run hardware smoke on bee + fuji
//! (`YSERVER_RENDER_MODEL=v2 just yserver-xfce-hw`) is the
//! load-bearing Stage 2 close gate; this file covers the
//! correctness oracle that gates against pixel-level regressions.

#![cfg(target_os = "linux")]

use yserver::kms::v2::KmsBackendV2;
use yserver_core::backend::{AnyHandle, Backend, DrawState, FillState};
use yserver_protocol::x11::ClipRectangles;

/// Acceptance sequence:
/// 1. create_pixmap (depth=32, 8×8)
/// 2. PutImage a horizontal gradient
/// 3. GetImage round-trip — must be byte-identical
/// 4. PolyFillRectangle in a sub-rect — overwrites the gradient
/// 5. GetImage — verifies overwrite at the rect, gradient outside
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_put_image_fill_get_image_oracle() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let pix = b.create_pixmap(None, 32, 8, 8).expect("create_pixmap");
    let xid = pix.as_raw();

    // 8×8 RGBA gradient (wire format = BGRA8 ZPixmap).
    let mut src = vec![0u8; 8 * 8 * 4];
    for y in 0..8 {
        for x in 0..8 {
            let off = (y * 8 + x) * 4;
            src[off] = (x as u8) * 0x20; // B
            src[off + 1] = (y as u8) * 0x20; // G
            src[off + 2] = ((x + y) as u8) * 0x10; // R
            src[off + 3] = 0xFF; // A
        }
    }
    b.put_image(None, xid, 32, 8, 8, 0, 0, &src)
        .expect("put_image");

    let out = b
        .get_image_pixels_for_tests(xid, 2 /* ZPixmap */, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some(bytes)");
    assert_eq!(out, src, "PutImage→GetImage byte-identical (depth-32)");

    // PolyFillRectangle: paint a 4×4 red square at (2, 2).
    // Foreground 0xFFFF0000 = ARGB(0xFF, R=0xFF, G=0, B=0).
    let rect_bytes = {
        let mut buf = Vec::new();
        buf.extend_from_slice(&i16::to_le_bytes(2)); // x
        buf.extend_from_slice(&i16::to_le_bytes(2)); // y
        buf.extend_from_slice(&u16::to_le_bytes(4)); // w
        buf.extend_from_slice(&u16::to_le_bytes(4)); // h
        buf
    };
    b.poly_fill_rectangle(None, xid, 0xFFFF0000, &rect_bytes)
        .expect("poly_fill_rectangle");

    let after = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some");
    // (3, 3) — inside the fill — must be red: BGRA = [0,0,0xFF,0xFF].
    let off_3_3 = (3 * 8 + 3) * 4;
    assert_eq!(
        &after[off_3_3..off_3_3 + 4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "fill rect interior is red",
    );
    // (0, 0) — outside the fill — must match the gradient.
    assert_eq!(
        &after[0..4],
        &src[0..4],
        "outside fill rect preserves the gradient",
    );
}

/// Acceptance for `CopyArea` between disjoint pixmaps.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_copy_area_disjoint_oracle() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let src_xid = b.create_pixmap(None, 32, 4, 4).unwrap().as_raw();
    let dst_xid = b.create_pixmap(None, 32, 8, 4).unwrap().as_raw();

    // Fill src with red (BGRA: B=0, G=0, R=0xFF, A=0xFF) via
    // fill_rectangle. Foreground 0xFFFF0000.
    b.fill_rectangle(None, src_xid, 0xFFFF0000, 0, 0, 4, 4)
        .expect("fill_rectangle src");
    // Fill dst with blue (0xFF0000FF → BGRA [0xFF, 0, 0, 0xFF]).
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 4)
        .expect("fill_rectangle dst");
    // Copy src into dst at (4, 0).
    b.copy_area(None, src_xid, dst_xid, 0, 0, 4, 0, 4, 4)
        .expect("copy_area");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 4, !0)
        .expect("get_image")
        .expect("Some");
    // Left half blue, right half red.
    for y in 0..4 {
        for x in 0..4 {
            let off = (y * 8 + x) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0x00, 0x00, 0xFF],
                "left blue at ({x},{y})",
            );
        }
        for x in 4..8 {
            let off = (y * 8 + x) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0x00, 0x00, 0xFF, 0xFF],
                "right red at ({x},{y})",
            );
        }
    }
}

/// Telemetry assertion: after a full sequence, lifetime counts
/// reflect the expected number of paint/one-shot submits and
/// `vk_queue_wait_idle` stays at zero outside the implicit
/// get_image internal wait (which is part of the
/// `record_one_shot_submit` path, not a free-standing
/// `record_vk_queue_wait_idle` call).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_telemetry_lifetime_after_sequence() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let xid = b.create_pixmap(None, 32, 4, 4).unwrap().as_raw();

    // 3 fills.
    for _ in 0..3 {
        b.fill_rectangle(None, xid, 0xFFFF0000, 0, 0, 4, 4).unwrap();
    }
    // 1 put_image.
    let buf = vec![0xFFu8; 4 * 4 * 4];
    b.put_image(None, xid, 32, 4, 4, 0, 0, &buf).unwrap();
    // 1 get_image.
    let _ = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, 4, 4, !0)
        .unwrap();

    let t = b.telemetry();
    assert_eq!(t.lifetime.paint_submits, 4, "3 fills + 1 put_image");
    assert_eq!(t.lifetime.one_shot_submits, 1, "1 get_image");
    assert_eq!(
        t.lifetime.queue_submit2, 5,
        "every paint + one-shot bumps queue_submit2",
    );
    // Stage 2 plan §"vk_queue_wait_idle target zero": our
    // record_vk_queue_wait_idle counter is independent of the
    // implicit FenceTicket::wait inside get_image. It should
    // never fire outside actual queue_wait_idle calls.
    assert_eq!(
        t.lifetime.vk_queue_wait_idle, 0,
        "no queue_wait_idle on the v2 hot path",
    );
    assert_eq!(
        t.lifetime.cpu_fence_wait_count, 1,
        "one fence wait per get_image"
    );
}

/// Stage 3c.3 acceptance: RENDER paint paths must NOT consult the
/// ambient GC clip (`KmsCore.current_clip`). Set a restrictive
/// 1×1 GC clip rectangle, then drive a `render_composite` whose
/// picture clip is `None`; the result must paint the full dst
/// rect — proof that the GC clip didn't leak into the RENDER
/// pipeline (plan §4 cross-cutting rule).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_render_composite_no_gc_clip_leak() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // 4×4 dst pixmap pre-filled with blue (pixel 0xFF0000FF).
    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 4, 4)
        .expect("fill_rectangle pre");

    // RENDER picture wrapping the pixmap, no value-mask.
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture")
        .expect("Some(PictureHandle)");
    // SolidFill source: opaque red (premul wire u16 RGBA:
    // R=0xFFFF, G=0, B=0, A=0xFFFF — little-endian per channel).
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF])
        .expect("render_create_solid_fill")
        .expect("Some(PictureHandle)");

    // Restrictive GC clip: only (0, 0) 1×1.
    let mut rects = Vec::new();
    rects.extend_from_slice(&i16::to_le_bytes(0));
    rects.extend_from_slice(&i16::to_le_bytes(0));
    rects.extend_from_slice(&u16::to_le_bytes(1));
    rects.extend_from_slice(&u16::to_le_bytes(1));
    b.set_clip_rectangles(
        None,
        Some(ClipRectangles {
            ordering: 0,
            x_origin: 0,
            y_origin: 0,
            rectangles: rects,
        }),
    )
    .expect("set_clip_rectangles");

    // Composite covers the full 4×4 dst — the picture's clip is
    // None (no `render_set_picture_clip_rectangles` call), so the
    // engine should paint everywhere. If the backend leaked the GC
    // clip into the RENDER path, only (0, 0) would be painted.
    b.render_composite(
        None,
        1, // Src
        src_pic.as_raw(),
        0,
        dst_pic.as_raw(),
        0,
        0,
        0,
        0,
        0,
        0,
        4,
        4,
    )
    .expect("render_composite");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
        .expect("get_image")
        .expect("Some(bytes)");
    // Every pixel must be red BGRA = [0, 0, 0xFF, 0xFF].
    for y in 0..4 {
        for x in 0..4 {
            let off = (y * 4 + x) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0x00, 0x00, 0xFF, 0xFF],
                "GC clip leaked into RENDER paint at ({x},{y})",
            );
        }
    }
}

/// Stage 3d v1-bug-fix gate (plan §3d): v1's
/// `try_vk_render_composite_glyphs` reads but **ignores** the dst
/// picture's clip (`kms::backend.rs:5313`); v2 must honour it via
/// per-rect scissoring. The test stamps two 4×4 white glyphs at
/// dst (0, 0) and (4, 0) onto an 8×4 blue pixmap with the picture
/// clip set to the top-left 4×4 rect. Result: left half painted
/// white; right half stays blue. v1 would paint both glyphs.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_composite_glyphs_clip_intersects_picture() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // 8×4 dst pixmap pre-filled with blue (pixel 0xFF0000FF).
    let dst_pix = b.create_pixmap(None, 32, 8, 4).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 4)
        .expect("fill_rectangle pre");

    // SolidFill source: opaque premultiplied white (R=G=B=A=0xFFFF).
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill")
        .expect("Some(PictureHandle)");

    // Dst picture wrapping the pixmap.
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture")
        .expect("Some(PictureHandle)");

    // Picture clip: top-left 4×4 only.
    // Wire body for render_set_picture_clip_rectangles: picture(4)
    // + clip_x_origin(INT16) + clip_y_origin(INT16) + N×rectangles
    // (INT16 x, INT16 y, CARD16 w, CARD16 h).
    let mut clip_body: Vec<u8> = Vec::new();
    clip_body.extend_from_slice(&dst_pic.as_raw().to_le_bytes());
    clip_body.extend_from_slice(&i16::to_le_bytes(0)); // clip_x_origin
    clip_body.extend_from_slice(&i16::to_le_bytes(0)); // clip_y_origin
    clip_body.extend_from_slice(&i16::to_le_bytes(0)); // rect.x
    clip_body.extend_from_slice(&i16::to_le_bytes(0)); // rect.y
    clip_body.extend_from_slice(&u16::to_le_bytes(4)); // rect.w
    clip_body.extend_from_slice(&u16::to_le_bytes(4)); // rect.h
    b.render_set_picture_clip_rectangles(None, dst_pic.as_raw(), &clip_body)
        .expect("set_picture_clip_rectangles");

    // Glyphset with one 4×4 A8 glyph at id=1 (all 0xFF alpha,
    // x_off=4 so consecutive glyphs sit edge-to-edge).
    // RENDER_FMT_A8 = the standard a8 picture format id (depends
    // on the server's PictFormat catalogue; the backend's
    // render_create_glyphset matches on ynest_format constants).
    let gs = b
        .render_create_glyphset(None, yserver_protocol::x11::RENDER_FMT_A8)
        .expect("glyphset")
        .expect("Some");

    // render_add_glyphs body shape (from parse_add_glyphs):
    // body_tail = n(u32) + n×id(u32) + n×info(12 bytes) +
    // n×pixels(stride×h).
    // info layout (per parse_add_glyphs): width(u16) height(u16)
    // x(i16) y(i16) x_off(i16) y_off(i16) — 12 bytes.
    // A8 stride for w=4: (4+3) & !3 = 4. Total pixel bytes = 4×4 = 16.
    let mut add_body: Vec<u8> = Vec::new();
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // n
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // id = 1
    add_body.extend_from_slice(&u16::to_le_bytes(4)); // width
    add_body.extend_from_slice(&u16::to_le_bytes(4)); // height
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // x bearing
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y bearing
    add_body.extend_from_slice(&i16::to_le_bytes(4)); // x_off
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y_off
    add_body.extend_from_slice(&[0xFFu8; 16]); // pixels: 4×4 all opaque
    b.render_add_glyphs(None, gs.as_raw(), &add_body)
        .expect("add_glyphs");

    // CompositeGlyphs8 items: one element with count=2 glyphs id=1
    // (pen starts at dx=0,dy=0, glyph 1 stamps at (0,0), pen
    // advances to (4,0), glyph 2 stamps at (4,0)).
    // Element header: count(u8) + 3 pad + dx(i16) + dy(i16) = 8 bytes.
    // Then 2 × 1-byte ids = 2 bytes, padded to 4. Total 12 bytes.
    let mut items: Vec<u8> = Vec::new();
    items.extend_from_slice(&[2u8, 0, 0, 0]); // count + pad
    items.extend_from_slice(&i16::to_le_bytes(0)); // dx
    items.extend_from_slice(&i16::to_le_bytes(0)); // dy
    items.extend_from_slice(&[1u8, 1, 0, 0]); // 2 ids + pad

    b.render_composite_glyphs(
        None,
        23, // CompositeGlyphs8
        3,  // Over
        src_pic.as_raw(),
        dst_pic.as_raw(),
        0, // mask_fmt — unused
        gs.as_raw(),
        0,
        0,
        &items,
        0,
        0,
    )
    .expect("render_composite_glyphs");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 4, !0)
        .expect("get_image")
        .expect("Some(bytes)");

    // Left half (x=0..4): glyph painted white over blue with
    // premul srcover (atlas alpha 0xFF, foreground white) →
    // result white. Right half (x=4..8): clip excluded the glyph
    // → blue preserved. If v1's _clip-unused bug were present,
    // both halves would be white.
    for y in 0..4 {
        for x in 0..4u32 {
            let off = (y * 8 + x as usize) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0xFF, 0xFF, 0xFF],
                "left half should be white at ({x},{y}); got {:?}",
                &out[off..off + 4],
            );
        }
        for x in 4..8u32 {
            let off = (y * 8 + x as usize) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0x00, 0x00, 0xFF],
                "right half should stay blue at ({x},{y}) — picture clip honoured; got {:?}",
                &out[off..off + 4],
            );
        }
    }
}

/// Stage 3e.1 acceptance: CopyPlane on a depth-1 source pixmap.
/// Wire bits MSB-first packed at 1 bpp; bit set → foreground,
/// bit clear → background. Test exercises the depth-1 reader +
/// rect decomposition + fg/bg fill ordering.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_copy_plane_depth1_extracts_mask_bits() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // depth-1 source 8×1. Bits LSB-first in one byte (matches the
    // server's advertised `bitmap-bit-order`):
    // 0b0000_0101 = bit 0 + bit 2 set → pixels [1, 0, 1, 0, 0, 0, 0, 0].
    let src_pix = b
        .create_pixmap(None, 1, 8, 1)
        .expect("create_pixmap depth=1");
    // Depth-1 wire row stride = ceil(w/32)*4 = 4 bytes (one
    // scanline). Bit pattern in the low byte, zero pad.
    let src_bytes: Vec<u8> = vec![0b0000_0101, 0, 0, 0];
    b.put_image(None, src_pix.as_raw(), 1, 8, 1, 0, 0, &src_bytes)
        .expect("put_image depth=1");

    // 8×1 dst pixmap, opaque green pre-fill so untouched pixels
    // are visibly distinct from fg/bg.
    let dst_pix = b.create_pixmap(None, 32, 8, 1).expect("dst pixmap");
    b.fill_rectangle(None, dst_pix.as_raw(), 0xFF00FF00, 0, 0, 8, 1)
        .expect("dst pre-fill green");

    // Foreground = red (0xFFFF0000), background = blue
    // (0xFF0000FF). copy_plane reads these from KmsCore via
    // apply_draw_state.
    b.apply_draw_state(
        None,
        &DrawState {
            foreground: 0xFFFF_0000,
            background: 0xFF00_00FF,
            ..DrawState::default()
        },
    )
    .expect("apply_draw_state");

    b.copy_plane(
        None,
        src_pix.as_raw(),
        dst_pix.as_raw(),
        0,
        0,
        0,
        0,
        8,
        1,
        1, // plane = bit 0
    )
    .expect("copy_plane");

    let out = b
        .get_image_pixels_for_tests(dst_pix.as_raw(), 2, 0, 0, 8, 1, !0)
        .expect("get_image dst")
        .expect("Some(bytes)");
    // Expected per-pixel: bit set → red BGRA = [0,0,0xFF,0xFF];
    // bit clear → blue BGRA = [0xFF,0,0,0xFF].
    let want = [
        [0x00, 0x00, 0xFF, 0xFF], // x=0 bit=1 red
        [0xFF, 0x00, 0x00, 0xFF], // x=1 bit=0 blue
        [0x00, 0x00, 0xFF, 0xFF], // x=2 bit=1 red
        [0xFF, 0x00, 0x00, 0xFF], // x=3 bit=0 blue
        [0xFF, 0x00, 0x00, 0xFF], // x=4 bit=0
        [0xFF, 0x00, 0x00, 0xFF], // x=5 bit=0
        [0xFF, 0x00, 0x00, 0xFF], // x=6 bit=0
        [0xFF, 0x00, 0x00, 0xFF], // x=7 bit=0
    ];
    for (x, exp) in want.iter().enumerate() {
        let off = x * 4;
        assert_eq!(&out[off..off + 4], exp, "copy_plane mismatch at x={x}",);
    }
}

/// Stage 3e.2 acceptance: a 4×4 axis-aligned trapezoid (= filled
/// rect) painted via `render_trapezoids` must produce full coverage
/// in the trap interior. Validates the entire GPU pipeline: trap
/// rasterize → mask scratch → composite with SolidFill src. v1
/// has the equivalent rendercheck-driven gate; this is the v2
/// in-tree oracle.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_render_trapezoids_renders_filled_rect() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_pix = b.create_pixmap(None, 32, 8, 8).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("pre-fill blue");

    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid_fill red")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");

    // 16.16 fixed-point axis-aligned trapezoid:
    // top=2, bottom=6, left x=2, right x=6 → 4×4 inset rect.
    let mut traps: Vec<u8> = Vec::with_capacity(40);
    let fields: [i32; 10] = [
        2 << 16, // top
        6 << 16, // bottom
        2 << 16, // left_p1.x
        2 << 16, // left_p1.y
        2 << 16, // left_p2.x
        6 << 16, // left_p2.y
        6 << 16, // right_p1.x
        2 << 16, // right_p1.y
        6 << 16, // right_p2.x
        6 << 16, // right_p2.y
    ];
    for v in fields {
        traps.extend_from_slice(&v.to_le_bytes());
    }

    b.render_trapezoids(
        None,
        3, // Over
        src_pic.as_raw(),
        dst_pic.as_raw(),
        0, // mask_format — ignored at parity scope
        0,
        0,
        &traps,
        0,
        0,
    )
    .expect("render_trapezoids");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some");
    // Trap interior pixel (3, 3) — solidly inside — must be red.
    let off_inside = (3 * 8 + 3) * 4;
    assert_eq!(
        &out[off_inside..off_inside + 4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "trap interior should be red (got {:?})",
        &out[off_inside..off_inside + 4],
    );
    // Outside the trap (0, 0) must stay blue.
    assert_eq!(
        &out[0..4],
        &[0xFF, 0x00, 0x00, 0xFF],
        "outside trap should stay blue (got {:?})",
        &out[0..4],
    );
}

/// Repro for the xeyes "pupils missing" hardware-smoke bug
/// reported 2026-05-16. xeyes paints:
///
/// 1. Trapezoids op=Over src=<SolidFill white> at the eye region
/// 2. Trapezoids op=Over src=<SolidFill black> at a smaller
///    pupil region inside it
///
/// On hardware the eye whites render correctly but the black
/// pupils never appear. v1's PaintBatch coalesces multiple paints
/// into ONE CB with in-CB barriers; v2's per-op CB shape means each
/// `render_trapezoids` call has its own CB. Both CBs share the
/// engine's single 1×1 `solid_src_image` scratch — CB1 clears it
/// to white + samples, CB2 clears it to black + samples. Hypothesis:
/// the cross-CB barrier on `solid_src_image` either isn't strong
/// enough to prevent CB2's clear from racing CB1's sample, or some
/// other piece of state is shared without proper sync.
///
/// Test: 16×16 dst pre-filled green; an 8×8 axis-aligned white
/// trap, then a 4×4 axis-aligned black trap inside it. The final
/// dst should read:
///
/// - black at the centre (inside both traps)
/// - white between (inside white but outside black)
/// - green at corners (outside both traps)
///
/// If the second paint loses its black source (race on
/// `solid_src_image`), the centre will read white or undefined.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_back_to_back_trapezoids_different_solidfill_colors() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_pix = b.create_pixmap(None, 32, 16, 16).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    // Pre-fill green: 0xFF00FF00 ARGB. BGRA wire bytes:
    // B=0, G=0xFF, R=0, A=0xFF.
    b.fill_rectangle(None, dst_xid, 0xFF00FF00, 0, 0, 16, 16)
        .expect("pre-fill green");

    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");

    // White SolidFill: RGBA(0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF).
    let white_src = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill white")
        .expect("Some");
    // Black SolidFill: RGBA(0, 0, 0, 0xFFFF).
    let black_src = b
        .render_create_solid_fill(None, [0, 0, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid_fill black")
        .expect("Some");

    // Helper: build an axis-aligned trapezoid wire blob (40 bytes
    // per trap, 16.16 fixed-point).
    let trap_bytes = |top: i32, bot: i32, left: i32, right: i32| -> Vec<u8> {
        let mut v: Vec<u8> = Vec::with_capacity(40);
        let fields: [i32; 10] = [
            top << 16,
            bot << 16,
            left << 16,
            top << 16,
            left << 16,
            bot << 16,
            right << 16,
            top << 16,
            right << 16,
            bot << 16,
        ];
        for f in fields {
            v.extend_from_slice(&f.to_le_bytes());
        }
        v
    };

    // 8×8 white trap at (4..12, 4..12) — analogous to xeyes' eye
    // white.
    b.render_trapezoids(
        None,
        3, // Over
        white_src.as_raw(),
        dst_pic.as_raw(),
        0,
        0,
        0,
        &trap_bytes(4, 12, 4, 12),
        0,
        0,
    )
    .expect("render_trapezoids white");

    // 4×4 black trap at (6..10, 6..10) — analogous to xeyes' pupil
    // inside the eye.
    b.render_trapezoids(
        None,
        3, // Over
        black_src.as_raw(),
        dst_pic.as_raw(),
        0,
        0,
        0,
        &trap_bytes(6, 10, 6, 10),
        0,
        0,
    )
    .expect("render_trapezoids black");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 16, 16, !0)
        .expect("get_image")
        .expect("Some");

    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 16 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };

    // Centre (8, 8): inside black trap → must read black.
    assert_eq!(
        pixel(8, 8),
        [0x00, 0x00, 0x00, 0xFF],
        "centre must be black (pupil): {:?} — if white, the second \
         render_trapezoids' SolidFill source was lost (shared \
         solid_src_image race?)",
        pixel(8, 8),
    );
    // (5, 5): inside white but outside black → must read white.
    assert_eq!(
        pixel(5, 5),
        [0xFF, 0xFF, 0xFF, 0xFF],
        "(5,5) must be white (eye): got {:?}",
        pixel(5, 5),
    );
    // (1, 1): outside both → must stay green.
    assert_eq!(
        pixel(1, 1),
        [0x00, 0xFF, 0x00, 0xFF],
        "(1,1) must stay green (root bg): got {:?}",
        pixel(1, 1),
    );
}

/// xeyes "stripes-in-the-eye-white" repro. xeyes builds each eye
/// out of ~16 stacked horizontal trapezoids that share their
/// top/bottom edges (trap N's bottom = trap N+1's top). The shared
/// edge sits on a non-integer Y coordinate (xeyes' ellipse math
/// rounds to fixed-point 16.16). For pixels straddling the
/// boundary, the AA edge formula must produce coverages from the
/// two adjacent traps that SUM to ~1.0 — otherwise the boundary
/// rows under-cover and you see horizontal stripes inside the
/// eye whites.
///
/// Pre-3f.x fix: trap.frag.glsl's `c_top` / `c_bot` formulas
/// computed `clamp(p.y - top, 0, 1)` instead of
/// `clamp(0.5 + (p.y - top), 0, 1)` — off by 0.5 vs the slanted-
/// edge formula. At a shared boundary y=12.788, pixel center
/// y=12.5: trap1 c_bot = clamp(0.288, 0, 1) = 0.288; trap2 c_top
/// = clamp(-0.288, 0, 1) = 0; total = 0.288, leaving 0.712
/// missing coverage at that row.
///
/// Test: two adjacent axis-aligned traps sharing y=4.5. Centre
/// row (y=4) should read fully opaque white.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_adjacent_trapezoids_share_horizontal_boundary_cleanly() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_pix = b.create_pixmap(None, 32, 10, 10).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 10, 10)
        .expect("pre-fill blue");

    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill white")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");

    // Two adjacent trapezoids sharing y=4.5 boundary.
    // Both span x∈[2, 8].
    // 16.16 fixed-point: pixel * 65536; half-pixel = 32768.
    let fields1: [i32; 10] = [
        2 << 16,            // top = 2
        (4 << 16) | 0x8000, // bottom = 4.5
        2 << 16,
        2 << 16,
        2 << 16,
        (4 << 16) | 0x8000,
        8 << 16,
        2 << 16,
        8 << 16,
        (4 << 16) | 0x8000,
    ];
    let fields2: [i32; 10] = [
        (4 << 16) | 0x8000, // top = 4.5
        7 << 16,            // bottom = 7
        2 << 16,
        (4 << 16) | 0x8000,
        2 << 16,
        7 << 16,
        8 << 16,
        (4 << 16) | 0x8000,
        8 << 16,
        7 << 16,
    ];
    let mut traps: Vec<u8> = Vec::with_capacity(80);
    for v in fields1 {
        traps.extend_from_slice(&v.to_le_bytes());
    }
    for v in fields2 {
        traps.extend_from_slice(&v.to_le_bytes());
    }
    b.render_trapezoids(
        None,
        3,
        src_pic.as_raw(),
        dst_pic.as_raw(),
        0,
        0,
        0,
        &traps,
        0,
        0,
    )
    .expect("render_trapezoids");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 10, 10, !0)
        .expect("get_image")
        .expect("Some");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 10 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };

    // Row 4 (centre y=4.5, straddles the trap boundary). Should
    // read white (≈ full coverage). Pre-fix: ≈ partial coverage,
    // pixel is mostly white but blended with blue under-fill →
    // visible stripe.
    for x in 3..7 {
        let p = pixel(x, 4);
        // Each channel near 0xFF (allow ±16 for AA softening at
        // slanted side edges — but x=3..7 is well-inside the
        // trapezoid horizontally so the slanted-edge AA is full).
        assert!(
            p[0] >= 0xE0 && p[1] >= 0xE0 && p[2] >= 0xE0,
            "row 4 should be ~white at x={x} (got {:?}); pre-fix bug = horizontal stripe",
            p,
        );
    }
}

/// Regression for the xeyes-resize bug (2026-05-16): the user
/// resizes the xeyes window larger; the new bigger eyes paint
/// correctly but the OLD small-eye-white pixels at the original
/// (smaller) positions remain visible in the upper-left of the
/// window. Indicates the storage isn't being cleared on resize, or
/// the clear doesn't cover the full new extent.
///
/// Test: create a 16×16 window, paint a red rect inside it,
/// configure to 64×64, then get_image the new (bigger) storage at
/// position (5, 5) — where the old red would still live if the
/// resize-fill didn't run. Expect the safe-default depth-32 colour
/// (transparent black), not red.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_subwindow_resize_clears_old_paint() {
    use yserver_core::{
        backend::WindowHandle,
        host_x11::{HostSubwindowConfig, HostSubwindowVisual},
    };
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Create depth-32 child window at 16×16 with no bg attributes.
    // 3f.14's allocate_window_storage fills it with transparent-
    // black on creation.
    let parent = WindowHandle::from_raw(1).expect("root WindowHandle");
    let child = b
        .create_subwindow(
            None,
            parent,
            0,
            0,
            16,
            16,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("create_subwindow");
    let xid = child.as_raw();

    // Paint red into the 16×16 window so the "old paint" exists.
    // Foreground 0xFFFF0000 = ARGB(0xFF, R=0xFF, G=0, B=0).
    b.fill_rectangle(None, xid, 0xFFFF0000, 0, 0, 16, 16)
        .expect("paint red");

    // Resize to 64×64 via configure_subwindow. This is the path
    // v2's WMs (e16 / fvwm / etc.) drive on window-frame resize.
    b.configure_subwindow(
        None,
        xid,
        HostSubwindowConfig {
            x: None,
            y: None,
            width: Some(64),
            height: Some(64),
            border_width: None,
            stack_mode: None,
            sibling: None,
        },
    )
    .expect("configure_subwindow resize");

    // Read back the resized storage at (5, 5) — inside the OLD
    // 16×16 region. Pre-3f.14 / pre-fix: still red (leftover old
    // paint). 3f.14 expectation: depth-32 safe default
    // (transparent black, BGRA = [0, 0, 0, 0]).
    //
    // get_image waits on its internal fence, which lets the
    // OLD storage's pending_retire entry actually retire via
    // destroy_now. The decref-PendingFence path detached
    // `by_xid[xid]` for the old drawable; the new storage's
    // allocate re-installed it. When the old storage's
    // destroy_now fires inside this get_image's drain, it MUST
    // NOT remove `by_xid[xid]` (which now points to the NEW
    // drawable). Pre-fix: destroy_now blindly removed the xid
    // mapping → new storage orphaned → get_image returns None.
    let out = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, 64, 64, !0)
        .expect("get_image returned Err (storage orphaned by destroy_now?)")
        .expect("Some — by_xid[xid] resolved");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 64 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };
    // (5, 5) is well-inside the old 16×16 footprint.
    assert_eq!(
        pixel(5, 5),
        [0x00, 0x00, 0x00, 0x00],
        "post-resize storage at (5,5) must be cleared to safe-default \
         transparent black (got {:?}); old red would mean the resize-fill \
         didn't cover this position",
        pixel(5, 5),
    );
    // (30, 30) is outside the old footprint, well inside the new.
    assert_eq!(
        pixel(30, 30),
        [0x00, 0x00, 0x00, 0x00],
        "post-resize storage at (30,30) must also be cleared (got {:?})",
        pixel(30, 30),
    );
}

/// Stage 3f.14 follow-on: fresh pixmaps must read back as
/// transparent-black (depth-32) or opaque-black (depth-24),
/// NOT random Vk-undefined bytes.
///
/// Repro for the xeyes-resize artifact on mate + marco: xeyes
/// creates a depth-24 offscreen pixmap, sets a SHAPE clip
/// matching the eye outlines, paints eyes (only shape-clipped
/// pixels get content), then Present-Pixmaps the whole pixmap
/// to the window. Pre-fix: the non-eye-shape pixels of the
/// pixmap held undefined Vk memory → visible garbage in the
/// window. Post-fix: depth-appropriate safe-default clear on
/// create.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_fresh_pixmap_reads_back_zero() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let pix32 = b.create_pixmap(None, 32, 16, 16).expect("depth-32 pixmap");
    let pix24 = b.create_pixmap(None, 24, 16, 16).expect("depth-24 pixmap");

    let out32 = b
        .get_image_pixels_for_tests(pix32.as_raw(), 2, 0, 0, 16, 16, !0)
        .expect("get_image depth-32")
        .expect("Some");
    let out24 = b
        .get_image_pixels_for_tests(pix24.as_raw(), 2, 0, 0, 16, 16, !0)
        .expect("get_image depth-24")
        .expect("Some");

    // depth-32 = transparent black (premul no-op).
    for (i, px) in out32.chunks_exact(4).enumerate() {
        assert_eq!(
            &px[0..4],
            &[0, 0, 0, 0],
            "fresh depth-32 pixmap pixel #{i} should be (0,0,0,0); got {:?}",
            &px[0..4],
        );
    }
    // depth-24 = opaque black.
    for (i, px) in out24.chunks_exact(4).enumerate() {
        assert_eq!(
            &px[0..4],
            &[0, 0, 0, 0xFF],
            "fresh depth-24 pixmap pixel #{i} should be (0,0,0,0xFF); got {:?}",
            &px[0..4],
        );
    }
}

/// Diagnostic: same trap geometry shape as
/// v2_render_trapezoids_renders_filled_rect but with a LARGE bbox
/// (covering most of mask_scratch's 256×256 default extent). If
/// this passes while the 4×4 variant fails, the bug is
/// bbox-size-vs-mask-extent ratio — Intel rasterizer culls tiny
/// quads in big viewports. The fix would be to size the viewport
/// to the bbox, not the full mask.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_render_trapezoids_large_bbox_repro() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // 200×200 dst pre-filled blue.
    let dst_pix = b.create_pixmap(None, 32, 200, 200).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 200, 200)
        .expect("fill pre-blue");

    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid_fill red")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");

    // Big axis-aligned trap: 100×100 inside the 200×200 dst.
    let mut traps: Vec<u8> = Vec::with_capacity(40);
    let fields: [i32; 10] = [
        50 << 16,
        150 << 16,
        50 << 16,
        50 << 16,
        50 << 16,
        150 << 16,
        150 << 16,
        50 << 16,
        150 << 16,
        150 << 16,
    ];
    for v in fields {
        traps.extend_from_slice(&v.to_le_bytes());
    }
    b.render_trapezoids(
        None,
        3,
        src_pic.as_raw(),
        dst_pic.as_raw(),
        0,
        0,
        0,
        &traps,
        0,
        0,
    )
    .expect("render_trapezoids");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 200, 200, !0)
        .expect("get_image")
        .expect("Some");
    // Center pixel (100, 100) — well inside trap (50..150, 50..150).
    let off = (100 * 200 + 100) * 4;
    assert_eq!(
        &out[off..off + 4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "center should be red (got {:?})",
        &out[off..off + 4],
    );
}

/// Stage 3f.3 acceptance: a `Tiled` fill driven through
/// `apply_fill_state` + `poly_fill_rectangle` replicates the tile
/// pixmap across the destination via the engine's RENDER composite
/// path (`OP_SRC`, `Repeat::Normal`). e16 popup chrome paint
/// depends on this exact shape.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_tiled_fill_replicates_tile_pixmap() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // 2×2 tile pixmap pre-filled with red.
    let tile = b.create_pixmap(None, 32, 2, 2).expect("tile pixmap");
    b.fill_rectangle(None, tile.as_raw(), 0xFFFF_0000, 0, 0, 2, 2)
        .expect("tile fill red");

    // 4×4 dst pre-filled with blue so untouched pixels are visibly
    // distinct from the tile colour.
    let dst = b.create_pixmap(None, 32, 4, 4).expect("dst pixmap");
    b.fill_rectangle(None, dst.as_raw(), 0xFF00_00FF, 0, 0, 4, 4)
        .expect("dst pre-fill blue");

    // Activate Tiled fill state with origin (0, 0).
    b.apply_fill_state(
        None,
        &FillState::Tiled {
            pixmap: tile,
            origin: (0, 0),
        },
    )
    .expect("apply Tiled fill");

    // poly_fill_rectangle over the whole 4×4 dst — fg ignored for
    // tiled fill; the tile colour is what lands.
    let rect_bytes = {
        let mut buf = Vec::new();
        buf.extend_from_slice(&i16::to_le_bytes(0));
        buf.extend_from_slice(&i16::to_le_bytes(0));
        buf.extend_from_slice(&u16::to_le_bytes(4));
        buf.extend_from_slice(&u16::to_le_bytes(4));
        buf
    };
    b.poly_fill_rectangle(None, dst.as_raw(), 0x0000_0000, &rect_bytes)
        .expect("poly_fill_rectangle tiled");

    let out = b
        .get_image_pixels_for_tests(dst.as_raw(), 2, 0, 0, 4, 4, !0)
        .expect("get_image")
        .expect("Some");
    // Every pixel should now be red (tile colour), not the blue
    // pre-fill. BGRA8 wire bytes: [B=0, G=0, R=0xFF, A=0xFF].
    for (i, px) in out.chunks_exact(4).enumerate() {
        assert_eq!(
            &px[0..4],
            &[0x00, 0x00, 0xFF, 0xFF],
            "tile-filled pixel {i} must be red (got {:?})",
            &px[0..4]
        );
    }

    // Reset fill state so trailing test wiring doesn't inherit it.
    b.set_gc_fill_solid(None).expect("reset solid");
}

/// Stage 3f.14 acceptance: `set_container_background_pixmap`
/// tiles the source pixmap across the **entire root extent**, not
/// just the top-left corner. Pre-3f.14 v2 did a single `copy_area`
/// at (0, 0) and left the rest of root unchanged — fvwm3's floral
/// wallpaper covered only the top-left of the screen on bee. v1
/// tiles via its compositor pipeline; v2 routes through
/// `engine.render_composite` with `OP_SRC + Repeat::Normal`.
///
/// Test: 4×4 pixmap pre-filled red, set as root bg, read back two
/// points on root storage: (0, 0) and (5, 5) (which maps to tile
/// (1, 1) under the wrap rule). Both should read red. A point
/// outside the for_tests fb (`fb_w` = 800) is not exercised — the
/// fb is much larger than the tile so any (x, y) within
/// [0, 800) × [0, 600) hits a tiled tile.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_set_container_background_pixmap_tiles_across_root() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let tile = b.create_pixmap(None, 32, 4, 4).expect("tile pixmap");
    b.fill_rectangle(None, tile.as_raw(), 0xFFFF_0000, 0, 0, 4, 4)
        .expect("tile fill red");

    b.set_container_background_pixmap(None, tile.as_raw())
        .expect("set bg pixmap");

    // Read 8×8 of root from the origin. With a 4×4 red tile the
    // first 8×8 must be entirely red. The root xid is 1 in v2's
    // test fixture (`KmsCore.window_id`).
    let root_xid = 1u32;
    let out = b
        .get_image_pixels_for_tests(root_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some");
    assert_eq!(out.len(), 8 * 8 * 4, "8×8 BGRA8");
    for (i, px) in out.chunks_exact(4).enumerate() {
        // BGRA wire bytes for red (alpha-pre-applied opaque):
        // B=0, G=0, R=0xFF, A=0xFF.
        assert_eq!(
            &px[0..4],
            &[0x00, 0x00, 0xFF, 0xFF],
            "tiled root pixel #{i} must be red (got {:?})",
            &px[0..4],
        );
    }
}

/// `ClearArea` on a window with `bg_pixmap` must tile the pixmap
/// relative to the window origin, not issue a one-shot copy from the
/// same `(x, y)` source offset. fvwm3 frame/panel clears rely on this.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_clear_area_with_bg_pixmap_tiles_window_background() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let tile = b.create_pixmap(None, 32, 2, 2).expect("tile");
    b.fill_rectangle(None, tile.as_raw(), 0xFFFF_0000, 0, 0, 2, 2)
        .expect("tile red");

    let root = WindowHandle::from_raw(1).expect("root");
    let window = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            8,
            8,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("window");
    let xid = window.as_raw();

    b.fill_rectangle(None, xid, 0xFF00_00FF, 0, 0, 8, 8)
        .expect("window blue");
    b.clear_area(None, xid, 0, Some(tile.as_raw()), 3, 3, 4, 4)
        .expect("clear_area bg_pixmap");

    let out = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some bytes");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 8 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };

    assert_eq!(
        pixel(0, 0),
        [0xFF, 0x00, 0x00, 0xFF],
        "outside clear stays blue"
    );
    assert_eq!(
        pixel(3, 3),
        [0x00, 0x00, 0xFF, 0xFF],
        "clear origin tiles red"
    );
    assert_eq!(
        pixel(4, 3),
        [0x00, 0x00, 0xFF, 0xFF],
        "tile repeats horizontally inside clear"
    );
    assert_eq!(
        pixel(6, 6),
        [0x00, 0x00, 0xFF, 0xFF],
        "tile repeats over the whole cleared region"
    );
    assert_eq!(
        pixel(7, 7),
        [0xFF, 0x00, 0x00, 0xFF],
        "outside clear stays blue at bottom-right"
    );
}

/// Resizing a window that has a `bg_pixmap` must seed the fresh
/// storage from that pixmap, not from `bg_pixel`/default fill only.
/// The right-side fvwm panel exercises exactly this path when its
/// child window is resized from a small initial geometry to a tall
/// column.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_resize_with_bg_pixmap_reseeds_new_storage_from_background_pixmap() {
    use yserver_core::{
        backend::WindowHandle,
        host_x11::{HostSubwindowConfig, HostSubwindowVisual},
    };

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let tile = b.create_pixmap(None, 32, 2, 2).expect("tile");
    b.fill_rectangle(None, tile.as_raw(), 0xFFFF_0000, 0, 0, 2, 2)
        .expect("tile red");

    let root = WindowHandle::from_raw(1).expect("root");
    let window = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            8,
            8,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            Some(tile.as_raw()),
        )
        .expect("window");
    let xid = window.as_raw();

    b.fill_rectangle(None, xid, 0xFF00_00FF, 0, 0, 8, 8)
        .expect("window blue");
    b.configure_subwindow(
        None,
        xid,
        HostSubwindowConfig {
            width: Some(32),
            height: Some(32),
            ..HostSubwindowConfig::default()
        },
    )
    .expect("resize");

    let out = b
        .get_image_pixels_for_tests(xid, 2, 20, 20, 1, 1, !0)
        .expect("get_image")
        .expect("Some");
    assert_eq!(out.len(), 4, "single BGRA8 pixel");
    assert_eq!(
        &out[0..4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "freshly grown storage must come from tiled bg_pixmap, not default white/black fill",
    );
}

/// Stage 3f.14 acceptance: a fresh window storage allocated with
/// `bg_pixel == None` (no `CWBackPixel` attribute) reads back as a
/// depth-appropriate safe-default colour, **not** whatever bytes
/// the pool returner left. Pre-3f.14 the alloc path skipped the
/// fill entirely when `bg_pixel.is_none()`, so the v2 PixmapPool
/// (3f.10) handed back stale content — caja's drag exhibited this
/// as widget-rect islands on black. Test: create a 16×16 depth-32
/// subwindow, register it through the Backend trait, then
/// get_image its xid and assert every pixel is transparent black
/// (depth-32 safe default).
///
/// We don't directly exercise the pool here — the test fixture's
/// platform has no `pixmap_pool` attached, so fresh allocs always
/// come from a Vk allocator. The test still asserts the
/// fill-on-alloc invariant via the *initial* read: depth-32 →
/// `(0, 0, 0, 0)` BGRA bytes. Without the 3f.14 fill, the freshly
/// allocated Vk image would have UNDEFINED layout content and the
/// readback would be either driver-defined zero or
/// garbage — driver-dependent. The fill makes it explicit.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_window_storage_no_bg_pixel_inits_to_safe_default() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // create_subwindow with `background_pixel=None` +
    // `background_pixmap=None` (no CWBackPixel / CWBackPixmap on
    // the request — pre-3f.14 v2 left fresh storage at pool
    // returner content for this case).
    let parent = WindowHandle::from_raw(1).expect("root WindowHandle");
    let child = b
        .create_subwindow(
            None,
            parent,
            0, // x
            0, // y
            16,
            16,
            0, // border_width
            // Depth-32 (ARGB) needs an explicit visual config.
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None, // background_pixel
            None, // background_pixmap
        )
        .expect("create_subwindow");
    let child_xid = child.as_raw();

    let out = b
        .get_image_pixels_for_tests(child_xid, 2, 0, 0, 16, 16, !0)
        .expect("get_image")
        .expect("Some");
    assert_eq!(out.len(), 16 * 16 * 4);
    // Depth-32 → transparent black `(0, 0, 0, 0)` per
    // `default_window_init_color`.
    for (i, px) in out.chunks_exact(4).enumerate() {
        assert_eq!(
            &px[0..4],
            &[0x00, 0x00, 0x00, 0x00],
            "fresh depth-32 storage pixel #{i} must be transparent black (got {:?})",
            &px[0..4],
        );
    }
}

/// Stage 3f.15: PolySegment with N segments produces ONE paint
/// submit, not N. v2 used to call `engine.fill_rect` once per
/// Bresenham-output rect inside `fill_solid_rects`; the batch entry
/// point added in 3f.15 records every rect into a single
/// `cmd_clear_attachments` call. fvwm3 drag stutter + caja apparent
/// hangs both traced back to PolySegment fan-out → many tiny
/// per-segment Vk submits. This test drives 8 segments through the
/// Backend surface and asserts the lifetime paint_submits delta is
/// exactly 1.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_poly_segment_coalesces_to_one_submit() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let xid = b.create_pixmap(None, 32, 32, 32).unwrap().as_raw();

    // Snapshot lifetime counters before the stroke op.
    let before_paint = b.telemetry().lifetime.paint_submits;
    let before_q = b.telemetry().lifetime.queue_submit2;

    // Build 8 disjoint diagonal-ish segments. Each segment is
    // (x1, y1, x2, y2) as four i16's LE = 8 bytes. Bresenham
    // produces ~6-8 1×1 rects per segment, so the call passes
    // ~50 rects through `fill_solid_rects`. Pre-3f.15 this would
    // be ~50 paint_submits; post-3f.15 the count must be 1.
    let mut wire = Vec::with_capacity(8 * 8);
    let segs: [(i16, i16, i16, i16); 8] = [
        (0, 0, 6, 6),
        (8, 0, 14, 6),
        (16, 0, 22, 6),
        (24, 0, 30, 6),
        (0, 8, 6, 14),
        (8, 8, 14, 14),
        (16, 8, 22, 14),
        (24, 8, 30, 14),
    ];
    for (x1, y1, x2, y2) in segs {
        wire.extend_from_slice(&x1.to_le_bytes());
        wire.extend_from_slice(&y1.to_le_bytes());
        wire.extend_from_slice(&x2.to_le_bytes());
        wire.extend_from_slice(&y2.to_le_bytes());
    }
    b.poly_segment(None, xid, 0xFFFF_FFFF, &wire)
        .expect("poly_segment");

    let after_paint = b.telemetry().lifetime.paint_submits;
    let after_q = b.telemetry().lifetime.queue_submit2;
    assert_eq!(
        after_paint - before_paint,
        1,
        "PolySegment with 8 segments must coalesce to one paint submit (before={before_paint}, after={after_paint})",
    );
    assert_eq!(
        after_q - before_q,
        1,
        "queue_submit2 should also tick by exactly one for the batch",
    );
}

// ───── Stage 4a — resolve_paint_target via redirect routing ─────

/// Allocate two pixmaps W and B, install `redirected_target(W) =
/// Some(B)` via the test-only setter, then drive `fill_rectangle`
/// against W's xid. Pre-4a: paint would land in W's storage.
/// Post-4a: paint resolves through the redirect and lands in B.
/// GetImage on both reads back the redirected colour from B (also
/// resolved) and B (raw lookup); the same buffer in both cases.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_set_redirected_target_routes_fill_to_backing() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let bk_xid = b.create_pixmap(None, 32, 8, 8).expect("B").as_raw();
    // Pre-fill W with red and B with blue so we can tell which one
    // a subsequent paint actually hit.
    b.fill_rectangle(None, w_xid, 0xFFFF0000, 0, 0, 8, 8)
        .expect("seed W red");
    b.fill_rectangle(None, bk_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("seed B blue");

    // Install the redirect AFTER the seed fills so the seed paints
    // landed in their respective storage (W has red, B has blue
    // pre-redirect).
    assert!(
        b.test_set_redirected_target(w_xid, bk_xid),
        "test_set_redirected_target failed — xids resolvable?",
    );

    // Paint green via W's xid. Under redirect this lands in B,
    // overwriting the blue.
    b.fill_rectangle(None, w_xid, 0xFF00FF00, 0, 0, 8, 8)
        .expect("redirected fill");

    // GetImage on B's xid (raw, no redirect on a Pixmap) returns
    // the green — the redirected fill landed here.
    let img_b = b
        .get_image_pixels_for_tests(bk_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image B")
        .expect("Some B bytes");
    assert_eq!(
        &img_b[..4],
        &[0x00, 0xFF, 0x00, 0xFF],
        "B's (0,0) must read green (BGRA) after the redirected fill",
    );

    // GetImage on W's xid ALSO resolves through the redirect per
    // Risk 1, so it reads the same green from B — NOT the seeded
    // red on W's own storage.
    let img_w = b
        .get_image_pixels_for_tests(w_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image W")
        .expect("Some W bytes");
    assert_eq!(
        &img_w[..4],
        &[0x00, 0xFF, 0x00, 0xFF],
        "GetImage(W) under redirect must read from B (green), \
         not the leaf storage (still red)",
    );
}

/// Set up parent-W with a sub-child C at position (2, 3). Redirect
/// W to backing B. A fill rect at (1, 1, 4, 4) against C's xid must
/// land at (3, 4, 4, 4) in B — the C-relative offset accumulated
/// through `resolve_paint_target`. Tests the descendant-offset
/// path end-to-end through the Backend trait.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_set_redirected_target_descendant_fill_lands_at_offset() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Create depth-32 W under root, 16×16; then C at (2, 3) under
    // W, 8×8. allocate_window_storage will fill both with the
    // depth-32 safe default (transparent black).
    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            16,
            16,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("create W");
    let w_xid = w.as_raw();
    let c = b
        .create_subwindow(
            None,
            w,
            2,
            3,
            8,
            8,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("create C");
    let c_xid = c.as_raw();

    // Allocate B (a pixmap) for the backing storage. Seed it black
    // so the post-fill check can detect green-at-offset.
    let bk_xid = b.create_pixmap(None, 32, 16, 16).expect("B").as_raw();
    b.fill_rectangle(None, bk_xid, 0xFF000000, 0, 0, 16, 16)
        .expect("seed B black");

    // Install the redirect W → B.
    assert!(
        b.test_set_redirected_target(w_xid, bk_xid),
        "redirect install (W={w_xid:#x}, B={bk_xid:#x})"
    );

    // Fill green on C at (1, 1, 4, 4) — C-window-local coords.
    // Expected outcome: paint resolves through C→W (ancestor walk)
    // with accumulated offset (2, 3), then through W's redirect
    // to B. Result: green rect at B coords (3, 4, 4, 4).
    b.fill_rectangle(None, c_xid, 0xFF00FF00, 1, 1, 4, 4)
        .expect("descendant fill");

    // GetImage on B directly. Stride for depth-32 is `w * 4`.
    let img = b
        .get_image_pixels_for_tests(bk_xid, 2, 0, 0, 16, 16, !0)
        .expect("get_image B")
        .expect("Some bytes");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 16 + x) * 4;
        [img[off], img[off + 1], img[off + 2], img[off + 3]]
    };
    // Inside the redirected rect: (3,4)..(7,8).
    assert_eq!(
        pixel(3, 4),
        [0x00, 0xFF, 0x00, 0xFF],
        "B at (3,4) must be green — descendant offset (2,3) plus rect (1,1) sums to (3,4)",
    );
    assert_eq!(
        pixel(6, 7),
        [0x00, 0xFF, 0x00, 0xFF],
        "B at (6,7) — last pixel of the redirected rect — must also be green",
    );
    // Outside the rect: still the seeded black.
    assert_eq!(
        pixel(0, 0),
        [0x00, 0x00, 0x00, 0xFF],
        "B at (0,0) must stay black — fill lands at (3,4), not the origin",
    );
    assert_eq!(
        pixel(8, 8),
        [0x00, 0x00, 0x00, 0xFF],
        "B at (8,8) must stay black — past the redirected rect's bottom-right",
    );
}

// ───── Stage 4b — allocate_redirected_backing / name_window_pixmap /
// ───── release_redirected_backing
//
// Each test drives the Backend-trait surface for the COMPOSITE
// redirect lifecycle. v1's reference impls live in
// `crates/yserver/src/kms/backend.rs:9523-9607`; v2 mirrors the
// shape via `KmsCore.alias_registry` + `KmsCore.host_window_to_backing`
// (already in tree as shared state).

/// Plan §4b: `allocate_redirected_backing(W, w, h, depth)` allocates
/// a fresh backing pixmap, seeds `alias_registry` with refcount=1,
/// and maps `host_window_to_backing[W] = B`. The returned
/// `PixmapHandle` is what `name_window_pixmap(W)` returns on every
/// subsequent call (with incremented refcount).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_allocate_redirected_backing_seeds_refcount_and_map() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Allocate a pixmap to act as the "window" — v2 doesn't care
    // about W being a real Window-kind drawable for the activation
    // path; what matters is the xid resolves in the store so the
    // `set_redirected_target` step succeeds. (In 4c real-app paths
    // W is a top-level Window-kind drawable; the seed-copy path
    // tested separately in `v2_redirect_seed_copies_window_content`
    // exercises that shape.)
    let w_xid = b.create_pixmap(None, 32, 16, 16).expect("W").as_raw();
    let w_handle = WindowHandle::from_raw(w_xid).expect("WindowHandle");

    let backing = b
        .allocate_redirected_backing(None, w_handle, 16, 16, 32)
        .expect("allocate_redirected_backing must succeed in v2");
    let raw = backing.as_raw();
    assert_ne!(raw, 0, "backing handle is non-zero");
    assert_ne!(
        raw, w_xid,
        "backing xid distinct from window xid (fresh pixmap)",
    );

    // Inspect the shared state via the read-only test helper.
    let entry = b
        .test_alias_registry_get(raw)
        .expect("alias_registry must have a Reason-1 hold");
    assert_eq!(entry.refcount, 1, "Reason-1 seed → refcount = 1");
    assert_eq!(entry.width, 16);
    assert_eq!(entry.height, 16);
    assert_eq!(entry.depth, 32);

    let mapped = b
        .test_host_window_to_backing(w_xid)
        .expect("host_window_to_backing must point at the backing");
    assert_eq!(mapped, raw, "map points at the backing xid");
}

/// Plan §4b: a second `allocate_redirected_backing(W, …)` for an
/// already-redirected W returns the SAME handle with NO refcount
/// bump (it's the redirect-activation hold, not an alias). v1
/// idempotency path at `kms/backend.rs:9581-9588`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_allocate_redirected_backing_is_idempotent() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let first = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    let second = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    assert_eq!(
        first.as_raw(),
        second.as_raw(),
        "idempotent allocation returns the same handle",
    );
    let entry = b.test_alias_registry_get(first.as_raw()).unwrap();
    assert_eq!(
        entry.refcount, 1,
        "no incref on the idempotent path — Reason-1 is single-instance",
    );
}

/// Plan §4b: `name_window_pixmap(W)` after activation returns the
/// existing backing and increments refcount (Reason-2 alias hold).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_name_window_pixmap_returns_existing_backing() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let backing = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    let aliased = b.name_window_pixmap(None, w).unwrap();
    assert_eq!(
        aliased.as_raw(),
        backing.as_raw(),
        "alias handle equals backing handle (same xid on every call)",
    );
    let entry = b.test_alias_registry_get(backing.as_raw()).unwrap();
    assert_eq!(
        entry.refcount, 2,
        "alias bumps refcount to 2 (Reason-1 + Reason-2)",
    );
}

/// Plan §4b: `name_window_pixmap(W)` against an un-redirected W
/// returns `NotFound` (X11 protocol error → BadWindow upstream).
/// v1 uses `io::ErrorKind::NotFound` at `kms/backend.rs:9534`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_name_window_pixmap_without_redirect_errors_not_found() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let err = b
        .name_window_pixmap(None, w)
        .expect_err("name without redirect must error");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "v1-parity: NotFound (got {err:?})",
    );
}

/// Plan §4b: `release_redirected_backing` decrefs the Reason-1
/// hold; with no aliases held, the backing storage is destroyed.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_release_redirected_backing_drops_storage_when_no_aliases() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let backing = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    let bxid = backing.as_raw();

    b.release_redirected_backing(None, backing).unwrap();

    assert!(
        b.test_alias_registry_get(bxid).is_none(),
        "alias_registry entry removed (refcount → 0)",
    );
    assert!(
        b.test_host_window_to_backing(w_xid).is_none(),
        "host_window_to_backing entry cleared",
    );
}

/// Audit #6 (2026-05-19) — Xorg parity. `compNewPixmap`
/// (composite/compalloc.c:541-606) seeds the backing pixmap from
/// the PARENT's storage at W's position (with IncludeInferiors),
/// NOT from W's own storage. This is the fix for the recurring
/// "black band on map" symptom: a freshly mapped window that's
/// redirected on map has a default-init (opaque black or
/// transparent) storage; copying that into B would show black
/// where W is until the client's first paint. Seeding from the
/// parent shows continuity with what was on-screen before W
/// appeared.
///
/// Repro: paint root red at the W-footprint area; create W as a
/// child of root with NO paint of its own; activate redirect.
/// The backing must read red — parent's pixels at W's position —
/// NOT W's default-init colour.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_redirect_seed_uses_parent_content_at_w_position() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let root = WindowHandle::from_raw(1).expect("root");
    let root_xid = root.as_raw();

    // Paint a known red into the root at the area that W will cover.
    // We paint a 16×16 region from (5, 7) so it strictly contains W
    // (8×8 at (5, 7) inside root).
    b.fill_rectangle(None, root_xid, 0xFFFF0000, 5, 7, 16, 16)
        .expect("seed root red at W footprint");

    let w_handle = b
        .create_subwindow(
            None,
            root,
            5,
            7,
            8,
            8,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("create W as child of root");
    // Deliberately do NOT paint W — its storage stays at the
    // default init colour (depth-32 → (0, 0, 0, 0) transparent).

    let backing = b
        .allocate_redirected_backing(None, w_handle, 8, 8, 32)
        .expect("allocate must succeed");
    let bxid = backing.as_raw();

    let img = b
        .get_image_pixels_for_tests(bxid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some bytes");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 8 + x) * 4;
        [img[off], img[off + 1], img[off + 2], img[off + 3]]
    };

    // Pre-fix: backing reads W's default-init (0,0,0,0) — invisible /
    // black-band depending on the scene blend. Post-fix: parent's red
    // at the source position (5, 7), copied into B at (0, 0).
    assert_eq!(
        pixel(0, 0),
        [0x00, 0x00, 0xFF, 0xFF],
        "backing's (0, 0) must read parent's red at W's screen \
         position (5, 7); pre-fix the seed copied W's default-init \
         colour and produced (0, 0, 0, 0).",
    );
    assert_eq!(
        pixel(7, 7),
        [0x00, 0x00, 0xFF, 0xFF],
        "backing's (7, 7) must read parent's red (the W-footprint \
         region of root was filled red strictly larger than W).",
    );
}

/// Plan §4b: a `NameWindowPixmap` alias keeps the backing alive
/// past `release_redirected_backing` — the alias's FreePixmap
/// is what eventually drops the storage.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_release_redirected_backing_survives_named_alias() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).unwrap();
    let backing = b.allocate_redirected_backing(None, w, 8, 8, 32).unwrap();
    let bxid = backing.as_raw();
    let alias = b.name_window_pixmap(None, w).unwrap();
    assert_eq!(alias.as_raw(), bxid, "alias is the backing xid");

    // Drop Reason 1. Reason 2 (alias) keeps it alive.
    b.release_redirected_backing(None, backing).unwrap();
    let entry = b
        .test_alias_registry_get(bxid)
        .expect("alias still holds the backing");
    assert_eq!(entry.refcount, 1, "Reason-1 dropped, Reason-2 remains");
    assert!(
        b.test_host_window_to_backing(w_xid).is_none(),
        "redirect map cleared — only the alias refers to the backing now",
    );

    // FreePixmap on the alias must drop the storage.
    b.free_pixmap(None, alias.as_raw()).unwrap();
    assert!(
        b.test_alias_registry_get(bxid).is_none(),
        "alias FreePixmap drops the last hold",
    );
}

// ───── Stage 4c.5 — Vk-backed participation + mode-flip oracles ────
//
// Test #5 (`v2_redirected_paint_lands_in_backing`) from the task spec
// is already covered by `v2_set_redirected_target_routes_fill_to_backing`
// above — that test pre-fills B blue, installs the redirect, paints
// green through W's xid, and asserts B reads green. Skipped here to
// keep the suite mean (single-purpose oracles).

/// Stage 4c.5 — Automatic-mode redirect: paint through W's xid lands
/// in B (per 4a's `resolve_paint_target`) AND accumulates presentation
/// damage on B (since B's `scene_participating=true`). The scene
/// walk's `peek_presentation_damage` (scene.rs:1148 via 4c.3's
/// `source_id` indirection) is what picks up that damage; the
/// participation flag on B is the gate (`peek` returns None when
/// `!scene_participating`).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_automatic_redirect_backing_is_scene_participating() {
    use yserver_core::backend::{PixmapHandle, WindowHandle};

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Use a depth-32 pixmap as W (the redirect surface). `for_tests`
    // doesn't drive a real CreateWindow flow; v2's
    // `allocate_redirected_backing` accepts any drawable xid in the
    // store (the `name_window_pixmap_returns_existing_backing` test
    // above uses the same shape).
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).expect("WindowHandle");
    let backing = b
        .allocate_redirected_backing(None, w, 8, 8, 32)
        .expect("allocate backing");
    let bxid = backing.as_raw();
    let bk_handle = PixmapHandle::from_raw(bxid).expect("PixmapHandle");

    // Automatic-mode protocol pairing: W AND B both flip to
    // scene_participating=true.
    b.set_window_scene_participation(None, w, true)
        .expect("set_window_scene_participation(true)");
    b.set_backing_scene_participation(None, bk_handle, true)
        .expect("set_backing_scene_participation(true)");

    // Per-store assertion: B's scene_participating flipped on.
    // Reach into the doc-hidden test helpers via the public store
    // surface — `get_by_xid` is `pub(crate)`, so use the
    // presentation-damage probe below as the contract check.
    // First confirm the flag flipped by checking that
    // peek_presentation_damage doesn't `None` out (it would on
    // !scene_participating, even after we paint).

    // Paint green via W's xid. Per 4a's `resolve_paint_target` this
    // lands in B; per 3f's damage accounting that fires
    // `store.damage` on B's drawable, which (with B
    // scene_participating=true) accumulates as presentation damage.
    b.fill_rectangle(None, w_xid, 0xFF00FF00, 1, 2, 3, 4)
        .expect("redirected fill via W");

    // GetImage on B confirms the paint landed there (sanity — the
    // damage assertion below relies on the paint actually hitting).
    let img = b
        .get_image_pixels_for_tests(bxid, 2, 0, 0, 8, 8, !0)
        .expect("get_image B")
        .expect("Some B bytes");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 8 + x) * 4;
        [img[off], img[off + 1], img[off + 2], img[off + 3]]
    };
    assert_eq!(
        pixel(1, 2),
        [0x00, 0xFF, 0x00, 0xFF],
        "B at (1,2) — top-left of the redirected fill — must be green",
    );

    // The key oracle: presentation damage accumulated on B (because
    // B is scene_participating=true). A pre-4c backing with the
    // default scene_participating=false would have produced a
    // damage record that `peek_presentation_damage` returns as None
    // (see store.rs:670 — the gate is the `scene_participating`
    // flag). `test_peek_presentation_damage_nonempty` rolls both
    // checks into one bool to keep this oracle terse.
    assert!(
        b.test_peek_presentation_damage_nonempty(bxid),
        "B must have peekable, non-empty presentation damage from the redirected fill \
         (false ⇒ either scene_participating=false or region empty at paint time)",
    );
}

/// Stage 4c.5 — mode-flip preserves the backing and any
/// `NameWindowPixmap` aliases. Per Stage 4 plan §"Cross-cutting:
/// Mode-flip semantics", `RedirectWindow(W, Mode)` issued a second
/// time on an already-redirected W must reuse the existing backing
/// (no destroy + recreate) so client aliases stay valid and content
/// is preserved. This test exercises the at-this-layer simulation:
///
/// - alloc backing for W
/// - name_window_pixmap(W) → alias bumps refcount to 2
/// - paint a sentinel into B
/// - simulate a Manual→Automatic mode flip by toggling participation
///   (Automatic-mode protocol pairing)
/// - assert: backing's xid unchanged, alias refcount unchanged, B's
///   sentinel content preserved
///
/// Note (per task spec): the protocol-handler `flip_redirect_target_mode`
/// path in `yserver-core/src/core_loop/process_request.rs` isn't
/// drivable from `tests/v2_acceptance.rs` without protocol scaffolding
/// (see TODO comments below). The participation-toggle dance covers
/// the same backend-trait invariants the protocol handler exercises.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_mode_flip_preserves_backing_and_aliases() {
    use yserver_core::backend::{PixmapHandle, WindowHandle};

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let w_xid = b.create_pixmap(None, 32, 8, 8).expect("W").as_raw();
    let w = WindowHandle::from_raw(w_xid).expect("WindowHandle");

    // Initial Manual-mode setup: allocate backing, flip W off-scene.
    let backing = b
        .allocate_redirected_backing(None, w, 8, 8, 32)
        .expect("allocate backing");
    let bxid_pre_flip = backing.as_raw();
    b.set_window_scene_participation(None, w, false)
        .expect("Manual activation (W→false)");

    // Create a NameWindowPixmap alias — refcount goes 1 → 2.
    let alias = b.name_window_pixmap(None, w).expect("name_window_pixmap");
    assert_eq!(
        alias.as_raw(),
        bxid_pre_flip,
        "alias xid must equal the backing xid (Reason-2 incref on the same handle)",
    );
    let entry_before = b
        .test_alias_registry_get(bxid_pre_flip)
        .expect("alias_registry entry present");
    assert_eq!(
        entry_before.refcount, 2,
        "post-alias refcount = Reason-1 (1) + Reason-2 (1) = 2",
    );

    // Paint a sentinel into B before the flip — magenta at (0,0).
    b.fill_rectangle(None, bxid_pre_flip, 0xFFFF00FF, 0, 0, 8, 8)
        .expect("sentinel paint into B");
    let img_pre = b
        .get_image_pixels_for_tests(bxid_pre_flip, 2, 0, 0, 8, 8, !0)
        .expect("get_image pre-flip")
        .expect("Some bytes pre-flip");
    let pre_pixel: [u8; 4] = [img_pre[0], img_pre[1], img_pre[2], img_pre[3]];
    assert_eq!(
        pre_pixel,
        [0xFF, 0x00, 0xFF, 0xFF],
        "fixture sanity: B's (0,0) must read the sentinel magenta pre-flip",
    );

    // Mode flip: Manual → Automatic. The protocol handler's
    // `flip_redirect_target_mode` ultimately calls
    // `set_window_scene_participation(W, true)` +
    // `set_backing_scene_participation(B, true)`.
    let bk_handle = PixmapHandle::from_raw(bxid_pre_flip).expect("PixmapHandle");
    b.set_window_scene_participation(None, w, true)
        .expect("Automatic activation (W→true)");
    b.set_backing_scene_participation(None, bk_handle, true)
        .expect("Automatic activation (B→true)");

    // Backing xid unchanged.
    let bxid_post = b
        .test_host_window_to_backing(w_xid)
        .expect("host_window_to_backing still maps W → B");
    assert_eq!(
        bxid_post, bxid_pre_flip,
        "mode flip must NOT recreate the backing (xid must be stable)",
    );

    // Alias refcount unchanged (still Reason-1 + Reason-2).
    let entry_after = b
        .test_alias_registry_get(bxid_pre_flip)
        .expect("alias_registry entry still present post-flip");
    assert_eq!(
        entry_after.refcount, entry_before.refcount,
        "alias refcount must be preserved across mode flip \
         (pre={}, post={})",
        entry_before.refcount, entry_after.refcount,
    );

    // Content preserved — B's (0,0) still magenta.
    let img_post = b
        .get_image_pixels_for_tests(bxid_pre_flip, 2, 0, 0, 8, 8, !0)
        .expect("get_image post-flip")
        .expect("Some bytes post-flip");
    let post_pixel: [u8; 4] = [img_post[0], img_post[1], img_post[2], img_post[3]];
    assert_eq!(
        post_pixel, pre_pixel,
        "B's content must be preserved across mode flip \
         (pre={pre_pixel:?}, post={post_pixel:?})",
    );
}

// ───── Stage 4c.5 — deferred protocol-level tests ───────────────────
//
// The Stage 4b.9 / 4c plan also lists these protocol-level invariants
// that require driving the X11 wire bytes through
// `yserver-core::core_loop::process_request::handle_composite_request`.
// yserver-core has no test scaffolding for that path today, and
// building it is its own substage's worth of work. The hardware-smoke
// gate at 4c.6 is the actual coverage for these invariants until the
// scaffolding lands.
//
// TODO(4c.7 or post-4c): needs `handle_composite_request` test scaffolding
// - v2_map_window_after_redirect_subwindows_keeps_manual_participation
//     RedirectSubwindows(parent, Manual) → MapWindow(child) — child's
//     participation must stay Manual (off-scene); the post-map hook
//     must not flip it back on.
//
// TODO(4c.7 or post-4c): needs `handle_composite_request` test scaffolding
// - v2_map_subwindows_redirects_each_child
//     RedirectSubwindows(parent, Manual) → MapSubwindows(parent) —
//     every child gets its own `allocate_redirected_backing` call
//     via the per-child redirect hook.
//
// TODO(4c.7 or post-4c): needs `handle_composite_request` test scaffolding
// - v2_name_window_pixmap_on_unviewable_returns_bad_match
//     NameWindowPixmap(W) on an unmapped (unviewable) window must
//     return `BadMatch` per the X11 COMPOSITE spec, not silently
//     succeed with an alias to whatever backing exists.
//
// TODO(4c.7 or post-4c): needs `handle_composite_request` test scaffolding
// - v2_existing_alias_survives_window_unmap
//     A held NameWindowPixmap alias must keep the backing alive past
//     a subsequent UnmapWindow(W) (no race that drops the storage
//     when the redirect map clears).

/// Stage 4d — paint into the Composite Overlay Window via its xid
/// after `GetOverlayWindow`, and assert the paint lands on COW
/// storage with presentation damage accumulated. This is the load-
/// bearing v2 path for compositing WMs (marco-compositing,
/// xfwm4-compositing): pre-4d the COW xid resolved to nothing in
/// the store, so every `render_composite` against it gap-logged
/// and dropped paint.
///
/// Oracle shape: scanout dump integration is heavyweight (needs
/// `dump_scanout` wiring that test fixtures don't have); per the
/// stage brief, the acceptable surrogate is
/// `test_peek_presentation_damage_nonempty(0x103)` after a
/// `put_image` against the COW xid — confirms (a) the xid resolves,
/// (b) the storage is `scene_participating`, and (c) the paint
/// accumulated presentation damage that a scene tick would consume.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_cow_paint_appears_on_scanout() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Step 1: GetOverlayWindow — allocates COW storage at xid 0x103.
    b.get_overlay_window(None).expect("get_overlay_window");
    let cow_xid = 0x103u32;

    // Step 2: paint a known red square at (0, 0). put_image with a
    // 4-byte BGRA pixel goes through the engine.put_image path; on
    // a Vk-backed fixture this lands on COW storage.
    let pixels: Vec<u8> = vec![
        // 2×2 of red (BGRA premul: B=0, G=0, R=0xFF, A=0xFF)
        0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF,
        0xFF,
    ];
    b.put_image(None, cow_xid, 24, 2, 2, 0, 0, &pixels)
        .expect("put_image into COW xid");

    // Step 3: GetImage on the COW xid round-trips back the red
    // pixels — confirms the put_image actually landed on COW
    // storage (vs being dropped into the gap-logged no-op path).
    let img = b
        .get_image_pixels_for_tests(cow_xid, 2, 0, 0, 2, 2, !0)
        .expect("get_image COW")
        .expect("Some COW bytes");
    assert_eq!(
        &img[..4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "COW (0,0) must round-trip the painted red",
    );

    // Step 4: presentation damage accumulated on COW. The scene
    // tick would consume this on next composite; here we assert
    // the storage is in the right state (scene_participating=true
    // + non-empty damage region) to be picked up by build_scene.
    assert!(
        b.test_peek_presentation_damage_nonempty(cow_xid),
        "COW must have non-empty presentation damage after put_image — \
         false ⇒ either xid resolved to nothing (pre-4d shape) or \
         scene_participating=false (4d wiring missing)",
    );

    // Step 5: release drops the storage; the xid must no longer
    // resolve.
    b.release_overlay_window(None).expect("release");
    let img_after = b.get_image_pixels_for_tests(cow_xid, 2, 0, 0, 2, 2, !0);
    assert!(
        img_after.is_err() || img_after.as_ref().unwrap().is_none(),
        "GetImage on COW xid after final release must fail or return None \
         (storage destroyed) — got {img_after:?}",
    );
}

/// Regression: Present-Pixmap compositors (picom-style, some marco
/// builds) Redirect→GetOverlayWindow→Present::Pixmap onto COW. The
/// `Present::Pixmap` handler in the dispatcher resolves to v2's
/// `copy_area(src=pixmap, dst=COW)`, which lands content in COW
/// storage. But `build_scene` gates the COW draw on `inner.cow`,
/// which is set only by `SceneCompositor::register_cow`. Pre-fix,
/// `register_cow` was defined but no live backend ever called it
/// (status.md ~1769-1773 — un-wired during a 2026-05-17 xfwm4 fix
/// because xfwm4's compositor paints into a child window and the
/// zero-filled COW covers the real output). Net effect: the
/// compositor's full-screen composited image (including soft
/// drop shadows) lived in COW storage forever, invisible to the
/// scanout. User-visible symptom: opaque dark borders where soft
/// shadows should be.
///
/// Trigger semantics: lazy-register on the first paint into COW.
/// Allocating COW without ever painting (xfwm4 case) leaves the
/// scene entry off, so the depth-24 force-opaque initial fill does
/// not cover scene content.
///
/// Oracle: `get_overlay_window` → `put_image` against the COW xid →
/// scene reports the COW is registered. Pre-fix this assertion is
/// the failing one — every other observable (store lookup, refcount,
/// presentation damage) is correct because the bug is exclusively
/// at the scene-registration boundary.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_paint_into_cow_registers_scene_entry() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Allocation alone must NOT register: xfwm4-style compositors
    // allocate COW but never paint into it; registering eagerly
    // would force-opaque-cover the entire scene with the initial
    // zero-fill (depth-24 padding byte → α=1.0 through the
    // sample-side swizzle).
    b.get_overlay_window(None).expect("get_overlay_window");
    assert!(
        !b.test_scene_cow_registered(),
        "GetOverlayWindow alone must not register the COW scene entry \
         — xfwm4 allocates COW but paints into a child window, so an \
         eager registration would cover the real output with the \
         depth-24 force-opaque initial fill",
    );

    // First paint into COW (Present-Pixmap path goes through
    // `copy_area`; `put_image` is the test-fixture-friendly
    // equivalent paint vector). After this, the scene MUST see
    // the COW registered so `build_scene` emits it at the top of z.
    let cow_xid = 0x103u32;
    let pixels: Vec<u8> = vec![0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF];
    b.put_image(None, cow_xid, 24, 2, 1, 0, 0, &pixels)
        .expect("put_image into COW xid");
    assert!(
        b.test_scene_cow_registered(),
        "after the first client paint into COW, the scene must have \
         the COW registered as a top-of-z entry — otherwise the \
         compositor's painted content lives in storage but never \
         reaches the scanout (the active 'no shadows' bug)",
    );

    // Final release unregisters the scene entry so a subsequent
    // GetOverlayWindow round-trip starts cleanly. The scene field
    // tracks the storage lifetime, not the protocol refcount.
    b.release_overlay_window(None).expect("release");
    assert!(
        !b.test_scene_cow_registered(),
        "final ReleaseOverlayWindow must unregister the scene entry: \
         the storage is gone, so a stale registration would point at \
         a destroyed drawable",
    );
}

/// Stage 4d X11 Render `PictFormat` fix — marco-compositing widgets-
/// invisible repro.
///
/// Bug: redirected window backings end up with `α = 0x00` in their
/// storage (depth-24 padding byte) but marco's `Over` operator
/// samples that α=0 source, blends it with the dst, and produces
/// no contribution — widget contents stay invisible. The X11
/// Render spec says samples from a picture wrapping a depth-24
/// drawable must return `α = 1.0` (`PictFormat.alpha_mask = 0`).
///
/// Oracle: fill a depth-24 pixmap to a known RGB with α=0 in the
/// storage byte, then composite (`OP_SRC`) it onto a depth-32
/// pixmap pre-filled to transparent black. After the composite,
/// the dst must have `α = 0xFF` everywhere (force-opaque), not
/// `α = 0x00` (the pre-fix bug).
///
/// `OP_SRC` (op=1) was chosen because it's the simplest predicate:
/// `dst = src`. Any α-blending op would also work but introduces
/// more failure modes. The fix is exclusively shader-side, so the
/// minimal-blend op is the cleanest gate.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_render_composite_depth24_src_samples_opaque_alpha() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Step 1: depth-24 src pixmap, 4×4. fill_rectangle with
    // foreground 0x00_AABBCC: alpha-byte = 0x00, R = 0xAA, G = 0xBB,
    // B = 0xCC. v2's RGB→storage path lays this down as BGRA
    // `[0xCC, 0xBB, 0xAA, 0x00]` — α byte 0x00, exactly the
    // depth-24 padding case the marco-compositing bug hits.
    let src_pix = b.create_pixmap(None, 24, 4, 4).expect("create src d24");
    let src_xid = src_pix.as_raw();
    b.fill_rectangle(None, src_xid, 0x00_AA_BB_CC, 0, 0, 4, 4)
        .expect("fill_rectangle src d24 with α=0 in storage");

    // Step 2: depth-32 dst pixmap, 4×4, pre-cleared to transparent
    // black (α=0). After Composite the dst's α byte is the gate:
    // pre-fix it remains 0x00 (sampled from src storage), post-fix
    // it must be 0xFF (forced by the shader on depth-24 sources).
    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("create dst d32");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0x00_00_00_00, 0, 0, 4, 4)
        .expect("fill_rectangle dst d32 to transparent black");

    // Step 3: Pictures. Default formats — the backend picks
    // depth-matched PictFormats per the standard X11 Render
    // table (depth-24 → x8r8g8b8; depth-32 → a8r8g8b8).
    let src_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(src_pix), 0, 0, &[])
        .expect("render_create_picture src")
        .expect("Some(src PictureHandle)");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture dst")
        .expect("Some(dst PictureHandle)");

    // Step 4: Composite OP_SRC, full 4×4 cover. No mask, no
    // transform, no clip — the simplest path through
    // `RenderEngine::render_composite`. The src picture wraps a
    // depth-24 drawable; the force-opaque resolver flags it; the
    // shader pins sampled α = 1.0 (= src_uv.z, which is 1.0
    // everywhere inside the 4×4 cover); OP_SRC writes
    // `(R, G, B, 1.0)` into the dst.
    b.render_composite(
        None,
        1, // Src
        src_pic.as_raw(),
        0,
        dst_pic.as_raw(),
        0,
        0,
        0,
        0,
        0,
        0,
        4,
        4,
    )
    .expect("render_composite");

    // Step 5: Read dst back. Every pixel's α byte must be 0xFF —
    // the post-fix invariant. Pre-fix, α would be 0x00 (the src
    // padding byte).
    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
        .expect("get_image dst")
        .expect("Some(dst bytes)");
    assert_eq!(out.len(), 4 * 4 * 4, "4×4 BGRA8 readback");
    for y in 0..4 {
        for x in 0..4 {
            let off = (y * 4 + x) * 4;
            let px = &out[off..off + 4];
            // BGRA storage. The RGB channels come through from
            // src; the load-bearing assertion is α = 0xFF.
            assert_eq!(
                px[3], 0xFF,
                "dst ({x},{y}) α must be 0xFF (force-opaque); got {px:?}. \
                 Pre-fix this would be 0x00 — the depth-24 src padding byte.",
            );
        }
    }
}

/// Scene-path α-leak fix — sibling to
/// `v2_render_composite_depth24_src_samples_opaque_alpha` above,
/// covering the scene compositor side instead of the engine RENDER
/// side.
///
/// Bug: `Storage::image_view` is created with IDENTITY component
/// swizzle (required by VUID-VkFramebufferCreateInfo-pAttachments-00891
/// because the same view doubles as a colour attachment). The
/// engine's RENDER path avoids the depth-24 α-leak by sampling via
/// a separate cached view with `BgraNoAlpha` swizzle
/// (`engine::ensure_drawable_view`), but the scene compositor binds
/// `storage.image_view` directly in every `CompositeDraw`
/// (`scene::build_scene` four sites — root, window subtree, COW,
/// cursor). With `alpha_passthrough=true` on window draws, the
/// shader samples raw padding bytes as α; for a depth-24 BGRA8
/// drawable that has been filled with α-byte = 0 in storage (any
/// `put_image` of `0x00RRGGBB` wire bytes, the depth-24 default),
/// the scene blends with α=0 and the layer below shows through —
/// matching the `mate-with-compositing wallpaper bleeds through
/// COW` and `bits appear/disappear` symptoms.
///
/// Fix: `Storage` carries a second view `sample_view` built with
/// format-aware swizzle (α=ONE for depth-24 BGRA8). Scene draws
/// bind `sample_view`. This test only proves the field exists,
/// differs from `image_view` for a depth-24 drawable, and is a
/// real (non-null) `vk::ImageView`. End-to-end pixel-level scene
/// verification needs scanout-dump test scaffolding the v2
/// acceptance harness does not yet have — but the swizzle helper
/// itself is the load-bearing piece and is also covered by
/// engine-side composite tests.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_storage_depth24_has_distinct_sample_view() {
    use ash::vk;

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Depth-24 pixmap — the case where the BgraNoAlpha swizzle
    // (α=ONE) must differ from the identity attachment view.
    let pix24 = b.create_pixmap(None, 24, 4, 4).expect("create d24");
    let views24 = b
        .test_storage_views(pix24.as_raw())
        .expect("d24 storage resolves");
    assert_ne!(
        views24.0,
        vk::ImageView::null(),
        "d24 image_view must be non-null",
    );
    assert_ne!(
        views24.1,
        vk::ImageView::null(),
        "d24 sample_view must be non-null after the scene-α fix \
         (pre-fix: sample_view field did not exist, scene bound \
         image_view directly with identity swizzle, depth-24 \
         padding α leaked)",
    );
    assert_ne!(
        views24.0, views24.1,
        "d24 sample_view must be a different VkImageView than \
         image_view (different ComponentMapping — α=ONE vs \
         IDENTITY). Same handle would mean either the fix \
         wasn't applied or the format-aware swizzle defaulted \
         to identity for BGRA8/depth-24.",
    );

    // Depth-32 pixmap — sample_view's swizzle is also identity
    // (real α passes through), so the *swizzle* is the same as
    // image_view, but they must still be distinct VkImageView
    // handles (the attachment view must keep IDENTITY swizzle
    // unconditionally per VUID 00891, and the sample_view is
    // owned/destroyed separately by Storage). Asserting non-null
    // proves the plumbing is wired for depth-32 too.
    let pix32 = b.create_pixmap(None, 32, 4, 4).expect("create d32");
    let views32 = b
        .test_storage_views(pix32.as_raw())
        .expect("d32 storage resolves");
    assert_ne!(views32.0, vk::ImageView::null());
    assert_ne!(views32.1, vk::ImageView::null());
}

/// Stage 5 Task 4 layer 1 telemetry primer gate: after a single
/// render_composite call the backend telemetry must reflect ≥ 1
/// descriptor_pool_creates lifetime. Without backend wiring the
/// ring's lifetime counter increments but Telemetry stays at zero.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_render_composite_bumps_pool_create_telemetry() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 4, 4)
        .expect("pre-fill");
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("pic")
        .expect("Some");

    b.render_composite(
        None,
        3,
        src_pic.as_raw(),
        0,
        dst_pic.as_raw(),
        0,
        0,
        0,
        0,
        0,
        0,
        4,
        4,
    )
    .expect("composite");

    let t = b.telemetry();
    assert!(
        t.lifetime.descriptor_pool_creates >= 1,
        "expected ≥ 1 pool create, got {}",
        t.lifetime.descriptor_pool_creates,
    );
}

/// Stage 5 Task 4 layer 1 acceptance: N render_composite ops with
/// bounded in-flight depth must (1) bound pool creates, (2) actually
/// recycle pools (resets observed), (3) keep pool residency small.
/// Spec § 'Integration tests'.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_render_composite_pool_creates_bounded_after_warmup() {
    const N: u32 = 2000;
    // 256 sets per pool inside the ring (mirrors SETS_PER_POOL).
    const SETS_PER_POOL: u32 = 256;
    const WARMUP_SLACK: u64 = 4;
    let expected_creates_upper = u64::from(N / SETS_PER_POOL) + WARMUP_SLACK;
    let expected_resets_lower = u64::from(N / SETS_PER_POOL).saturating_sub(WARMUP_SLACK);

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("dst pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 4, 4)
        .expect("pre-fill blue");
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid red")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst pic")
        .expect("Some");

    for i in 0..N {
        b.render_composite(
            None,
            3,
            src_pic.as_raw(),
            0,
            dst_pic.as_raw(),
            0,
            0,
            0,
            0,
            0,
            0,
            4,
            4,
        )
        .unwrap_or_else(|e| panic!("composite #{i} failed: {e:?}"));
        // Retire often — every 32 ops drives the ring through full
        // recycle cycles. Without retirement the ring just grows
        // InFlight pools and never resets.
        if i % 32 == 31 {
            // Force fence completion via a sync get_image, then
            // drive the retirement loop explicitly (page flips don't
            // run in the pixmap-only fixture).
            let _ = b
                .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
                .expect("get_image");
            b.for_tests_poll_retired();
        }
    }
    // Final retirement to flush any remaining in-flight ops.
    let _ = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
        .expect("final get_image");
    b.for_tests_poll_retired();

    let t = b.telemetry();
    let creates = t.lifetime.descriptor_pool_creates;
    let resets = t.lifetime.descriptor_pool_resets;
    let residency = b.descriptor_pool_ring_pool_count();

    assert!(
        creates <= expected_creates_upper,
        "creates={creates}, expected <= {expected_creates_upper} (N={N})",
    );
    assert!(
        resets >= expected_resets_lower,
        "resets={resets}, expected >= {expected_resets_lower} \
         — recycle path didn't run; pools may be leaking as InFlight",
    );
    assert!(
        residency <= 4,
        "pool_count={residency} after warm-up; expected <= 4",
    );
}

/// Stage 5 Task 4 layer 1 acceptance for the traps call site. Same
/// three-assertion shape as render_composite — landing both makes
/// the regression surface explicit since the two engine paths share
/// the ring acquire helper.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_render_traps_pool_creates_bounded_after_warmup() {
    const N: u32 = 2000;
    const SETS_PER_POOL: u32 = 256;
    const WARMUP_SLACK: u64 = 4;
    let expected_creates_upper = u64::from(N / SETS_PER_POOL) + WARMUP_SLACK;
    let expected_resets_lower = u64::from(N / SETS_PER_POOL).saturating_sub(WARMUP_SLACK);

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let dst_pix = b.create_pixmap(None, 32, 8, 8).expect("dst pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("pre-fill blue");
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid red")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst pic")
        .expect("Some");

    // Same axis-aligned 4×4 trap used by
    // v2_render_trapezoids_renders_filled_rect.
    let mut traps: Vec<u8> = Vec::with_capacity(40);
    let fields: [i32; 10] = [
        2 << 16,
        6 << 16,
        2 << 16,
        2 << 16,
        2 << 16,
        6 << 16,
        6 << 16,
        2 << 16,
        6 << 16,
        6 << 16,
    ];
    for v in fields {
        traps.extend_from_slice(&v.to_le_bytes());
    }

    for i in 0..N {
        b.render_trapezoids(
            None,
            3,
            src_pic.as_raw(),
            dst_pic.as_raw(),
            0,
            0,
            0,
            &traps,
            0,
            0,
        )
        .unwrap_or_else(|e| panic!("trap #{i} failed: {e:?}"));
        if i % 32 == 31 {
            let _ = b
                .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
                .expect("get_image");
            b.for_tests_poll_retired();
        }
    }
    let _ = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("final get_image");
    b.for_tests_poll_retired();

    let t = b.telemetry();
    let creates = t.lifetime.descriptor_pool_creates;
    let resets = t.lifetime.descriptor_pool_resets;
    let residency = b.descriptor_pool_ring_pool_count();

    assert!(
        creates <= expected_creates_upper,
        "creates={creates}, expected <= {expected_creates_upper}",
    );
    assert!(
        resets >= expected_resets_lower,
        "resets={resets}, expected >= {expected_resets_lower}",
    );
    assert!(
        residency <= 4,
        "pool_count={residency} after warm-up; expected <= 4",
    );
}

/// Acceptance for GC clip-mask (depth-1 pixmap clip on Core paint).
/// This is the wmaker title-bar button glyph path: ChangeGC
/// clip-mask=<mask_pixmap> + PolyFillRectangle full_button. The depth-1
/// mask gates per-pixel paint to the mask shape.
///
/// Workflow:
///   1. Create depth-24 dst 8x8 pre-filled blue.
///   2. Create depth-1 mask 8x8 with the top half all ones and the
///      bottom half all zeros.
///   3. PutImage the mask bits (MSB-first packed, scanline-pad=4).
///   4. set_clip_pixmap mask at origin (0, 0).
///   5. poly_fill_rectangle full 8x8 in red.
///   6. clear_clip_rectangles to drop the clip.
///   7. GetImage dst: top half (rows 0..4) must be red; bottom half
///      (rows 4..8) must remain blue.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_clip_pixmap_mask_gates_poly_fill_to_mask_shape() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // Dst depth-24 8x8 pre-filled blue (0xFF0000FF → BGRA [0xFF,0,0,0xFF]).
    let dst_xid = b.create_pixmap(None, 24, 8, 8).unwrap().as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("fill_rectangle dst blue");

    // Mask depth-1 8x8: rows 0..4 all ones, rows 4..8 all zeros.
    // Each row is 8 bits = 1 byte of data; scanline-padded to 4 bytes.
    let mask_xid = b.create_pixmap(None, 1, 8, 8).unwrap().as_raw();
    let mut mask_bits = vec![0u8; 4 * 8];
    for row in 0..4 {
        mask_bits[row * 4] = 0xFF;
    }
    b.put_image(None, mask_xid, 1, 8, 8, 0, 0, &mask_bits)
        .expect("put_image mask");

    // Route through `apply_clip_state` — the actual live entry point
    // for ChangeGC clip-mask=<pixmap>. `set_clip_pixmap` is only used
    // by the host_x11/ynest path; KMS dispatch goes
    // `handle_change_gc -> resolve_draw_state ->
    // backend.apply_clip_state(&ClipState::Pixmap)`.
    use yserver_core::backend::{ClipState, PixmapHandle as ApplyPixmapHandle};
    let mask_handle = ApplyPixmapHandle::from_raw(mask_xid).expect("mask handle");
    b.apply_clip_state(
        None,
        &ClipState::Pixmap {
            origin: (0, 0),
            pixmap: mask_handle,
        },
    )
    .expect("apply_clip_state Pixmap");

    // PolyFillRectangle full 8x8 in red. Without the clip-mask path
    // honoured, every pixel turns red. With it, only the top half does.
    let rect_bytes = {
        let mut buf = Vec::new();
        buf.extend_from_slice(&i16::to_le_bytes(0));
        buf.extend_from_slice(&i16::to_le_bytes(0));
        buf.extend_from_slice(&u16::to_le_bytes(8));
        buf.extend_from_slice(&u16::to_le_bytes(8));
        buf
    };
    b.poly_fill_rectangle(None, dst_xid, 0xFFFF0000, &rect_bytes)
        .expect("poly_fill_rectangle");

    b.clear_clip_rectangles(None).expect("clear clip");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some(bytes)");

    // Top half rows: red (BGRA [0,0,0xFF,0xFF]).
    for row in 0..4 {
        for col in 0..8 {
            let off = (row * 8 + col) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0x00, 0x00, 0xFF, 0xFF],
                "row {row} col {col} should be red (mask=1)",
            );
        }
    }
    // Bottom half rows: blue (BGRA [0xFF,0,0,0xFF]).
    for row in 4..8 {
        for col in 0..8 {
            let off = (row * 8 + col) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0x00, 0x00, 0xFF],
                "row {row} col {col} should remain blue (mask=0)",
            );
        }
    }
}
