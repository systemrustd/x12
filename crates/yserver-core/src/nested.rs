use std::{
    collections::HashMap,
    fs,
    io::{self, ErrorKind, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU16, AtomicU32, Ordering},
    },
    thread,
};

use log::{debug, error, info, warn};
use yserver_protocol::x11::{
    self, AtomId, ClientByteOrder, ClientId, RequestHeader, ResourceId, SequenceNumber,
    randr as x11randr,
};

use crate::{
    host_x11::{HostEvent, HostInputPump, HostInputPumpHandle, HostX11},
    resources::{
        MapState, Pixmap, ROOT_COLORMAP, ROOT_VISUAL, ROOT_WINDOW, ReparentWindowError, Window,
    },
    server::{ClientHandle, EventTarget, ServerState, fanout_event, fanout_raw_event},
};

static NEXT_CLIENT_ID: AtomicU32 = AtomicU32::new(1);

const RANDR_MAJOR_OPCODE: u8 = 128;
const RANDR_FIRST_EVENT: u8 = 89;
const RANDR_FIRST_ERROR: u8 = 147;

struct OwnedGetPropertyReply {
    format: u8,
    r#type: AtomId,
    bytes_after: u32,
    value_len: u32,
    value: Vec<u8>,
}

pub fn run(display: u16) -> io::Result<()> {
    let socket_dir = PathBuf::from("/tmp/.X11-unix");
    fs::create_dir_all(&socket_dir)?;

    let socket_path = socket_dir.join(format!("X{display}"));
    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    let listener = UnixListener::bind(&socket_path)?;
    info!("ynest listening on DISPLAY=:{display}");

    let host = match HostX11::open_from_env() {
        Ok(host) => {
            info!("host X11 container window: 0x{:x}", host.window_id());
            Some(Arc::new(Mutex::new(host)))
        }
        Err(err) => {
            error!("could not open host X11 window: {err}");
            None
        }
    };
    if let Some(host) = host.as_ref() {
        let _ = host.lock().map(|mut host| host.ping());
    }
    let host_window_id = host
        .as_ref()
        .and_then(|host| host.lock().ok().map(|host| host.window_id()));

    if let Some(window_id) = host_window_id {
        spawn_window_close_watcher(window_id);
    }

    let server = Arc::new(Mutex::new(ServerState::new()));

    let input_pump_handle: Option<HostInputPumpHandle> = match host_window_id {
        Some(window_id) => match HostInputPump::open_from_env(window_id) {
            Ok(mut pump) => {
                let handle = pump.handle();
                let server_for_thread = server.clone();
                let xid_map = handle.xid_map();
                thread::spawn(move || {
                    loop {
                        match pump.read_event() {
                            Ok(HostEvent::Key(_)) => {}
                            Ok(HostEvent::Pointer(event)) => {
                                crate::server::pointer_event_fanout(
                                    &server_for_thread,
                                    &xid_map,
                                    event,
                                );
                            }
                            Ok(HostEvent::Closed) => {
                                info!("host pump: window closed, exiting");
                                std::process::exit(0);
                            }
                            Err(err) => {
                                info!("host pump: connection lost ({err}), exiting");
                                std::process::exit(0);
                            }
                        }
                    }
                });
                Some(handle)
            }
            Err(err) => {
                warn!("could not start host input pump: {err}");
                None
            }
        },
        None => None,
    };

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let client_id = ClientId(NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed));
                let host = host.clone();
                let server = server.clone();
                let input_handle = input_pump_handle.clone();
                thread::spawn(move || {
                    if let Err(err) = handle_client(
                        client_id,
                        stream,
                        server,
                        host,
                        host_window_id,
                        input_handle,
                    ) {
                        info!("client {} disconnected: {err}", client_id.0);
                    }
                });
            }
            Err(err) => error!("accept failed: {err}"),
        }
    }

    Ok(())
}

fn lock_server(server: &Mutex<ServerState>) -> io::Result<std::sync::MutexGuard<'_, ServerState>> {
    server
        .lock()
        .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "server state poisoned"))
}

fn emit_x11_error(
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    code: u8,
    bad_value: u32,
    major_opcode: u8,
) -> io::Result<()> {
    let mut w = writer
        .lock()
        .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))?;
    x11::write_error(&mut *w, sequence, code, bad_value, 0, major_opcode)
}

fn collect_destroy_order(
    table: &crate::resources::ResourceTable,
    root: ResourceId,
    out: &mut Vec<ResourceId>,
) {
    let Some(w) = table.window(root) else {
        return;
    };
    for child in w.children.clone() {
        collect_destroy_order(table, child, out);
    }
    out.push(root);
}

struct PendingDestroy {
    window: ResourceId,
    parent: ResourceId,
    was_mapped: bool,
    host_xid: Option<u32>,
    on_window: Vec<EventTarget>,
    on_parent: Vec<EventTarget>,
}

fn fanout_destroy_sequence(pending: &PendingDestroy) {
    if pending.was_mapped {
        fanout_event(&pending.on_window, |buf, seq, order| {
            x11::encode_unmap_notify_event(buf, seq, order, pending.window, pending.window, false);
        });
        fanout_event(&pending.on_parent, |buf, seq, order| {
            x11::encode_unmap_notify_event(buf, seq, order, pending.parent, pending.window, false);
        });
    }
    fanout_event(&pending.on_window, |buf, seq, order| {
        x11::encode_destroy_notify_event(buf, seq, order, pending.window, pending.window);
    });
    fanout_event(&pending.on_parent, |buf, seq, order| {
        x11::encode_destroy_notify_event(buf, seq, order, pending.parent, pending.window);
    });
}

