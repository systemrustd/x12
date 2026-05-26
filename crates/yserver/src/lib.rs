pub mod clock;
pub mod drm;
pub mod input;
pub mod input_thread;
pub mod kms;
pub mod present;

use std::{
    fs,
    io::{self, ErrorKind},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
    thread,
};

use nix::sys::{
    signal::{SigSet, SigmaskHow, Signal, sigprocmask},
    signalfd::SignalFd,
};

use yserver_core::{
    core_loop::{self, Message, poll_tokens::ClientIdAllocator},
    server::ServerState,
};

const DISPLAY: u16 = 7;

pub fn run() -> io::Result<()> {
    #[cfg(not(target_os = "linux"))]
    panic!("yserver only supports Linux (DRM/KMS, libinput, evdev, virtual consoles)");

    log::info!("yserver: Phase 6.4 KMS bootstrap — startup (single-threaded core)");

    // Vulkan-call-rate telemetry: emit a per-second snapshot of
    // call counters from `kms::vk::call_stats::VK_CALLS`. Gated on
    // the same `YSERVER_LOOP_TELEMETRY` env var the core-loop
    // telemetry uses so the two rollups appear together. The
    // counter increments at each call site are unconditional
    // (atomic-add is ~1ns); only the per-second emission is
    // env-gated.
    if std::env::var_os("YSERVER_LOOP_TELEMETRY").is_some() {
        thread::spawn(|| {
            use std::time::Duration;
            // Previous-snapshot cache for the pool delta. The pool's
            // stats counters are cumulative; we emit per-second
            // deltas so the line reads the same way as the vk-call
            // rates.
            let mut prev_pool = crate::kms::vk::pixmap_pool::PixmapPoolStats::default();
            loop {
                thread::sleep(Duration::from_secs(1));
                let s = crate::kms::vk::call_stats::VK_CALLS.snapshot_and_reset();
                log::info!(
                    "vk call rate [1s]: barrier2={} draw={} bind_pl={} bind_ds={} \
                     push_const={} viewport={} scissor={} begin_rendering={} \
                     end_rendering={} copy_b2i={} copy_i={} copy_i2b={} \
                     clear_color_image={} queue_submit2={} begin_cb={} end_cb={}",
                    s.cmd_pipeline_barrier2,
                    s.cmd_draw,
                    s.cmd_bind_pipeline,
                    s.cmd_bind_descriptor_sets,
                    s.cmd_push_constants,
                    s.cmd_set_viewport,
                    s.cmd_set_scissor,
                    s.cmd_begin_rendering,
                    s.cmd_end_rendering,
                    s.cmd_copy_buffer_to_image,
                    s.cmd_copy_image,
                    s.cmd_copy_image_to_buffer,
                    s.cmd_clear_color_image,
                    s.queue_submit2,
                    s.begin_command_buffer,
                    s.end_command_buffer,
                );
                // Submit attribution: which call sites drive
                // queue_submit2. Sum should approximately equal
                // queue_submit2 above (off by ≤ Idle-flush count from
                // the flush_if_needed pre-attribution).
                log::info!(
                    "vk submit src [1s]: vis_composite={} readback={} ext_sync={} \
                     protocol_barrier={} size_limit={} latency_limit={} shutdown={} \
                     one_shot={} compositor={} other={}",
                    s.submit_visible_composite,
                    s.submit_readback,
                    s.submit_external_sync,
                    s.submit_protocol_barrier,
                    s.submit_size_limit,
                    s.submit_latency_limit,
                    s.submit_shutdown,
                    s.submit_one_shot,
                    s.submit_compositor,
                    s.submit_other,
                );
                // ProtocolBarrier per-site breakdown — the sum of
                // these eight counters equals `protocol_barrier`
                // above. Identifies which lifecycle path drives the
                // ProtocolBarrier flush rate.
                log::info!(
                    "vk pb src [1s]: drawable_destroy={} window_resize={} \
                     image_dealloc_fb={} dmabuf_release={} picture_destroy={} \
                     cursor_picture={}",
                    s.pb_drawable_destroy,
                    s.pb_window_resize,
                    s.pb_image_dealloc_fallback,
                    s.pb_dmabuf_release,
                    s.pb_picture_destroy,
                    s.pb_cursor_picture,
                );
                // submit_other per-caller breakdown — sum equals
                // `other` above. Distinguishes cursor / window /
                // pixmap mirror init clears.
                log::info!(
                    "vk init_clear src [1s]: cursor={} window={} pixmap={}",
                    s.init_clear_cursor,
                    s.init_clear_window,
                    s.init_clear_pixmap,
                );
                // PixmapPool deltas — cumulative counters minus the
                // previous snapshot. Tells us per second whether the
                // pool is being consulted (takes_hit+takes_miss),
                // whether mirrors return to it (returns_accepted),
                // and which rejection path fires (bucket_full means
                // PIXMAP_POOL_BUCKET_CAP is too small; oversize
                // means MAX_POOLED_DIM is too small).
                if let Some(cur) = crate::kms::vk::pixmap_pool::telemetry_snapshot() {
                    let d_hit = cur.total_takes_hit.wrapping_sub(prev_pool.total_takes_hit);
                    let d_miss = cur
                        .total_takes_miss
                        .wrapping_sub(prev_pool.total_takes_miss);
                    let d_acc = cur
                        .total_returns_accepted
                        .wrapping_sub(prev_pool.total_returns_accepted);
                    let d_full = cur
                        .total_returns_rejected_bucket_full
                        .wrapping_sub(prev_pool.total_returns_rejected_bucket_full);
                    let d_over = cur
                        .total_returns_rejected_oversize
                        .wrapping_sub(prev_pool.total_returns_rejected_oversize);
                    // Per-bin oversize-reject breakdown by max(width, height).
                    // Bins match `pixmap_pool::OVERSIZE_BIN_THRESHOLDS`:
                    // `<=256`, `<=512`, `<=1024`, `>1024`.
                    let d_over_bins: [u64; 4] = std::array::from_fn(|i| {
                        cur.total_returns_rejected_oversize_by_bucket[i]
                            .wrapping_sub(prev_pool.total_returns_rejected_oversize_by_bucket[i])
                    });
                    log::info!(
                        "pixmap pool [1s]: takes_hit={} takes_miss={} \
                         returns_accepted={} returns_rejected_bucket_full={} \
                         returns_rejected_oversize={} \
                         returns_rejected_oversize_by_bin[<=256,<=512,<=1024,>1024]=[{},{},{},{}]",
                        d_hit,
                        d_miss,
                        d_acc,
                        d_full,
                        d_over,
                        d_over_bins[0],
                        d_over_bins[1],
                        d_over_bins[2],
                        d_over_bins[3],
                    );
                    prev_pool = cur;
                }
            }
        });
    }

    let signal_fd = block_termination_signals()?;

    // Take over the console TTY before opening anything else: stops the
    // kernel keyboard driver from delivering Ctrl-C / Ctrl-Z / etc. as
    // signals to the controlling TTY's foreground process group, which
    // would otherwise kill the whole session when the user hits Ctrl-C
    // inside an X client. Skipped silently when not on a Linux VC (pty
    // under SSH or a graphical terminal emulator).
    #[cfg(target_os = "linux")]
    let _console_guard = crate::kms::console::ConsoleGuard::acquire()?;
    let device_path = resolve_drm_device()?;
    log::info!("yserver: opening DRM device {device_path}");

    let mut backend = crate::kms::KmsBackendKind::open_from_env(&device_path)?;
    let (fb_w, fb_h) = backend.fb_dimensions();
    log::info!("yserver: scanout {fb_w}x{fb_h}");

    let randr_outputs = backend.randr_outputs();
    let mut state = ServerState::with_randr_outputs(fb_w, fb_h, randr_outputs);

    let socket_dir = PathBuf::from("/tmp/.X11-unix");
    fs::create_dir_all(&socket_dir).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("create_dir_all({}): {e}", socket_dir.display()),
        )
    })?;
    let socket_path = socket_dir.join(format!("X{DISPLAY}"));
    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(io::Error::new(
                err.kind(),
                format!("remove_file({}): {err}", socket_path.display()),
            ));
        }
    }
    let listener = UnixListener::bind(&socket_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("UnixListener::bind({}): {e}", socket_path.display()),
        )
    })?;
    // X clients connect as the invoking user; the socket needs world write
    // (connect() on AF_UNIX requires `w`). Xorg sets 0777 on /tmp/.X11-unix/X*.
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o777)).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("set_permissions({}, 0o777): {e}", socket_path.display()),
        )
    })?;
    log::info!("yserver: listening on unix socket DISPLAY=:{DISPLAY}");

    // Initial composite+flip so the screen has a known frame before any
    // client connects.
    if let Err(e) = backend.composite_and_flip() {
        log::warn!("yserver: initial composite_and_flip failed: {e}");
    }

    // Build the channel + waker before spawning anything: senders need
    // a clone, run_core needs the receiver.
    let (poll, sender, rx) = core_loop::channel()?;

    // Hand the libinput context off to the dedicated input thread. After
    // this `take_input_ctx`, the backend's `poll_fds()` returns only
    // the DRM fd, so run_core's E3 registration step won't double-poll
    // libinput.
    if let Some(input_ctx) = backend.take_input_ctx() {
        let input_sender = sender.clone_handle();
        log::info!("yserver: spawning libinput sender thread");
        thread::Builder::new()
            .name("yserver-libinput".into())
            .spawn(move || {
                if let Err(err) =
                    input_thread::run(input_ctx, input_sender, u32::from(fb_w), u32::from(fb_h))
                {
                    log::warn!("yserver: libinput thread exited: {err}");
                }
            })?;
    }

    // signalfd → Message bridge. yserver-core deliberately doesn't
    // depend on nix; a tiny thread wraps the SignalFd read so run_core
    // only sees channel-side messages. SIGINT/SIGTERM map to
    // `Shutdown`; SIGUSR1 maps to `DumpScanout` (diagnostic — backend
    // dumps the current scanout BO to a file in cwd).
    let signal_sender = sender.clone_handle();
    thread::Builder::new()
        .name("yserver-signalfd".into())
        .spawn(move || {
            let signal_fd = signal_fd;
            loop {
                match signal_fd.read_signal() {
                    Ok(Some(siginfo)) => {
                        let signo = siginfo.ssi_signo as i32;
                        if signo == nix::libc::SIGUSR1 {
                            log::info!("yserver: received SIGUSR1, dumping scanout");
                            if signal_sender.send(Message::DumpScanout).is_err() {
                                return;
                            }
                            // Stay alive — SIGUSR1 isn't fatal.
                            continue;
                        }
                        if signo == nix::libc::SIGUSR2 {
                            log::info!("yserver: received SIGUSR2, dumping drawables");
                            if signal_sender.send(Message::DumpDrawables).is_err() {
                                return;
                            }
                            // Stay alive — SIGUSR2 isn't fatal.
                            continue;
                        }
                        log::info!("yserver: received signal {signo}, requesting shutdown");
                        let _ = signal_sender.send(Message::Shutdown);
                        return;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        log::warn!("yserver: signalfd read error: {err}");
                        let _ = signal_sender.send(Message::Shutdown);
                        return;
                    }
                }
            }
        })?;

    let alloc = ClientIdAllocator::new();
    log::info!("yserver: entering single-threaded core loop");
    let result = core_loop::run_core(
        poll,
        rx,
        sender,
        &mut state,
        backend.as_dyn_backend_mut(),
        Some(listener),
        &alloc,
    );
    if let Err(err) = &result {
        log::warn!("yserver: run_core returned error: {err}");
    }

    log::info!("yserver: shutting down, disabling output");
    if let Err(e) = backend.disable_output() {
        log::warn!("yserver: disable_output failed: {e}");
    }
    // Stage 5 Task 6.1: fan out any PRESENT completions deferred past
    // shutdown drain — events must reach clients before we tear down
    // the socket.
    for entry in backend.take_shutdown_present_events() {
        yserver_core::core_loop::process_request::fire_present_completion_events(
            &mut state, &entry,
        );
    }

    let _ = fs::remove_file(&socket_path);
    log::info!("yserver: master released, exiting");
    result
}

fn resolve_drm_device() -> io::Result<String> {
    if let Ok(explicit) = std::env::var("YSERVER_DRM_DEVICE") {
        return Ok(explicit);
    }
    let candidates = ["/dev/dri/card0", "/dev/dri/card1"];
    let mut last_err: Option<io::Error> = None;
    for path in candidates {
        match drm::Device::open(path) {
            Ok(_) => return Ok(path.to_string()),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                last_err = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_err
        .unwrap_or_else(|| io::Error::new(ErrorKind::NotFound, "no DRM card devices found")))
}

fn block_termination_signals() -> io::Result<SignalFd> {
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGTERM);
    // SIGUSR1 → diagnostic scanout dump. Blocked so signalfd consumes
    // it instead of the default-action (which would terminate us).
    mask.add(Signal::SIGUSR1);
    // SIGUSR2 → diagnostic drawable-storage dump (root + COW + every
    // redirected backing). Same blocking rationale as SIGUSR1.
    mask.add(Signal::SIGUSR2);
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&mask), None)
        .map_err(|err| io::Error::other(format!("sigprocmask SIG_BLOCK: {err}")))?;
    SignalFd::new(&mask).map_err(|err| io::Error::other(format!("signalfd: {err}")))
}
