//! Skeleton of the single-threaded core loop.
//!
//! B4 established the shape; D4 wired `Message::Request` against the
//! new `process_request` entry point and the lifecycle arms
//! (SetupAllocate, ClientSetupComplete, ClientDisconnected,
//! HostInput, PageFlipReady). E3/E4 (DRM + signalfd) and F2
//! (host-X11) supply the missing token arms; D5 supplies the
//! listener.

use std::{
    io,
    os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
    },
};

use log::{error, warn};
use mio::{Events, Interest, Poll, unix::SourceFd};

use super::{
    client_io::{self, WriteOutcome},
    message::{HostInputEvent, Message, SetupAllocateResponse},
    poll_tokens::{
        ClientIdAllocator, DRM_TOKEN, HOST_X11_TOKEN, LIBINPUT_TOKEN, LISTENER_TOKEN, NOTIFY_TOKEN,
        client_token, token_to_client,
    },
    process_request::{RequestOutcome, process_request},
    sender::{CoreReceiver, CoreSender},
    setup_thread::{self, SetupRegistry},
};
use crate::{
    backend::{Backend, BackendFdKind, HostSocketStatus},
    host_x11::HostEvent,
    server::ServerState,
};

/// Run the core loop until `Message::Shutdown` is observed.
///
/// `poll` must already have its waker registered against `NOTIFY_TOKEN`
/// (see `core_loop::channel`). Additional fds (listener, client
/// writers, drm, libinput, signalfd, host-X11) get registered by their
/// respective phase tasks before this function takes over the thread.
///
/// `state` and `backend` are owned by the core loop for the duration
/// of the run — the whole point of the single-threaded refactor is
/// that only this thread can mutate them.
pub fn run_core(
    mut poll: Poll,
    rx: CoreReceiver,
    sender: CoreSender,
    state: &mut ServerState,
    backend: &mut dyn Backend,
    listener: Option<UnixListener>,
    client_id_allocator: &ClientIdAllocator,
) -> io::Result<()> {
    let setup_registry = setup_thread::make_registry();
    let listener = if let Some(listener) = listener {
        listener.set_nonblocking(true)?;
        let raw = listener.as_raw_fd();
        poll.registry()
            .register(&mut SourceFd(&raw), LISTENER_TOKEN, Interest::READABLE)?;
        Some(listener)
    } else {
        None
    };

    // E3: register backend-owned fds with the core poller. KMS returns
    // `Drm` only after `take_input_ctx`; the libinput context, when
    // present, is owned by the dedicated libinput thread (E2/E4) so
    // the core never sees the libinput fd in production. The Libinput
    // arm is registered defensively in case a backend variant chooses
    // to skip the dedicated thread and run libinput on the core poll.
    for (fd, kind) in backend.poll_fds() {
        let token = match kind {
            BackendFdKind::Drm => DRM_TOKEN,
            BackendFdKind::Libinput => LIBINPUT_TOKEN,
            BackendFdKind::HostX11 => HOST_X11_TOKEN,
        };
        poll.registry()
            .register(&mut SourceFd(&fd), token, Interest::READABLE)?;
    }

    let mut events = Events::with_capacity(64);
    loop {
        poll.poll(&mut events, None)?;
        for ev in events.iter() {
            match ev.token() {
                LISTENER_TOKEN => {
                    if let Some(listener) = listener.as_ref() {
                        accept_pending(listener, client_id_allocator, &sender, &setup_registry);
                    }
                }
                DRM_TOKEN => {
                    // DRM completion event(s). Backend drains
                    // page-flip completions and submits the next
                    // composite/flip if the screen is dirty. mio is
                    // edge-triggered; the backend itself owns
                    // batching, so this dispatch never coalesces.
                    backend.on_page_flip_ready(state);
                }
                LIBINPUT_TOKEN => {
                    // Libinput on the core poll is reserved for a
                    // future configuration that skips the dedicated
                    // libinput thread. In production today the
                    // libinput thread owns the fd; if this arm fires,
                    // poll_fds() registered the libinput fd against
                    // a backend that doesn't define a corresponding
                    // dispatch entry-point. Log so a regression here
                    // is visible.
                    warn!("core_loop::run: LIBINPUT_TOKEN ready but no on-core dispatch path");
                }
                HOST_X11_TOKEN => {
                    // F2: host X11 connection is readable. Drain
                    // whatever frames are buffered into the backend's
                    // pending_replies / pending_events queues. Fanout
                    // happens at the outer-loop boundary so a host
                    // request issued from inside fanout cannot
                    // recursively re-enter event dispatch.
                    match backend.drain_host_socket() {
                        Ok(HostSocketStatus::WouldBlock) => {}
                        Ok(HostSocketStatus::Eof) => {
                            log::info!("host X11 connection closed; shutting down");
                            return Ok(());
                        }
                        Err(err) => {
                            log::warn!("drain_host_socket: {err}");
                            return Ok(());
                        }
                    }
                }
                tok if let Some(client_id) = token_to_client(tok) => {
                    // I3: WRITABLE-readiness on a client writer fd.
                    // Drain the outbound buffer; if it empties, the
                    // post-loop interest reconciliation drops
                    // WRITABLE. If the peer disappeared, mark the
                    // client for disconnect.
                    if !ev.is_writable() {
                        // mio always reports both READABLE+WRITABLE
                        // as readiness even when only one was asked
                        // for; the writer fd's READABLE wakeups are
                        // ignored — the reader thread owns reads.
                        continue;
                    }
                    let Some(client) = state.clients.get_mut(&client_id.0) else {
                        // Already removed by a prior disconnect; the
                        // poller will be deregistered after.
                        continue;
                    };
                    match client_io::drain_outbound(client) {
                        Ok(WriteOutcome::Done | WriteOutcome::WouldBlock) => {}
                        Ok(WriteOutcome::Disconnect) | Err(_) => {
                            crate::core_loop::process_disconnect::process_disconnect(
                                state, backend, client_id,
                            );
                        }
                    }
                }
                NOTIFY_TOKEN => {
                    for msg in rx.try_recv_all() {
                        match msg {
                            Message::Shutdown => {
                                setup_thread::shutdown_all(&setup_registry);
                                return Ok(());
                            }
                            Message::Request {
                                id,
                                sequence,
                                header,
                                body,
                                attached_fd,
                            } => {
                                let outcome = process_request(
                                    state,
                                    backend,
                                    id,
                                    sequence,
                                    header,
                                    &body,
                                    attached_fd,
                                )?;
                                if let RequestOutcome::Disconnect(disc_id) = outcome {
                                    crate::core_loop::process_disconnect::process_disconnect(
                                        state, backend, disc_id,
                                    );
                                }
                            }
                            Message::SetupAllocate { id, response_tx } => {
                                handle_setup_allocate(state, id, response_tx);
                            }
                            Message::ClientSetupComplete {
                                id,
                                stream,
                                resource_id_base,
                                resource_id_mask,
                                byte_order,
                            } => {
                                if let Err(err) = handle_client_setup_complete(
                                    poll.registry(),
                                    &sender,
                                    &setup_registry,
                                    state,
                                    id,
                                    stream,
                                    resource_id_base,
                                    resource_id_mask,
                                    byte_order,
                                ) {
                                    error!("ClientSetupComplete for client {} failed: {err}", id.0);
                                    crate::core_loop::process_disconnect::process_disconnect(
                                        state, backend, id,
                                    );
                                }
                            }
                            Message::ClientDisconnected { id, reason: _ } => {
                                crate::core_loop::process_disconnect::process_disconnect(
                                    state, backend, id,
                                );
                            }
                            Message::HostInput(ev) => handle_host_input(state, backend, ev),
                            Message::PageFlipReady => backend.on_page_flip_ready(state),
                        }
                    }
                }
                tok => {
                    warn!("core_loop::run: unhandled poll token {tok:?}");
                }
            }
        }
        // F2: drain any host-X11 events the backend decoded during
        // this iteration. Fanout runs at the outermost stack frame
        // — no `wait_for_reply` is on the stack here — so handlers
        // that issue further host requests are safe.
        dispatch_pending_host_events(state, backend);

        // F2: if a `wait_for_reply` (called by `process_request`
        // mid-handler) saw the host close, propagate it as a clean
        // shutdown. The IO error already surfaced to the caller; we
        // observe the EOF flag here and stop the core loop.
        if backend.host_socket_eof() {
            log::info!("host X11 EOF observed; shutting down");
            return Ok(());
        }

        // I2: walk clients once per loop iteration and reconcile
        // poll interest against the live state of `outbound`. A
        // client whose buffer just became non-empty needs WRITABLE;
        // one that just drained back to empty drops it. Swallows
        // reregister errors that mean "fd already deregistered" so a
        // disconnect that ran during this iteration doesn't break
        // the next one.
        for disc_id in reconcile_client_writable_interest(poll.registry(), state) {
            crate::core_loop::process_disconnect::process_disconnect(state, backend, disc_id);
        }
    }
}

