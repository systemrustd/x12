pub mod drm;
pub mod input;
pub mod present;

use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use nix::sys::signal::{SigSet, SigmaskHow, Signal, sigprocmask};
use nix::sys::signalfd::SignalFd;

use crate::present::State;

const RECT_VEL_X: f32 = 220.0;
const RECT_VEL_Y: f32 = 175.0;

pub fn run() -> io::Result<()> {
    log::info!("yserver: Phase 6 bootstrap — startup");

    let signal_fd = block_termination_signals()?;

    let device = Arc::new(open_drm_device()?);
    log::info!(
        "yserver: opened DRM device {}, master + atomic capabilities acquired",
        device.path()
    );

    let output = drm::modeset::discover_output(&device)?;
    drm::modeset::dump_properties(&device, &output)?;

    let fb_w = output.picked.width;
    let fb_h = output.picked.height;

    let mut state = State {
        rect_x: f32::from(fb_w) / 2.0,
        rect_y: f32::from(fb_h) / 2.0,
        vel_x: RECT_VEL_X,
        vel_y: RECT_VEL_Y,
        cursor_x: f32::from(fb_w) / 2.0,
        cursor_y: f32::from(fb_h) / 2.0,
    };

    let mut buffers = Vec::with_capacity(2);
    for idx in 0..2 {
        let mut b = drm::Buffer::new(Arc::clone(&device), fb_w, fb_h)?;
        present::paint(&state, &mut b);
        log::info!(
            "yserver: allocated buffer[{idx}] {}x{} fb=0x{:x}",
            b.width(),
            b.height(),
            u32::from(b.fb_id())
        );
        buffers.push(b);
    }
    let initial_fb = buffers[0].fb_id();
    drm::modeset::commit_modeset(&device, &output, initial_fb)?;
    log::info!(
        "yserver: atomic modeset committed — buffer[0] on {}",
        output.connector_name
    );

    let mut swapchain = drm::Swapchain::with_initial_scanout(buffers, 0);

    let mut input_ctx = match input::Context::new() {
        Ok(ctx) => {
            log::info!("yserver: libinput context attached to seat0");
            Some(ctx)
        }
        Err(err) => {
            log::warn!("yserver: libinput unavailable, continuing without input: {err}");
            None
        }
    };

    let running = Arc::new(AtomicBool::new(true));

    // Submit the first flip with buffer[1].
    if let Some(idx) = swapchain.acquire_idx() {
        let fb_id = swapchain.buffer(idx).fb_id();
        drm::page_flip::submit_flip(&device, &output, fb_id)?;
        swapchain
            .submit(idx)
            .map_err(|e| io::Error::other(format!("swapchain.submit: {e}")))?;
    }

    let loop_result = present::run_loop(
        &device,
        &output,
        &mut swapchain,
        input_ctx.as_mut(),
        &signal_fd,
        &mut state,
        fb_w,
        fb_h,
        &running,
    );

    log::info!("yserver: disabling plane + CRTC");
    if let Err(err) = drm::modeset::disable_output(&device, &output) {
        log::warn!("yserver: disable_output failed (continuing shutdown): {err}");
    }

    drop(swapchain);
    drop(input_ctx);
    drop(device);
    log::info!("yserver: master released, exiting");
    loop_result
}

fn open_drm_device() -> io::Result<drm::Device> {
    if let Ok(explicit) = std::env::var("YSERVER_DRM_DEVICE") {
        return drm::Device::open(&explicit);
    }
    let candidates = ["/dev/dri/card0", "/dev/dri/card1"];
    let mut last_err: Option<io::Error> = None;
    for path in candidates {
        match drm::Device::open(path) {
            Ok(device) => return Ok(device),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                log::debug!("yserver: {path} not present, trying next candidate");
                last_err = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no DRM card devices found")
    }))
}

fn block_termination_signals() -> io::Result<SignalFd> {
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGTERM);
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&mask), None)
        .map_err(|err| io::Error::other(format!("sigprocmask SIG_BLOCK: {err}")))?;
    SignalFd::new(&mask).map_err(|err| io::Error::other(format!("signalfd: {err}")))
}
