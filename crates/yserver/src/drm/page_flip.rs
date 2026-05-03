//! Page-flip submission + completion drain.
//!
//! `submit_flip` atomic-commits a new FB_ID on the primary plane with
//! PAGE_FLIP_EVENT | NONBLOCK; the kernel produces a completion event
//! on the DRM fd when scanout latches the new buffer.
//!
//! `drain_events` reads pending events with `Device::receive_events()`
//! and dispatches PageFlip completions to a closure. The drm crate's
//! parser drops the kernel `user_data` field (it folds it into `crtc`),
//! so the closure receives no per-flip identifier; callers identify the
//! flipped buffer via [`crate::drm::Swapchain::submitted_idx`] —
//! at most one buffer is `Submitted` at a time per the state machine.

use std::io;

use drm::control::{
    AtomicCommitFlags, Device as ControlDevice, Event, atomic::AtomicModeReq, framebuffer,
};

use crate::drm::{Device, modeset::Output};

pub fn submit_flip(device: &Device, output: &Output, fb_id: framebuffer::Handle) -> io::Result<()> {
    let mut req = AtomicModeReq::new();
    req.add_raw_property(
        output.plane.into(),
        output.plane_fb_id_prop,
        u64::from(u32::from(fb_id)),
    );
    req.add_raw_property(
        output.plane.into(),
        output.plane_crtc_id_prop,
        u64::from(u32::from(output.crtc)),
    );

    device.atomic_commit(
        AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
        req,
    )
}

pub fn drain_events<F: FnMut()>(device: &Device, mut on_page_flip: F) -> io::Result<()> {
    for event in device.receive_events()? {
        match event {
            Event::PageFlip(_) => on_page_flip(),
            Event::Vblank(_) | Event::Unknown(_) => {}
        }
    }
    Ok(())
}