/// F2: pop every pending host event off the backend and fan it out
/// to nested clients. Runs at the outer-loop boundary so a host
/// request issued inside fanout (CreateWindow forwarding,
/// SetClipRectangles, etc.) cannot recursively re-dispatch — the new
/// request's reply lands in `pending_replies` and the next
/// outer-loop iteration drains anything `wait_for_reply` re-enqueued.
fn dispatch_pending_host_events(state: &mut ServerState, backend: &mut dyn Backend) {
    while let Some(event) = backend.pop_pending_host_event() {
        // The fanout helpers borrow `xid_map` immutably — clone the
        // map up-front so we can release the immutable borrow on
        // backend before mutating `state`'s per-client outbound
        // buffers. The map is a few hundred entries even on a busy
        // session.
        let xid_map = backend.xid_map().clone();
        match event {
            HostEvent::Pointer(ev) => {
                use crate::core_loop::pointer_fanout::pointer_event_fanout_to_state;
                let _dropped = pointer_event_fanout_to_state(state, &xid_map, ev, true);
            }
            HostEvent::Expose(ev) => {
                use crate::core_loop::fanout::expose_event_fanout_to_state;
                let _dropped = expose_event_fanout_to_state(state, &xid_map, ev);
            }
            HostEvent::Key(ev) => {
                use crate::core_loop::key_fanout::key_event_fanout_to_state;
                let _dropped = key_event_fanout_to_state(state, ev);
            }
            HostEvent::Configure(ev) => {
                if backend.window_id() == ev.host_xid {
                    handle_host_container_resize(state, ev);
                }
            }
            HostEvent::Closed => {
                log::info!("host container window destroyed; shutting down");
                // Triggering shutdown via a flag is awkward without
                // sender access here — return Ok from run_core via
                // host_socket_eof check on next iteration.
            }
        }
    }
}

