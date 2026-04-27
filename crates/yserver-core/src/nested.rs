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
};

use crate::{
    host_x11::{HostEvent, HostKeyboard, HostX11},
    resources::{MapState, Pixmap, ROOT_COLORMAP, ROOT_VISUAL, ROOT_WINDOW, ResourceTable, Window},
};

const RESOURCE_ID_BASE: u32 = 0x0020_0000;
const RESOURCE_ID_MASK: u32 = 0x001f_ffff;

static NEXT_CLIENT_ID: AtomicU32 = AtomicU32::new(1);

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

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let client_id = ClientId(NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed));
                let host = host.clone();
                thread::spawn(move || {
                    if let Err(err) = handle_client(client_id, stream, host, host_window_id) {
                        info!("client {} disconnected: {err}", client_id.0);
                    }
                });
            }
            Err(err) => error!("accept failed: {err}"),
        }
    }

    Ok(())
}

fn handle_client(
    client_id: ClientId,
    mut stream: UnixStream,
    host: Option<Arc<Mutex<HostX11>>>,
    host_window_id: Option<u32>,
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

    info!(
        "client {} setup: protocol {}.{}, auth_name_len={}, auth_data_len={}",
        client_id.0,
        setup.protocol_major,
        setup.protocol_minor,
        setup.auth_protocol_name.len(),
        setup.auth_protocol_data.len()
    );

    x11::write_setup_success(
        &mut stream,
        x11::SetupSuccess {
            protocol_major: setup.protocol_major,
            protocol_minor: setup.protocol_minor,
            release_number: 1,
            resource_id_base: RESOURCE_ID_BASE,
            resource_id_mask: RESOURCE_ID_MASK,
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
                current_input_masks: 0,
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
    if let Some(host_window_id) = host_window_id {
        match HostKeyboard::open_from_env(host_window_id) {
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

    let mut state = ClientState::new();
    let mut sequence = SequenceNumber(0);
    loop {
        let Some((header, body)) = x11::read_request(&mut reader)? else {
            return Ok(());
        };
        sequence = sequence.next();
        last_sequence.store(sequence.0, Ordering::Relaxed);
        let mut writer = writer
            .lock()
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))?;
        handle_request(
            client_id,
            &mut state,
            host.as_ref(),
            &mut writer,
            &focused_window,
            sequence,
            header,
            &body,
        )?;
    }
}

fn spawn_window_close_watcher(window_id: u32) {
    thread::spawn(move || {
        debug!("window-close watcher starting for 0x{window_id:x}");
        let mut watcher = match HostKeyboard::open_from_env(window_id) {
            Ok(w) => w,
            Err(err) => {
                error!("could not start window-close watcher: {err}");
                return;
            }
        };
        debug!("window-close watcher ready");
        loop {
            match watcher.read_event() {
                Ok(HostEvent::Key(_)) => {}
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
    mut keyboard: HostKeyboard,
    writer: Arc<Mutex<UnixStream>>,
    focused_window: Arc<Mutex<ResourceId>>,
    last_sequence: Arc<AtomicU16>,
) {
    thread::spawn(move || {
        loop {
            let event = match keyboard.read_event() {
                Ok(HostEvent::Key(event)) => event,
                Ok(HostEvent::Closed) => {
                    info!("host window closed, exiting");
                    std::process::exit(0);
                }
                Err(err) => {
                    info!("host connection lost ({err}), exiting");
                    std::process::exit(0);
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

fn set_focused_window(
    focused_window: &Arc<Mutex<ResourceId>>,
    stream: &mut UnixStream,
    sequence: SequenceNumber,
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

    if *focused_window != ROOT_WINDOW {
        x11::write_focus_event(stream, sequence, false, *focused_window)?;
    }
    *focused_window = window;
    x11::write_focus_event(stream, sequence, true, window)
}

fn focus_if_window_wants_keys(
    focused_window: &Arc<Mutex<ResourceId>>,
    state: &ClientState,
    stream: &mut UnixStream,
    sequence: SequenceNumber,
    window: ResourceId,
) -> io::Result<()> {
    const KEY_PRESS_MASK: u32 = 1 << 0;
    const KEY_RELEASE_MASK: u32 = 1 << 1;

    if state.resources.window(window).is_some_and(|window| {
        window.map_state == MapState::Viewable
            && window.event_mask & (KEY_PRESS_MASK | KEY_RELEASE_MASK) != 0
    }) {
        debug!("focus key window 0x{:x}", window.0);
        set_focused_window(focused_window, stream, sequence, window)?;
    }
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

struct ClientState {
    atoms_by_name: HashMap<String, AtomId>,
    atom_names: HashMap<u32, String>,
    resources: ResourceTable,
    next_atom_id: u32,
}

impl ClientState {
    fn new() -> Self {
        Self {
            atoms_by_name: HashMap::new(),
            atom_names: HashMap::new(),
            resources: ResourceTable::new(),
            next_atom_id: 69,
        }
    }

    fn intern_atom(&mut self, name: &str, only_if_exists: bool) -> AtomId {
        if let Some(atom) = x11::well_known_atom(name) {
            return atom;
        }

        if let Some(atom) = self.atoms_by_name.get(name).copied() {
            return atom;
        }

        if only_if_exists {
            return AtomId(0);
        }

        let atom = AtomId(self.next_atom_id);
        self.next_atom_id += 1;
        self.atoms_by_name.insert(name.to_owned(), atom);
        self.atom_names.insert(atom.0, name.to_owned());
        atom
    }

    fn atom_name(&self, atom: AtomId) -> Option<&str> {
        x11::well_known_atom_name(atom).or_else(|| self.atom_names.get(&atom.0).map(String::as_str))
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_request(
    client_id: ClientId,
    state: &mut ClientState,
    host: Option<&Arc<Mutex<HostX11>>>,
    stream: &mut UnixStream,
    focused_window: &Arc<Mutex<ResourceId>>,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<()> {
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
                state.resources.create_window(request);
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
                state.resources.change_window_attributes(request);
                focus_if_window_wants_keys(
                    focused_window,
                    state,
                    stream,
                    sequence,
                    request.window,
                )?;
            }
            log_void(client_id, sequence, "ChangeWindowAttributes")
        }
        3 => {
            log_reply(client_id, sequence, "GetWindowAttributes");
            let window = x11::drawable_request_id(body)
                .and_then(|id| state.resources.window(id))
                .or_else(|| state.resources.window(ROOT_WINDOW));
            x11::write_get_window_attributes_reply(stream, sequence, window_attributes(window))
        }
        4 => {
            if let Some(window) = x11::free_resource_id(body) {
                state.resources.destroy_window(window);
            }
            log_void(client_id, sequence, "DestroyWindow")
        }
        7 => log_void(client_id, sequence, "ReparentWindow"),
        8 => {
            if let Some(window) = x11::map_window_id(body) {
                state.resources.map_window(window);
                focus_if_window_wants_keys(focused_window, state, stream, sequence, window)?;
                if let Some(window_state) = state.resources.window(window) {
                    x11::write_map_notify_event(
                        stream,
                        sequence,
                        window_state.parent,
                        window,
                        window_state.override_redirect,
                    )?;
                    x11::write_expose_event(
                        stream,
                        sequence,
                        window,
                        window_state.width,
                        window_state.height,
                    )?;
                }
            }
            log_void(client_id, sequence, "MapWindow")
        }
        9 => {
            if let Some(parent) = x11::map_window_id(body) {
                let children = state.resources.children(parent).to_vec();
                for child in children {
                    state.resources.map_window(child);
                    focus_if_window_wants_keys(focused_window, state, stream, sequence, child)?;
                    if let Some(window_state) = state.resources.window(child) {
                        x11::write_expose_event(
                            stream,
                            sequence,
                            child,
                            window_state.width,
                            window_state.height,
                        )?;
                    }
                }
            }
            log_void(client_id, sequence, "MapSubwindows")
        }
        10 => {
            if let Some(window) = x11::map_window_id(body) {
                state.resources.unmap_window(window);
            }
            log_void(client_id, sequence, "UnmapWindow")
        }
        12 => {
            if let Some(request) = x11::configure_window_request(body)
                && let Some(window_state) = state.resources.configure_window(request)
            {
                let geometry = window_geometry(window_state);
                x11::write_configure_notify_event(
                    stream,
                    sequence,
                    window_state.id,
                    window_state.id,
                    geometry,
                    window_state.override_redirect,
                )?;
            }
            log_void(client_id, sequence, "ConfigureWindow")
        }
        14 => {
            log_reply(client_id, sequence, "GetGeometry");
            let drawable = x11::drawable_request_id(body).unwrap_or(ROOT_WINDOW);
            let geometry = state
                .resources
                .window(drawable)
                .map(window_geometry)
                .or_else(|| state.resources.pixmap(drawable).map(pixmap_geometry))
                .unwrap_or_else(|| {
                    window_geometry(
                        state
                            .resources
                            .window(ROOT_WINDOW)
                            .expect("root window exists"),
                    )
                });
            x11::write_get_geometry_reply(stream, sequence, geometry)
        }
        15 => {
            log_reply(client_id, sequence, "QueryTree");
            let window = x11::drawable_request_id(body).unwrap_or(ROOT_WINDOW);
            let window_state = state
                .resources
                .window(window)
                .or_else(|| state.resources.window(ROOT_WINDOW))
                .expect("root window exists");
            x11::write_query_tree_reply(
                stream,
                sequence,
                ROOT_WINDOW,
                window_state.parent,
                &window_state.children,
            )
        }
        16 => {
            let name = x11::intern_atom_name(body);
            let atom = state.intern_atom(&name, header.data != 0);
            debug!(
                "client {} #{} InternAtom {:?} -> {}",
                client_id.0, sequence.0, name, atom.0
            );
            x11::write_intern_atom_reply(stream, sequence, atom)
        }
        17 => {
            let atom = x11::request_atom(body);
            let name = state.atom_name(atom).unwrap_or("UNKNOWN");
            debug!(
                "client {} #{} GetAtomName {} -> {:?}",
                client_id.0, sequence.0, atom.0, name
            );
            x11::write_get_atom_name_reply(stream, sequence, name)
        }
        18 => log_void(client_id, sequence, "ChangeProperty"),
        19 => log_void(client_id, sequence, "DeleteProperty"),
        20 => {
            log_reply(client_id, sequence, "GetProperty");
            x11::write_get_property_reply(stream, sequence)
        }
        22 => log_void(client_id, sequence, "SetSelectionOwner"),
        23 => {
            log_reply(client_id, sequence, "GetSelectionOwner");
            x11::write_get_selection_owner_reply(stream, sequence, ResourceId(0))
        }
        25 => log_void(client_id, sequence, "SendEvent"),
        26 => {
            log_reply(client_id, sequence, "GrabPointer");
            x11::write_grab_reply(stream, sequence, 0)
        }
        27 => log_void(client_id, sequence, "UngrabPointer"),
        28 => log_void(client_id, sequence, "GrabButton"),
        29 => log_void(client_id, sequence, "UngrabButton"),
        31 => {
            log_reply(client_id, sequence, "GrabKeyboard");
            x11::write_grab_reply(stream, sequence, 0)
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
            x11::write_query_pointer_reply(stream, sequence, reply_data)
        }
        40 => {
            log_reply(client_id, sequence, "TranslateCoordinates");
            x11::write_translate_coordinates_reply(stream, sequence, ResourceId(0), 0, 0)
        }
        42 => {
            if let Some(window) = x11::input_focus_window(body) {
                set_focused_window(focused_window, stream, sequence, window)?;
            }
            log_void(client_id, sequence, "SetInputFocus")
        }
        43 => {
            log_reply(client_id, sequence, "GetInputFocus");
            let focus = focused_window
                .lock()
                .map(|focus| *focus)
                .unwrap_or(ROOT_WINDOW);
            x11::write_get_input_focus_reply(stream, sequence, focus)
        }
        44 => {
            log_reply(client_id, sequence, "QueryKeymap");
            x11::write_query_keymap_reply(stream, sequence)
        }
        45 => {
            if let Some(request) = x11::open_font_request(body) {
                debug!(
                    "client {} #{} OpenFont {:?}",
                    client_id.0, sequence.0, request.name
                );
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    match host.open_font(&request.name) {
                        Ok((host_xid, metrics)) => {
                            state.resources.install_font(
                                request.font,
                                request.name,
                                host_xid,
                                metrics,
                            );
                        }
                        Err(err) => {
                            warn!(
                                "client {} OpenFont {:?} failed on host: {err}",
                                client_id.0, request.name
                            );
                        }
                    }
                }
                Ok(())
            } else {
                log_void(client_id, sequence, "OpenFont")
            }
        }
        46 => {
            if let Some(font) = x11::free_resource_id(body)
                && let Some(removed) = state.resources.close_font(font)
                && let Some(host) = host
                && let Ok(mut host) = host.lock()
            {
                let _ = host.close_font(removed.host_xid);
            }
            log_void(client_id, sequence, "CloseFont")
        }
        47 => {
            log_reply(client_id, sequence, "QueryFont");
            let metrics = x11::drawable_request_id(body)
                .and_then(|id| state.resources.fontable(id))
                .map(|font| font.metrics.clone())
                .unwrap_or_default();
            x11::write_query_font_reply(stream, sequence, &metrics)
        }
        48 => {
            log_reply(client_id, sequence, "QueryTextExtents");
            let extents = x11::query_text_extents_request(header.data, body)
                .and_then(|req| {
                    state
                        .resources
                        .fontable(req.fontable)
                        .map(|font| font.metrics.text_extents(&req.chars))
                })
                .unwrap_or_default();
            x11::write_query_text_extents_reply(stream, sequence, extents)
        }
        49 => {
            log_reply(client_id, sequence, "ListFonts");
            if let Some(request) = x11::list_fonts_request(body)
                && let Some(host) = host
                && let Ok(mut host) = host.lock()
                && let Ok(mut reply) = host.list_fonts_proxy(request.max_names, &request.pattern)
            {
                rewrite_reply_sequence(&mut reply, sequence);
                stream.write_all(&reply)?;
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
                    stream.write_all(&reply)?;
                }
            }
            Ok(())
        }
        53 => {
            if let Some(request) = x11::create_pixmap_request(header.data, body) {
                state.resources.create_pixmap(request);
            }
            log_void(client_id, sequence, "CreatePixmap")
        }
        54 => {
            if let Some(pixmap) = x11::free_resource_id(body) {
                state.resources.free_pixmap(pixmap);
            }
            log_void(client_id, sequence, "FreePixmap")
        }
        55 => {
            if let Some(request) = x11::create_gc_request(body) {
                state.resources.create_gc(request);
            }
            log_void(client_id, sequence, "CreateGC")
        }
        56 => {
            if let Some(request) = x11::change_gc_request(body) {
                state.resources.change_gc(request);
            }
            log_void(client_id, sequence, "ChangeGC")
        }
        59 => log_void(client_id, sequence, "SetClipRectangles"),
        60 => {
            if let Some(gc) = x11::free_resource_id(body) {
                state.resources.free_gc(gc);
            }
            log_void(client_id, sequence, "FreeGC")
        }
        61 => {
            if let Some(request) = x11::clear_area_request(body)
                && let Some(window) = state.resources.window(request.window)
            {
                let width = clear_extent(request.width, request.x, window.width);
                let height = clear_extent(request.height, request.y, window.height);
                if width != 0
                    && height != 0
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.fill_rectangle(
                        window.background_pixel,
                        request.x,
                        request.y,
                        width,
                        height,
                    )?;
                }
            }
            log_void(client_id, sequence, "ClearArea")
        }
        62 => log_void(client_id, sequence, "CopyArea"),
        64 => log_void(client_id, sequence, "PolyPoint"),
        65 => {
            if let Some((gc_id, points)) = x11::poly_line_data(body) {
                let foreground = state.resources.gc_foreground(ResourceId(gc_id));
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.poly_line(foreground, header.data, points)?;
                }
            }
            log_void(client_id, sequence, "PolyLine")
        }
        66 => log_void(client_id, sequence, "PolySegment"),
        67 => log_void(client_id, sequence, "PolyRectangle"),
        68 => {
            if let Some((gc_id, arcs)) = x11::poly_arc_data(body) {
                let foreground = state.resources.gc_foreground(ResourceId(gc_id));
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.poly_arc(foreground, arcs)?;
                }
            }
            log_void(client_id, sequence, "PolyArc")
        }
        69 => log_void(client_id, sequence, "FillPoly"),
        70 => {
            if let Some((gc_id, rectangles)) = x11::poly_fill_rectangle_data(body) {
                let foreground = state.resources.gc_foreground(ResourceId(gc_id));
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.poly_fill_rectangle(foreground, rectangles)?;
                }
            }
            log_void(client_id, sequence, "PolyFillRectangle")
        }
        71 => {
            if let Some((gc_id, arcs)) = x11::poly_fill_arc_data(body) {
                let foreground = state.resources.gc_foreground(ResourceId(gc_id));
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.poly_fill_arc(foreground, arcs)?;
                }
            }
            log_void(client_id, sequence, "PolyFillArc")
        }
        72 => log_void(client_id, sequence, "PutImage"),
        74 => {
            if let Some((_drawable, gc_id, text_body)) = x11::poly_text_data(body) {
                let foreground = state.resources.gc_foreground(ResourceId(gc_id));
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.poly_text8(foreground, text_body)?;
                }
            }
            log_void(client_id, sequence, "PolyText8")
        }
        76 => {
            if let Some((drawable, gc_id, text_body)) = x11::image_text8_data(body) {
                debug!("focus text drawable 0x{drawable:x}");
                set_focused_window(focused_window, stream, sequence, ResourceId(drawable))?;
                let gc = ResourceId(gc_id);
                let foreground = state.resources.gc_foreground(gc);
                let background = state.resources.gc_background(gc);
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.image_text8(foreground, background, header.data, text_body)?;
                }
            }
            log_void(client_id, sequence, "ImageText8")
        }
        78 => log_void(client_id, sequence, "CreateColormap"),
        84 => {
            log_reply(client_id, sequence, "AllocColor");
            let color = x11::alloc_color_request(body).unwrap_or_default();
            x11::write_alloc_color_reply(stream, sequence, color)
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
            x11::write_alloc_named_color_reply(stream, sequence, color)
        }
        91 => {
            let pixels = x11::query_colors_pixels(body);
            debug!(
                "client {} #{} QueryColors {} pixels",
                client_id.0,
                sequence.0,
                pixels.len()
            );
            x11::write_query_colors_reply(stream, sequence, &pixels)
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
            x11::write_lookup_color_reply(stream, sequence, color)
        }
        94 => {
            if let Some(cursor) = x11::create_glyph_cursor_id(body) {
                state.resources.create_glyph_cursor(cursor);
            }
            log_void(client_id, sequence, "CreateGlyphCursor")
        }
        95 => {
            if let Some(cursor) = x11::free_resource_id(body) {
                state.resources.free_cursor(cursor);
            }
            log_void(client_id, sequence, "FreeCursor")
        }
        96 => log_void(client_id, sequence, "RecolorCursor"),
        98 => {
            let name = x11::query_extension_name(body);
            debug!(
                "client {} #{} QueryExtension {:?} -> absent",
                client_id.0, sequence.0, name
            );
            x11::write_query_extension_reply(stream, sequence, false, 0, 0, 0)
        }
        99 => {
            log_reply(client_id, sequence, "ListExtensions");
            x11::write_list_extensions_reply(stream, sequence)
        }
        101 => {
            log_reply(client_id, sequence, "GetKeyboardMapping");
            let first_keycode = body.first().copied().unwrap_or(0);
            let keycode_count = body.get(1).copied().unwrap_or(0);
            x11::write_get_keyboard_mapping_reply(stream, sequence, first_keycode, keycode_count, 4)
        }
        103 => log_void(client_id, sequence, "Bell"),
        104 => log_void(client_id, sequence, "ChangeKeyboardControl"),
        108 => log_void(client_id, sequence, "SetScreenSaver"),
        110 => {
            log_reply(client_id, sequence, "ListHosts");
            x11::write_list_hosts_reply(stream, sequence)
        }
        115 => {
            log_reply(client_id, sequence, "ForceScreenSaver");
            Ok(())
        }
        116 => log_void(client_id, sequence, "SetPointerMapping"),
        117 => {
            log_reply(client_id, sequence, "GetPointerMapping");
            x11::write_get_pointer_mapping_reply(stream, sequence)
        }
        118 => log_void(client_id, sequence, "SetModifierMapping"),
        119 => {
            log_reply(client_id, sequence, "GetModifierMapping");
            x11::write_get_modifier_mapping_reply(stream, sequence)
        }
        127 => log_void(client_id, sequence, "NoOperation"),
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

fn log_void(client_id: ClientId, sequence: SequenceNumber, name: &str) -> io::Result<()> {
    debug!("client {} #{} {name}", client_id.0, sequence.0);
    Ok(())
}

fn log_reply(client_id: ClientId, sequence: SequenceNumber, name: &str) {
    debug!("client {} #{} {name}", client_id.0, sequence.0);
}

fn window_attributes(window: Option<&Window>) -> x11::WindowAttributes {
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
        all_event_masks: window.event_mask,
        your_event_mask: window.event_mask,
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
