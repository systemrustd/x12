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
        .get_image(None, xid, 2 /* ZPixmap */, 0, 0, 8, 8, !0)
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
        .get_image(None, xid, 2, 0, 0, 8, 8, !0)
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
        .get_image(None, dst_xid, 2, 0, 0, 8, 4, !0)
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
    let _ = b.get_image(None, xid, 2, 0, 0, 4, 4, !0).unwrap();

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
        .get_image(None, dst_xid, 2, 0, 0, 4, 4, !0)
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
        .get_image(None, dst_xid, 2, 0, 0, 8, 4, !0)
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

    // depth-1 source 8×1. Bits MSB-first in one byte:
    // 0b1010_0000 → [1, 0, 1, 0, 0, 0, 0, 0].
    let src_pix = b
        .create_pixmap(None, 1, 8, 1)
        .expect("create_pixmap depth=1");
    // Depth-1 wire row stride = ceil(w/32)*4 = 4 bytes (one
    // scanline). Bit pattern in the high byte, zero pad.
    let src_bytes: Vec<u8> = vec![0b1010_0000, 0, 0, 0];
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
        .get_image(None, dst_pix.as_raw(), 2, 0, 0, 8, 1, !0)
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
        .get_image(None, dst_xid, 2, 0, 0, 8, 8, !0)
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
///   1. Trapezoids op=Over src=<SolidFill white> at the eye region
///   2. Trapezoids op=Over src=<SolidFill black> at a smaller
///      pupil region inside it
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
///   - black at the centre (inside both traps)
///   - white between (inside white but outside black)
///   - green at corners (outside both traps)
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
        .get_image(None, dst_xid, 2, 0, 0, 16, 16, !0)
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
        .get_image(None, dst_xid, 2, 0, 0, 10, 10, !0)
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
    let out = b
        .get_image(None, xid, 2, 0, 0, 64, 64, !0)
        .expect("get_image")
        .expect("Some");
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
        .get_image(None, dst_xid, 2, 0, 0, 200, 200, !0)
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
        .get_image(None, dst.as_raw(), 2, 0, 0, 4, 4, !0)
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
        .get_image(None, root_xid, 2, 0, 0, 8, 8, !0)
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
        .get_image(None, child_xid, 2, 0, 0, 16, 16, !0)
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