pub(crate) fn handle_host_container_resize(
    state: &mut ServerState,
    ev: crate::host_x11::HostConfigureEvent,
) {
    use std::sync::atomic::Ordering;
    use yserver_protocol::x11::{self, SequenceNumber, randr as x11randr};

    const RANDR_FIRST_EVENT: u8 = 89;

    if ev.width == 0
        || ev.height == 0
        || (state.randr.screen_width == ev.width && state.randr.screen_height == ev.height)
    {
        return;
    }
    let timestamp = state.timestamp_now();
    state.randr.resize(timestamp, ev.width, ev.height);
    if let Some(root) = state.resources.window_mut(crate::resources::ROOT_WINDOW) {
        root.width = ev.width;
        root.height = ev.height;
    }
    let width = ev.width;
    let height = ev.height;
    let width_mm = u16::try_from(state.randr.width_mm).unwrap_or(u16::MAX);
    let height_mm = u16::try_from(state.randr.height_mm).unwrap_or(u16::MAX);

    // Core ConfigureNotify on root for non-RANDR-aware clients
    // selecting StructureNotifyMask. Spec-correct ordering: emit this
    // before the RANDR fanout so non-RANDR-aware clients (panels,
    // "fill the screen" apps) reflow at the same point in the event
    // stream that RANDR-aware toolkits see screen-change.
    let _dropped = crate::core_loop::fanout::emit_window_event_to_state(
        state,
        crate::resources::ROOT_WINDOW,
        0x0002_0000, // StructureNotifyMask
        |buf, seq, order| {
            x11::encode_configure_notify_event(
                buf,
                seq,
                order,
                crate::resources::ROOT_WINDOW,
                crate::resources::ROOT_WINDOW,
                x11::Geometry {
                    root: crate::resources::ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width,
                    height,
                    border_width: 0,
                    depth: 24,
                },
                false,
            );
        },
    );

    // RANDR ScreenChangeNotify / CrtcChangeNotify / OutputChangeNotify
    // fanout. Snapshot the subscribers first so the per-client mut
    // borrow on `state.clients` in the inner loop doesn't conflict.
    let subscribers: Vec<(u32, yserver_protocol::x11::ResourceId, u16)> = state
        .randr_select_masks
        .iter()
        .map(|((owner, window), mask)| (*owner, *window, *mask))
        .collect();
    let crtc = crate::randr::CRTC_ID;
    let mode = crate::randr::MODE_ID;
    let output = crate::randr::OUTPUT_ID;
    for (owner, request_window, mask) in subscribers {
        let Some(client) = state.clients.get_mut(&owner) else {
            continue;
        };
        let sequence = SequenceNumber(client.last_sequence.load(Ordering::Relaxed));
        if mask & x11randr::NOTIFY_MASK_SCREEN_CHANGE != 0 {
            let event = x11randr::encode_screen_change_notify_event(
                client.byte_order,
                RANDR_FIRST_EVENT,
                sequence,
                x11randr::ScreenChangeNotify {
                    timestamp,
                    config_timestamp: timestamp,
                    root: crate::resources::ROOT_WINDOW.0,
                    request_window: request_window.0,
                    width,
                    height,
                    width_mm,
                    height_mm,
                },
            );
            let _ = client_io::write_or_buffer(client, &event);
        }
        if mask & x11randr::NOTIFY_MASK_CRTC_CHANGE != 0 {
            let event = x11randr::encode_crtc_change_notify_event(
                client.byte_order,
                RANDR_FIRST_EVENT,
                sequence,
                x11randr::CrtcChangeNotify {
                    timestamp,
                    request_window: request_window.0,
                    crtc,
                    mode,
                    x: ev.x,
                    y: ev.y,
                    width,
                    height,
                },
            );
            let _ = client_io::write_or_buffer(client, &event);
        }
        if mask & x11randr::NOTIFY_MASK_OUTPUT_CHANGE != 0 {
            let event = x11randr::encode_output_change_notify_event(
                client.byte_order,
                RANDR_FIRST_EVENT,
                sequence,
                x11randr::OutputChangeNotify {
                    timestamp,
                    config_timestamp: timestamp,
                    request_window: request_window.0,
                    output,
                    crtc,
                    mode,
                },
            );
            let _ = client_io::write_or_buffer(client, &event);
        }
    }
}