// `server` and `host` are Arc clones that are logically "owned" by this
// thread; `input_handle` holds a shared handle we keep alive for the session.
// Clippy pedantic flags these as needless_pass_by_value but they cannot be
// references because they are moved into the thread.
#[allow(clippy::needless_pass_by_value)]
fn handle_client(
    client_id: ClientId,
    mut stream: UnixStream,
    server: Arc<Mutex<ServerState>>,
    host: Option<Arc<Mutex<HostX11>>>,
    host_window_id: Option<u32>,
    input_handle: Option<HostInputPumpHandle>,
) -> io::Result<()> {
    let setup = x11::read_setup_request(&mut stream)?;
    if setup.byte_order != ClientByteOrder::LittleEndian {
        x11::write_setup_failed(
            &mut stream,
            setup.byte_order,
            "ynest currently supports only little-endian clients",
        )?;
        return Ok(());
    }

    let allocated = lock_server(&server)?.id_allocator.allocate();
    let Some((resource_id_base, resource_id_mask)) = allocated else {
        x11::write_setup_failed(
            &mut stream,
            setup.byte_order,
            "ynest exhausted resource ID space",
        )?;
        return Ok(());
    };

    info!(
        "client {} setup: protocol {}.{}, base=0x{:x}",
        client_id.0, setup.protocol_major, setup.protocol_minor, resource_id_base
    );

    let current_input_masks: u32 = {
        let s = lock_server(&server)?;
        s.clients
            .values()
            .filter_map(|c| c.event_masks.get(&ROOT_WINDOW).copied())
            .fold(0u32, |a, b| a | b)
    };

    x11::write_setup_success(
        &mut stream,
        x11::SetupSuccess {
            protocol_major: setup.protocol_major,
            protocol_minor: setup.protocol_minor,
            release_number: 1,
            resource_id_base,
            resource_id_mask,
            motion_buffer_size: 0,
            maximum_request_length: u16::MAX,
            image_byte_order: setup.byte_order,
            bitmap_format_bit_order: setup.byte_order,
            bitmap_format_scanline_unit: 32,
            bitmap_format_scanline_pad: 32,
            min_keycode: 8,
            max_keycode: 255,
            vendor: "yserver",
            root: x11::Screen {
                root: ROOT_WINDOW,
                default_colormap: ROOT_COLORMAP,
                white_pixel: 0x00ff_ffff,
                black_pixel: 0,
                current_input_masks,
                width_px: 800,
                height_px: 600,
                width_mm: 211,
                height_mm: 158,
                min_installed_maps: 1,
                max_installed_maps: 1,
                root_visual: ROOT_VISUAL,
                root_depth: 24,
            },
        },
    )?;

    let mut reader = stream.try_clone()?;
    let writer = Arc::new(Mutex::new(stream));
    let focused_window = Arc::new(Mutex::new(ROOT_WINDOW));
    let last_sequence = Arc::new(AtomicU16::new(0));

    {
        let mut s = lock_server(&server)?;
        s.clients.insert(
            client_id.0,
            ClientHandle {
                writer: writer.clone(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: last_sequence.clone(),
                resource_id_base,
                resource_id_mask,
                event_masks: HashMap::new(),
            },
        );
    }

    if let Some(host_window_id) = host_window_id {
        match HostInputPump::open_from_env(host_window_id) {
            Ok(keyboard) => spawn_keyboard_forwarder(
                client_id,
                keyboard,
                writer.clone(),
                focused_window.clone(),
                last_sequence.clone(),
            ),
            Err(err) => warn!("client {} keyboard forwarding disabled: {err}", client_id.0),
        }
    }

    #[allow(clippy::redundant_closure_call)]
    let result: io::Result<()> = (|| {
        let mut sequence = SequenceNumber(0);
        loop {
            let Some((header, body)) = x11::read_request(&mut reader)? else {
                return Ok(());
            };
            sequence = sequence.next();
            last_sequence.store(sequence.0, Ordering::Relaxed);
            handle_request(
                client_id,
                &server,
                host.as_ref(),
                input_handle.as_ref(),
                &writer,
                &focused_window,
                sequence,
                header,
                &body,
            )?;
        }
    })();

    let (closed_fonts, freed_pixmaps, pending_destroys) = {
        let mut s = lock_server(&server)?;
        let mut owned_roots: Vec<ResourceId> = Vec::new();
        s.resources
            .collect_owned_window_roots(client_id, &mut owned_roots);

        let mut pending: Vec<PendingDestroy> = Vec::new();
        let mut all_destroyed: Vec<ResourceId> = Vec::new();
        for root in owned_roots {
            let mut order = Vec::new();
            collect_destroy_order(&s.resources, root, &mut order);
            for w in &order {
                let (parent, was_mapped, host_xid) =
                    s.resources
                        .window(*w)
                        .map_or((ROOT_WINDOW, false, None), |win| {
                            (
                                win.parent,
                                win.map_state != MapState::Unmapped,
                                win.host_xid,
                            )
                        });
                let on_w = s.subscribers(*w, 0x0002_0000);
                let on_p = s.subscribers(parent, 0x0008_0000);
                pending.push(PendingDestroy {
                    window: *w,
                    parent,
                    was_mapped,
                    host_xid,
                    on_window: on_w,
                    on_parent: on_p,
                });
            }
            let _ = s.resources.destroy_window(root);
            all_destroyed.extend(order);
        }
        s.drop_window_subscriptions(&all_destroyed);
        let (fonts, freed_pixmaps) = s.resources.remove_non_window_resources_owned_by(client_id);
        s.clients.remove(&client_id.0);
        s.button_grabs.retain(|g| g.owner != client_id);
        if s.pointer_grab.is_some_and(|(owner, _)| owner == client_id) {
            s.pointer_grab = None;
            s.pointer_grab_is_passive = false;
            s.frozen_pointer_event = None;
        }
        (fonts, freed_pixmaps, pending)
    };
    for pending in pending_destroys {
        if let Some(xid) = pending.host_xid {
            if let Some(host) = host.as_ref()
                && let Ok(mut h) = host.lock()
            {
                let _ = h.destroy_subwindow(xid);
            }
            if let Some(input_handle) = input_handle.as_ref() {
                input_handle.unregister_top_level(xid);
            }
        }
        fanout_destroy_sequence(&pending);
    }
    if let Some(host) = host.as_ref()
        && let Ok(mut h) = host.lock()
    {
        for xid in closed_fonts {
            let _ = h.close_font(xid);
        }
        for xid in freed_pixmaps {
            let _ = h.free_pixmap(xid);
        }
    }
    result
}

fn spawn_window_close_watcher(window_id: u32) {
    thread::spawn(move || {
        debug!("window-close watcher starting for 0x{window_id:x}");
        let mut watcher = match HostInputPump::open_from_env(window_id) {
            Ok(w) => w,
            Err(err) => {
                error!("could not start window-close watcher: {err}");
                return;
            }
        };
        debug!("window-close watcher ready");
        loop {
            match watcher.read_event() {
                Ok(HostEvent::Key(_) | HostEvent::Pointer(_)) => {}
                Ok(HostEvent::Closed) => {
                    info!("host window closed, exiting");
                    std::process::exit(0);
                }
                Err(err) => {
                    info!("host connection lost ({err}), exiting");
                    std::process::exit(0);
                }
            }
        }
    });
}

fn spawn_keyboard_forwarder(
    client_id: ClientId,
    mut keyboard: HostInputPump,
    writer: Arc<Mutex<UnixStream>>,
    focused_window: Arc<Mutex<ResourceId>>,
    last_sequence: Arc<AtomicU16>,
) {
    thread::spawn(move || {
        loop {
            let event = loop {
                match keyboard.read_event() {
                    Ok(HostEvent::Key(event)) => break event,
                    Ok(HostEvent::Pointer(_)) => continue,
                    Ok(HostEvent::Closed) => {
                        info!("host window closed, exiting");
                        std::process::exit(0);
                    }
                    Err(err) => {
                        info!("host connection lost ({err}), exiting");
                        std::process::exit(0);
                    }
                }
            };
            let focus = focused_window
                .lock()
                .map(|focus| *focus)
                .unwrap_or(ROOT_WINDOW);
            if focus == ROOT_WINDOW {
                continue;
            }

            debug!(
                "client {} key {} {} -> 0x{:x}",
                client_id.0,
                if event.pressed { "press" } else { "release" },
                event.keycode,
                focus.0
            );
            let Some(mut writer) = writer.lock().ok() else {
                return;
            };
            if let Err(err) = x11::write_key_event(
                &mut *writer,
                x11::KeyEvent {
                    pressed: event.pressed,
                    keycode: event.keycode,
                    sequence: SequenceNumber(last_sequence.load(Ordering::Relaxed)),
                    time: event.time,
                    root: ROOT_WINDOW,
                    event: focus,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.event_x,
                    event_y: event.event_y,
                    state: event.state & 0x004d,
                },
            ) {
                warn!("client {} keyboard forwarding stopped: {err}", client_id.0);
                return;
            }
        }
    });
}

/// Walk every mapped descendant of `root` and send Expose to those that
/// selected ExposureMask.  Used after a top-level window becomes viewable so
/// that deeply-nested widgets (e.g. Xt ClockWidget) redraw immediately.
fn emit_expose_subtree(server: &Arc<Mutex<ServerState>>, root: ResourceId) {
    let children = match server.lock() {
        Ok(s) => s.resources.children(root).to_vec(),
        Err(_) => return,
    };
    for child in children {
        let extents = match server.lock() {
            Ok(s) => s
                .resources
                .window(child)
                .filter(|w| w.map_state == MapState::Viewable)
                .map(|w| (w.width, w.height)),
            Err(_) => None,
        };
        if let Some((w, h)) = extents {
            crate::server::emit_window_event(server, child, 0x0000_8000, |buf, seq, order| {
                x11::encode_expose_event(buf, seq, order, child, w, h);
            });
            emit_expose_subtree(server, child);
        }
    }
}

fn set_focused_window(
    focused_window: &Arc<Mutex<ResourceId>>,
    server: &Arc<Mutex<ServerState>>,
    window: ResourceId,
) -> io::Result<()> {
    if window == ResourceId(0) {
        return Ok(());
    }
    let Ok(mut focused_window) = focused_window.lock() else {
        return Ok(());
    };
    if *focused_window == window {
        return Ok(());
    }

    let prev = *focused_window;
    *focused_window = window;
    drop(focused_window);

    if prev != ROOT_WINDOW {
        crate::server::emit_window_event(server, prev, 0x0020_0000, |buf, seq, order| {
            x11::encode_focus_event(buf, seq, order, false, prev);
        });
    }
    crate::server::emit_window_event(server, window, 0x0020_0000, |buf, seq, order| {
        x11::encode_focus_event(buf, seq, order, true, window);
    });
    Ok(())
}

fn clear_extent(requested: u16, offset: i16, window_extent: u16) -> u16 {
    if requested != 0 {
        return requested;
    }

    if offset <= 0 {
        window_extent
    } else {
        window_extent.saturating_sub(offset as u16)
    }
}

fn handle_randr_request(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    minor: u8,
    body: &[u8],
) -> io::Result<()> {
    let lock_writer = || -> io::Result<std::sync::MutexGuard<'_, UnixStream>> {
        writer
            .lock()
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))
    };

    match minor {
        x11randr::RR_QUERY_VERSION => {
            let (reply_major, reply_minor) = x11randr::parse_query_version(body)
                .map(|r| {
                    let reply_major = x11randr::MAJOR_VERSION;
                    let reply_minor = if r.major < x11randr::MAJOR_VERSION {
                        r.minor
                    } else {
                        x11randr::MINOR_VERSION
                    };
                    (reply_major, reply_minor)
                })
                .unwrap_or((x11randr::MAJOR_VERSION, x11randr::MINOR_VERSION));
            debug!(
                "client {} #{} RANDR::QueryVersion -> {}.{}",
                client_id.0, sequence.0, reply_major, reply_minor
            );
            let buf = x11randr::encode_query_version_reply(sequence, reply_major, reply_minor);
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_GET_SCREEN_SIZE_RANGE => {
            debug!(
                "client {} #{} RANDR::GetScreenSizeRange",
                client_id.0, sequence.0
            );
            let (min_w, min_h, max_w, max_h) = {
                let s = lock_server(server)?;
                s.randr.screen_size_range()
            };
            let buf =
                x11randr::encode_get_screen_size_range_reply(sequence, min_w, min_h, max_w, max_h);
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_GET_SCREEN_RESOURCES | x11randr::RR_GET_SCREEN_RESOURCES_CURRENT => {
            debug!(
                "client {} #{} RANDR::GetScreenResources(Current) minor={}",
                client_id.0, sequence.0, minor
            );
            let resources = {
                let s = lock_server(server)?;
                s.randr.screen_resources_current()
            };
            let buf = x11randr::encode_get_screen_resources_current_reply(sequence, &resources);
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_GET_OUTPUT_INFO => {
            let req = match x11randr::parse_output_request(body) {
                Some(r) => r,
                None => {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_VALUE,
                        0,
                        RANDR_MAJOR_OPCODE,
                    );
                }
            };
            debug!(
                "client {} #{} RANDR::GetOutputInfo output={}",
                client_id.0, sequence.0, req.output
            );
            let info_data = {
                let s = lock_server(server)?;
                s.randr.output_info(req.output, req.config_timestamp)
            };
            let Some(info_data) = info_data else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_VALUE,
                    req.output,
                    RANDR_MAJOR_OPCODE,
                );
            };
            let crtc_ids = [crate::randr::CRTC_ID];
            let mode_ids = [crate::randr::MODE_ID];
            let buf = x11randr::encode_get_output_info_reply(
                sequence,
                &x11randr::OutputInfoReply {
                    timestamp: info_data.timestamp,
                    crtc: info_data.crtc,
                    width_mm: info_data.width_mm,
                    height_mm: info_data.height_mm,
                    connection: 0,     // Connected
                    subpixel_order: 0, // Unknown
                    crtcs: &crtc_ids,
                    modes: &mode_ids,
                    clones: &[],
                    name: b"ynest-0",
                },
            );
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_GET_CRTC_INFO => {
            let req = match x11randr::parse_crtc_request(body) {
                Some(r) => r,
                None => {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_VALUE,
                        0,
                        RANDR_MAJOR_OPCODE,
                    );
                }
            };
            debug!(
                "client {} #{} RANDR::GetCrtcInfo crtc={}",
                client_id.0, sequence.0, req.crtc
            );
            let crtc_data = {
                let s = lock_server(server)?;
                s.randr.crtc_info(req.crtc, req.config_timestamp)
            };
            let Some(crtc_data) = crtc_data else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_VALUE,
                    req.crtc,
                    RANDR_MAJOR_OPCODE,
                );
            };
            let output_ids = [crate::randr::OUTPUT_ID];
            let buf = x11randr::encode_get_crtc_info_reply(
                sequence,
                &x11randr::CrtcInfoReply {
                    timestamp: crtc_data.timestamp,
                    x: 0,
                    y: 0,
                    width: crtc_data.width,
                    height: crtc_data.height,
                    mode: crate::randr::MODE_ID,
                    rotation: 1,  // RR_Rotate_0
                    rotations: 1, // only normal rotation supported
                    outputs: &output_ids,
                    possible: &output_ids,
                },
            );
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_GET_CRTC_TRANSFORM => {
            debug!(
                "client {} #{} RANDR::GetCrtcTransform -> identity",
                client_id.0, sequence.0
            );
            let buf = x11randr::encode_get_crtc_transform_reply(sequence);
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_LIST_OUTPUT_PROPERTIES => {
            debug!(
                "client {} #{} RANDR::ListOutputProperties -> 0 props",
                client_id.0, sequence.0
            );
            let buf = x11randr::encode_list_output_properties_reply(sequence);
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_GET_PANNING => {
            debug!(
                "client {} #{} RANDR::GetPanning -> no panning",
                client_id.0, sequence.0
            );
            let timestamp = { lock_server(server)?.randr.timestamp };
            let buf = x11randr::encode_get_panning_reply(sequence, timestamp);
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_GET_OUTPUT_PRIMARY => {
            debug!(
                "client {} #{} RANDR::GetOutputPrimary -> none",
                client_id.0, sequence.0
            );
            let buf = x11randr::encode_get_output_primary_reply(sequence, 0);
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_GET_PROVIDERS => {
            debug!(
                "client {} #{} RANDR::GetProviders -> 0 providers",
                client_id.0, sequence.0
            );
            let timestamp = { lock_server(server)?.randr.timestamp };
            let buf = x11randr::encode_get_providers_reply(sequence, timestamp);
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_GET_MONITORS => {
            debug!("client {} #{} RANDR::GetMonitors", client_id.0, sequence.0);
            let (timestamp, width, height, width_mm, height_mm, name_atom) = {
                let mut s = lock_server(server)?;
                let t = s.randr.timestamp;
                let w = s.randr.screen_width;
                let h = s.randr.screen_height;
                let wmm = s.randr.width_mm;
                let hmm = s.randr.height_mm;
                let atom = s.atoms.intern("ynest-0", false).0;
                (t, w, h, wmm, hmm, atom)
            };
            let output_ids = [crate::randr::OUTPUT_ID];
            let buf = x11randr::encode_get_monitors_reply(
                sequence,
                timestamp,
                &[x11randr::MonitorInfo {
                    name: name_atom,
                    primary: true,
                    x: 0,
                    y: 0,
                    width,
                    height,
                    width_mm,
                    height_mm,
                    outputs: &output_ids,
                }],
            );
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_GET_CRTC_GAMMA_SIZE => {
            debug!(
                "client {} #{} RANDR::GetCrtcGammaSize -> size=0",
                client_id.0, sequence.0
            );
            let buf = x11randr::encode_get_crtc_gamma_size_reply(sequence, 0);
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_GET_CRTC_GAMMA => {
            debug!(
                "client {} #{} RANDR::GetCrtcGamma -> size=0",
                client_id.0, sequence.0
            );
            let buf = x11randr::encode_get_crtc_gamma_reply(sequence, 0);
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_SELECT_INPUT => {
            debug!(
                "client {} #{} RANDR::SelectInput (accepted, not stored)",
                client_id.0, sequence.0
            );
            // TODO: store event masks when RRScreenChangeNotify is implemented
            Ok(())
        }
        x11randr::RR_SET_SCREEN_CONFIG | x11randr::RR_SET_CRTC_CONFIG => {
            debug!(
                "client {} #{} RANDR::SetConfig minor={} -> BadValue (read-only)",
                client_id.0, sequence.0, minor
            );
            emit_x11_error(
                writer,
                sequence,
                x11::error::BAD_VALUE,
                0,
                RANDR_MAJOR_OPCODE,
            )
        }
        other => {
            debug!(
                "client {} #{} RANDR::unknown minor={}",
                client_id.0, sequence.0, other
            );
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn handle_request(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
    input_handle: Option<&HostInputPumpHandle>,
    writer: &Arc<Mutex<UnixStream>>,
    focused_window: &Arc<Mutex<ResourceId>>,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<()> {
    let lock_writer = || -> io::Result<std::sync::MutexGuard<'_, UnixStream>> {
        writer
            .lock()
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))
    };
    match header.opcode {
        1 => {
            if let Some(request) = x11::create_window_request(header.data, body) {
                debug!(
                    "client {} create window 0x{:x} parent=0x{:x} mask=0x{:x}",
                    client_id.0,
                    request.window.0,
                    request.parent.0,
                    request.event_mask.unwrap_or(0)
                );
                let new_id = request.window.0;
                let mask = request.event_mask.unwrap_or(0);
                let window_id = request.window;
                let parent = request.parent;
                let geometry = (request.x, request.y, request.width, request.height);
                let validation_failed = {
                    let s = lock_server(server)?;
                    let handle = s.clients.get(&client_id.0).expect("client registered");
                    let owned = crate::server::IdAllocator::validate_owned(
                        new_id,
                        handle.resource_id_base,
                        handle.resource_id_mask,
                    );
                    let in_use = s.resources.any_resource_exists(request.window);
                    !owned || in_use
                };
                if validation_failed {
                    return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 1);
                }
                {
                    let mut s = lock_server(server)?;
                    s.resources.create_window(client_id, request);
                    if mask != 0 {
                        s.clients
                            .get_mut(&client_id.0)
                            .expect("client registered")
                            .event_masks
                            .insert(window_id, mask);
                    }
                }
                // Top-level only: allocate host xid + create host subwindow + register.
                if parent == ROOT_WINDOW
                    && let Some(host) = host
                {
                    let allocated_xid: Option<u32> = host.lock().ok().and_then(|mut h| {
                        let xid = h.allocate_xid();
                        if let Err(err) =
                            h.create_subwindow(xid, geometry.0, geometry.1, geometry.2, geometry.3)
                        {
                            warn!(
                                "client {} create_subwindow for 0x{:x} failed: {err}",
                                client_id.0, new_id
                            );
                            return None;
                        }
                        Some(xid)
                    });

                    if let Some(host_xid) = allocated_xid {
                        {
                            let mut s = lock_server(server)?;
                            if let Some(w) = s.resources.window_mut(window_id) {
                                w.host_xid = Some(host_xid);
                            }
                        }
                        if let Some(input_handle) = input_handle
                            && let Err(err) = input_handle.register_top_level(window_id, host_xid)
                        {
                            warn!(
                                "client {} register_top_level for 0x{:x} failed: {err}",
                                client_id.0, new_id
                            );
                        }
                    }
                }
                let wants_focus = {
                    let s = lock_server(server)?;
                    let mask = s
                        .clients
                        .get(&client_id.0)
                        .and_then(|c| c.event_masks.get(&window_id).copied())
                        .unwrap_or(0);
                    let viewable = s
                        .resources
                        .window(window_id)
                        .is_some_and(|w| w.map_state == MapState::Viewable);
                    viewable && (mask & 0x3) != 0
                };
                if wants_focus {
                    set_focused_window(focused_window, server, window_id)?;
                }
            }
            log_void(client_id, sequence, "CreateWindow")
        }
        2 => {
            if let Some(request) = x11::change_window_attributes_request(body) {
                if let Some(event_mask) = request.event_mask {
                    debug!(
                        "client {} attrs window 0x{:x} mask=0x{:x}",
                        client_id.0, request.window.0, event_mask
                    );
                }
                let target_window = request.window;
                let want_focus_check;
                let viewable;
                {
                    let mut s = lock_server(server)?;
                    if let Some(event_mask) = request.event_mask {
                        let entry = s.clients.get_mut(&client_id.0).expect("client registered");
                        if event_mask == 0 {
                            entry.event_masks.remove(&target_window);
                        } else {
                            entry.event_masks.insert(target_window, event_mask);
                        }
                    }
                    s.resources.change_window_attributes(request);
                    want_focus_check = s
                        .clients
                        .get(&client_id.0)
                        .and_then(|c| c.event_masks.get(&target_window).copied())
                        .unwrap_or(0);
                    viewable = s
                        .resources
                        .window(target_window)
                        .is_some_and(|w| w.map_state == MapState::Viewable);
                }
                if viewable && want_focus_check & 0x3 != 0 {
                    set_focused_window(focused_window, server, target_window)?;
                }
            }
            log_void(client_id, sequence, "ChangeWindowAttributes")
        }
        3 => {
            log_reply(client_id, sequence, "GetWindowAttributes");
            let attrs = {
                let s = lock_server(server)?;
                let id = x11::drawable_request_id(body).unwrap_or(ROOT_WINDOW);
                let target = if s.resources.window(id).is_some() {
                    id
                } else {
                    ROOT_WINDOW
                };
                let your_event_mask = s
                    .clients
                    .get(&client_id.0)
                    .and_then(|c| c.event_masks.get(&target).copied())
                    .unwrap_or(0);
                let all_event_masks: u32 = s
                    .clients
                    .values()
                    .filter_map(|c| c.event_masks.get(&target).copied())
                    .fold(0u32, |a, b| a | b);
                window_attributes(s.resources.window(target), all_event_masks, your_event_mask)
            };
            x11::write_get_window_attributes_reply(&mut *lock_writer()?, sequence, attrs)
        }
        4 => {
            if let Some(window) = x11::free_resource_id(body) {
                let pending = {
                    let mut s = lock_server(server)?;
                    let mut order = Vec::new();
                    collect_destroy_order(&s.resources, window, &mut order);
                    let mut pending: Vec<PendingDestroy> = Vec::new();
                    for w in &order {
                        let (parent, was_mapped, host_xid) =
                            s.resources
                                .window(*w)
                                .map_or((ROOT_WINDOW, false, None), |win| {
                                    (
                                        win.parent,
                                        win.map_state != MapState::Unmapped,
                                        win.host_xid,
                                    )
                                });
                        let on_window = s.subscribers(*w, 0x0002_0000); // StructureNotify
                        let on_parent = s.subscribers(parent, 0x0008_0000); // SubstructureNotify
                        pending.push(PendingDestroy {
                            window: *w,
                            parent,
                            was_mapped,
                            host_xid,
                            on_window,
                            on_parent,
                        });
                    }
                    let _ = s.resources.destroy_window(window);
                    s.drop_window_subscriptions(&order);
                    pending
                };
                for pending in pending {
                    if let Some(xid) = pending.host_xid {
                        if let Some(host) = host
                            && let Ok(mut h) = host.lock()
                        {
                            let _ = h.destroy_subwindow(xid);
                        }
                        if let Some(input_handle) = input_handle {
                            input_handle.unregister_top_level(xid);
                        }
                    }
                    fanout_destroy_sequence(&pending);
                }
            }
            log_void(client_id, sequence, "DestroyWindow")
        }
        7 => {
            if let Some(request) = x11::reparent_window_request(body) {
                let snapshot = {
                    let mut s = lock_server(server)?;
                    match s.resources.reparent_window(request) {
                        Ok(result) => {
                            let on_window = s.subscribers(result.window, 0x0002_0000);
                            let on_old_parent = s.subscribers(result.old_parent, 0x0008_0000);
                            let on_new_parent = if result.old_parent == result.new_parent {
                                Vec::new()
                            } else {
                                s.subscribers(result.new_parent, 0x0008_0000)
                            };
                            Ok((result, on_window, on_old_parent, on_new_parent))
                        }
                        Err(ReparentWindowError::BadWindow) => {
                            Err((x11::error::BAD_WINDOW, request.window.0))
                        }
                        Err(ReparentWindowError::BadMatch) => {
                            Err((x11::error::BAD_MATCH, request.window.0))
                        }
                    }
                };
                let (result, on_window, on_old_parent, on_new_parent) = match snapshot {
                    Ok(snapshot) => snapshot,
                    Err((code, bad_value)) => {
                        return emit_x11_error(writer, sequence, code, bad_value, 7);
                    }
                };
                if let Some(xid) = result.host_xid {
                    if result.new_parent == ROOT_WINDOW {
                        // Window moved back to root: reposition its host subwindow.
                        if let Some(host) = host
                            && let Ok(mut h) = host.lock()
                        {
                            let _ = h.configure_subwindow(
                                xid,
                                Some(result.x),
                                Some(result.y),
                                None,
                                None,
                            );
                        }
                    } else if result.old_parent == ROOT_WINDOW {
                        // Window moved away from root: its host subwindow is stale.
                        // top_level_host_target will route drawing through the new top-level.
                        if let Some(host) = host
                            && let Ok(mut h) = host.lock()
                        {
                            let _ = h.destroy_subwindow(xid);
                        }
                        if let Some(input_handle) = input_handle {
                            input_handle.unregister_top_level(xid);
                        }
                    }
                }
                fanout_event(&on_window, |buf, seq, order| {
                    x11::encode_reparent_notify_event(
                        buf,
                        seq,
                        order,
                        result.window,
                        result.window,
                        result.new_parent,
                        result.x,
                        result.y,
                        result.override_redirect,
                    );
                });
                fanout_event(&on_old_parent, |buf, seq, order| {
                    x11::encode_reparent_notify_event(
                        buf,
                        seq,
                        order,
                        result.old_parent,
                        result.window,
                        result.new_parent,
                        result.x,
                        result.y,
                        result.override_redirect,
                    );
                });
                fanout_event(&on_new_parent, |buf, seq, order| {
                    x11::encode_reparent_notify_event(
                        buf,
                        seq,
                        order,
                        result.new_parent,
                        result.window,
                        result.new_parent,
                        result.x,
                        result.y,
                        result.override_redirect,
                    );
                });
            }
            log_void(client_id, sequence, "ReparentWindow")
        }
        8 => {
            if let Some(window) = x11::map_window_id(body) {
                // Check for SubstructureRedirect before mapping.
                let pre = {
                    let s = lock_server(server)?;
                    let win = s.resources.window(window);
                    win.map(|w| (w.parent, w.override_redirect))
                };
                if let Some((parent, override_redirect)) = pre {
                    // Redirect to WM only when: not override_redirect, and a DIFFERENT client
                    // (not the requester) holds SubstructureRedirectMask on the parent.
                    let redirect_targets = if !override_redirect {
                        let s = lock_server(server)?;
                        let requester_has = s
                            .clients
                            .get(&client_id.0)
                            .and_then(|c| c.event_masks.get(&parent).copied())
                            .map_or(false, |m| m & 0x0010_0000 != 0);
                        if requester_has {
                            Vec::new()
                        } else {
                            s.subscribers(parent, 0x0010_0000)
                        }
                    } else {
                        Vec::new()
                    };
                    if !redirect_targets.is_empty() {
                        // A WM holds SubstructureRedirect on the parent: send MapRequest instead.
                        fanout_event(&redirect_targets, |buf, seq, order| {
                            x11::encode_map_request_event(buf, seq, order, parent, window);
                        });
                    } else {
                        let (map_info, host_xid) = {
                            let mut s = lock_server(server)?;
                            let _ = s.resources.map_window(window);
                            let host_xid = s.resources.window(window).and_then(|w| w.host_xid);
                            let map_info = s
                                .resources
                                .window(window)
                                .map(|w| (w.parent, w.override_redirect, w.width, w.height));
                            (map_info, host_xid)
                        };
                        if let Some(xid) = host_xid
                            && let Some(host) = host
                            && let Ok(mut h) = host.lock()
                        {
                            let _ = h.map_subwindow(xid);
                        }
                        let wants_focus = {
                            let s = lock_server(server)?;
                            let mask = s
                                .clients
                                .get(&client_id.0)
                                .and_then(|c| c.event_masks.get(&window).copied())
                                .unwrap_or(0);
                            let viewable = s
                                .resources
                                .window(window)
                                .is_some_and(|w| w.map_state == MapState::Viewable);
                            viewable && (mask & 0x3) != 0
                        };
                        if wants_focus {
                            debug!("focus key window 0x{:x}", window.0);
                            set_focused_window(focused_window, server, window)?;
                        }
                        if let Some((_parent, override_redir, width, height)) = map_info {
                            crate::server::emit_window_event(
                                server,
                                window,
                                0x0002_0000,
                                |buf, seq, order| {
                                    x11::encode_map_notify_event(
                                        buf,
                                        seq,
                                        order,
                                        window,
                                        window,
                                        override_redir,
                                    );
                                },
                            );
                            crate::server::emit_window_event(
                                server,
                                window,
                                0x0000_8000,
                                |buf, seq, order| {
                                    x11::encode_expose_event(
                                        buf, seq, order, window, width, height,
                                    );
                                },
                            );
                            // Descendants that were already mapped (e.g. Xt widget children)
                            // are now viewable; send them Expose so they redraw immediately.
                            if host_xid.is_some() {
                                emit_expose_subtree(server, window);
                            }
                        }
                    }
                }
            }
            log_void(client_id, sequence, "MapWindow")
        }
        9 => {
            if let Some(parent) = x11::map_window_id(body) {
                let children = {
                    let s = lock_server(server)?;
                    s.resources.children(parent).to_vec()
                };
                for child in children {
                    let (extents, host_xid, was_unmapped, override_redirect) = {
                        let mut s = lock_server(server)?;
                        let was_unmapped = s.resources.map_window(child);
                        let host_xid = s.resources.window(child).and_then(|w| w.host_xid);
                        let extents = s.resources.window(child).map(|w| (w.width, w.height));
                        let override_redirect = s
                            .resources
                            .window(child)
                            .is_some_and(|w| w.override_redirect);
                        (extents, host_xid, was_unmapped, override_redirect)
                    };
                    if let Some(xid) = host_xid
                        && let Some(host) = host
                        && let Ok(mut h) = host.lock()
                    {
                        let _ = h.map_subwindow(xid);
                    }
                    let wants_focus = {
                        let s = lock_server(server)?;
                        let mask = s
                            .clients
                            .get(&client_id.0)
                            .and_then(|c| c.event_masks.get(&child).copied())
                            .unwrap_or(0);
                        let viewable = s
                            .resources
                            .window(child)
                            .is_some_and(|w| w.map_state == MapState::Viewable);
                        viewable && (mask & 0x3) != 0
                    };
                    if wants_focus {
                        debug!("focus key window 0x{:x}", child.0);
                        set_focused_window(focused_window, server, child)?;
                    }
                    if was_unmapped {
                        crate::server::emit_window_event(
                            server,
                            child,
                            0x0002_0000,
                            |buf, seq, order| {
                                x11::encode_map_notify_event(
                                    buf,
                                    seq,
                                    order,
                                    child,
                                    child,
                                    override_redirect,
                                );
                            },
                        );
                    }
                    if let Some((width, height)) = extents {
                        crate::server::emit_window_event(
                            server,
                            child,
                            0x0000_8000,
                            |buf, seq, order| {
                                x11::encode_expose_event(buf, seq, order, child, width, height);
                            },
                        );
                    }
                }
            }
            log_void(client_id, sequence, "MapSubwindows")
        }
        10 => {
            if let Some(window) = x11::map_window_id(body) {
                let (snapshot, host_xid) = {
                    let mut s = lock_server(server)?;
                    let host_xid = s.resources.window(window).and_then(|w| w.host_xid);
                    let was_mapped = s.resources.unmap_window(window);
                    let snapshot = if was_mapped {
                        let parent = s.resources.window(window).map_or(ROOT_WINDOW, |w| w.parent);
                        let on_window = s.subscribers(window, 0x0002_0000); // StructureNotify
                        let on_parent = s.subscribers(parent, 0x0008_0000); // SubstructureNotify
                        Some((parent, on_window, on_parent))
                    } else {
                        None
                    };
                    (snapshot, host_xid)
                };
                if let Some(xid) = host_xid
                    && let Some(host) = host
                    && let Ok(mut h) = host.lock()
                {
                    let _ = h.unmap_subwindow(xid);
                }
                if let Some((parent, on_window, on_parent)) = snapshot {
                    fanout_event(&on_window, |buf, seq, order| {
                        x11::encode_unmap_notify_event(buf, seq, order, window, window, false);
                    });
                    fanout_event(&on_parent, |buf, seq, order| {
                        x11::encode_unmap_notify_event(buf, seq, order, parent, window, false);
                    });
                }
            }
            log_void(client_id, sequence, "UnmapWindow")
        }
        11 => {
            if let Some(parent) = x11::map_window_id(body) {
                struct PendingUnmap {
                    child: ResourceId,
                    host_xid: Option<u32>,
                    on_child: Vec<EventTarget>,
                    on_parent: Vec<EventTarget>,
                }
                let pending = {
                    let mut s = lock_server(server)?;
                    let Some(children) = s.resources.mapped_children_bottom_to_top(parent) else {
                        return emit_x11_error(
                            writer,
                            sequence,
                            x11::error::BAD_WINDOW,
                            parent.0,
                            11,
                        );
                    };
                    let mut pending = Vec::new();
                    for child in children {
                        let host_xid = s.resources.window(child).and_then(|w| w.host_xid);
                        if s.resources.unmap_window(child) {
                            pending.push(PendingUnmap {
                                child,
                                host_xid,
                                on_child: s.subscribers(child, 0x0002_0000),
                                on_parent: s.subscribers(parent, 0x0008_0000),
                            });
                        }
                    }
                    pending
                };
                for item in pending {
                    if let Some(xid) = item.host_xid
                        && let Some(host) = host
                        && let Ok(mut h) = host.lock()
                    {
                        let _ = h.unmap_subwindow(xid);
                    }
                    fanout_event(&item.on_child, |buf, seq, order| {
                        x11::encode_unmap_notify_event(
                            buf, seq, order, item.child, item.child, false,
                        );
                    });
                    fanout_event(&item.on_parent, |buf, seq, order| {
                        x11::encode_unmap_notify_event(buf, seq, order, parent, item.child, false);
                    });
                }
            }
            log_void(client_id, sequence, "UnmapSubwindows")
        }
        12 => {
            if let Some(request) = x11::configure_window_request(body) {
                // Check for SubstructureRedirect on the window's parent.
                let pre = {
                    let s = lock_server(server)?;
                    s.resources
                        .window(request.window)
                        .map(|w| (w.parent, w.override_redirect))
                };
                let redirect_targets = if let Some((parent, false)) = pre {
                    let s = lock_server(server)?;
                    let requester_has = s
                        .clients
                        .get(&client_id.0)
                        .and_then(|c| c.event_masks.get(&parent).copied())
                        .map_or(false, |m| m & 0x0010_0000 != 0);
                    if requester_has {
                        Vec::new()
                    } else {
                        s.subscribers(parent, 0x0010_0000)
                    }
                } else {
                    Vec::new()
                };
                if !redirect_targets.is_empty() {
                    // WM holds SubstructureRedirect: forward as ConfigureRequest.
                    let parent = pre.map(|(p, _)| p).unwrap_or(ROOT_WINDOW);
                    fanout_event(&redirect_targets, |buf, seq, order| {
                        x11::encode_configure_request_event(
                            buf,
                            seq,
                            order,
                            parent,
                            request.window,
                            &request,
                        );
                    });
                } else {
                    let (configure, host_xid) = {
                        let mut s = lock_server(server)?;
                        let configure = s
                            .resources
                            .configure_window(request)
                            .map(|w| (w.id, window_geometry(w), w.override_redirect));
                        let host_xid = configure.as_ref().and_then(|(id, _, _)| {
                            s.resources.window(*id).and_then(|w| w.host_xid)
                        });
                        (configure, host_xid)
                    };
                    if let Some(xid) = host_xid
                        && let Some(host) = host
                        && let Ok(mut h) = host.lock()
                    {
                        let _ = h.configure_subwindow(
                            xid,
                            request.x,
                            request.y,
                            request.width,
                            request.height,
                        );
                    }
                    if let Some((window_id, geometry, override_redirect)) = configure {
                        crate::server::emit_window_event(
                            server,
                            window_id,
                            0x0002_0000,
                            |buf, seq, order| {
                                x11::encode_configure_notify_event(
                                    buf,
                                    seq,
                                    order,
                                    window_id,
                                    window_id,
                                    geometry,
                                    override_redirect,
                                );
                            },
                        );
                    }
                }
            }
            log_void(client_id, sequence, "ConfigureWindow")
        }
        14 => {
            log_reply(client_id, sequence, "GetGeometry");
            let geometry = {
                let s = lock_server(server)?;
                let drawable = x11::drawable_request_id(body).unwrap_or(ROOT_WINDOW);
                s.resources
                    .window(drawable)
                    .map(window_geometry)
                    .or_else(|| s.resources.pixmap(drawable).map(pixmap_geometry))
                    .unwrap_or_else(|| {
                        window_geometry(
                            s.resources.window(ROOT_WINDOW).expect("root window exists"),
                        )
                    })
            };
            x11::write_get_geometry_reply(&mut *lock_writer()?, sequence, geometry)
        }
        15 => {
            log_reply(client_id, sequence, "QueryTree");
            let (parent, children) = {
                let s = lock_server(server)?;
                let window = x11::drawable_request_id(body).unwrap_or(ROOT_WINDOW);
                let window_state = s
                    .resources
                    .window(window)
                    .or_else(|| s.resources.window(ROOT_WINDOW))
                    .expect("root window exists");
                (window_state.parent, window_state.children.clone())
            };
            x11::write_query_tree_reply(
                &mut *lock_writer()?,
                sequence,
                ROOT_WINDOW,
                parent,
                &children,
            )
        }
        16 => {
            let name = x11::intern_atom_name(body);
            let atom = {
                let mut s = lock_server(server)?;
                s.atoms.intern(&name, header.data != 0)
            };
            debug!(
                "client {} #{} InternAtom {:?} -> {}",
                client_id.0, sequence.0, name, atom.0
            );
            x11::write_intern_atom_reply(&mut *lock_writer()?, sequence, atom)
        }
        17 => {
            let atom = x11::request_atom(body);
            let name = {
                let s = lock_server(server)?;
                s.atoms.name(atom).unwrap_or("UNKNOWN").to_owned()
            };
            debug!(
                "client {} #{} GetAtomName {} -> {:?}",
                client_id.0, sequence.0, atom.0, name
            );
            x11::write_get_atom_name_reply(&mut *lock_writer()?, sequence, &name)
        }
        18 => {
            let Some(req) = x11::change_property_request(header.data, body) else {
                return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, 0, 18);
            };

            let Some(mode) = crate::properties::ChangeMode::from_protocol(req.mode) else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(req.mode),
                    18,
                );
            };
            let Some(format) = crate::properties::PropertyFormat::from_protocol(req.format) else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(req.format),
                    18,
                );
            };
            let expected_bytes = (req.length as usize).checked_mul(format.bytes());
            if expected_bytes != Some(req.data.len()) {
                return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, 0, 18);
            }

            let (timestamp, subscribers) = {
                let mut s = lock_server(server)?;
                if s.resources.window(req.window).is_none() {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_WINDOW,
                        req.window.0,
                        18,
                    );
                }
                if !s.atoms.exists(req.property) {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ATOM,
                        req.property.0,
                        18,
                    );
                }
                if !s.atoms.exists(req.r#type) {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ATOM,
                        req.r#type.0,
                        18,
                    );
                }
                let existing = s
                    .resources
                    .window_property(req.window, req.property)
                    .cloned();
                let new_value = match crate::properties::apply_change(
                    existing.as_ref(),
                    mode,
                    req.r#type,
                    format,
                    &req.data,
                ) {
                    Ok(v) => v,
                    Err(crate::properties::ChangePropertyError::BadMatch) => {
                        drop(s);
                        return emit_x11_error(
                            writer,
                            sequence,
                            x11::error::BAD_MATCH,
                            req.window.0,
                            18,
                        );
                    }
                    Err(crate::properties::ChangePropertyError::BadAlloc) => {
                        drop(s);
                        return emit_x11_error(writer, sequence, x11::error::BAD_ALLOC, 0, 18);
                    }
                    Err(crate::properties::ChangePropertyError::BadValue) => {
                        drop(s);
                        return emit_x11_error(writer, sequence, x11::error::BAD_VALUE, 0, 18);
                    }
                };
                s.resources
                    .set_window_property(req.window, req.property, new_value);
                let timestamp = s.timestamp_now();
                let subs = s.subscribers(req.window, 0x0040_0000);
                (timestamp, subs)
            };

            for target in subscribers {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_property_notify_event(
                    &mut buf,
                    seq,
                    target.byte_order,
                    req.window,
                    req.property,
                    timestamp,
                    false,
                );
                if let Ok(mut w) = target.writer.lock() {
                    let _ = w.write_all(&buf);
                }
            }
            Ok(())
        }
        19 => {
            let Some(req) = x11::delete_property_request(body) else {
                return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, 0, 19);
            };
            let (existed, timestamp, subscribers) = {
                let mut s = lock_server(server)?;
                if s.resources.window(req.window).is_none() {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_WINDOW,
                        req.window.0,
                        19,
                    );
                }
                if !s.atoms.exists(req.property) {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ATOM,
                        req.property.0,
                        19,
                    );
                }
                let existed = s
                    .resources
                    .delete_window_property(req.window, req.property)
                    .is_some();
                let timestamp = s.timestamp_now();
                let subs = if existed {
                    s.subscribers(req.window, 0x0040_0000)
                } else {
                    Vec::new()
                };
                (existed, timestamp, subs)
            };
            if existed {
                for target in subscribers {
                    let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                    let mut buf = Vec::with_capacity(32);
                    x11::encode_property_notify_event(
                        &mut buf,
                        seq,
                        target.byte_order,
                        req.window,
                        req.property,
                        timestamp,
                        true,
                    );
                    if let Ok(mut w) = target.writer.lock() {
                        let _ = w.write_all(&buf);
                    }
                }
            }
            Ok(())
        }
        20 => {
            let Some(req) = x11::get_property_request(header.data, body) else {
                return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, 0, 20);
            };
            let (reply_owned, delete_subscribers, timestamp) = {
                let mut s = lock_server(server)?;
                if s.resources.window(req.window).is_none() {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_WINDOW,
                        req.window.0,
                        20,
                    );
                }
                if !s.atoms.exists(req.property) {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ATOM,
                        req.property.0,
                        20,
                    );
                }
                if req.r#type.0 != 0 && !s.atoms.exists(req.r#type) {
                    drop(s);
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ATOM,
                        req.r#type.0,
                        20,
                    );
                }
                let existing = s
                    .resources
                    .window_property(req.window, req.property)
                    .cloned();
                let slice = match crate::properties::slice_for_get(
                    existing.as_ref(),
                    req.r#type,
                    req.long_offset,
                    req.long_length,
                ) {
                    Ok(s) => s,
                    Err(crate::properties::ChangePropertyError::BadValue) => {
                        drop(s);
                        return emit_x11_error(
                            writer,
                            sequence,
                            x11::error::BAD_VALUE,
                            req.long_offset,
                            20,
                        );
                    }
                    Err(_) => unreachable!("slice_for_get only returns BadValue on error"),
                };
                let value_len_units = if slice.format == 0 {
                    0
                } else {
                    slice.value.len() as u32 / u32::from(slice.format / 8)
                };
                let owned = OwnedGetPropertyReply {
                    format: slice.format,
                    r#type: slice.r#type,
                    bytes_after: slice.bytes_after,
                    value_len: value_len_units,
                    value: slice.value.to_vec(),
                };

                // Decide whether `delete=1` actually fires.
                let type_matched = existing
                    .as_ref()
                    .is_some_and(|p| req.r#type.0 == 0 || req.r#type == p.r#type);
                let mut subs = Vec::new();
                let mut timestamp = 0u32;
                if req.delete && type_matched && slice.bytes_after == 0 && existing.is_some() {
                    s.resources.delete_window_property(req.window, req.property);
                    timestamp = s.timestamp_now();
                    subs = s.subscribers(req.window, 0x0040_0000);
                }
                (owned, subs, timestamp)
            };

            {
                let mut w = lock_writer()?;
                x11::write_get_property_reply(
                    &mut *w,
                    sequence,
                    x11::GetPropertyReply {
                        format: reply_owned.format,
                        r#type: reply_owned.r#type,
                        bytes_after: reply_owned.bytes_after,
                        value_len: reply_owned.value_len,
                        value: &reply_owned.value,
                    },
                )?;
            }
            for target in delete_subscribers {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_property_notify_event(
                    &mut buf,
                    seq,
                    target.byte_order,
                    req.window,
                    req.property,
                    timestamp,
                    true,
                );
                if let Ok(mut w) = target.writer.lock() {
                    let _ = w.write_all(&buf);
                }
            }
            Ok(())
        }
        22 => {
            // SetSelectionOwner: window(4) selection(4) time(4)
            if body.len() >= 8 {
                let window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let selection = AtomId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
                let name = {
                    let mut s = lock_server(server)?;
                    if window.0 == 0 {
                        s.selections.remove(&selection);
                    } else {
                        s.selections.insert(selection, window);
                    }
                    s.atoms.name(selection).map(str::to_owned)
                };
                debug!(
                    "client {} #{} SetSelectionOwner {} -> 0x{:x}",
                    client_id.0,
                    sequence.0,
                    name.as_deref().unwrap_or("?"),
                    window.0
                );
            } else {
                debug!(
                    "client {} #{} SetSelectionOwner (short body)",
                    client_id.0, sequence.0
                );
            }
            Ok(())
        }
        23 => {
            // GetSelectionOwner: selection(4)
            let owner = if body.len() >= 4 {
                let selection = AtomId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let s = lock_server(server)?;
                s.selections
                    .get(&selection)
                    .copied()
                    .unwrap_or(ResourceId(0))
            } else {
                ResourceId(0)
            };
            debug!(
                "client {} #{} GetSelectionOwner -> 0x{:x}",
                client_id.0, sequence.0, owner.0
            );
            x11::write_get_selection_owner_reply(&mut *lock_writer()?, sequence, owner)
        }
        25 => {
            if let Some(req) = x11::send_event_request(header.data, body) {
                let event_type = req.event[0] & 0x7f;
                if event_type != 33 {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_VALUE,
                        u32::from(event_type),
                        25,
                    );
                }
                let format = req.event[1];
                if !matches!(format, 8 | 16 | 32) {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_VALUE,
                        u32::from(format),
                        25,
                    );
                }
                let message_window = ResourceId(u32::from_le_bytes(
                    req.event[4..8].try_into().expect("fixed slice"),
                ));
                let message_type = AtomId(u32::from_le_bytes(
                    req.event[8..12].try_into().expect("fixed slice"),
                ));

                let targets = {
                    let s = lock_server(server)?;
                    if s.resources.window(req.destination).is_none() {
                        return emit_x11_error(
                            writer,
                            sequence,
                            x11::error::BAD_WINDOW,
                            req.destination.0,
                            25,
                        );
                    }
                    if message_window != req.destination
                        && s.resources.window(message_window).is_none()
                    {
                        return emit_x11_error(
                            writer,
                            sequence,
                            x11::error::BAD_WINDOW,
                            message_window.0,
                            25,
                        );
                    }
                    if !s.atoms.exists(message_type) {
                        return emit_x11_error(
                            writer,
                            sequence,
                            x11::error::BAD_ATOM,
                            message_type.0,
                            25,
                        );
                    }

                    if req.event_mask == 0 {
                        s.resources
                            .window_owner(req.destination)
                            .and_then(|owner| s.client_target(owner))
                            .into_iter()
                            .collect::<Vec<_>>()
                    } else {
                        let mut current = req.destination;
                        loop {
                            let targets = s.subscribers_intersecting(current, req.event_mask);
                            if !targets.is_empty() || !req.propagate {
                                break targets;
                            }
                            let Some(parent) = s.resources.parent_of(current) else {
                                break Vec::new();
                            };
                            if parent == current {
                                break Vec::new();
                            }
                            current = parent;
                        }
                    }
                };

                let mut event = *req.event;
                event[0] |= 0x80;
                fanout_raw_event(&targets, &event);
            }
            log_void(client_id, sequence, "SendEvent")
        }
        26 => {
            if body.len() >= 4 {
                let grab_window =
                    ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let mut s = lock_server(server)?;
                s.pointer_grab = Some((client_id, grab_window));
                s.pointer_grab_is_passive = false;
            }
            log_reply(client_id, sequence, "GrabPointer");
            x11::write_grab_reply(&mut *lock_writer()?, sequence, 0)
        }
        27 => {
            let mut s = lock_server(server)?;
            s.pointer_grab = None;
            s.pointer_grab_is_passive = false;
            s.frozen_pointer_event = None;
            drop(s);
            log_void(client_id, sequence, "UngrabPointer")
        }
        28 => {
            if body.len() >= 20 {
                let button = if body.len() >= 17 { body[16] } else { 0 };
                let grab_window =
                    ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let event_mask = u32::from(u16::from_le_bytes([body[4], body[5]]));
                let pointer_mode = body[6];
                let modifiers = u16::from_le_bytes([body[18], body[19]]);
                let mut s = lock_server(server)?;
                s.button_grabs.retain(|g| {
                    !(g.owner == client_id
                        && g.grab_window == grab_window
                        && g.button == button
                        && g.modifiers == modifiers)
                });
                s.button_grabs.push(crate::server::PassiveButtonGrab {
                    owner: client_id,
                    grab_window,
                    button,
                    modifiers,
                    event_mask,
                    pointer_mode,
                });
                debug!(
                    "client {} GrabButton window=0x{:x} button={} modifiers=0x{:x}",
                    client_id.0, grab_window.0, button, modifiers
                );
            }
            log_void(client_id, sequence, "GrabButton")
        }
        29 => {
            if body.len() >= 6 {
                let button = header.data;
                let grab_window =
                    ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let modifiers = u16::from_le_bytes([body[4], body[5]]);
                let mut s = lock_server(server)?;
                s.button_grabs.retain(|g| {
                    !(g.owner == client_id
                        && g.grab_window == grab_window
                        && (g.button == button || button == 0)
                        && (g.modifiers == modifiers || modifiers == 0x8000))
                });
            }
            log_void(client_id, sequence, "UngrabButton")
        }
        31 => {
            log_reply(client_id, sequence, "GrabKeyboard");
            x11::write_grab_reply(&mut *lock_writer()?, sequence, 0)
        }
        32 => log_void(client_id, sequence, "UngrabKeyboard"),
        33 => log_void(client_id, sequence, "GrabKey"),
        34 => log_void(client_id, sequence, "UngrabKey"),
        36 => log_void(client_id, sequence, "GrabServer"),
        37 => log_void(client_id, sequence, "UngrabServer"),
        38 => {
            log_reply(client_id, sequence, "QueryPointer");
            let pointer = host
                .and_then(|host| host.lock().ok()?.query_pointer().ok())
                .filter(|pointer| pointer.same_screen);
            let reply_data = if let Some(pointer) = pointer {
                x11::QueryPointerReply {
                    root: ROOT_WINDOW,
                    child: ROOT_WINDOW,
                    root_x: pointer.root_x,
                    root_y: pointer.root_y,
                    win_x: pointer.win_x,
                    win_y: pointer.win_y,
                    mask: pointer.mask,
                }
            } else {
                x11::QueryPointerReply {
                    root: ROOT_WINDOW,
                    child: ROOT_WINDOW,
                    ..Default::default()
                }
            };
            x11::write_query_pointer_reply(&mut *lock_writer()?, sequence, reply_data)
        }
        40 => {
            log_reply(client_id, sequence, "TranslateCoordinates");
            x11::write_translate_coordinates_reply(
                &mut *lock_writer()?,
                sequence,
                ResourceId(0),
                0,
                0,
            )
        }
        42 => {
            if let Some(window) = x11::input_focus_window(body) {
                set_focused_window(focused_window, server, window)?;
            }
            log_void(client_id, sequence, "SetInputFocus")
        }
        43 => {
            log_reply(client_id, sequence, "GetInputFocus");
            let focus = focused_window
                .lock()
                .map(|focus| *focus)
                .unwrap_or(ROOT_WINDOW);
            x11::write_get_input_focus_reply(&mut *lock_writer()?, sequence, focus)
        }
        44 => {
            log_reply(client_id, sequence, "QueryKeymap");
            x11::write_query_keymap_reply(&mut *lock_writer()?, sequence)
        }
        45 => {
            if let Some(request) = x11::open_font_request(body) {
                debug!(
                    "client {} #{} OpenFont {:?}",
                    client_id.0, sequence.0, request.name
                );
                let new_id = request.font.0;
                let validation_failed = {
                    let s = lock_server(server)?;
                    let handle = s.clients.get(&client_id.0).expect("client registered");
                    !crate::server::IdAllocator::validate_owned(
                        new_id,
                        handle.resource_id_base,
                        handle.resource_id_mask,
                    ) || s.resources.any_resource_exists(request.font)
                };
                if validation_failed {
                    return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 45);
                }
                let host_result = if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    match host.open_font(&request.name) {
                        Ok(pair) => Some(pair),
                        Err(err) => {
                            warn!(
                                "client {} OpenFont {:?} failed on host: {err}",
                                client_id.0, request.name
                            );
                            None
                        }
                    }
                } else {
                    None
                };
                if let Some((host_xid, metrics)) = host_result {
                    let mut s = lock_server(server)?;
                    s.resources.install_font(
                        client_id,
                        request.font,
                        request.name,
                        host_xid,
                        metrics,
                    );
                }
                Ok(())
            } else {
                log_void(client_id, sequence, "OpenFont")
            }
        }
        46 => {
            if let Some(font) = x11::free_resource_id(body) {
                let removed = {
                    let mut s = lock_server(server)?;
                    s.resources.close_font(font)
                };
                if let Some(removed) = removed
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    let _ = host.close_font(removed.host_xid);
                }
            }
            log_void(client_id, sequence, "CloseFont")
        }
        47 => {
            log_reply(client_id, sequence, "QueryFont");
            let metrics = {
                let s = lock_server(server)?;
                x11::drawable_request_id(body)
                    .and_then(|id| s.resources.fontable(id))
                    .map(|font| font.metrics.clone())
                    .unwrap_or_default()
            };
            x11::write_query_font_reply(&mut *lock_writer()?, sequence, &metrics)
        }
        48 => {
            log_reply(client_id, sequence, "QueryTextExtents");
            let extents = {
                let s = lock_server(server)?;
                x11::query_text_extents_request(header.data, body)
                    .and_then(|req| {
                        s.resources
                            .fontable(req.fontable)
                            .map(|font| font.metrics.text_extents(&req.chars))
                    })
                    .unwrap_or_default()
            };
            x11::write_query_text_extents_reply(&mut *lock_writer()?, sequence, extents)
        }
        49 => {
            log_reply(client_id, sequence, "ListFonts");
            if let Some(request) = x11::list_fonts_request(body)
                && let Some(host) = host
                && let Ok(mut host) = host.lock()
                && let Ok(mut reply) = host.list_fonts_proxy(request.max_names, &request.pattern)
            {
                rewrite_reply_sequence(&mut reply, sequence);
                lock_writer()?.write_all(&reply)?;
            }
            Ok(())
        }
        50 => {
            log_reply(client_id, sequence, "ListFontsWithInfo");
            if let Some(request) = x11::list_fonts_request(body)
                && let Some(host) = host
                && let Ok(mut host) = host.lock()
                && let Ok(replies) =
                    host.list_fonts_with_info_proxy(request.max_names, &request.pattern)
            {
                for mut reply in replies {
                    rewrite_reply_sequence(&mut reply, sequence);
                    lock_writer()?.write_all(&reply)?;
                }
            }
            Ok(())
        }
        53 => {
            if let Some(request) = x11::create_pixmap_request(header.data, body) {
                let new_id = request.pixmap.0;
                let (validation_failed, drawable_exists) = {
                    let s = lock_server(server)?;
                    let handle = s.clients.get(&client_id.0).expect("client registered");
                    let owned = crate::server::IdAllocator::validate_owned(
                        new_id,
                        handle.resource_id_base,
                        handle.resource_id_mask,
                    );
                    let in_use = s.resources.any_resource_exists(request.pixmap);
                    let drawable_exists = s.resources.window(request.drawable).is_some()
                        || s.resources.pixmap(request.drawable).is_some();
                    (!owned || in_use, drawable_exists)
                };
                if validation_failed {
                    return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 53);
                }
                if !drawable_exists {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_DRAWABLE,
                        request.drawable.0,
                        53,
                    );
                }
                if !supported_pixmap_depth(request.depth) {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_VALUE,
                        u32::from(request.depth),
                        53,
                    );
                }
                let host_xid = if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    let xid = host.allocate_xid();
                    match host.create_pixmap(xid, request.depth, request.width, request.height) {
                        Ok(()) => Some(xid),
                        Err(err) => {
                            warn!("client {} host CreatePixmap failed: {err}", client_id.0);
                            None
                        }
                    }
                } else {
                    None
                };
                {
                    let mut s = lock_server(server)?;
                    s.resources.create_pixmap(client_id, request);
                    if let Some(xid) = host_xid {
                        let updated = s.resources.set_pixmap_host_xid(request.pixmap, xid);
                        debug_assert!(updated, "pixmap was just inserted above");
                    }
                }
            }
            log_void(client_id, sequence, "CreatePixmap")
        }
        54 => {
            if let Some(pixmap) = x11::free_resource_id(body) {
                let removed = {
                    let mut s = lock_server(server)?;
                    s.resources.free_pixmap(pixmap)
                };
                if let Some(removed_pixmap) = removed
                    && let Some(xid) = removed_pixmap.host_xid
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.free_pixmap(xid)?;
                }
            }
            log_void(client_id, sequence, "FreePixmap")
        }
        55 => {
            if let Some(request) = x11::create_gc_request(body) {
                let new_id = request.gc.0;
                let validation_failed = {
                    let s = lock_server(server)?;
                    let handle = s.clients.get(&client_id.0).expect("client registered");
                    let owned = crate::server::IdAllocator::validate_owned(
                        new_id,
                        handle.resource_id_base,
                        handle.resource_id_mask,
                    );
                    let in_use = s.resources.any_resource_exists(request.gc);
                    !owned || in_use
                };
                if validation_failed {
                    return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 55);
                }
                {
                    let mut s = lock_server(server)?;
                    s.resources.create_gc(client_id, request);
                }
            }
            log_void(client_id, sequence, "CreateGC")
        }
        56 => {
            if let Some(request) = x11::change_gc_request(body) {
                let mut s = lock_server(server)?;
                s.resources.change_gc(request);
            }
            log_void(client_id, sequence, "ChangeGC")
        }
        59 => {
            if let Some(request) = x11::set_clip_rectangles_request(header.data, body) {
                let mut s = lock_server(server)?;
                s.resources.set_clip_rectangles(request);
            }
            log_void(client_id, sequence, "SetClipRectangles")
        }
        60 => {
            if let Some(gc) = x11::free_resource_id(body) {
                let mut s = lock_server(server)?;
                s.resources.free_gc(gc);
            }
            log_void(client_id, sequence, "FreeGC")
        }
        61 => {
            if let Some(request) = x11::clear_area_request(body) {
                let (extents, bg_pixmap_host, target) = {
                    let s = lock_server(server)?;
                    let extents = s
                        .resources
                        .window(request.window)
                        .map(|w| (w.background_pixel, w.width, w.height));
                    let bg_pixmap_host = s
                        .resources
                        .window_background_pixmap_host_xid(request.window);
                    let target = s.resources.top_level_host_target(request.window);
                    (extents, bg_pixmap_host, target)
                };
                if let Some((background_pixel, w_width, w_height)) = extents
                    && let Some(target) = target
                {
                    let width = clear_extent(request.width, request.x, w_width);
                    let height = clear_extent(request.height, request.y, w_height);
                    if width != 0
                        && height != 0
                        && let Some(host) = host
                        && let Ok(mut host) = host.lock()
                    {
                        host.clear_clip_rectangles()?;
                        if let Some(bg_host_xid) = bg_pixmap_host {
                            host.copy_area(
                                bg_host_xid,
                                target.host_xid,
                                request.x,
                                request.y,
                                translate_i16(request.x, target.x_offset),
                                translate_i16(request.y, target.y_offset),
                                width,
                                height,
                            )?;
                        } else {
                            host.fill_rectangle(
                                target.host_xid,
                                background_pixel,
                                translate_i16(request.x, target.x_offset),
                                translate_i16(request.y, target.y_offset),
                                width,
                                height,
                            )?;
                        }
                    }
                }
            }
            log_void(client_id, sequence, "ClearArea")
        }
        62 => {
            if let Some(request) = x11::copy_area_request(body) {
                if request.width == 0 || request.height == 0 {
                    return log_void(client_id, sequence, "CopyArea");
                }

                let (gc_exists, src_exists, dst_exists, clip, src, dst) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc(request.gc).is_some(),
                        s.resources.window(request.src).is_some()
                            || s.resources.pixmap(request.src).is_some(),
                        s.resources.window(request.dst).is_some()
                            || s.resources.pixmap(request.dst).is_some(),
                        s.resources.gc_clip_rectangles(request.gc),
                        s.resources.host_drawable_target(request.src),
                        s.resources.host_drawable_target(request.dst),
                    )
                };
                if !gc_exists {
                    return emit_x11_error(writer, sequence, x11::error::BAD_GC, request.gc.0, 62);
                }
                if !src_exists {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_DRAWABLE,
                        request.src.0,
                        62,
                    );
                }
                if !dst_exists {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_DRAWABLE,
                        request.dst.0,
                        62,
                    );
                }
                // src/dst exist but have no host backing yet — silently drop
                if let (Some(src), Some(dst)) = (src, dst) {
                    if src.depth() != dst.depth() {
                        return emit_x11_error(
                            writer,
                            sequence,
                            x11::error::BAD_MATCH,
                            request.dst.0,
                            62,
                        );
                    }
                    if let Some(host) = host
                        && let Ok(mut host) = host.lock()
                    {
                        host.set_clip_rectangles(clip, dst.x_offset(), dst.y_offset())?;
                        host.copy_area(
                            src.host_xid(),
                            dst.host_xid(),
                            translate_i16(request.src_x, src.x_offset()),
                            translate_i16(request.src_y, src.y_offset()),
                            translate_i16(request.dst_x, dst.x_offset()),
                            translate_i16(request.dst_y, dst.y_offset()),
                            request.width,
                            request.height,
                        )?;
                    }
                }
            }
            log_void(client_id, sequence, "CopyArea")
        }
        64 => log_void(client_id, sequence, "PolyPoint"),
        65 => {
            if let Some((gc_id, points)) = x11::poly_line_data(body)
                && let Some(drawable) = x11::drawable_request_id(body)
            {
                let (foreground, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(ResourceId(gc_id)),
                        s.resources.gc_clip_rectangles(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.set_clip_rectangles(clip, target.x_offset(), target.y_offset())?;
                    let translated = translated_points(
                        points,
                        header.data,
                        target.x_offset(),
                        target.y_offset(),
                    );
                    host.poly_line(target.host_xid(), foreground, header.data, &translated)?;
                }
            }
            log_void(client_id, sequence, "PolyLine")
        }
        66 => {
            if let Some((gc_id, segments)) = x11::poly_segment_data(body)
                && let Some(drawable) = x11::drawable_request_id(body)
            {
                let (foreground, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(ResourceId(gc_id)),
                        s.resources.gc_clip_rectangles(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.set_clip_rectangles(clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_segments(segments, target.x_offset(), target.y_offset());
                    host.poly_segment(target.host_xid(), foreground, &translated)?;
                }
            }
            log_void(client_id, sequence, "PolySegment")
        }
        67 => {
            if let Some((gc_id, rectangles)) = x11::poly_fill_rectangle_data(body)
                && let Some(drawable) = x11::drawable_request_id(body)
            {
                let (foreground, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(ResourceId(gc_id)),
                        s.resources.gc_clip_rectangles(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.set_clip_rectangles(clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_records(rectangles, 8, target.x_offset(), target.y_offset());
                    host.poly_rectangle(target.host_xid(), foreground, &translated)?;
                }
            }
            log_void(client_id, sequence, "PolyRectangle")
        }
        68 => {
            if let Some((gc_id, arcs)) = x11::poly_arc_data(body)
                && let Some(drawable) = x11::drawable_request_id(body)
            {
                let (foreground, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(ResourceId(gc_id)),
                        s.resources.gc_clip_rectangles(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.set_clip_rectangles(clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_records(arcs, 12, target.x_offset(), target.y_offset());
                    host.poly_arc(target.host_xid(), foreground, &translated)?;
                }
            }
            log_void(client_id, sequence, "PolyArc")
        }
        69 => log_void(client_id, sequence, "FillPoly"),
        70 => {
            if let Some((gc_id, rectangles)) = x11::poly_fill_rectangle_data(body)
                && let Some(drawable) = x11::drawable_request_id(body)
            {
                let (foreground, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(ResourceId(gc_id)),
                        s.resources.gc_clip_rectangles(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.set_clip_rectangles(clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_records(rectangles, 8, target.x_offset(), target.y_offset());
                    host.poly_fill_rectangle(target.host_xid(), foreground, &translated)?;
                }
            }
            log_void(client_id, sequence, "PolyFillRectangle")
        }
        71 => {
            if let Some((gc_id, arcs)) = x11::poly_fill_arc_data(body)
                && let Some(drawable) = x11::drawable_request_id(body)
            {
                let (foreground, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(ResourceId(gc_id)),
                        s.resources.gc_clip_rectangles(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.set_clip_rectangles(clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_records(arcs, 12, target.x_offset(), target.y_offset());
                    host.poly_fill_arc(target.host_xid(), foreground, &translated)?;
                }
            }
            log_void(client_id, sequence, "PolyFillArc")
        }
        72 => {
            if let Some(request) = x11::put_image_request(header.data, body) {
                if request.width == 0 || request.height == 0 {
                    return log_void(client_id, sequence, "PutImage");
                }

                let (gc_exists, drawable_exists, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc(request.gc).is_some(),
                        s.resources.window(request.drawable).is_some()
                            || s.resources.pixmap(request.drawable).is_some(),
                        s.resources.gc_clip_rectangles(request.gc),
                        s.resources.host_drawable_target(request.drawable),
                    )
                };
                if !gc_exists {
                    return emit_x11_error(writer, sequence, x11::error::BAD_GC, request.gc.0, 72);
                }
                if !drawable_exists {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_DRAWABLE,
                        request.drawable.0,
                        72,
                    );
                }

                // XYBitmap and XYPixmap are out of scope for Phase 1; drop silently
                // rather than returning BadValue, which kills clients (xeyes, xterm).
                if request.format != x11::ImageFormat::ZPixmap {
                    return log_void(client_id, sequence, "PutImage");
                }
                // left_pad must be 0 for ZPixmap; malformed requests are dropped.
                if request.left_pad != 0 {
                    return log_void(client_id, sequence, "PutImage");
                }

                // target is None when the drawable has no host backing yet — drop silently.
                // Depth mismatches and unsupported depths are also dropped silently for Phase 1
                // compatibility; returning BadMatch kills clients (xterm).
                if let Some(target) = target {
                    if request.depth != target.depth() {
                        return log_void(client_id, sequence, "PutImage");
                    }
                    let Some(expected_len) =
                        zpixmap_expected_len(request.width, request.height, request.depth)
                    else {
                        return log_void(client_id, sequence, "PutImage");
                    };
                    if request.data.len() < expected_len {
                        return log_void(client_id, sequence, "PutImage");
                    }

                    if let Some(host) = host
                        && let Ok(mut host) = host.lock()
                    {
                        host.set_clip_rectangles(clip, target.x_offset(), target.y_offset())?;
                        host.put_image(
                            target.host_xid(),
                            request.depth,
                            request.width,
                            request.height,
                            translate_i16(request.dst_x, target.x_offset()),
                            translate_i16(request.dst_y, target.y_offset()),
                            &request.data[..expected_len],
                        )?;
                    }
                }
            }
            log_void(client_id, sequence, "PutImage")
        }
        73 => {
            log_reply(client_id, sequence, "GetImage");
            let Some(req) = x11::get_image_request(header.data, body) else {
                return Ok(());
            };
            let (order, host_xid, x_off, y_off) = {
                let s = lock_server(server)?;
                let order = s
                    .clients
                    .get(&client_id.0)
                    .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
                let (host_xid, x_off, y_off) = match s.resources.host_drawable_target(req.drawable)
                {
                    Some(crate::resources::HostDrawableTarget::Window {
                        host_xid,
                        x_offset,
                        y_offset,
                        ..
                    }) => (Some(host_xid), x_offset, y_offset),
                    Some(crate::resources::HostDrawableTarget::Pixmap { host_xid, .. }) => {
                        (Some(host_xid), 0, 0)
                    }
                    None => (None, 0, 0),
                };
                (order, host_xid, x_off, y_off)
            };
            // Try to proxy to the host; fall back to a blank image on any error.
            let host_reply = host_xid.and_then(|xid| {
                host.as_ref().and_then(|h| {
                    h.lock().ok().and_then(|mut h| {
                        h.get_image(
                            xid,
                            req.format,
                            req.x.saturating_add(x_off),
                            req.y.saturating_add(y_off),
                            req.width.max(1),
                            req.height.max(1),
                            req.plane_mask,
                        )
                        .ok()
                        .flatten()
                    })
                })
            });
            if let Some(mut bytes) = host_reply {
                // Patch in the client's sequence number and our visual ID.
                if bytes.len() >= 4 {
                    let s = sequence.0.to_le_bytes();
                    bytes[2] = s[0];
                    bytes[3] = s[1];
                }
                if bytes.len() >= 12 {
                    let v = crate::resources::ROOT_VISUAL.0.to_le_bytes();
                    bytes[8] = v[0];
                    bytes[9] = v[1];
                    bytes[10] = v[2];
                    bytes[11] = v[3];
                }
                lock_writer()?.write_all(&bytes)?;
                Ok(())
            } else {
                x11::write_get_image_reply(
                    &mut *lock_writer()?,
                    sequence,
                    order,
                    &req,
                    crate::resources::ROOT_VISUAL.0,
                )
            }
        }
        74 => {
            if let Some((drawable_raw, gc_id, text_body)) = x11::poly_text_data(body) {
                let drawable = ResourceId(drawable_raw);
                let (foreground, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(ResourceId(gc_id)),
                        s.resources.gc_clip_rectangles(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.set_clip_rectangles(clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_text_body(text_body, target.x_offset(), target.y_offset());
                    host.poly_text8(target.host_xid(), foreground, &translated)?;
                }
            }
            log_void(client_id, sequence, "PolyText8")
        }
        76 => {
            if let Some((drawable, gc_id, text_body)) = x11::image_text8_data(body) {
                debug!("focus text drawable 0x{drawable:x}");
                set_focused_window(focused_window, server, ResourceId(drawable))?;
                let gc = ResourceId(gc_id);
                let (foreground, background, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(gc),
                        s.resources.gc_background(gc),
                        s.resources.gc_clip_rectangles(gc),
                        s.resources.host_drawable_target(ResourceId(drawable)),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.set_clip_rectangles(clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_text_body(text_body, target.x_offset(), target.y_offset());
                    host.image_text8(
                        target.host_xid(),
                        foreground,
                        background,
                        header.data,
                        &translated,
                    )?;
                }
            }
            log_void(client_id, sequence, "ImageText8")
        }
        77 => {
            if let Some((drawable, gc_id, text_body)) = x11::image_text8_data(body) {
                debug!("focus text drawable 0x{drawable:x}");
                set_focused_window(focused_window, server, ResourceId(drawable))?;
                let gc = ResourceId(gc_id);
                let (foreground, background, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(gc),
                        s.resources.gc_background(gc),
                        s.resources.gc_clip_rectangles(gc),
                        s.resources.host_drawable_target(ResourceId(drawable)),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.set_clip_rectangles(clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_text_body(text_body, target.x_offset(), target.y_offset());
                    host.image_text16(
                        target.host_xid(),
                        foreground,
                        background,
                        header.data,
                        &translated,
                    )?;
                }
            }
            log_void(client_id, sequence, "ImageText16")
        }
        78 => log_void(client_id, sequence, "CreateColormap"),
        84 => {
            log_reply(client_id, sequence, "AllocColor");
            let color = x11::alloc_color_request(body).unwrap_or_default();
            x11::write_alloc_color_reply(&mut *lock_writer()?, sequence, color)
        }
        85 => {
            let name = x11::alloc_named_color_name(body);
            let color = x11::lookup_color_name(&name).unwrap_or_else(|| {
                debug!(
                    "client {} #{} AllocNamedColor unknown name {:?} -> fallback gray",
                    client_id.0, sequence.0, name
                );
                x11::Rgb16 {
                    red: 0xc0c0,
                    green: 0xc0c0,
                    blue: 0xc0c0,
                }
            });
            debug!(
                "client {} #{} AllocNamedColor {:?}",
                client_id.0, sequence.0, name
            );
            x11::write_alloc_named_color_reply(&mut *lock_writer()?, sequence, color)
        }
        91 => {
            let pixels = x11::query_colors_pixels(body);
            debug!(
                "client {} #{} QueryColors {} pixels",
                client_id.0,
                sequence.0,
                pixels.len()
            );
            x11::write_query_colors_reply(&mut *lock_writer()?, sequence, &pixels)
        }
        92 => {
            let name = x11::alloc_named_color_name(body);
            let color = x11::lookup_color_name(&name).unwrap_or_else(|| {
                debug!(
                    "client {} #{} LookupColor unknown name {:?} -> fallback gray",
                    client_id.0, sequence.0, name
                );
                x11::Rgb16 {
                    red: 0xc0c0,
                    green: 0xc0c0,
                    blue: 0xc0c0,
                }
            });
            debug!(
                "client {} #{} LookupColor {:?}",
                client_id.0, sequence.0, name
            );
            x11::write_lookup_color_reply(&mut *lock_writer()?, sequence, color)
        }
        94 => {
            if let Some(cursor) = x11::create_glyph_cursor_id(body) {
                let new_id = cursor.0;
                let validation_failed = {
                    let s = lock_server(server)?;
                    let handle = s.clients.get(&client_id.0).expect("client registered");
                    let owned = crate::server::IdAllocator::validate_owned(
                        new_id,
                        handle.resource_id_base,
                        handle.resource_id_mask,
                    );
                    let in_use = s.resources.any_resource_exists(cursor);
                    !owned || in_use
                };
                if validation_failed {
                    return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 94);
                }
                {
                    let mut s = lock_server(server)?;
                    s.resources.create_glyph_cursor(client_id, cursor);
                }
            }
            log_void(client_id, sequence, "CreateGlyphCursor")
        }
        95 => {
            if let Some(cursor) = x11::free_resource_id(body) {
                let mut s = lock_server(server)?;
                s.resources.free_cursor(cursor);
            }
            log_void(client_id, sequence, "FreeCursor")
        }
        96 => log_void(client_id, sequence, "RecolorCursor"),
        98 => {
            let name = x11::query_extension_name(body);
            let (present, major_opcode, first_event, first_error) = if name == "RANDR" {
                (
                    true,
                    RANDR_MAJOR_OPCODE,
                    RANDR_FIRST_EVENT,
                    RANDR_FIRST_ERROR,
                )
            } else {
                (false, 0, 0, 0)
            };
            debug!(
                "client {} #{} QueryExtension {:?} -> {}",
                client_id.0,
                sequence.0,
                name,
                if present { "present" } else { "absent" }
            );
            x11::write_query_extension_reply(
                &mut *lock_writer()?,
                sequence,
                present,
                major_opcode,
                first_event,
                first_error,
            )
        }
        99 => {
            log_reply(client_id, sequence, "ListExtensions");
            x11::write_list_extensions_reply(&mut *lock_writer()?, sequence)
        }
        101 => {
            log_reply(client_id, sequence, "GetKeyboardMapping");
            let first_keycode = body.first().copied().unwrap_or(0);
            let keycode_count = body.get(1).copied().unwrap_or(0);
            x11::write_get_keyboard_mapping_reply(
                &mut *lock_writer()?,
                sequence,
                first_keycode,
                keycode_count,
                4,
            )
        }
        103 => log_void(client_id, sequence, "Bell"),
        104 => log_void(client_id, sequence, "ChangeKeyboardControl"),
        108 => log_void(client_id, sequence, "SetScreenSaver"),
        110 => {
            log_reply(client_id, sequence, "ListHosts");
            x11::write_list_hosts_reply(&mut *lock_writer()?, sequence)
        }
        115 => {
            log_reply(client_id, sequence, "ForceScreenSaver");
            Ok(())
        }
        116 => log_void(client_id, sequence, "SetPointerMapping"),
        117 => {
            log_reply(client_id, sequence, "GetPointerMapping");
            x11::write_get_pointer_mapping_reply(&mut *lock_writer()?, sequence)
        }
        118 => log_void(client_id, sequence, "SetModifierMapping"),
        119 => {
            log_reply(client_id, sequence, "GetModifierMapping");
            x11::write_get_modifier_mapping_reply(&mut *lock_writer()?, sequence)
        }
        127 => log_void(client_id, sequence, "NoOperation"),
        RANDR_MAJOR_OPCODE => handle_randr_request(
            client_id,
            server,
            writer,
            sequence,
            header.data, // RANDR minor opcode
            body,
        ),
        35 => {
            let mode = header.data;
            if mode == 0 || mode == 1 || mode == 2 {
                let mut s = lock_server(server)?;
                s.frozen_pointer_event = None;
                if mode == 0 || mode == 1 {
                    // AsyncPointer / SyncPointer: release passive grab
                    if s.pointer_grab_is_passive {
                        s.pointer_grab = None;
                        s.pointer_grab_is_passive = false;
                    }
                }
                // ReplayPointer (mode==2): frozen event is cleared; normal routing will
                // handle next events. Full replay needs inter-thread plumbing (follow-up).
            }
            log_void(client_id, sequence, "AllowEvents")
        }
        opcode => {
            debug!(
                "client {} #{} unsupported opcode {} ({} bytes)",
                client_id.0,
                sequence.0,
                opcode,
                body.len() + 4
            );
            Ok(())
        }
    }
}

fn rewrite_reply_sequence(reply: &mut [u8], sequence: SequenceNumber) {
    if reply.len() >= 4 {
        let bytes = sequence.0.to_le_bytes();
        reply[2] = bytes[0];
        reply[3] = bytes[1];
    }
}

fn supported_pixmap_depth(depth: u8) -> bool {
    matches!(depth, 1 | 24 | 32)
}

fn zpixmap_expected_len(width: u16, height: u16, depth: u8) -> Option<usize> {
    let bits_per_pixel: usize = match depth {
        24 | 32 => 32,
        _ => return None,
    };
    let stride_bits = usize::from(width).checked_mul(bits_per_pixel)?;
    let stride_bytes = stride_bits.div_ceil(32).checked_mul(4)?;
    stride_bytes.checked_mul(usize::from(height))
}

fn translate_i16(value: i16, offset: i16) -> i16 {
    value.wrapping_add(offset)
}

fn read_i16_from(bytes: &[u8], offset: usize) -> Option<i16> {
    Some(i16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn write_i16_to(bytes: &mut [u8], offset: usize, value: i16) -> Option<()> {
    bytes
        .get_mut(offset..offset + 2)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

fn translate_i16_pair(bytes: &mut [u8], offset: usize, x_offset: i16, y_offset: i16) -> Option<()> {
    let x = translate_i16(read_i16_from(bytes, offset)?, x_offset);
    let y = translate_i16(read_i16_from(bytes, offset + 2)?, y_offset);
    write_i16_to(bytes, offset, x)?;
    write_i16_to(bytes, offset + 2, y)
}

fn translated_records(data: &[u8], record_len: usize, x_offset: i16, y_offset: i16) -> Vec<u8> {
    let mut out = data.to_vec();
    for record in out.chunks_exact_mut(record_len) {
        let _ = translate_i16_pair(record, 0, x_offset, y_offset);
    }
    out
}

fn translated_points(points: &[u8], coordinate_mode: u8, x_offset: i16, y_offset: i16) -> Vec<u8> {
    let mut out = points.to_vec();
    if coordinate_mode == 0 {
        for point in out.chunks_exact_mut(4) {
            let _ = translate_i16_pair(point, 0, x_offset, y_offset);
        }
    } else if out.len() >= 4 {
        let _ = translate_i16_pair(&mut out, 0, x_offset, y_offset);
    }
    out
}

fn translated_text_body(body: &[u8], x_offset: i16, y_offset: i16) -> Vec<u8> {
    let mut out = body.to_vec();
    if out.len() >= 12 {
        let _ = translate_i16_pair(&mut out, 8, x_offset, y_offset);
    }
    out
}

fn translated_segments(data: &[u8], x_offset: i16, y_offset: i16) -> Vec<u8> {
    let mut out = data.to_vec();
    for seg in out.chunks_exact_mut(8) {
        let _ = translate_i16_pair(seg, 0, x_offset, y_offset);
        let _ = translate_i16_pair(seg, 4, x_offset, y_offset);
    }
    out
}

fn log_void(client_id: ClientId, sequence: SequenceNumber, name: &str) -> io::Result<()> {
    debug!("client {} #{} {name}", client_id.0, sequence.0);
    Ok(())
}

fn log_reply(client_id: ClientId, sequence: SequenceNumber, name: &str) {
    debug!("client {} #{} {name}", client_id.0, sequence.0);
}

fn window_attributes(
    window: Option<&Window>,
    all_event_masks: u32,
    your_event_mask: u32,
) -> x11::WindowAttributes {
    let window = window.expect("root window exists");
    x11::WindowAttributes {
        visual: window.visual,
        class: window.class.protocol_value(),
        bit_gravity: 1,
        win_gravity: 1,
        backing_planes: u32::MAX,
        backing_pixel: window.background_pixel,
        save_under: false,
        map_is_installed: true,
        map_state: window.map_state.protocol_value(),
        override_redirect: window.override_redirect,
        colormap: ROOT_COLORMAP,
        all_event_masks,
        your_event_mask,
        do_not_propagate_mask: 0,
    }
}

fn window_geometry(window: &Window) -> x11::Geometry {
    x11::Geometry {
        root: ROOT_WINDOW,
        x: window.x,
        y: window.y,
        width: window.width,
        height: window.height,
        border_width: window.border_width,
        depth: window.depth,
    }
}

fn pixmap_geometry(pixmap: &Pixmap) -> x11::Geometry {
    x11::Geometry {
        root: ROOT_WINDOW,
        x: 0,
        y: 0,
        width: pixmap.width,
        height: pixmap.height,
        border_width: 0,
        depth: pixmap.depth,
    }
}

#[cfg(test)]
mod tests {
    use super::zpixmap_expected_len;

    #[test]
    fn zpixmap_expected_len_depth24_2x3() {
        assert_eq!(zpixmap_expected_len(2, 3, 24), Some(24));
    }

    #[test]
    fn zpixmap_expected_len_depth32_2x3() {
        assert_eq!(zpixmap_expected_len(2, 3, 32), Some(24));
    }

    #[test]
    fn zpixmap_expected_len_unsupported_depth_returns_none() {
        assert_eq!(zpixmap_expected_len(2, 3, 16), None);
        assert_eq!(zpixmap_expected_len(2, 3, 1), None);
    }

    #[test]
    fn zpixmap_expected_len_zero_width_returns_zero() {
        assert_eq!(zpixmap_expected_len(0, 3, 24), Some(0));
    }
}
