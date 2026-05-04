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
    backend::Backend,
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

    let input_fd = {
        let b = backend_arc.lock().unwrap();
        let kms = b.as_any().downcast_ref::<KmsBackend>().unwrap();
        kms.input_fd()
    };
    if let Some(fd) = input_fd {
        let input_borrow = unsafe { BorrowedFd::borrow_raw(fd) };
        epoll.add(
            input_borrow,
            EpollEvent::new(EpollFlags::EPOLLIN, INPUT_TOKEN),
        )?;
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
                    let mut b = backend_arc.lock().unwrap();
                    let kms = b.as_any_mut().downcast_mut::<KmsBackend>().unwrap();
                    if let Err(e) = kms.process_input_events() {
                        log::warn!("yserver: input event error: {e}");
                    }
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