/// I2: re-arm `WRITABLE` interest on each client's writer fd to track
/// `outbound` state. Called once per outer poll iteration so per-event
/// processing doesn't have to thread the registry through every
/// fanout helper.
/// Drain any buffered outbound, then reconcile each client's poller
/// interest with whether it still has bytes pending. Returns the ids of
/// clients whose drain attempts surfaced peer-gone errors so the caller
/// can run `process_disconnect`.
///
/// The proactive drain is load-bearing: mio uses edge-triggered epoll on
/// Linux, so when `write_or_buffer` partial-writes and buffers the tail,
/// the kernel can transition the fd writable *before* this function
/// re-registers WRITABLE interest. Without an immediate drain attempt
/// we'd register for an edge that has already passed and the buffered
/// tail would never go out — clients see truncated replies and stall.
fn reconcile_client_writable_interest(
    registry: &mio::Registry,
    state: &mut ServerState,
) -> Vec<yserver_protocol::x11::ClientId> {
    let mut to_disconnect = Vec::new();
    for (id, client) in state.clients.iter_mut() {
        if !client.outbound.is_empty() {
            match client_io::drain_outbound(client) {
                Ok(WriteOutcome::Done | WriteOutcome::WouldBlock) => {}
                Ok(WriteOutcome::Disconnect) | Err(_) => {
                    to_disconnect.push(yserver_protocol::x11::ClientId(*id));
                    continue;
                }
            }
        }
        let needs_writable = !client.outbound.is_empty();
        if needs_writable == client.watching_writable {
            continue;
        }
        let raw = std::os::fd::AsRawFd::as_raw_fd(&*client.writer.lock().unwrap());
        let interest = if needs_writable {
            Interest::READABLE | Interest::WRITABLE
        } else {
            Interest::READABLE
        };
        match registry.reregister(
            &mut SourceFd(&raw),
            client_token(yserver_protocol::x11::ClientId(*id)),
            interest,
        ) {
            Ok(()) => client.watching_writable = needs_writable,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // fd already deregistered (disconnect path); nothing
                // to track.
            }
            Err(err) => {
                warn!("reregister client {} writable interest: {err}", id);
            }
        }
    }
    to_disconnect
}

fn handle_setup_allocate(
    state: &mut ServerState,
    id: yserver_protocol::x11::ClientId,
    response_tx: crossbeam_channel::Sender<SetupAllocateResponse>,
) {
    let _ = id;
    let response = match state.id_allocator.allocate() {
        Some((base, mask)) => SetupAllocateResponse {
            resource_id_base: base,
            resource_id_mask: mask,
            screen_width_px: state.randr.screen_width,
            screen_height_px: state.randr.screen_height,
            current_input_masks: state
                .clients
                .values()
                .filter_map(|c| c.event_masks.get(&crate::resources::ROOT_WINDOW).copied())
                .fold(0u32, |a, b| a | b),
        },
        None => SetupAllocateResponse {
            resource_id_base: 0,
            resource_id_mask: 0,
            screen_width_px: 0,
            screen_height_px: 0,
            current_input_masks: 0,
        },
    };
    let _ = response_tx.send(response);
}

