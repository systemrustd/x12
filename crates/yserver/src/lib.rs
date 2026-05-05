pub mod drm;
pub mod input;
pub mod kms;
pub mod present;

use std::{
    fs,
    io::{self, ErrorKind},
    os::{
        fd::{AsRawFd, BorrowedFd},
        unix::{fs::PermissionsExt, net::UnixListener},
    },
    path::PathBuf,
    sync::Arc,
    thread,
};

use nix::sys::{
    epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout},
    signal::{SigSet, SigmaskHow, Signal, sigprocmask},
    signalfd::SignalFd,
};

use yserver_core::{
    backend::{Backend, BackendEvent, BackendEventSink},
    host_x11::HostEvent,
    nested::handle_client,
    server::{ServerState, host_pump_event_sink},
};

use crate::kms::KmsBackend;

const DISPLAY: u16 = 7;
const LISTENER_TOKEN: u64 = 0;
const DRM_TOKEN: u64 = 1;
const INPUT_TOKEN: u64 = 2;
const SIGNAL_TOKEN: u64 = 3;

pub fn run() -> io::Result<()> {
    log::info!("yserver: Phase 6.4 KMS bootstrap — startup");

    let signal_fd = block_termination_signals()?;
    let device_path = resolve_drm_device()?;
    log::info!("yserver: opening DRM device {device_path}");

    let backend = KmsBackend::open(&device_path)?;
    let fb_w = backend.fb_dimensions().0;
    let fb_h = backend.fb_dimensions().1;
    log::info!("yserver: scanout {fb_w}x{fb_h}");

    let backend_arc: Arc<std::sync::Mutex<dyn Backend>> = Arc::new(std::sync::Mutex::new(backend));

    let server = Arc::new(std::sync::Mutex::new(ServerState::with_geometry(
        fb_w, fb_h,
    )));

    let xid_map = backend_arc.lock().unwrap().xid_map();
    let window_id = backend_arc.lock().unwrap().window_id();
    let sink = host_pump_event_sink(server.clone(), xid_map, window_id);
    // Clone the sink so the input thread can dispatch buffered pointer
    // events through it WITHOUT holding the backend mutex. The sink in
    // the backend itself is unused for the KMS pointer path (see the
    // `pending_pointer_events` doc) but stays wired for the
    // `BackendEventSink` trait surface.
    let input_sink = sink.clone();
    backend_arc
        .lock()
        .unwrap()
        .set_event_sink(Some(Box::new(sink)));

    let socket_dir = PathBuf::from("/tmp/.X11-unix");
    fs::create_dir_all(&socket_dir)?;
    let socket_path = socket_dir.join(format!("X{DISPLAY}"));
    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let listener = UnixListener::bind(&socket_path)?;
    // X clients connect as the invoking user; the socket needs world write
    // (connect() on AF_UNIX requires `w`). Xorg sets 0777 on /tmp/.X11-unix/X*.
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o777))?;
    log::info!("yserver: listening on unix socket DISPLAY=:{DISPLAY}");

    // Submit initial flip with root background
    {
        let mut b = backend_arc.lock().unwrap();
        let kms = b.as_any_mut().downcast_mut::<KmsBackend>().unwrap();
        if let Err(e) = kms.composite_and_flip() {
            log::warn!("yserver: initial composite_and_flip failed: {e}");
        }
    }

    let epoll = Epoll::new(EpollCreateFlags::empty())?;

    // SAFETY: we're borrowing the fd from objects that outlive the epoll set
    let drm_fd = {
        let b = backend_arc.lock().unwrap();
        let kms = b.as_any().downcast_ref::<KmsBackend>().unwrap();
        kms.drm_fd()
    };
    let drm_borrow = unsafe { BorrowedFd::borrow_raw(drm_fd) };
    epoll.add(drm_borrow, EpollEvent::new(EpollFlags::EPOLLIN, DRM_TOKEN))?;

    // Spawn a dedicated thread for libinput dispatch. The thread owns
    // the libinput context, polls the input fd, and for each event
    // briefly acquires the backend mutex (microseconds) to call
    // process_one_input_event. This decouples motion-event delivery
    // from per-client X11 request handlers — without it, a tight burst
    // of X11 requests (e.g. fvwm processing a Press) holds the backend
    // mutex long enough that motion events accumulate in libinput and
    // arrive at fvwm too late, breaking drag-to-move and similar
    // interactions that depend on prompt motion delivery.
    let input_ctx = {
        let mut b = backend_arc.lock().unwrap();
        let kms = b.as_any_mut().downcast_mut::<KmsBackend>().unwrap();
        kms.take_input_ctx()
    };
    if let Some(mut input_ctx) = input_ctx {
        let backend_for_input = backend_arc.clone();
        let mut input_sink = input_sink;
        log::info!("yserver: spawning dedicated libinput thread");
        thread::spawn(move || {
            // Dedicated input epoll set — wakes only on libinput events,
            // never blocked by other epoll consumers.
            let input_epoll = match Epoll::new(EpollCreateFlags::empty()) {
                Ok(e) => e,
                Err(err) => {
                    log::error!("input thread: epoll_create failed: {err}");
                    return;
                }
            };
            let fd = input_ctx.fd();
            let borrow = unsafe { BorrowedFd::borrow_raw(fd) };
            if let Err(err) = input_epoll.add(borrow, EpollEvent::new(EpollFlags::EPOLLIN, 0)) {
                log::error!("input thread: epoll_add failed: {err}");
                return;
            }
            let mut buf = [EpollEvent::empty(); 4];
            loop {
                match input_epoll.wait(&mut buf, EpollTimeout::NONE) {
                    Ok(_) => {}
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(err) => {
                        log::warn!("input thread: epoll_wait error: {err}");
                        continue;
                    }
                }
                let events = match input_ctx.dispatch() {
                    Ok(evs) => evs,
                    Err(err) => {
                        log::warn!("input thread: libinput dispatch error: {err}");
                        continue;
                    }
                };
                for event in events {
                    // Hold the backend mutex only for the state mutation
                    // and event buffering. Forwarding to the sink — which
                    // takes `server.lock()` — must happen AFTER the
                    // backend mutex is dropped, otherwise we deadlock
                    // against request handlers that hold `server.lock()`
                    // while reaching for the backend mutex.
                    let pending = {
                        let mut b = match backend_for_input.lock() {
                            Ok(b) => b,
                            Err(_) => return,
                        };
                        let Some(kms) = b.as_any_mut().downcast_mut::<KmsBackend>() else {
                            return;
                        };
                        kms.process_one_input_event(event);
                        kms.drain_pending_pointer_events()
                    };
                    for ptr_event in pending {
                        input_sink
                            .handle_backend_event(BackendEvent::HostEvent(HostEvent::Pointer(
                                ptr_event,
                            )));
                    }
                }
            }
        });
    }

    let listener_fd = listener.as_raw_fd();
    let listener_borrow = unsafe { BorrowedFd::borrow_raw(listener_fd) };
    epoll.add(
        listener_borrow,
        EpollEvent::new(EpollFlags::EPOLLIN, LISTENER_TOKEN),
    )?;

    epoll.add(
        &signal_fd,
        EpollEvent::new(EpollFlags::EPOLLIN, SIGNAL_TOKEN),
    )?;

    let mut events_buf = [EpollEvent::empty(); 8];
    let mut running = true;
    let mut client_count: u32 = 0;

    log::info!("yserver: entering epoll event loop");

    while running {
        let n = match epoll.wait(&mut events_buf, EpollTimeout::NONE) {
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(err) => return Err(io::Error::other(format!("epoll_wait: {err}"))),
        };

        for ev in &events_buf[..n] {
            match ev.data() {
                LISTENER_TOKEN => match listener.accept() {
                    Ok((stream, _addr)) => {
                        let client_id = yserver_protocol::x11::ClientId(client_count);
                        client_count += 1;
                        let host = backend_arc.clone();
                        let server = server.clone();
                        let handle = thread::spawn(move || {
                            if let Err(err) = handle_client(
                                client_id,
                                stream,
                                server,
                                Some(host),
                                Some(window_id),
                            ) {
                                log::info!("client {} disconnected: {err}", client_id.0);
                            }
                        });
                        thread::spawn(move || {
                            if let Err(panic) = handle.join() {
                                let msg = panic
                                    .downcast_ref::<String>()
                                    .map(|s| s.as_str())
                                    .or_else(|| panic.downcast_ref::<&str>().copied())
                                    .unwrap_or("(non-string panic)");
                                log::error!("client {} panicked: {msg}", client_id.0);
                            }
                        });
                        log::info!("yserver: client {} connected", client_id.0);
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                    Err(err) => log::warn!("yserver: accept failed: {err}"),
                },
                DRM_TOKEN => {
                    let mut b = backend_arc.lock().unwrap();
                    let kms = b.as_any_mut().downcast_mut::<KmsBackend>().unwrap();
                    if let Err(e) = kms.drain_page_flips_and_composite() {
                        log::warn!("yserver: page flip / composite error: {e}");
                    }
                }
                INPUT_TOKEN => {
                    // Input is handled by a dedicated thread now; this
                    // arm should never fire because we don't register
                    // INPUT_TOKEN with epoll. Kept as a defensive
                    // no-op in case a future refactor re-introduces the
                    // registration.
                }
                SIGNAL_TOKEN => match signal_fd.read_signal() {
                    Ok(Some(siginfo)) => {
                        log::info!(
                            "yserver: received signal {}, shutting down",
                            siginfo.ssi_signo
                        );
                        running = false;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        log::warn!("yserver: signalfd read error: {err}");
                    }
                },
                _ => {}
            }
        }
    }

    log::info!("yserver: shutting down, disabling output");
    {
        let b = backend_arc.lock().unwrap();
        let kms = b.as_any().downcast_ref::<KmsBackend>().unwrap();
        if let Err(e) = kms.disable_output() {
            log::warn!("yserver: disable_output failed: {e}");
        }
    }

    let _ = fs::remove_file(&socket_path);
    log::info!("yserver: master released, exiting");
    Ok(())
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
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&mask), None)
        .map_err(|err| io::Error::other(format!("sigprocmask SIG_BLOCK: {err}")))?;
    SignalFd::new(&mask).map_err(|err| io::Error::other(format!("signalfd: {err}")))
}
