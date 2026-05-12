//! Server-fixture smoke tests — Phase A (composite plan) step A.1a.
//!
//! Exercises the in-process integration harness used by every L1
//! alpha-invariant test that lands after this one. The smoke
//! assertion only needs the headless `KmsBackend` boot — Vulkan
//! attach lands in A.1c.

#![cfg(target_os = "linux")]

mod common;

use common::server_fixture::ServerFixture;

#[test]
fn fixture_starts_and_creates_root_resources() {
    let fix = ServerFixture::start();
    assert!(fix.root_window().0 != 0);
    assert!(fix.has_default_visuals());
}

#[test]
fn fixture_can_fill_rectangle() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(100, 100, 24);
    fix.map_window(win);
    let gc = fix.create_gc(win, 0x00_ff_00_00);
    fix.fill_rectangle(win, gc, 10, 10, 30, 30);
    // No assertion on mirror contents yet — A.3 adds that. Here we only
    // prove the request path dispatches without producing a protocol error.
    assert!(fix.dispatched_without_error());
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn fixture_captures_window_mirror_pixels() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    // Green pixel under the root visual's masks (green_mask=0x0000ff00).
    // The plan uses 0x00_ff_00_00 in several tests but that's red
    // (red_mask=0x00ff0000); flagged so the plan can be patched.
    let gc = fix.create_gc(win, 0x00_00_ff_00);
    fix.fill_rectangle(win, gc, 0, 0, 64, 64);
    let img = fix.capture_window_mirror(win);
    // Green channel set on painted pixels (α policy lands in A.3).
    assert_eq!(img.pixel(32, 32).g, 0xff);
    assert_eq!(img.dimensions(), (64, 64));
}