fn handle_host_input(state: &mut ServerState, backend: &mut dyn Backend, ev: HostInputEvent) {
    backend.on_host_input(state, ev);
}

/// Wire a freshly-completed setup handshake into the core's bookkeeping:
///   - try_clone the stream for the writer (set non-blocking on the
///     core's clone)
///   - build the (`reader_control_tx`, `reader_control_rx`) channel
///   - install a `ClientState` for `id`
///   - drop the entry from the setup-thread teardown registry (the
///     setup thread is exiting)
///   - register the writer fd with the poller (no interest yet — I2
///     re-registers `WRITABLE` only when there's pending outbound)
///   - spawn the reader thread (the only path that produces
///     `Message::Request` for this client)
#[allow(clippy::too_many_arguments)]
fn handle_client_setup_complete(
    registry: &mio::Registry,
    sender: &CoreSender,
    setup_registry: &SetupRegistry,
    state: &mut ServerState,
    id: yserver_protocol::x11::ClientId,
    stream: UnixStream,
    resource_id_base: u32,
    resource_id_mask: u32,
    byte_order: yserver_protocol::x11::ClientByteOrder,
) -> io::Result<()> {
    use std::sync::{Arc, Mutex, atomic::AtomicU16};
    let writer = stream.try_clone()?;
    writer.set_nonblocking(true)?;
    let writer_fd = writer.as_raw_fd();

    let (reader_control_tx, reader_control_rx) = crossbeam_channel::unbounded();

    state.clients.insert(
        id.0,
        crate::server::ClientState {
            writer: Arc::new(Mutex::new(writer)),
            byte_order,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base,
            resource_id_mask,
            event_masks: std::collections::HashMap::new(),
            save_set: std::collections::HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: std::collections::HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: Some(reader_control_tx),
        },
    );

    setup_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);

    // Initial interest is READABLE — mio doesn't accept empty interest.
    // I2 reregisters WRITABLE-only when `client.outbound` becomes
    // non-empty and back to READABLE when it drains. The reader thread
    // already polls the peer fd directly, so this registration's only
    // wake-up role today is the eventual WRITABLE-on-drain edge.
    registry.register(
        &mut SourceFd(&writer_fd),
        crate::core_loop::poll_tokens::client_token(id),
        Interest::READABLE,
    )?;

    const BIG_REQUESTS_MAJOR_OPCODE: u8 = 135;
    crate::core_loop::client_reader::spawn(
        id,
        stream,
        byte_order,
        BIG_REQUESTS_MAJOR_OPCODE,
        reader_control_rx,
        sender.clone_handle(),
    )?;

    Ok(())
}

/// Drain pending accepts on the listener. For each, allocate a fresh
/// `ClientId` and spawn a setup thread that does the X11 handshake.
fn accept_pending(
    listener: &UnixListener,
    client_id_allocator: &ClientIdAllocator,
    sender: &CoreSender,
    registry: &SetupRegistry,
) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let id = client_id_allocator.allocate();
                if let Err(err) =
                    setup_thread::spawn(id, stream, sender.clone_handle(), registry.clone())
                {
                    error!("setup thread spawn failed for client {}: {err}", id.0);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                warn!("accept failed: {err}");
                break;
            }
        }
    }
}

