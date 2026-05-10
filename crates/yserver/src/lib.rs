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

use crate::kms::KmsBackend;

const DISPLAY: u16 = 7;

pub fn run() -> io::Result<()> {
    #[cfg(not(target_os = "linux"))]
    panic!("yserver only supports Linux (DRM/KMS, libinput, evdev, virtual consoles)");

    log::info!("yserver: Phase 6.4 KMS bootstrap — startup (single-threaded core)");

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

    let mut backend = KmsBackend::open(&device_path)?;
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
        &mut backend,
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
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&mask), None)
        .map_err(|err| io::Error::other(format!("sigprocmask SIG_BLOCK: {err}")))?;
    SignalFd::new(&mask).map_err(|err| io::Error::other(format!("signalfd: {err}")))
}
