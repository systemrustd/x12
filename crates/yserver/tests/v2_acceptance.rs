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
use yserver_core::backend::{AnyHandle, Backend};
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