// Silence unused-import lints when the listener path is only exercised
// indirectly. Concrete uses below.
#[allow(dead_code)]
fn _hint(_: UnixStream) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_loop::sender::channel;
    use std::time::Duration;

    /// I5 test: `reconcile_client_writable_interest` toggles a client's
    /// `watching_writable` flag in lock-step with `outbound`'s emptiness,
    /// and is a no-op when nothing changed. Tests against a real
    /// `mio::Registry` so the reregister error path is also exercised.
    #[test]
    fn reconcile_writable_interest_tracks_outbound_state() {
        use crate::server::ClientState;
        use mio::{Interest, Poll, unix::SourceFd};
        use std::{
            collections::{HashMap, HashSet, VecDeque},
            io::{Read, Write},
            os::{fd::AsRawFd, unix::net::UnixStream},
            sync::{Arc, Mutex, atomic::AtomicU16},
        };
        use yserver_protocol::x11::{ClientByteOrder, ClientId as Cid};

        let poll = Poll::new().unwrap();
        // We just need a real fd registered with the poller.
        let (mut peer, writer) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let writer_arc = Arc::new(Mutex::new(writer));
        let raw = writer_arc.lock().unwrap().as_raw_fd();
        let token = client_token(Cid(7));
        poll.registry()
            .register(&mut SourceFd(&raw), token, Interest::READABLE)
            .unwrap();

        let mut state = ServerState::new();
        state.clients.insert(
            7,
            ClientState {
                writer: writer_arc,
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0,
                resource_id_mask: 0,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );

        // outbound is empty, watching_writable is false → no-op.
        let disc = reconcile_client_writable_interest(poll.registry(), &mut state);
        assert!(disc.is_empty());
        assert!(!state.clients[&7].watching_writable);

        // Outbound becomes non-empty AND the peer doesn't read → reconcile's
        // proactive drain attempt cannot empty it, so watching_writable
        // flips on.
        //
        // Fill the kernel buffer first so any drain attempt returns
        // WouldBlock instead of writing through to `peer`.
        let big = vec![0xABu8; 256 * 1024];
        let _ = state
            .clients
            .get_mut(&7)
            .unwrap()
            .writer
            .lock()
            .unwrap()
            .write(&big); // partial write fills the kernel buffer
        state
            .clients
            .get_mut(&7)
            .unwrap()
            .outbound
            .extend([1u8, 2, 3]);
        let disc = reconcile_client_writable_interest(poll.registry(), &mut state);
        assert!(disc.is_empty());
        assert!(state.clients[&7].watching_writable);

        // Peer drains → kernel buffer empties → drain succeeds inside reconcile,
        // outbound goes empty, watching_writable flips off.
        let mut sink = vec![0u8; 1024 * 1024];
        peer.set_nonblocking(true).unwrap();
        let _ = peer.read(&mut sink);
        let disc = reconcile_client_writable_interest(poll.registry(), &mut state);
        assert!(disc.is_empty());
        assert!(state.clients[&7].outbound.is_empty());
        assert!(!state.clients[&7].watching_writable);

        drop(peer);
    }

    /// E3 liveness test: back-to-back `PageFlipReady` messages each
    /// reach `Backend::on_page_flip_ready` — no dedup, no rate-limit,
    /// no message-coalescing. Codex's missing-test bullet for E3.
    #[test]
    fn back_to_back_page_flip_ready_dispatches_each_time() {
        use crate::backend::recording::RecordingBackend;
        use std::sync::atomic::Ordering;

        let (poll, sender, rx) = channel().unwrap();
        let sender_for_core = sender.clone_handle();
        let mut backend = RecordingBackend::new();
        let handle = std::thread::spawn(move || {
            let mut state = ServerState::new();
            let alloc = ClientIdAllocator::new();
            let result = run_core(
                poll,
                rx,
                sender_for_core,
                &mut state,
                &mut backend,
                None,
                &alloc,
            );
            (result, backend)
        });
        for _ in 0..3 {
            sender.send(Message::PageFlipReady).unwrap();
        }
        sender.send(Message::Shutdown).unwrap();
        for _ in 0..50 {
            if handle.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished(), "run_core did not return");
        let (result, backend) = handle.join().unwrap();
        result.unwrap();
        assert_eq!(
            backend.page_flip_count.load(Ordering::Relaxed),
            3,
            "expected 3 on_page_flip_ready dispatches",
        );
    }

    #[test]
    fn shutdown_returns() {
        use crate::{
            backend::recording::RecordingBackend, core_loop::poll_tokens::ClientIdAllocator,
        };

        let (poll, sender, rx) = channel().unwrap();
        let sender_for_core = sender.clone_handle();
        let handle = std::thread::spawn(move || {
            let mut state = ServerState::new();
            let mut backend = RecordingBackend::new();
            let alloc = ClientIdAllocator::new();
            run_core(
                poll,
                rx,
                sender_for_core,
                &mut state,
                &mut backend,
                None,
                &alloc,
            )
        });
        sender.send(Message::Shutdown).unwrap();
        // Bound the wait so a regression that fails to return does not
        // hang the test runner.
        for _ in 0..50 {
            if handle.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished(), "run_core did not return on Shutdown");
        handle.join().unwrap().unwrap();
    }
}
