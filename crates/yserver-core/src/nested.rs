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
    composite as x11composite, damage as x11damage, present as x11present, randr as x11randr,
    shape as x11shape, sync as x11sync, xfixes as x11xfixes,
};

use crate::{
    host_x11::{
        HostEvent, HostInputPump, HostInputPumpHandle, HostSubwindowConfig, HostSubwindowVisual,
        HostX11,
    },
    resources::{
        ARGB_VISUAL, GcClipState, GlyphSetState, HostDrawableTarget, MapState,
        NamedCompositePixmap, PictureState, Pixmap, ROOT_COLORMAP, ROOT_VISUAL, ROOT_WINDOW,
        ReparentWindowError, Window,
    },
    server::{
        ClientHandle, DamageObject, EventTarget, PresentEventSelection, ServerState, SyncAlarm,
        SyncCounter, XFixesRegion, fanout_event, fanout_raw_event,
    },
    unix_fd::FdReader,
};

static NEXT_CLIENT_ID: AtomicU32 = AtomicU32::new(1);

const RANDR_MAJOR_OPCODE: u8 = 128;
const RANDR_FIRST_EVENT: u8 = 89;
const RANDR_FIRST_ERROR: u8 = 147;

const RENDER_MAJOR_OPCODE: u8 = 133;
const RENDER_FIRST_EVENT: u8 = 0;
const RENDER_FIRST_ERROR: u8 = 152;

const GE_MAJOR_OPCODE: u8 = 138;

const BIG_REQUESTS_MAJOR_OPCODE: u8 = 135;
const BIG_REQUESTS_FIRST_EVENT: u8 = 0;
const BIG_REQUESTS_FIRST_ERROR: u8 = 0;

const XKB_MAJOR_OPCODE: u8 = 136;

const XI2_MAJOR_OPCODE: u8 = 137;
const XI2_FIRST_EVENT: u8 = 90;
const XI2_FIRST_ERROR: u8 = 153;

const XFIXES_MAJOR_OPCODE: u8 = 140;
const XFIXES_FIRST_EVENT: u8 = 91;
const XFIXES_FIRST_ERROR: u8 = 154;

const SHAPE_MAJOR_OPCODE: u8 = 141;
const SHAPE_FIRST_EVENT: u8 = 92;
const SHAPE_FIRST_ERROR: u8 = 155;

const SYNC_MAJOR_OPCODE: u8 = 142;
const SYNC_FIRST_EVENT: u8 = 93;
const SYNC_FIRST_ERROR: u8 = 156;

const DAMAGE_MAJOR_OPCODE: u8 = 143;
const DAMAGE_FIRST_EVENT: u8 = 94;
const DAMAGE_FIRST_ERROR: u8 = 157;

const COMPOSITE_MAJOR_OPCODE: u8 = 144;
const COMPOSITE_FIRST_EVENT: u8 = 0;
const COMPOSITE_FIRST_ERROR: u8 = 158;

const PRESENT_MAJOR_OPCODE: u8 = 145;
const PRESENT_FIRST_EVENT: u8 = 95;
const PRESENT_FIRST_ERROR: u8 = 159;

const MIT_SHM_MAJOR_OPCODE: u8 = 130;
const MIT_SHM_FIRST_EVENT: u8 = 96;
const MIT_SHM_FIRST_ERROR: u8 = 160;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExtensionAvailability {
    Always,
    HostRender,
    HostXkb,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnsupportedMinorPolicy {
    HandledInline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExtensionMetadata {
    name: &'static str,
    major_opcode: u8,
    first_event: u8,
    first_error: u8,
    availability: ExtensionAvailability,
    unsupported_minor_policy: UnsupportedMinorPolicy,
}

const EXTENSIONS: &[ExtensionMetadata] = &[
    ExtensionMetadata {
        name: "RANDR",
        major_opcode: RANDR_MAJOR_OPCODE,
        first_event: RANDR_FIRST_EVENT,
        first_error: RANDR_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "RENDER",
        major_opcode: RENDER_MAJOR_OPCODE,
        first_event: RENDER_FIRST_EVENT,
        first_error: RENDER_FIRST_ERROR,
        availability: ExtensionAvailability::HostRender,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "Generic Event Extension",
        major_opcode: GE_MAJOR_OPCODE,
        first_event: 0,
        first_error: 0,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "BIG-REQUESTS",
        major_opcode: BIG_REQUESTS_MAJOR_OPCODE,
        first_event: BIG_REQUESTS_FIRST_EVENT,
        first_error: BIG_REQUESTS_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "XKEYBOARD",
        major_opcode: XKB_MAJOR_OPCODE,
        first_event: 0,
        first_error: 0,
        availability: ExtensionAvailability::HostXkb,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "XInputExtension",
        major_opcode: XI2_MAJOR_OPCODE,
        first_event: XI2_FIRST_EVENT,
        first_error: XI2_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "XFIXES",
        major_opcode: XFIXES_MAJOR_OPCODE,
        first_event: XFIXES_FIRST_EVENT,
        first_error: XFIXES_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "SHAPE",
        major_opcode: SHAPE_MAJOR_OPCODE,
        first_event: SHAPE_FIRST_EVENT,
        first_error: SHAPE_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "SYNC",
        major_opcode: SYNC_MAJOR_OPCODE,
        first_event: SYNC_FIRST_EVENT,
        first_error: SYNC_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "DAMAGE",
        major_opcode: DAMAGE_MAJOR_OPCODE,
        first_event: DAMAGE_FIRST_EVENT,
        first_error: DAMAGE_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "Composite",
        major_opcode: COMPOSITE_MAJOR_OPCODE,
        first_event: COMPOSITE_FIRST_EVENT,
        first_error: COMPOSITE_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "Present",
        major_opcode: PRESENT_MAJOR_OPCODE,
        first_event: PRESENT_FIRST_EVENT,
        first_error: PRESENT_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "MIT-SHM",
        major_opcode: MIT_SHM_MAJOR_OPCODE,
        first_event: MIT_SHM_FIRST_EVENT,
        first_error: MIT_SHM_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
];

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
    // Route root-window drawing/clearing to the host container window so
    // clients that paint the root (e.g. fvwm3 setting its desktop bg pixmap)
    // produce visible output in the nested viewport.
    if let Some(host_window_id) = host_window_id
        && let Ok(mut s) = server.lock()
        && let Some(root) = s.resources.window_mut(ROOT_WINDOW)
    {
        root.host_xid = Some(host_window_id);
    }

    // Push host visual / colormap xids into the resource table so that
    // CreateWindow forwarding can translate our visual ids to host ones.
    if let Some(host_arc) = host.as_ref()
        && let Ok(host) = host_arc.lock()
        && let Ok(mut s) = server.lock()
    {
        s.resources
            .set_visual_host_xid(crate::resources::ROOT_VISUAL, host.root_visual_xid());
        if let Some(host_colormap) = host.argb_colormap_xid() {
            s.resources
                .set_colormap_host_xid(crate::resources::ARGB_COLORMAP, host_colormap);
        }
        if let Some(host_argb_visual) = host.argb_visual_xid() {
            s.resources
                .set_visual_host_xid(crate::resources::ARGB_VISUAL, host_argb_visual);
        }
    }

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
                            Ok(HostEvent::Expose(ev)) => {
                                expose_event_fanout(&server_for_thread, &xid_map, ev);
                            }
                            Ok(HostEvent::Configure(ev)) => {
                                if ev.host_xid == window_id {
                                    handle_host_container_resize(&server_for_thread, ev);
                                }
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

fn extension_metadata(name: &str) -> Option<&'static ExtensionMetadata> {
    EXTENSIONS.iter().find(|ext| ext.name == name)
}

fn extension_is_available(ext: &ExtensionMetadata, host: Option<&Arc<Mutex<HostX11>>>) -> bool {
    match ext.availability {
        ExtensionAvailability::Always => true,
        ExtensionAvailability::HostRender => host
            .and_then(|h| h.lock().ok())
            .is_some_and(|h| h.render_opcode().is_some()),
        ExtensionAvailability::HostXkb => host
            .and_then(|h| h.lock().ok())
            .is_some_and(|h| h.xkb_opcode().is_some()),
    }
}

fn extension_query_reply(name: &str, host: Option<&Arc<Mutex<HostX11>>>) -> Option<(u8, u8, u8)> {
    let ext = extension_metadata(name)?;
    if !extension_is_available(ext, host) {
        return None;
    }
    if ext.availability == ExtensionAvailability::HostXkb {
        let (_, first_event, first_error) = host?.lock().ok()?.xkb_info()?;
        return Some((ext.major_opcode, first_event, first_error));
    }
    Some((ext.major_opcode, ext.first_event, ext.first_error))
}

fn advertised_extension_names(host: Option<&Arc<Mutex<HostX11>>>) -> Vec<&'static str> {
    EXTENSIONS
        .iter()
        .filter(|ext| extension_is_available(ext, host))
        .map(|ext| ext.name)
        .collect()
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

/// Pick the host CreateWindow visual / depth / colormap for a freshly
/// created top-level by inspecting the local Window's resolved visual.
/// `ROOT_VISUAL` matches the host container's visual so we use
/// `CopyFromParent` (no colormap value needed). `ARGB_VISUAL` requires
/// explicit visual + colormap host xids — when those aren't available
/// (host advertises no depth-32 TrueColor visual) we fall back to
/// `CopyFromParent` so the window still appears, just at depth 24.
fn resolve_host_subwindow_visual(
    server: &Mutex<ServerState>,
    window: ResourceId,
) -> HostSubwindowVisual {
    let Ok(s) = server.lock() else {
        return HostSubwindowVisual::CopyFromParent;
    };
    let Some(window) = s.resources.window(window) else {
        return HostSubwindowVisual::CopyFromParent;
    };
    if window.visual == crate::resources::ROOT_VISUAL {
        return HostSubwindowVisual::CopyFromParent;
    }
    let Some(visual) = s.resources.visual(window.visual) else {
        return HostSubwindowVisual::CopyFromParent;
    };
    let Some(visual_xid) = visual.host_visual_xid else {
        return HostSubwindowVisual::CopyFromParent;
    };
    let Some(colormap) = s.resources.colormap_for_visual(window.visual) else {
        return HostSubwindowVisual::CopyFromParent;
    };
    let Some(colormap_xid) = colormap.host_colormap_xid else {
        return HostSubwindowVisual::CopyFromParent;
    };
    HostSubwindowVisual::Explicit {
        depth: visual.depth,
        visual_xid,
        colormap_xid,
    }
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
                argb_visual: ARGB_VISUAL,
                root_depth: 24,
            },
        },
    )?;

    let mut reader = FdReader::new(stream.try_clone()?);
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
                save_set: std::collections::HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
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
                Arc::clone(&server),
            ),
            Err(err) => warn!("client {} keyboard forwarding disabled: {err}", client_id.0),
        }
    }

    #[allow(clippy::redundant_closure_call)]
    let result: io::Result<()> = (|| {
        let mut sequence = SequenceNumber(0);
        loop {
            let big_enabled = server
                .lock()
                .ok()
                .and_then(|s| s.clients.get(&client_id.0).map(|c| c.big_requests_enabled))
                .unwrap_or(false);

            let Some((header, body)) = x11::read_request(&mut reader, big_enabled)? else {
                return Ok(());
            };
            sequence = sequence.next();
            last_sequence.store(sequence.0, Ordering::Relaxed);
            // MIT-SHM AttachFd carries its file descriptor in the cmsg of
            // the same message that delivered the request body. Pop it now
            // so handle_request can attach it to the segment table.
            let attached_fd = if header.opcode == MIT_SHM_MAJOR_OPCODE
                && header.data == x11::mit_shm::ATTACH_FD
            {
                reader.pop_fd()
            } else {
                None
            };
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
                attached_fd,
            )?;
        }
    })();

    let (removed, pending_destroys) = {
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
        let removed = s.resources.remove_non_window_resources_owned_by(client_id);
        s.clients.remove(&client_id.0);
        let dead_windows: std::collections::HashSet<ResourceId> =
            all_destroyed.iter().copied().collect();
        s.xfixes_regions
            .retain(|_, region| region.owner != client_id);
        s.xfixes_selection_masks
            .retain(|(owner, _, _), _| *owner != client_id.0);
        s.xfixes_cursor_masks
            .retain(|(owner, _), _| *owner != client_id.0);
        s.shape_windows
            .retain(|window, _| !dead_windows.contains(window));
        s.shape_select_masks
            .retain(|(owner, window), _| *owner != client_id.0 && !dead_windows.contains(window));
        s.sync_counters
            .retain(|_, counter| counter.owner != client_id);
        s.sync_alarms.retain(|_, alarm| alarm.owner != client_id);
        s.damage_objects.retain(|_, damage| {
            damage.owner != client_id && !dead_windows.contains(&damage.drawable)
        });
        s.composite_redirects
            .retain(|(window, _), _| !dead_windows.contains(window));
        s.present_event_selections.retain(|_, selection| {
            selection.owner != client_id && !dead_windows.contains(&selection.window)
        });
        s.present_msc
            .retain(|window, _| !dead_windows.contains(window));
        // MIT-SHM segments: drop any owned by this client. The Drop impl on
        // MitShmSegment unmaps and closes the FD.
        s.mit_shm_segments.retain(|_, seg| seg.owner != client_id);
        s.randr_select_masks
            .retain(|(owner, window), _| *owner != client_id.0 && !dead_windows.contains(window));
        s.xkb_select_event_masks
            .retain(|(owner, _), _| *owner != client_id.0);
        s.button_grabs.retain(|g| g.owner != client_id);
        if s.pointer_grab.is_some_and(|(owner, _)| owner == client_id) {
            s.pointer_grab = None;
            s.pointer_grab_is_passive = false;
            s.frozen_pointer_event = None;
        }
        s.selections
            .retain(|_, owner_window| !dead_windows.contains(owner_window));
        (removed, pending)
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
        for xid in removed.closed_fonts {
            let _ = h.close_font(xid);
        }
        for xid in removed.freed_pixmaps {
            let _ = h.free_pixmap(xid);
        }
        for (pic_xid, owned_pix) in removed.freed_pictures {
            let _ = h.render_free_picture(pic_xid);
            if let Some(pix_xid) = owned_pix {
                let _ = h.free_pixmap(pix_xid);
            }
        }
        for gs_xid in removed.freed_glyphsets {
            let _ = h.render_free_glyphset(gs_xid);
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
                Ok(
                    HostEvent::Key(_)
                    | HostEvent::Pointer(_)
                    | HostEvent::Expose(_)
                    | HostEvent::Configure(_),
                ) => {}
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum KeyTarget {
    Focus(ResourceId),
    Grab {
        client_id: ClientId,
        grab_window: ResourceId,
        writer: WriterTag,
    },
    Drop,
}

/// `WriterTag::Self_` means use the focused-client writer (same `client_id`
/// as the forwarder); `WriterTag::Other(id)` means look up the grab owner's
/// writer through `ServerState::client_target`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum WriterTag {
    Self_,
    Other(ClientId),
}

fn route_key_event(
    state: &mut ServerState,
    self_client: ClientId,
    focus: ResourceId,
    keycode: u8,
    state_mask: u16,
    pressed: bool,
) -> KeyTarget {
    use crate::server::{ActiveKeyboardGrab, ActiveKeyboardGrabSource};

    if let Some(g) = state.active_keyboard_grab {
        if !pressed
            && let ActiveKeyboardGrabSource::PassiveKey { keycode: kc } = g.source
            && kc == keycode
        {
            state.active_keyboard_grab = None;
        }
        let writer = if g.owner == self_client {
            WriterTag::Self_
        } else {
            WriterTag::Other(g.owner)
        };
        return KeyTarget::Grab {
            client_id: g.owner,
            grab_window: g.grab_window,
            writer,
        };
    }
    if pressed && let Some(grab) = state.find_key_grab(focus, keycode, state_mask) {
        let owner = grab.owner;
        let win = grab.grab_window;
        state.active_keyboard_grab = Some(ActiveKeyboardGrab {
            owner,
            grab_window: win,
            source: ActiveKeyboardGrabSource::PassiveKey { keycode },
        });
        let writer = if owner == self_client {
            WriterTag::Self_
        } else {
            WriterTag::Other(owner)
        };
        return KeyTarget::Grab {
            client_id: owner,
            grab_window: win,
            writer,
        };
    }
    if focus == ROOT_WINDOW {
        return KeyTarget::Drop;
    }
    KeyTarget::Focus(focus)
}

fn spawn_keyboard_forwarder(
    client_id: ClientId,
    mut keyboard: HostInputPump,
    writer: Arc<Mutex<UnixStream>>,
    focused_window: Arc<Mutex<ResourceId>>,
    last_sequence: Arc<AtomicU16>,
    server: Arc<Mutex<ServerState>>,
) {
    thread::spawn(move || {
        loop {
            let event = loop {
                match keyboard.read_event() {
                    Ok(HostEvent::Key(event)) => break event,
                    Ok(HostEvent::Pointer(_) | HostEvent::Expose(_) | HostEvent::Configure(_)) => {
                        continue;
                    }
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

            let (event_window, target_writer, target_seq) = {
                let mut s = match server.lock() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let target = route_key_event(
                    &mut s,
                    client_id,
                    focus,
                    event.keycode,
                    event.state,
                    event.pressed,
                );
                match target {
                    KeyTarget::Drop => continue,
                    KeyTarget::Focus(w) => (w, writer.clone(), last_sequence.clone()),
                    KeyTarget::Grab {
                        grab_window,
                        writer: tag,
                        ..
                    } => match tag {
                        WriterTag::Self_ => (grab_window, writer.clone(), last_sequence.clone()),
                        WriterTag::Other(cid) => match s.client_target(cid) {
                            Some(t) => (grab_window, t.writer.clone(), t.last_sequence.clone()),
                            None => continue,
                        },
                    },
                }
            };

            debug!(
                "client {} key {} {} -> 0x{:x}",
                client_id.0,
                if event.pressed { "press" } else { "release" },
                event.keycode,
                event_window.0
            );
            let Some(mut w) = target_writer.lock().ok() else {
                return;
            };
            if let Err(err) = x11::write_key_event(
                &mut *w,
                x11::KeyEvent {
                    pressed: event.pressed,
                    keycode: event.keycode,
                    sequence: SequenceNumber(target_seq.load(Ordering::Relaxed)),
                    time: event.time,
                    root: ROOT_WINDOW,
                    event: event_window,
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
            drop(w);

            let xi2_evtype = if event.pressed { 2u16 } else { 3u16 };
            let xi2_targets: Vec<_> = match server.lock() {
                Ok(s) => s
                    .clients
                    .values()
                    .filter(|client| {
                        let mask = crate::server::xi2_mask_for_client(
                            client,
                            event_window,
                            event_window,
                            &[3, 1, 0],
                        );
                        mask & (1 << xi2_evtype) != 0
                    })
                    .map(|client| crate::server::EventTarget {
                        writer: client.writer.clone(),
                        byte_order: client.byte_order,
                        last_sequence: client.last_sequence.clone(),
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            for target in xi2_targets {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(84);
                x11::encode_xi2_device_event(
                    &mut buf,
                    seq,
                    XI2_MAJOR_OPCODE,
                    xi2_evtype,
                    3,
                    event.time,
                    ROOT_WINDOW,
                    event_window,
                    event.root_x,
                    event.root_y,
                    event.event_x,
                    event.event_y,
                    event.state & 0x004d,
                    u32::from(event.keycode),
                    3,
                );
                if let Ok(mut writer) = target.writer.lock() {
                    let _ = writer.write_all(&buf);
                }
            }
        }
    });
}

/// Forward a host Expose event to nested clients that selected ExposureMask.
/// Called from the host input-pump thread when the host uncovers a subwindow.
fn expose_event_fanout(
    server: &Arc<Mutex<ServerState>>,
    xid_map: &crate::host_x11::HostXidMap,
    ev: crate::host_x11::HostExposeEvent,
) {
    let nested_id = match xid_map.lock() {
        Ok(map) => map.get(&ev.host_xid).copied(),
        Err(_) => None,
    };
    let Some(window) = nested_id else { return };
    crate::server::emit_window_event(server, window, 0x0000_8000, |buf, seq, order| {
        x11::encode_expose_event(
            buf, seq, order, window, ev.x, ev.y, ev.width, ev.height, ev.count,
        );
    });
    // Root-level exposes describe areas of the container that were uncovered
    // by a moved top-level. The other top-levels in the area still have their
    // own host subwindows and receive their own host exposes; descendants of
    // those top-levels are reached via that path. Walking root's descendants
    // here would double-deliver and produce flickering chrome.
    if window == ROOT_WINDOW {
        return;
    }
    // For top-level exposes, synthesize Expose for mapped sub-windows that
    // overlap the area. Sub-windows have no host counterpart (only top-levels
    // do), so without this wmaker's frame chrome (titlebar, resize handle) is
    // never told to repaint after a sibling top-level is dragged across it.
    let exposed = match server.lock() {
        Ok(s) => s.resources.descendants_in_exposed_area(
            window,
            ev.x as i16,
            ev.y as i16,
            ev.width,
            ev.height,
        ),
        Err(_) => return,
    };
    for rect in exposed {
        crate::server::emit_window_event(server, rect.window, 0x0000_8000, |buf, seq, order| {
            x11::encode_expose_event(
                buf,
                seq,
                order,
                rect.window,
                rect.x as u16,
                rect.y as u16,
                rect.width,
                rect.height,
                0,
            );
        });
    }
}

fn handle_host_container_resize(
    server: &Arc<Mutex<ServerState>>,
    ev: crate::host_x11::HostConfigureEvent,
) {
    #[allow(clippy::type_complexity)]
    let update = {
        let mut s = match server.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if ev.width == 0
            || ev.height == 0
            || (s.randr.screen_width == ev.width && s.randr.screen_height == ev.height)
        {
            return;
        }

        let timestamp = s.timestamp_now();
        s.randr.resize(timestamp, ev.width, ev.height);
        if let Some(root) = s.resources.window_mut(ROOT_WINDOW) {
            root.width = ev.width;
            root.height = ev.height;
        }

        let width_mm = u16::try_from(s.randr.width_mm).unwrap_or(u16::MAX);
        let height_mm = u16::try_from(s.randr.height_mm).unwrap_or(u16::MAX);
        let targets = s
            .randr_select_masks
            .iter()
            .filter_map(|((owner, window), mask)| {
                s.client_target(ClientId(*owner))
                    .map(|target| (target, *window, *mask))
            })
            .collect::<Vec<_>>();

        Some((timestamp, ev.width, ev.height, width_mm, height_mm, targets))
    };

    let Some((timestamp, width, height, width_mm, height_mm, targets)) = update else {
        return;
    };

    debug!(
        "host container resized to {}x{} at {}, emitting RANDR updates",
        width, height, timestamp
    );

    // Spec-correct: emit a core ConfigureNotify on root *before* the RANDR
    // fanout so non-RANDR-aware clients (panels, "fill the screen" apps)
    // reflow at the same point in the event stream that RANDR-aware toolkits
    // see screen-change. Subscribers selected via StructureNotifyMask on root.
    crate::server::emit_window_event(
        server,
        ROOT_WINDOW,
        0x0002_0000, // StructureNotifyMask
        |buf, seq, order| {
            x11::encode_configure_notify_event(
                buf,
                seq,
                order,
                ROOT_WINDOW,
                ROOT_WINDOW,
                x11::Geometry {
                    root: ROOT_WINDOW,
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

    for (target, request_window, mask) in targets {
        let sequence = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        if mask & x11randr::NOTIFY_MASK_SCREEN_CHANGE != 0 {
            let event = x11randr::encode_screen_change_notify_event(
                RANDR_FIRST_EVENT,
                sequence,
                x11randr::ScreenChangeNotify {
                    timestamp,
                    config_timestamp: timestamp,
                    root: ROOT_WINDOW.0,
                    request_window: request_window.0,
                    width,
                    height,
                    width_mm,
                    height_mm,
                },
            );
            if let Ok(mut writer) = target.writer.lock() {
                let _ = writer.write_all(&event);
            }
        }
        if mask & x11randr::NOTIFY_MASK_CRTC_CHANGE != 0 {
            let event = x11randr::encode_crtc_change_notify_event(
                RANDR_FIRST_EVENT,
                sequence,
                x11randr::CrtcChangeNotify {
                    timestamp,
                    request_window: request_window.0,
                    crtc: crate::randr::CRTC_ID,
                    mode: crate::randr::MODE_ID,
                    x: ev.x,
                    y: ev.y,
                    width,
                    height,
                },
            );
            if let Ok(mut writer) = target.writer.lock() {
                let _ = writer.write_all(&event);
            }
        }
        if mask & x11randr::NOTIFY_MASK_OUTPUT_CHANGE != 0 {
            let event = x11randr::encode_output_change_notify_event(
                RANDR_FIRST_EVENT,
                sequence,
                x11randr::OutputChangeNotify {
                    timestamp,
                    config_timestamp: timestamp,
                    request_window: request_window.0,
                    output: crate::randr::OUTPUT_ID,
                    crtc: crate::randr::CRTC_ID,
                    mode: crate::randr::MODE_ID,
                },
            );
            if let Ok(mut writer) = target.writer.lock() {
                let _ = writer.write_all(&event);
            }
        }
    }
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
                x11::encode_expose_event(buf, seq, order, child, 0, 0, w, h, 0);
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
        emit_xi2_focus_event(server, prev, 10);
    }
    crate::server::emit_window_event(server, window, 0x0020_0000, |buf, seq, order| {
        x11::encode_focus_event(buf, seq, order, true, window);
    });
    emit_xi2_focus_event(server, window, 9);
    Ok(())
}

fn emit_xi2_focus_event(server: &Arc<Mutex<ServerState>>, window: ResourceId, evtype: u16) {
    let targets: Vec<_> = match server.lock() {
        Ok(s) => s
            .clients
            .values()
            .filter(|client| {
                let mask = client
                    .xi2_masks
                    .get(&(window, 3))
                    .or_else(|| client.xi2_masks.get(&(window, 1)))
                    .or_else(|| client.xi2_masks.get(&(window, 0)))
                    .copied()
                    .unwrap_or(0);
                mask & (1 << evtype) != 0
            })
            .map(|client| crate::server::EventTarget {
                writer: client.writer.clone(),
                byte_order: client.byte_order,
                last_sequence: client.last_sequence.clone(),
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    crate::server::fanout_event(&targets, |buf, seq, _order| {
        x11::encode_xi2_focus_event(buf, seq, XI2_MAJOR_OPCODE, evtype, 3, 0, window, 0, 0);
    });
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

/// The two ChangePicture attribute kinds whose value is an XID and therefore
/// needs translation between client and host atom spaces before we can
/// forward the request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChangePictureAttr {
    /// CPAlphaMap (bit 1) — value is a `Picture` XID.
    AlphaMap,
    /// CPClipMask (bit 6) — value is a `Pixmap` XID (or 0 for None).
    ClipMask,
}

/// Translate any XID-valued attributes in a `ChangePicture` `values` slice.
///
/// Walks the encoded values in attribute-bit order; for each attribute whose
/// value is a non-zero XID (`CPAlphaMap` and `CPClipMask`), invokes
/// `translate(attr, value)` to obtain the host XID. Returns a fresh `Vec<u8>`
/// with the host XIDs substituted, or `None` if any translator returns
/// `None` (caller drops the request) or the input is shorter than
/// `value_mask` requires.
///
/// Scalar attributes and explicit `None` (zero) XID values are passed
/// through unchanged.
fn change_picture_translate_xids<F>(
    value_mask: u32,
    values: &[u8],
    mut translate: F,
) -> Option<Vec<u8>>
where
    F: FnMut(ChangePictureAttr, u32) -> Option<u32>,
{
    const CP_ALPHA_MAP: u32 = 1 << 1;
    const CP_CLIP_MASK: u32 = 1 << 6;

    let nvalues = value_mask.count_ones() as usize;
    if values.len() < nvalues * 4 {
        return None;
    }
    let mut out = values[..nvalues * 4].to_vec();
    let mut idx = 0usize;
    for bit in 0..32u32 {
        if value_mask & (1 << bit) == 0 {
            continue;
        }
        let attr = match 1 << bit {
            CP_ALPHA_MAP => Some(ChangePictureAttr::AlphaMap),
            CP_CLIP_MASK => Some(ChangePictureAttr::ClipMask),
            _ => None,
        };
        if let Some(attr) = attr {
            let v = u32::from_le_bytes([
                out[idx * 4],
                out[idx * 4 + 1],
                out[idx * 4 + 2],
                out[idx * 4 + 3],
            ]);
            if v != 0 {
                let host = translate(attr, v)?;
                out[idx * 4..idx * 4 + 4].copy_from_slice(&host.to_le_bytes());
            }
        }
        idx += 1;
    }
    Some(out)
}

#[allow(clippy::too_many_lines)]
fn handle_render_request(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
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
    let lock_host = || -> Option<std::sync::MutexGuard<'_, HostX11>> { host?.lock().ok() };

    match minor {
        // QueryVersion
        0 => {
            let (major, minor_ver) = lock_host()
                .and_then(|mut h| h.render_query_version().ok())
                .unwrap_or((0, 11));
            debug!(
                "client {} #{} RENDER::QueryVersion -> {}.{}",
                client_id.0, sequence.0, major, minor_ver
            );
            x11::write_render_query_version_reply(&mut *lock_writer()?, sequence, major, minor_ver)
        }
        // QueryPictFormats
        1 => {
            debug!(
                "client {} #{} RENDER::QueryPictFormats",
                client_id.0, sequence.0
            );
            x11::write_render_query_pict_formats_reply(
                &mut *lock_writer()?,
                sequence,
                crate::resources::ROOT_VISUAL,
                ARGB_VISUAL,
            )
        }
        // QueryPictIndexValues (minor=2)
        2 => {
            debug!(
                "client {} #{} RENDER::QueryPictIndexValues",
                client_id.0, sequence.0
            );
            x11::write_render_query_pict_index_values_reply(&mut *lock_writer()?, sequence)
        }
        // CreatePicture (minor=4)
        4 => {
            let Some(req) = x11::render_create_picture_request(body) else {
                return Ok(());
            };
            debug!(
                "client {} #{} RENDER::CreatePicture pic=0x{:x} drawable=0x{:x} fmt={}",
                client_id.0, sequence.0, req.picture.0, req.drawable.0, req.format
            );
            let (host_drawable_xid, x_off, y_off) = {
                let s = lock_server(server)?;
                match s.resources.host_drawable_target(req.drawable) {
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
                }
            };
            if host_drawable_xid.is_none() {
                debug!(
                    "client {} #{} RENDER::CreatePicture: drawable 0x{:x} has no host backing — picture pic=0x{:x} dropped",
                    client_id.0, sequence.0, req.drawable.0, req.picture.0
                );
            }
            let host_pic = host_drawable_xid.and_then(|host_drawable| {
                lock_host().map(|mut h| {
                    let xid = h.allocate_xid();
                    let _ = h.render_create_picture(
                        xid,
                        host_drawable,
                        req.format,
                        req.value_mask,
                        &req.values,
                    );
                    xid
                })
            });
            if let Some(host_pic) = host_pic {
                let mut s = lock_server(server)?;
                s.resources.create_picture(
                    req.picture,
                    PictureState {
                        client: client_id,
                        host_picture_xid: host_pic,
                        host_owned_pixmap: None,
                        x_offset: x_off,
                        y_offset: y_off,
                    },
                );
            }
            Ok(())
        }
        // ChangePicture (minor=5): translate XID attributes (CPAlphaMap,
        // CPClipMask) from client to host atom space, then forward.
        // CPClipMask=None (mask=0x40, value=0) is critical — without
        // forwarding, stale clips persist and cause CompositeGlyphs8 to
        // be clipped to a tiny rectangle on subsequent redraws. CPClipMask
        // = pixmap and CPAlphaMap = picture used to be dropped because we
        // hadn't wired XID translation; modern Xft text rendering and
        // shadow-text effects exercise these.
        5 => {
            if body.len() < 8 {
                return Ok(());
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            let value_mask = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);

            // Snapshot host XIDs for any pixmaps / pictures the values
            // reference. We do this up-front under a single server lock so
            // the translator closure stays cheap and synchronous.
            let translated = {
                let s = lock_server(server)?;
                change_picture_translate_xids(value_mask, &body[8..], |attr, xid| {
                    let resource = ResourceId(xid);
                    match attr {
                        ChangePictureAttr::ClipMask => {
                            s.resources.pixmap(resource).and_then(|p| p.host_xid)
                        }
                        ChangePictureAttr::AlphaMap => {
                            s.resources.picture(resource).map(|p| p.host_picture_xid)
                        }
                    }
                })
            };

            let Some(translated_values) = translated else {
                debug!(
                    "client {} #{} RENDER::ChangePicture pic=0x{:x} mask=0x{:x} dropped (XID translation failed)",
                    client_id.0, sequence.0, pic_id.0, value_mask,
                );
                return Ok(());
            };

            // Reassemble the body with the patched values slice.
            let mut patched = Vec::with_capacity(8 + translated_values.len());
            patched.extend_from_slice(&body[..8]);
            patched.extend_from_slice(&translated_values);

            debug!(
                "client {} #{} RENDER::ChangePicture pic=0x{:x} mask=0x{:x} forwarded",
                client_id.0, sequence.0, pic_id.0, value_mask
            );

            let host_pic = lock_server(server)?
                .resources
                .picture(pic_id)
                .map(|p| p.host_picture_xid);
            if let (Some(hp), Some(mut h)) = (host_pic, lock_host()) {
                let _ = h.render_change_picture(hp, &patched);
            }
            Ok(())
        }
        // Composite (minor=8): src + mask -> dst at (dst_x, dst_y)
        8 => {
            let Some(req) = x11::render_composite_request(body) else {
                return Ok(());
            };
            let (host_src, host_mask, host_dst, dst_x_off, dst_y_off) = {
                let s = lock_server(server)?;
                let host_src = s.resources.picture(req.src).map(|p| p.host_picture_xid);
                // mask is optional; xid 0 means None
                let host_mask = if req.mask.0 == 0 {
                    Some(0)
                } else {
                    s.resources.picture(req.mask).map(|p| p.host_picture_xid)
                };
                let (host_dst, x_off, y_off) =
                    s.resources.picture(req.dst).map_or((None, 0, 0), |p| {
                        (Some(p.host_picture_xid), p.x_offset, p.y_offset)
                    });
                (host_src, host_mask, host_dst, x_off, y_off)
            };
            debug!(
                "client {} #{} RENDER::Composite op={} src=0x{:x}->{:?} mask=0x{:x}->{:?} dst=0x{:x}->{:?} dst_xy=({},{}) size={}x{}",
                client_id.0,
                sequence.0,
                req.op,
                req.src.0,
                host_src,
                req.mask.0,
                host_mask,
                req.dst.0,
                host_dst,
                req.dst_x,
                req.dst_y,
                req.width,
                req.height
            );
            if let (Some(host_src), Some(host_mask), Some(host_dst), Some(mut h)) =
                (host_src, host_mask, host_dst, lock_host())
            {
                let _ = h.render_composite(
                    req.op,
                    host_src,
                    host_mask,
                    host_dst,
                    req.src_x,
                    req.src_y,
                    req.mask_x,
                    req.mask_y,
                    req.dst_x.wrapping_add(dst_x_off),
                    req.dst_y.wrapping_add(dst_y_off),
                    req.width,
                    req.height,
                );
            } else {
                debug!(
                    "client {} #{} RENDER::Composite SKIPPED (host_src={:?} host_mask={:?} host_dst={:?})",
                    client_id.0, sequence.0, host_src, host_mask, host_dst
                );
            }
            Ok(())
        }
        // Trapezoids (minor=10) — anti-aliased trapezoid list.
        // body: op(1) pad(3) src(4) dst(4) mask_format(4) src_xy(4) traps(N*40)
        10 => {
            if body.len() < 20 {
                return Ok(());
            }
            let op = body[0];
            let src = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
            let dst = ResourceId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
            let ynest_mask_format = u32::from_le_bytes([body[12], body[13], body[14], body[15]]);
            let src_x = i16::from_le_bytes([body[16], body[17]]);
            let src_y = i16::from_le_bytes([body[18], body[19]]);
            let traps = &body[20..];

            let (host_src, host_dst, dst_x_off, dst_y_off, host_mask_format) = {
                let s = lock_server(server)?;
                let host_src = s.resources.picture(src).map(|p| p.host_picture_xid);
                let (host_dst, x_off, y_off) = s.resources.picture(dst).map_or((None, 0, 0), |p| {
                    (Some(p.host_picture_xid), p.x_offset, p.y_offset)
                });
                let host_fmt = if ynest_mask_format == 0 {
                    Some(0u32)
                } else {
                    drop(s);
                    lock_host().and_then(|h| h.render_format_for_ynest_id(ynest_mask_format))
                };
                (host_src, host_dst, x_off, y_off, host_fmt)
            };

            debug!(
                "client {} #{} RENDER::Trapezoids op={} src=0x{:x}->{:?} dst=0x{:x}->{:?} traps={}",
                client_id.0,
                sequence.0,
                op,
                src.0,
                host_src,
                dst.0,
                host_dst,
                traps.len() / 40
            );
            if let (Some(host_src), Some(host_dst), Some(host_mask_fmt), Some(mut h)) =
                (host_src, host_dst, host_mask_format, lock_host())
            {
                let _ = h.render_trapezoids(
                    op,
                    host_src,
                    host_dst,
                    host_mask_fmt,
                    src_x,
                    src_y,
                    traps,
                    dst_x_off,
                    dst_y_off,
                );
            }
            Ok(())
        }
        // FreePicture (minor=7)
        7 => {
            let Some(pic_id) = x11::render_free_resource_id(body) else {
                return Ok(());
            };
            debug!(
                "client {} #{} RENDER::FreePicture pic=0x{:x}",
                client_id.0, sequence.0, pic_id.0
            );
            let state = {
                let mut s = lock_server(server)?;
                s.resources.free_picture(pic_id)
            };
            if let (Some(state), Some(mut h)) = (state, lock_host()) {
                let _ = h.render_free_picture(state.host_picture_xid);
                if let Some(pix) = state.host_owned_pixmap {
                    let _ = h.free_pixmap(pix);
                }
            }
            Ok(())
        }
        // CreateGlyphSet (minor=17)
        17 => {
            let Some((gs_id, fmt)) = x11::render_create_glyphset_request(body) else {
                return Ok(());
            };
            debug!(
                "client {} #{} RENDER::CreateGlyphSet gs=0x{:x} fmt={}",
                client_id.0, sequence.0, gs_id.0, fmt
            );
            let host_gs = lock_host().map(|mut h| {
                let xid = h.allocate_xid();
                let _ = h.render_create_glyphset(xid, fmt);
                xid
            });
            if let Some(host_gs) = host_gs {
                let mut s = lock_server(server)?;
                s.resources.create_glyphset(
                    gs_id,
                    GlyphSetState {
                        client: client_id,
                        host_glyphset_xid: host_gs,
                    },
                );
            }
            Ok(())
        }
        // ReferenceGlyphSet (minor=18)
        18 => {
            let Some((new_glyphset, existing)) = x11::render_reference_glyphset_request(body)
            else {
                return Ok(());
            };
            debug!(
                "client {} #{} RENDER::ReferenceGlyphSet new=0x{:x} existing=0x{:x}",
                client_id.0, sequence.0, new_glyphset.0, existing.0
            );
            let mut s = lock_server(server)?;
            let _ = s
                .resources
                .reference_glyphset(client_id, new_glyphset, existing);
            Ok(())
        }
        // FreeGlyphSet (minor=19)
        19 => {
            let Some(gs_id) = x11::render_free_resource_id(body) else {
                return Ok(());
            };
            debug!(
                "client {} #{} RENDER::FreeGlyphSet gs=0x{:x}",
                client_id.0, sequence.0, gs_id.0
            );
            let state = {
                let mut s = lock_server(server)?;
                s.resources.free_glyphset(gs_id)
            };
            if let (Some(state), Some(mut h)) = (state, lock_host()) {
                let _ = h.render_free_glyphset(state.host_glyphset_xid);
            }
            Ok(())
        }
        // AddGlyphs (minor=20)
        20 => {
            let Some((gs_id, tail)) = x11::render_add_glyphs_request(body) else {
                return Ok(());
            };
            debug!(
                "client {} #{} RENDER::AddGlyphs gs=0x{:x}",
                client_id.0, sequence.0, gs_id.0
            );
            let host_gs = {
                let s = lock_server(server)?;
                s.resources.glyphset(gs_id).map(|g| g.host_glyphset_xid)
            };
            if let (Some(host_gs), Some(mut h)) = (host_gs, lock_host()) {
                let _ = h.render_add_glyphs(host_gs, &tail);
            }
            Ok(())
        }
        // FreeGlyphs (minor=22)
        22 => {
            let Some((gs_id, glyph_ids)) = x11::render_free_glyphs_request(body) else {
                return Ok(());
            };
            debug!(
                "client {} #{} RENDER::FreeGlyphs gs=0x{:x} glyphs={}",
                client_id.0,
                sequence.0,
                gs_id.0,
                glyph_ids.len() / 4
            );
            let host_gs = {
                let s = lock_server(server)?;
                s.resources.glyphset(gs_id).map(|g| g.host_glyphset_xid)
            };
            if let (Some(host_gs), Some(mut h)) = (host_gs, lock_host()) {
                let _ = h.render_free_glyphs(host_gs, &glyph_ids);
            }
            Ok(())
        }
        // CompositeGlyphs8/16/32 (minor=23/24/25)
        23..=25 => {
            let Some(req) = x11::render_composite_glyphs_request(body) else {
                return Ok(());
            };
            debug!(
                "client {} #{} RENDER::CompositeGlyphs{} dst=0x{:x}",
                client_id.0,
                sequence.0,
                match minor {
                    23 => "8",
                    24 => "16",
                    _ => "32",
                },
                req.dst.0
            );
            let (host_src, host_dst, host_gs, x_off, y_off) = {
                let s = lock_server(server)?;
                let host_src = s.resources.picture(req.src).map(|p| p.host_picture_xid);
                let (host_dst, x_off, y_off) =
                    s.resources.picture(req.dst).map_or((None, 0, 0), |p| {
                        (Some(p.host_picture_xid), p.x_offset, p.y_offset)
                    });
                let host_gs = s
                    .resources
                    .glyphset(req.glyphset)
                    .map(|g| g.host_glyphset_xid);
                (host_src, host_dst, host_gs, x_off, y_off)
            };
            if let (Some(host_src), Some(host_dst), Some(host_gs), Some(mut h)) =
                (host_src, host_dst, host_gs, lock_host())
            {
                // mask_format is a PICTFORMAT id (1-4 in ynest), not a picture resource id
                let mask_fmt = if req.mask_format == 0 {
                    0
                } else {
                    h.render_format_for_ynest_id(req.mask_format).unwrap_or(0)
                };
                let _ = h.render_composite_glyphs(
                    minor, req.op, host_src, host_dst, mask_fmt, host_gs, req.src_x, req.src_y,
                    &req.items, x_off, y_off,
                );
            }
            Ok(())
        }
        // FillRectangles (minor=26)
        26 => {
            let Some(req) = x11::render_fill_rectangles_request(body) else {
                return Ok(());
            };
            debug!(
                "client {} #{} RENDER::FillRectangles dst=0x{:x}",
                client_id.0, sequence.0, req.dst.0
            );
            let (host_dst, x_off, y_off) = {
                let s = lock_server(server)?;
                s.resources.picture(req.dst).map_or((None, 0, 0), |p| {
                    (Some(p.host_picture_xid), p.x_offset, p.y_offset)
                })
            };
            if let (Some(host_dst), Some(mut h)) = (host_dst, lock_host()) {
                let _ =
                    h.render_fill_rectangles(host_dst, req.op, req.color, &req.rects, x_off, y_off);
            }
            Ok(())
        }
        // CreateSolidFill (minor=33)
        33 => {
            let Some((pic_id, color)) = x11::render_create_solid_fill_request(body) else {
                return Ok(());
            };
            debug!(
                "client {} #{} RENDER::CreateSolidFill pic=0x{:x}",
                client_id.0, sequence.0, pic_id.0
            );
            let host_pic = lock_host().map(|mut h| {
                let xid = h.allocate_xid();
                let _ = h.render_create_solid_fill(xid, color);
                xid
            });
            if let Some(host_pic) = host_pic {
                let mut s = lock_server(server)?;
                s.resources.create_picture(
                    pic_id,
                    PictureState {
                        client: client_id,
                        host_picture_xid: host_pic,
                        host_owned_pixmap: None,
                        x_offset: 0,
                        y_offset: 0,
                    },
                );
            }
            Ok(())
        }
        // CreateCursor (minor=27): create a cursor from a RENDER picture
        27 => {
            if body.len() < 12 {
                return Ok(());
            }
            let cursor_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            let src_pic_id = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
            let x = u16::from_le_bytes([body[8], body[9]]);
            let y = u16::from_le_bytes([body[10], body[11]]);
            debug!(
                "client {} #{} RENDER::CreateCursor cur=0x{:x} src=0x{:x} x={} y={}",
                client_id.0, sequence.0, cursor_id.0, src_pic_id.0, x, y
            );
            let host_src = {
                let s = lock_server(server)?;
                s.resources.picture(src_pic_id).map(|p| p.host_picture_xid)
            };
            if let Some(host_src) = host_src
                && let Some(mut h) = lock_host()
            {
                let cursor_xid = h.allocate_xid();
                let _ = h.render_create_cursor(cursor_xid, host_src, x, y);
                drop(h);
                let mut s = lock_server(server)?;
                s.resources.create_glyph_cursor(client_id, cursor_id);
                s.resources.set_cursor_host_xid(cursor_id, cursor_xid);
            }
            Ok(())
        }
        // SetPictureTransform (minor=28): picture(4) + 3×3 FIXED matrix (36 bytes)
        28 => {
            if body.len() < 40 {
                return Ok(());
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            let host_pic = lock_server(server)?
                .resources
                .picture(pic_id)
                .map(|p| p.host_picture_xid);
            debug!(
                "client {} #{} RENDER::SetPictureTransform pic=0x{:x} host={:?}",
                client_id.0, sequence.0, pic_id.0, host_pic
            );
            if let (Some(hp), Some(mut h)) = (host_pic, lock_host()) {
                let _ = h.render_set_picture_transform(hp, body);
            }
            Ok(())
        }
        // QueryFilters (minor=29)
        29 => {
            debug!(
                "client {} #{} RENDER::QueryFilters",
                client_id.0, sequence.0
            );
            x11::write_render_query_filters_reply(&mut *lock_writer()?, sequence)
        }
        // SetPictureFilter (minor=30): picture(4) + nbytes(2) + pad(2) + name + values
        30 => {
            if body.len() < 8 {
                return Ok(());
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            let host_pic = lock_server(server)?
                .resources
                .picture(pic_id)
                .map(|p| p.host_picture_xid);
            debug!(
                "client {} #{} RENDER::SetPictureFilter pic=0x{:x} host={:?}",
                client_id.0, sequence.0, pic_id.0, host_pic
            );
            if let (Some(hp), Some(mut h)) = (host_pic, lock_host()) {
                let _ = h.render_set_picture_filter(hp, body);
            }
            Ok(())
        }
        // SetPictureClipRectangles (minor=6): picture(4) + clip_x(INT16) + clip_y(INT16) + rects[]
        // Clip coords are in drawable-local space; must add the picture's window offset so they
        // align with Composite's dst_x/dst_y which are similarly adjusted.
        6 => {
            if body.len() < 8 {
                return Ok(());
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            let (host_pic, x_off, y_off) = lock_server(server)?
                .resources
                .picture(pic_id)
                .map_or((None, 0i16, 0i16), |p| {
                    (Some(p.host_picture_xid), p.x_offset, p.y_offset)
                });
            if let (Some(hp), Some(mut h)) = (host_pic, lock_host()) {
                debug!(
                    "client {} #{} RENDER::SetPictureClipRectangles pic=0x{:x} host={:?} off=({},{})",
                    client_id.0, sequence.0, pic_id.0, host_pic, x_off, y_off
                );
                if x_off == 0 && y_off == 0 {
                    let _ = h.render_set_picture_clip_rectangles(hp, body);
                } else {
                    let clip_x = i16::from_le_bytes([body[4], body[5]]).wrapping_add(x_off);
                    let clip_y = i16::from_le_bytes([body[6], body[7]]).wrapping_add(y_off);
                    let mut adj = body.to_vec();
                    adj[4..6].copy_from_slice(&clip_x.to_le_bytes());
                    adj[6..8].copy_from_slice(&clip_y.to_le_bytes());
                    let _ = h.render_set_picture_clip_rectangles(hp, &adj);
                }
            }
            Ok(())
        }
        // CreateLinearGradient (minor=34): picture(4) + p1(8) + p2(8) + num_stops(4) + data
        34 => {
            if body.len() < 24 {
                return Ok(());
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            debug!(
                "client {} #{} RENDER::CreateLinearGradient pic=0x{:x}",
                client_id.0, sequence.0, pic_id.0
            );
            let host_pic = lock_host().map(|mut h| {
                let xid = h.allocate_xid();
                let _ = h.render_create_linear_gradient(xid, body);
                xid
            });
            if let Some(host_pic) = host_pic {
                lock_server(server)?.resources.create_picture(
                    pic_id,
                    PictureState {
                        client: client_id,
                        host_picture_xid: host_pic,
                        host_owned_pixmap: None,
                        x_offset: 0,
                        y_offset: 0,
                    },
                );
            }
            Ok(())
        }
        // CreateAnimCursor (minor=31): no-op for now. The request is void, and
        // existing cursor paths use core CreateCursor or RENDER::CreateCursor.
        31 => {
            debug!(
                "client {} #{} RENDER::CreateAnimCursor (stub)",
                client_id.0, sequence.0
            );
            Ok(())
        }
        // AddTraps (minor=32): intentionally not implemented yet.
        32 => {
            debug!(
                "client {} #{} RENDER::AddTraps (stub)",
                client_id.0, sequence.0
            );
            Ok(())
        }
        // CreateRadialGradient (minor=35): picture(4) + inner_center(8) + outer_center(8) + radii(8) + num_stops(4) + data
        35 => {
            if body.len() < 32 {
                return Ok(());
            }
            let pic_id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            debug!(
                "client {} #{} RENDER::CreateRadialGradient pic=0x{:x}",
                client_id.0, sequence.0, pic_id.0
            );
            let host_pic = lock_host().map(|mut h| {
                let xid = h.allocate_xid();
                let _ = h.render_create_radial_gradient(xid, body);
                xid
            });
            if let Some(host_pic) = host_pic {
                lock_server(server)?.resources.create_picture(
                    pic_id,
                    PictureState {
                        client: client_id,
                        host_picture_xid: host_pic,
                        host_owned_pixmap: None,
                        x_offset: 0,
                        y_offset: 0,
                    },
                );
            }
            Ok(())
        }
        // CreateConicalGradient (minor=36): known RENDER request, not used by
        // current validation targets. Keep explicit so it is not confused with
        // CreateRadialGradient.
        36 => {
            debug!(
                "client {} #{} RENDER::CreateConicalGradient (stub)",
                client_id.0, sequence.0
            );
            Ok(())
        }
        _ => {
            debug!(
                "client {} #{} RENDER::unknown minor={}",
                client_id.0, sequence.0, minor
            );
            Ok(())
        }
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
        x11randr::RR_GET_OUTPUT_PROPERTY => {
            debug!(
                "client {} #{} RANDR::GetOutputProperty -> not found",
                client_id.0, sequence.0
            );
            let buf = x11randr::encode_get_output_property_reply(sequence);
            lock_writer()?.write_all(&buf)
        }
        x11randr::RR_SELECT_INPUT => {
            if let Some(req) = x11randr::parse_select_input(body) {
                let mut s = lock_server(server)?;
                if req.enable == 0 {
                    s.randr_select_masks
                        .remove(&(client_id.0, ResourceId(req.window)));
                } else {
                    s.randr_select_masks
                        .insert((client_id.0, ResourceId(req.window)), req.enable);
                }
            }
            debug!("client {} #{} RANDR::SelectInput", client_id.0, sequence.0);
            Ok(())
        }
        x11randr::RR_GET_SCREEN_INFO => {
            // Legacy RANDR 1.0/1.1 query. Old clients (e16) call this and
            // block waiting for the reply, so a missing handler hangs the
            // session at startup. Reply with the single synthetic
            // mode + 60Hz.
            debug!(
                "client {} #{} RANDR::GetScreenInfo",
                client_id.0, sequence.0
            );
            let (timestamp, config_timestamp, width, height, mwidth, mheight) = {
                let s = lock_server(server)?;
                (
                    s.randr.timestamp,
                    s.randr.config_timestamp,
                    s.randr.screen_width,
                    s.randr.screen_height,
                    u16::try_from(s.randr.width_mm).unwrap_or(u16::MAX),
                    u16::try_from(s.randr.height_mm).unwrap_or(u16::MAX),
                )
            };
            let buf = x11randr::encode_get_screen_info_reply(
                sequence,
                ROOT_WINDOW.0,
                timestamp,
                config_timestamp,
                width,
                height,
                mwidth,
                mheight,
            );
            lock_writer()?.write_all(&buf)
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

fn normalize_region_rects(mut rects: Vec<x11xfixes::RegionRect>) -> Vec<x11xfixes::RegionRect> {
    const MAX_RECTS: usize = 4096;
    rects.retain(|rect| !rect.is_empty());
    rects.truncate(MAX_RECTS);
    rects
}

fn region_extents(rects: &[x11xfixes::RegionRect]) -> x11xfixes::RegionRect {
    if rects.is_empty() {
        return x11xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    }
    let mut x1 = i32::from(rects[0].x);
    let mut y1 = i32::from(rects[0].y);
    let mut x2 = i32::from(rects[0].x) + i32::from(rects[0].width);
    let mut y2 = i32::from(rects[0].y) + i32::from(rects[0].height);
    for rect in &rects[1..] {
        x1 = x1.min(i32::from(rect.x));
        y1 = y1.min(i32::from(rect.y));
        x2 = x2.max(i32::from(rect.x) + i32::from(rect.width));
        y2 = y2.max(i32::from(rect.y) + i32::from(rect.height));
    }
    x11xfixes::RegionRect {
        x: x1.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
        y: y1.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
        width: (x2 - x1).clamp(0, i32::from(u16::MAX)) as u16,
        height: (y2 - y1).clamp(0, i32::from(u16::MAX)) as u16,
    }
}

fn intersect_rect(
    a: x11xfixes::RegionRect,
    b: x11xfixes::RegionRect,
) -> Option<x11xfixes::RegionRect> {
    let x1 = i32::from(a.x).max(i32::from(b.x));
    let y1 = i32::from(a.y).max(i32::from(b.y));
    let x2 = (i32::from(a.x) + i32::from(a.width)).min(i32::from(b.x) + i32::from(b.width));
    let y2 = (i32::from(a.y) + i32::from(a.height)).min(i32::from(b.y) + i32::from(b.height));
    if x2 <= x1 || y2 <= y1 {
        return None;
    }
    Some(x11xfixes::RegionRect {
        x: x1.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
        y: y1.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
        width: (x2 - x1).clamp(0, i32::from(u16::MAX)) as u16,
        height: (y2 - y1).clamp(0, i32::from(u16::MAX)) as u16,
    })
}

fn intersect_regions(
    a: &[x11xfixes::RegionRect],
    b: &[x11xfixes::RegionRect],
) -> Vec<x11xfixes::RegionRect> {
    let mut out = Vec::new();
    for ar in a {
        for br in b {
            if let Some(rect) = intersect_rect(*ar, *br) {
                out.push(rect);
            }
        }
    }
    normalize_region_rects(out)
}

fn translate_region(rects: &mut [x11xfixes::RegionRect], dx: i16, dy: i16) {
    for rect in rects {
        rect.x = rect.x.saturating_add(dx);
        rect.y = rect.y.saturating_add(dy);
    }
}

/// Handle `GetAtomName` (opcode 17). Atom IDs in our protocol stream can come
/// from host-proxied replies (notably the `FONTPROP` atoms inside
/// `ListFontsWithInfo`), so a client can legitimately ask us about an atom
/// we never interned ourselves. Fall back to the host before returning
/// `BadAtom`, otherwise e16 sees an atom in a font property reply, calls
/// `XGetAtomName` on it, gets `BadAtom`, and exits.
///
/// The `host_lookup` closure is the test seam — production callers pass a
/// closure that forwards to `host_x11::get_atom_name` over the host stream.
fn handle_get_atom_name_with_host_lookup<F>(
    server: &Arc<Mutex<ServerState>>,
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    atom: AtomId,
    host_lookup: F,
) -> io::Result<()>
where
    F: FnOnce(u32) -> Option<String>,
{
    let local = {
        let s = lock_server(server)?;
        s.atoms.name(atom).map(str::to_owned)
    };
    let name = local.or_else(|| host_lookup(atom.0));
    match name {
        Some(name) => {
            let mut w = writer.lock().map_err(|_| {
                io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned")
            })?;
            x11::write_get_atom_name_reply(&mut *w, sequence, &name)
        }
        None => emit_x11_error(writer, sequence, x11::error::BAD_ATOM, atom.0, 17),
    }
}

fn handle_get_atom_name(
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    atom: AtomId,
) -> io::Result<()> {
    handle_get_atom_name_with_host_lookup(server, writer, sequence, atom, |atom_id| {
        let host = host?;
        let mut h = host.lock().ok()?;
        h.get_atom_name(atom_id).ok().flatten()
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_mit_shm_request(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    minor: u8,
    body: &[u8],
    attached_fd: Option<std::os::fd::RawFd>,
) -> io::Result<()> {
    use yserver_protocol::x11::mit_shm as shm;
    let lock_writer = || -> io::Result<std::sync::MutexGuard<'_, UnixStream>> {
        writer
            .lock()
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))
    };
    debug!(
        "client {} #{} MIT-SHM dispatch minor={minor} body_len={}",
        client_id.0,
        sequence.0,
        body.len()
    );

    match minor {
        shm::QUERY_VERSION => {
            // We do not implement live shared pixmaps (Option A in the design).
            // Tell clients explicitly so toolkits fall back to MIT-SHM PutImage
            // for repeated uploads instead of relying on shared-pixmap liveness.
            debug!(
                "client {} #{} MIT-SHM::QueryVersion -> {}.{} shared_pixmaps=false",
                client_id.0,
                sequence.0,
                shm::MAJOR_VERSION,
                shm::MINOR_VERSION
            );
            let reply = shm::encode_query_version_reply(sequence, false);
            lock_writer()?.write_all(&reply)
        }
        shm::ATTACH => {
            let Some(req) = shm::parse_attach(body) else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            match crate::server::MitShmSegment::from_shmid(client_id, req.shmid, req.read_only) {
                Ok(segment) => {
                    let mut s = lock_server(server)?;
                    s.mit_shm_segments.insert(req.shmseg, segment);
                    debug!(
                        "client {} #{} MIT-SHM::Attach shmseg=0x{:x} shmid=0x{:x} read_only={}",
                        client_id.0, sequence.0, req.shmseg, req.shmid, req.read_only
                    );
                    Ok(())
                }
                Err(err) => {
                    debug!(
                        "client {} #{} MIT-SHM::Attach failed: {err}",
                        client_id.0, sequence.0
                    );
                    emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_VALUE,
                        req.shmseg,
                        MIT_SHM_MAJOR_OPCODE,
                    )
                }
            }
        }
        shm::ATTACH_FD => {
            let Some(req) = shm::parse_attach_fd(body) else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            let Some(fd) = attached_fd else {
                debug!(
                    "client {} #{} MIT-SHM::AttachFd shmseg=0x{:x} arrived without an FD",
                    client_id.0, sequence.0, req.shmseg
                );
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_VALUE,
                    req.shmseg,
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            match crate::server::MitShmSegment::from_fd(client_id, fd, req.read_only) {
                Ok(segment) => {
                    let mut s = lock_server(server)?;
                    s.mit_shm_segments.insert(req.shmseg, segment);
                    debug!(
                        "client {} #{} MIT-SHM::AttachFd shmseg=0x{:x} read_only={}",
                        client_id.0, sequence.0, req.shmseg, req.read_only
                    );
                    Ok(())
                }
                Err(err) => {
                    debug!(
                        "client {} #{} MIT-SHM::AttachFd failed: {err}",
                        client_id.0, sequence.0
                    );
                    emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_VALUE,
                        req.shmseg,
                        MIT_SHM_MAJOR_OPCODE,
                    )
                }
            }
        }
        shm::DETACH => {
            if let Some(shmseg) = shm::parse_detach(body) {
                lock_server(server)?.mit_shm_segments.remove(&shmseg);
                debug!(
                    "client {} #{} MIT-SHM::Detach shmseg=0x{:x}",
                    client_id.0, sequence.0, shmseg
                );
            }
            Ok(())
        }
        shm::CREATE_PIXMAP => {
            let Some(req) = shm::parse_create_pixmap(body) else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            handle_mit_shm_create_pixmap(client_id, server, host, writer, sequence, req)
        }
        shm::PUT_IMAGE => {
            let Some(req) = shm::parse_put_image(body) else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            handle_mit_shm_put_image(client_id, server, host, writer, sequence, req)
        }
        shm::GET_IMAGE => {
            let Some(req) = shm::parse_get_image(body) else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            handle_mit_shm_get_image(client_id, server, host, writer, sequence, req)
        }
        shm::CREATE_SEGMENT => {
            let Some(req) = shm::parse_create_segment(body) else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    MIT_SHM_MAJOR_OPCODE,
                );
            };
            handle_mit_shm_create_segment(client_id, server, writer, sequence, req)
        }
        other => {
            debug!(
                "client {} #{} MIT-SHM::unknown minor={other}",
                client_id.0, sequence.0
            );
            Ok(())
        }
    }
}

/// Handle `MIT-SHM::CreateSegment` (minor 7). The server allocates a
/// memfd of `size` bytes and sends the descriptor back to the client
/// via `SCM_RIGHTS` in the reply. The client then mmaps it directly,
/// just as if it had called `AttachFd` after `memfd_create`.
fn handle_mit_shm_create_segment(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    req: yserver_protocol::x11::mit_shm::CreateSegmentRequest,
) -> io::Result<()> {
    if req.size == 0 {
        return emit_x11_error(
            writer,
            sequence,
            x11::error::BAD_VALUE,
            req.shmseg,
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    // memfd_create with MFD_CLOEXEC for our own copy; the dup we send
    // via SCM_RIGHTS is delivered without CLOEXEC so the client gets a
    // normal fd it can mmap.
    let fd = unsafe { libc::memfd_create(c"yserver-shm".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        debug!(
            "client {} #{} MIT-SHM::CreateSegment memfd_create failed",
            client_id.0, sequence.0
        );
        return emit_x11_error(
            writer,
            sequence,
            x11::error::BAD_ALLOC,
            req.shmseg,
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    // Truncate to the requested size so mmap on either end has backing.
    if unsafe { libc::ftruncate(fd, libc::off_t::from(req.size as i32)) } < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        debug!(
            "client {} #{} MIT-SHM::CreateSegment ftruncate({}) failed: {err}",
            client_id.0, sequence.0, req.size
        );
        return emit_x11_error(
            writer,
            sequence,
            x11::error::BAD_ALLOC,
            req.shmseg,
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    // Dup so we have one fd for our own MitShmSegment (which mmaps it
    // and closes on Drop) and a separate fd to send to the client.
    let fd_for_client = unsafe { libc::dup(fd) };
    if fd_for_client < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        debug!(
            "client {} #{} MIT-SHM::CreateSegment dup failed: {err}",
            client_id.0, sequence.0
        );
        return emit_x11_error(
            writer,
            sequence,
            x11::error::BAD_ALLOC,
            req.shmseg,
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    let segment = match crate::server::MitShmSegment::from_fd(client_id, fd, req.read_only) {
        Ok(s) => s,
        Err(err) => {
            unsafe { libc::close(fd_for_client) };
            // `from_fd` already closed its `fd` on failure.
            debug!(
                "client {} #{} MIT-SHM::CreateSegment mmap failed: {err}",
                client_id.0, sequence.0
            );
            return emit_x11_error(
                writer,
                sequence,
                x11::error::BAD_ALLOC,
                req.shmseg,
                MIT_SHM_MAJOR_OPCODE,
            );
        }
    };
    {
        let mut s = lock_server(server)?;
        s.mit_shm_segments.insert(req.shmseg, segment);
    }
    // Send the reply along with `fd_for_client` via SCM_RIGHTS. The
    // kernel duplicates the fd into the client's table; we then close
    // our copy.
    let reply = yserver_protocol::x11::mit_shm::encode_create_segment_reply(sequence);
    let send_res = {
        let mut w = writer
            .lock()
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))?;
        crate::unix_fd::send_with_fd(&mut w, &reply, fd_for_client)
    };
    unsafe { libc::close(fd_for_client) };
    debug!(
        "client {} #{} MIT-SHM::CreateSegment shmseg=0x{:x} size={} read_only={}",
        client_id.0, sequence.0, req.shmseg, req.size, req.read_only
    );
    send_res
}

fn handle_mit_shm_create_pixmap(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    req: yserver_protocol::x11::mit_shm::CreatePixmapRequest,
) -> io::Result<()> {
    debug!(
        "client {} #{} MIT-SHM::CreatePixmap pid=0x{:x} drawable=0x{:x} {}x{} d{} shmseg=0x{:x}+{}",
        client_id.0,
        sequence.0,
        req.pid,
        req.drawable,
        req.width,
        req.height,
        req.depth,
        req.shmseg,
        req.offset,
    );
    // Validate ownership of the new pixmap XID.
    let (validation_failed, drawable_exists) = {
        let s = lock_server(server)?;
        let handle = s.clients.get(&client_id.0).expect("client registered");
        let owned = crate::server::IdAllocator::validate_owned(
            req.pid,
            handle.resource_id_base,
            handle.resource_id_mask,
        );
        let in_use = s.resources.any_resource_exists(ResourceId(req.pid));
        let drawable_exists = s.resources.window(ResourceId(req.drawable)).is_some()
            || s.resources.pixmap(ResourceId(req.drawable)).is_some();
        (!owned || in_use, drawable_exists)
    };
    if validation_failed {
        return emit_x11_error(
            writer,
            sequence,
            x11::error::BAD_ID_CHOICE,
            req.pid,
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    if !drawable_exists {
        return emit_x11_error(
            writer,
            sequence,
            x11::error::BAD_DRAWABLE,
            req.drawable,
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    if !supported_pixmap_depth(req.depth) {
        return emit_x11_error(
            writer,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(req.depth),
            MIT_SHM_MAJOR_OPCODE,
        );
    }
    let Some(expected_len) = zpixmap_expected_len(req.width, req.height, req.depth) else {
        return emit_x11_error(
            writer,
            sequence,
            x11::error::BAD_VALUE,
            req.shmseg,
            MIT_SHM_MAJOR_OPCODE,
        );
    };

    // Snapshot the segment bytes now (Option A — `shared_pixmaps=false`),
    // create a regular host pixmap, PutImage the bytes into it, then register
    // the local Pixmap pointing at the host xid. From here on it behaves like
    // any other CreatePixmap.
    let host_xid = if let Some(host) = host
        && let Ok(mut host) = host.lock()
    {
        let xid = host.allocate_xid();
        match host.create_pixmap(xid, req.depth, req.width, req.height) {
            Ok(()) => Some(xid),
            Err(err) => {
                warn!(
                    "client {} MIT-SHM::CreatePixmap host CreatePixmap failed: {err}",
                    client_id.0
                );
                None
            }
        }
    } else {
        None
    };

    // Lift the bytes from the segment.
    let snapshot: Vec<u8> = {
        let s = lock_server(server)?;
        let Some(segment) = s.mit_shm_segments.get(&req.shmseg) else {
            return emit_x11_error(
                writer,
                sequence,
                x11::error::BAD_VALUE,
                req.shmseg,
                MIT_SHM_MAJOR_OPCODE,
            );
        };
        let bytes = segment.as_slice();
        let start = req.offset as usize;
        let end = start.saturating_add(expected_len);
        if end > bytes.len() {
            return emit_x11_error(
                writer,
                sequence,
                x11::error::BAD_VALUE,
                req.offset,
                MIT_SHM_MAJOR_OPCODE,
            );
        }
        bytes[start..end].to_vec()
    };

    if let (Some(host), Some(host_xid)) = (host, host_xid)
        && let Ok(mut host) = host.lock()
    {
        // No client GC for MIT-SHM CreatePixmap snapshot — clear any
        // leftover clip-mask before the synthetic put_image.
        let _ = host.clear_clip_rectangles();
        if let Err(err) =
            host.put_image(host_xid, req.depth, req.width, req.height, 0, 0, &snapshot)
        {
            warn!(
                "client {} MIT-SHM::CreatePixmap put_image failed: {err}",
                client_id.0
            );
        }
    }

    // Register local pixmap aliasing the host xid. Reuse the regular
    // CreatePixmap path's local bookkeeping.
    {
        let mut s = lock_server(server)?;
        s.resources.create_pixmap(
            client_id,
            x11::CreatePixmapRequest {
                depth: req.depth,
                pixmap: ResourceId(req.pid),
                drawable: ResourceId(req.drawable),
                width: req.width,
                height: req.height,
            },
        );
        if let Some(xid) = host_xid {
            let updated = s.resources.set_pixmap_host_xid(ResourceId(req.pid), xid);
            debug_assert!(updated, "pixmap was just inserted above");
        }
    }
    debug!(
        "client {} #{} MIT-SHM::CreatePixmap pid=0x{:x} {}x{} d{} shmseg=0x{:x}+{} host_xid={:?}",
        client_id.0,
        sequence.0,
        req.pid,
        req.width,
        req.height,
        req.depth,
        req.shmseg,
        req.offset,
        host_xid
    );
    Ok(())
}

fn handle_mit_shm_put_image(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    req: yserver_protocol::x11::mit_shm::PutImageRequest,
) -> io::Result<()> {
    debug!(
        "client {} #{} MIT-SHM::PutImage entry drawable=0x{:x} shmseg=0x{:x} offset={} {}x{} d{} fmt={}",
        client_id.0,
        sequence.0,
        req.drawable,
        req.shmseg,
        req.offset,
        req.src_width,
        req.src_height,
        req.depth,
        req.format,
    );
    let target = {
        let s = lock_server(server)?;
        s.resources.host_drawable_target(ResourceId(req.drawable))
    };
    let Some(target) = target else {
        debug!(
            "client {} #{} MIT-SHM::PutImage drawable=0x{:x} no host backing",
            client_id.0, sequence.0, req.drawable
        );
        return Ok(());
    };
    let Some(stride_bytes) = zpixmap_expected_len(req.src_width, req.src_height, req.depth) else {
        debug!(
            "client {} #{} MIT-SHM::PutImage drawable=0x{:x} unsupported geometry/depth: {}x{} d{}",
            client_id.0, sequence.0, req.drawable, req.src_width, req.src_height, req.depth
        );
        return emit_x11_error(
            writer,
            sequence,
            x11::error::BAD_VALUE,
            req.shmseg,
            MIT_SHM_MAJOR_OPCODE,
        );
    };

    // Pull the requested rectangle out of the segment.
    let snapshot: Vec<u8> = {
        let s = lock_server(server)?;
        let Some(segment) = s.mit_shm_segments.get(&req.shmseg) else {
            debug!(
                "client {} #{} MIT-SHM::PutImage shmseg=0x{:x} not attached",
                client_id.0, sequence.0, req.shmseg
            );
            return emit_x11_error(
                writer,
                sequence,
                x11::error::BAD_VALUE,
                req.shmseg,
                MIT_SHM_MAJOR_OPCODE,
            );
        };
        let bytes = segment.as_slice();
        let start = req.offset as usize;
        let end = start.saturating_add(stride_bytes);
        if end > bytes.len() {
            debug!(
                "client {} #{} MIT-SHM::PutImage offset+stride out of range: {}+{} > {}",
                client_id.0,
                sequence.0,
                start,
                stride_bytes,
                bytes.len()
            );
            return emit_x11_error(
                writer,
                sequence,
                x11::error::BAD_VALUE,
                req.offset,
                MIT_SHM_MAJOR_OPCODE,
            );
        }
        bytes[start..end].to_vec()
    };

    if let Some(host) = host
        && let Ok(mut host) = host.lock()
    {
        // MIT-SHM PutImage doesn't carry a client GC; reset host clip so
        // a clip-mask left over from an unrelated draw (e.g. wmaker
        // close-button symbol) doesn't restrict the image upload.
        host.clear_clip_rectangles()?;
        host.put_image(
            target.host_xid(),
            req.depth,
            req.src_width,
            req.src_height,
            translate_i16(req.dst_x, target.x_offset()),
            translate_i16(req.dst_y, target.y_offset()),
            &snapshot,
        )?;
    }
    accumulate_damage(
        server,
        ResourceId(req.drawable),
        req.dst_x,
        req.dst_y,
        req.src_width,
        req.src_height,
    );
    debug!(
        "client {} #{} MIT-SHM::PutImage drawable=0x{:x} {}x{} d{}",
        client_id.0, sequence.0, req.drawable, req.src_width, req.src_height, req.depth
    );
    Ok(())
}

fn handle_mit_shm_get_image(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    req: yserver_protocol::x11::mit_shm::GetImageRequest,
) -> io::Result<()> {
    use yserver_protocol::x11::mit_shm as shm;
    let lock_writer = || -> io::Result<std::sync::MutexGuard<'_, UnixStream>> {
        writer
            .lock()
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))
    };
    // Validate write access to the segment.
    {
        let s = lock_server(server)?;
        let Some(segment) = s.mit_shm_segments.get(&req.shmseg) else {
            return emit_x11_error(
                writer,
                sequence,
                x11::error::BAD_VALUE,
                req.shmseg,
                MIT_SHM_MAJOR_OPCODE,
            );
        };
        if segment.read_only {
            return emit_x11_error(
                writer,
                sequence,
                x11::error::BAD_ACCESS,
                req.shmseg,
                MIT_SHM_MAJOR_OPCODE,
            );
        }
    }
    // Pull the bytes from the host.
    let host_bytes = if let Some(host) = host
        && let Ok(mut host) = host.lock()
    {
        let target = {
            let s = lock_server(server)?;
            s.resources.host_drawable_target(ResourceId(req.drawable))
        };
        let Some(target) = target else {
            return emit_x11_error(
                writer,
                sequence,
                x11::error::BAD_DRAWABLE,
                req.drawable,
                MIT_SHM_MAJOR_OPCODE,
            );
        };
        host.get_image(
            target.host_xid(),
            req.format,
            req.x.saturating_add(target.x_offset()),
            req.y.saturating_add(target.y_offset()),
            req.width,
            req.height,
            req.plane_mask,
        )
        .ok()
        .flatten()
    } else {
        None
    };
    let Some(host_reply_bytes) = host_bytes else {
        return emit_x11_error(
            writer,
            sequence,
            x11::error::BAD_DRAWABLE,
            req.drawable,
            MIT_SHM_MAJOR_OPCODE,
        );
    };
    // Host reply layout: 32-byte fixed header + pixel data. Strip the header.
    let pixel_data: Vec<u8> = host_reply_bytes.get(32..).unwrap_or(&[]).to_vec();
    // Write into the segment.
    {
        let mut s = lock_server(server)?;
        let Some(segment) = s.mit_shm_segments.get_mut(&req.shmseg) else {
            return emit_x11_error(
                writer,
                sequence,
                x11::error::BAD_VALUE,
                req.shmseg,
                MIT_SHM_MAJOR_OPCODE,
            );
        };
        let Some(buf) = segment.as_mut_slice() else {
            return emit_x11_error(
                writer,
                sequence,
                x11::error::BAD_ACCESS,
                req.shmseg,
                MIT_SHM_MAJOR_OPCODE,
            );
        };
        let start = req.offset as usize;
        let end = start.saturating_add(pixel_data.len());
        if end > buf.len() {
            return emit_x11_error(
                writer,
                sequence,
                x11::error::BAD_VALUE,
                req.offset,
                MIT_SHM_MAJOR_OPCODE,
            );
        }
        buf[start..end].copy_from_slice(&pixel_data);
    }
    // Reply with the visual + size.
    let depth = host_reply_bytes.first().copied().unwrap_or(24);
    let visual = ROOT_VISUAL.0;
    #[allow(clippy::cast_possible_truncation)]
    let size = pixel_data.len() as u32;
    let reply = shm::encode_get_image_reply(sequence, depth, visual, size);
    lock_writer()?.write_all(&reply)?;
    debug!(
        "client {} #{} MIT-SHM::GetImage drawable=0x{:x} -> {} bytes",
        client_id.0, sequence.0, req.drawable, size
    );
    Ok(())
}

fn handle_xfixes_request(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
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
        x11xfixes::QUERY_VERSION => {
            debug!(
                "client {} #{} XFIXES::QueryVersion -> {}.{}",
                client_id.0,
                sequence.0,
                x11xfixes::MAJOR_VERSION,
                x11xfixes::MINOR_VERSION
            );
            let reply = x11xfixes::encode_query_version_reply(
                sequence,
                x11xfixes::MAJOR_VERSION,
                x11xfixes::MINOR_VERSION,
            );
            lock_writer()?.write_all(&reply)
        }
        x11xfixes::SELECT_SELECTION_INPUT => {
            if let Some(req) = x11xfixes::parse_select_selection_input(body) {
                let mut s = lock_server(server)?;
                let key = (client_id.0, ResourceId(req.window), AtomId(req.selection));
                if req.event_mask == 0 {
                    s.xfixes_selection_masks.remove(&key);
                } else {
                    s.xfixes_selection_masks.insert(key, req.event_mask);
                }
            }
            debug!(
                "client {} #{} XFIXES::SelectSelectionInput",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11xfixes::SELECT_CURSOR_INPUT => {
            if let Some(req) = x11xfixes::parse_select_cursor_input(body) {
                let mut s = lock_server(server)?;
                let key = (client_id.0, ResourceId(req.window));
                if req.event_mask == 0 {
                    s.xfixes_cursor_masks.remove(&key);
                } else {
                    s.xfixes_cursor_masks.insert(key, req.event_mask);
                }
            }
            debug!(
                "client {} #{} XFIXES::SelectCursorInput",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11xfixes::GET_CURSOR_IMAGE => {
            debug!(
                "client {} #{} XFIXES::GetCursorImage",
                client_id.0, sequence.0
            );
            let reply = x11xfixes::encode_get_cursor_image_empty_reply(sequence);
            lock_writer()?.write_all(&reply)
        }
        x11xfixes::CREATE_REGION => {
            if let Some((region, rects)) = x11xfixes::parse_create_region(body) {
                let mut s = lock_server(server)?;
                s.xfixes_regions.insert(
                    region,
                    XFixesRegion {
                        owner: client_id,
                        rects: normalize_region_rects(rects),
                    },
                );
            }
            debug!(
                "client {} #{} XFIXES::CreateRegion",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11xfixes::CREATE_REGION_FROM_BITMAP | x11xfixes::CREATE_REGION_FROM_GC => {
            if let Some((region, _source)) = x11xfixes::parse_u32_pair(body) {
                let mut s = lock_server(server)?;
                s.xfixes_regions.insert(
                    region,
                    XFixesRegion {
                        owner: client_id,
                        rects: Vec::new(),
                    },
                );
            }
            debug!(
                "client {} #{} XFIXES::CreateRegionFromSource",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11xfixes::CREATE_REGION_FROM_WINDOW => {
            if let Some((region, window)) = x11xfixes::parse_u32_pair(body) {
                let rects = {
                    let s = lock_server(server)?;
                    s.resources
                        .window(ResourceId(window))
                        .map(|w| {
                            vec![x11xfixes::RegionRect {
                                x: 0,
                                y: 0,
                                width: w.width,
                                height: w.height,
                            }]
                        })
                        .unwrap_or_default()
                };
                let mut s = lock_server(server)?;
                s.xfixes_regions.insert(
                    region,
                    XFixesRegion {
                        owner: client_id,
                        rects,
                    },
                );
            }
            debug!(
                "client {} #{} XFIXES::CreateRegionFromWindow",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11xfixes::DESTROY_REGION => {
            if let Some((region, _)) = x11xfixes::parse_u32_pair(body) {
                lock_server(server)?.xfixes_regions.remove(&region);
            } else if body.len() >= 4 {
                let region = u32::from_le_bytes(body[0..4].try_into().unwrap());
                lock_server(server)?.xfixes_regions.remove(&region);
            }
            debug!(
                "client {} #{} XFIXES::DestroyRegion",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11xfixes::SET_REGION => {
            if let Some((region, rects)) = x11xfixes::parse_create_region(body) {
                let mut s = lock_server(server)?;
                s.xfixes_regions
                    .entry(region)
                    .and_modify(|r| r.rects = normalize_region_rects(rects.clone()))
                    .or_insert_with(|| XFixesRegion {
                        owner: client_id,
                        rects: normalize_region_rects(rects),
                    });
            }
            debug!("client {} #{} XFIXES::SetRegion", client_id.0, sequence.0);
            Ok(())
        }
        x11xfixes::COPY_REGION => {
            if let Some((source, dest)) = x11xfixes::parse_u32_pair(body) {
                let rects = lock_server(server)?
                    .xfixes_regions
                    .get(&source)
                    .map(|r| r.rects.clone())
                    .unwrap_or_default();
                lock_server(server)?.xfixes_regions.insert(
                    dest,
                    XFixesRegion {
                        owner: client_id,
                        rects,
                    },
                );
            }
            debug!("client {} #{} XFIXES::CopyRegion", client_id.0, sequence.0);
            Ok(())
        }
        x11xfixes::UNION_REGION | x11xfixes::INTERSECT_REGION | x11xfixes::SUBTRACT_REGION => {
            if let Some((source1, source2, dest)) = x11xfixes::parse_u32_triplet(body) {
                let (a, b) = {
                    let s = lock_server(server)?;
                    (
                        s.xfixes_regions
                            .get(&source1)
                            .map(|r| r.rects.clone())
                            .unwrap_or_default(),
                        s.xfixes_regions
                            .get(&source2)
                            .map(|r| r.rects.clone())
                            .unwrap_or_default(),
                    )
                };
                let rects = match minor {
                    x11xfixes::UNION_REGION => {
                        normalize_region_rects(a.into_iter().chain(b).collect())
                    }
                    x11xfixes::INTERSECT_REGION => intersect_regions(&a, &b),
                    // Conservative approximation: subtracting an overlapping region
                    // may over-clear, but never invents damaged/visible area.
                    x11xfixes::SUBTRACT_REGION => {
                        if intersect_regions(&a, &b).is_empty() {
                            a
                        } else {
                            Vec::new()
                        }
                    }
                    _ => unreachable!(),
                };
                lock_server(server)?.xfixes_regions.insert(
                    dest,
                    XFixesRegion {
                        owner: client_id,
                        rects,
                    },
                );
            }
            debug!(
                "client {} #{} XFIXES::RegionAlgebra minor={}",
                client_id.0, sequence.0, minor
            );
            Ok(())
        }
        x11xfixes::INVERT_REGION => {
            if let Some((_source, bounds, dest)) = x11xfixes::parse_invert_region(body) {
                lock_server(server)?.xfixes_regions.insert(
                    dest,
                    XFixesRegion {
                        owner: client_id,
                        rects: normalize_region_rects(vec![bounds]),
                    },
                );
            }
            debug!(
                "client {} #{} XFIXES::InvertRegion",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11xfixes::TRANSLATE_REGION => {
            if let Some((region, dx, dy)) = x11xfixes::parse_translate_region(body)
                && let Some(region) = lock_server(server)?.xfixes_regions.get_mut(&region)
            {
                translate_region(&mut region.rects, dx, dy);
            }
            debug!(
                "client {} #{} XFIXES::TranslateRegion",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11xfixes::REGION_EXTENTS => {
            if let Some((source, dest)) = x11xfixes::parse_u32_pair(body) {
                let rect = {
                    let s = lock_server(server)?;
                    s.xfixes_regions
                        .get(&source)
                        .map(|r| region_extents(&r.rects))
                        .unwrap_or(x11xfixes::RegionRect {
                            x: 0,
                            y: 0,
                            width: 0,
                            height: 0,
                        })
                };
                lock_server(server)?.xfixes_regions.insert(
                    dest,
                    XFixesRegion {
                        owner: client_id,
                        rects: normalize_region_rects(vec![rect]),
                    },
                );
            }
            debug!(
                "client {} #{} XFIXES::RegionExtents",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11xfixes::FETCH_REGION => {
            let region = body
                .get(0..4)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
                .unwrap_or(0);
            let (extents, rects) = {
                let s = lock_server(server)?;
                let rects = s
                    .xfixes_regions
                    .get(&region)
                    .map(|r| r.rects.clone())
                    .unwrap_or_default();
                (region_extents(&rects), rects)
            };
            debug!("client {} #{} XFIXES::FetchRegion", client_id.0, sequence.0);
            let reply = x11xfixes::encode_fetch_region_reply(sequence, extents, &rects);
            lock_writer()?.write_all(&reply)
        }
        x11xfixes::CHANGE_CURSOR_BY_NAME => {
            // Forward to the host so it can resolve the name against its own
            // cursor theme. e16 hits this path 7+ times during cursor theming;
            // without forwarding, the cursor never changes when hovering chrome.
            if let Some((cursor_xid, name_bytes)) = x11xfixes::parse_change_cursor_by_name(body) {
                let host_cursor = lock_server(server)?
                    .resources
                    .cursor_host_xid(ResourceId(cursor_xid));
                if let (Some(host), Some(host_cursor)) = (host, host_cursor) {
                    if let Ok(mut h) = host.lock()
                        && let Err(err) = h.xfixes_change_cursor_by_name(host_cursor, name_bytes)
                    {
                        debug!(
                            "host XFIXES::ChangeCursorByName failed for cursor 0x{cursor_xid:x}: {err}"
                        );
                    }
                } else {
                    debug!(
                        "client {} #{} XFIXES::ChangeCursorByName cursor=0x{:x} dropped (no host mapping)",
                        client_id.0, sequence.0, cursor_xid
                    );
                }
            }
            debug!(
                "client {} #{} XFIXES::ChangeCursorByName",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11xfixes::HIDE_CURSOR | x11xfixes::SHOW_CURSOR => {
            debug!(
                "client {} #{} XFIXES::{}Cursor (stub)",
                client_id.0,
                sequence.0,
                if minor == x11xfixes::HIDE_CURSOR {
                    "Hide"
                } else {
                    "Show"
                }
            );
            Ok(())
        }
        other => {
            debug!(
                "client {} #{} XFIXES::unknown minor={}",
                client_id.0, sequence.0, other
            );
            Ok(())
        }
    }
}

fn offset_rects(
    mut rects: Vec<x11xfixes::RegionRect>,
    dx: i16,
    dy: i16,
) -> Vec<x11xfixes::RegionRect> {
    translate_region(&mut rects, dx, dy);
    normalize_region_rects(rects)
}

fn default_shape_rect(server: &ServerState, window: ResourceId) -> x11xfixes::RegionRect {
    server.resources.window(window).map_or(
        x11xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        |w| x11xfixes::RegionRect {
            x: 0,
            y: 0,
            width: w.width,
            height: w.height,
        },
    )
}

fn shape_rects_for(
    server: &ServerState,
    window: ResourceId,
    kind: u8,
) -> Vec<x11xfixes::RegionRect> {
    server
        .shape_windows
        .get(&window)
        .and_then(|state| state.rects(kind).cloned())
        .unwrap_or_else(|| normalize_region_rects(vec![default_shape_rect(server, window)]))
}

fn shape_mask_source_rects(server: &ServerState, source: ResourceId) -> Vec<x11xfixes::RegionRect> {
    server
        .resources
        .pixmap(source)
        .map(|pixmap| {
            normalize_region_rects(vec![x11xfixes::RegionRect {
                x: 0,
                y: 0,
                width: pixmap.width,
                height: pixmap.height,
            }])
        })
        .unwrap_or_default()
}

fn shape_kind_is_set(server: &ServerState, window: ResourceId, kind: u8) -> bool {
    server
        .shape_windows
        .get(&window)
        .and_then(|state| state.rects(kind))
        .is_some()
}

fn apply_shape_op(
    current: Vec<x11xfixes::RegionRect>,
    source: Vec<x11xfixes::RegionRect>,
    op: u8,
) -> Vec<x11xfixes::RegionRect> {
    match op {
        x11shape::OP_SET => normalize_region_rects(source),
        x11shape::OP_UNION => normalize_region_rects(current.into_iter().chain(source).collect()),
        x11shape::OP_INTERSECT => intersect_regions(&current, &source),
        x11shape::OP_SUBTRACT => {
            if intersect_regions(&current, &source).is_empty() {
                current
            } else {
                Vec::new()
            }
        }
        x11shape::OP_INVERT => normalize_region_rects(source),
        _ => current,
    }
}

fn set_shape_rects(
    server: &mut ServerState,
    window: ResourceId,
    kind: u8,
    rects: Vec<x11xfixes::RegionRect>,
) {
    let state = server.shape_windows.entry(window).or_default();
    if let Some(slot) = state.rects_mut(kind) {
        *slot = Some(normalize_region_rects(rects));
    }
}

/// Resolve `window`'s host XID and current per-kind rect list, then forward
/// the resolved list to the host's SHAPE extension. No-op when the window has
/// no host backing (sub-windows below top-levels keep their local-only
/// behavior — the parent's host shape already clips them).
fn mirror_shape_to_host(
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
    window: ResourceId,
    kind: u8,
) {
    let Some(host) = host else { return };
    if kind != x11shape::KIND_BOUNDING && kind != x11shape::KIND_CLIP {
        return;
    }
    let (host_xid, rects) = {
        let s = match server.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        let Some(w) = s.resources.window(window) else {
            return;
        };
        let Some(host_xid) = w.host_xid else {
            return;
        };
        (host_xid, shape_rects_for(&s, window, kind))
    };
    if let Ok(mut h) = host.lock()
        && let Err(err) = h.set_shape_rectangles(host_xid, kind, &rects)
    {
        debug!(
            "host SHAPE mirror failed for window 0x{:x} kind={kind}: {err}",
            window.0
        );
    }
}

/// Reset the stored region for a single shape kind. Triggered by
/// `SHAPE::Mask` with `source = None`, which must clear the kind back to its
/// default (the unshaped window rectangle) rather than recording an empty
/// region. Used by e16 menu reparenting.
fn clear_shape_rects(server: &mut ServerState, window: ResourceId, kind: u8) {
    let Some(state) = server.shape_windows.get_mut(&window) else {
        return;
    };
    let Some(slot) = state.rects_mut(kind) else {
        return;
    };
    *slot = None;
    if state.bounding.is_none() && state.clip.is_none() && state.input.is_none() {
        server.shape_windows.remove(&window);
    }
}

fn handle_shape_request(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
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
        x11shape::QUERY_VERSION => {
            debug!("client {} #{} SHAPE::QueryVersion", client_id.0, sequence.0);
            let reply = x11shape::encode_query_version_reply(sequence);
            lock_writer()?.write_all(&reply)
        }
        x11shape::RECTANGLES => {
            let mirror_target = if let Some((req, rects)) = x11shape::parse_rectangles_request(body)
            {
                let window = ResourceId(req.dest);
                let source = offset_rects(rects, req.x_off, req.y_off);
                let mut s = lock_server(server)?;
                let current = shape_rects_for(&s, window, req.dest_kind);
                let rects = apply_shape_op(current, source, req.op);
                debug!(
                    "client {} #{} SHAPE::Rectangles dest=0x{:x} kind={} op={} rects={} extents={:?}",
                    client_id.0,
                    sequence.0,
                    window.0,
                    req.dest_kind,
                    req.op,
                    rects.len(),
                    region_extents(&rects)
                );
                set_shape_rects(&mut s, window, req.dest_kind, rects);
                Some((window, req.dest_kind))
            } else {
                debug!("client {} #{} SHAPE::Rectangles", client_id.0, sequence.0);
                None
            };
            if let Some((window, kind)) = mirror_target {
                mirror_shape_to_host(server, host, window, kind);
            }
            Ok(())
        }
        x11shape::MASK => {
            let mirror_target = if let Some(req) = x11shape::parse_mask_request(body) {
                let window = ResourceId(req.dest);
                if req.src == 0 {
                    let mut s = lock_server(server)?;
                    clear_shape_rects(&mut s, window, req.dest_kind);
                    let rects = shape_rects_for(&s, window, req.dest_kind);
                    debug!(
                        "client {} #{} SHAPE::Mask dest=0x{:x} kind={} op={} src=None clear extents={:?}",
                        client_id.0,
                        sequence.0,
                        window.0,
                        req.dest_kind,
                        req.op,
                        region_extents(&rects)
                    );
                    drop(s);
                    mirror_shape_to_host(server, host, window, req.dest_kind);
                    return Ok(());
                }
                let source = {
                    let s = lock_server(server)?;
                    shape_mask_source_rects(&s, ResourceId(req.src))
                };
                let source = offset_rects(source, req.x_off, req.y_off);
                let mut s = lock_server(server)?;
                let current = shape_rects_for(&s, window, req.dest_kind);
                let rects = apply_shape_op(current, source, req.op);
                debug!(
                    "client {} #{} SHAPE::Mask dest=0x{:x} kind={} op={} src=0x{:x} rects={} extents={:?}",
                    client_id.0,
                    sequence.0,
                    window.0,
                    req.dest_kind,
                    req.op,
                    req.src,
                    rects.len(),
                    region_extents(&rects)
                );
                set_shape_rects(&mut s, window, req.dest_kind, rects);
                Some((window, req.dest_kind))
            } else {
                debug!("client {} #{} SHAPE::Mask", client_id.0, sequence.0);
                None
            };
            if let Some((window, kind)) = mirror_target {
                mirror_shape_to_host(server, host, window, kind);
            }
            Ok(())
        }
        x11shape::COMBINE => {
            let mirror_target = if let Some(req) = x11shape::parse_combine_request(body) {
                let dest = ResourceId(req.dest);
                let src = ResourceId(req.src);
                let source = {
                    let s = lock_server(server)?;
                    offset_rects(shape_rects_for(&s, src, req.src_kind), req.x_off, req.y_off)
                };
                let mut s = lock_server(server)?;
                let current = shape_rects_for(&s, dest, req.dest_kind);
                let rects = apply_shape_op(current, source, req.op);
                debug!(
                    "client {} #{} SHAPE::Combine dest=0x{:x} kind={} op={} src=0x{:x} src_kind={} rects={} extents={:?}",
                    client_id.0,
                    sequence.0,
                    dest.0,
                    req.dest_kind,
                    req.op,
                    req.src,
                    req.src_kind,
                    rects.len(),
                    region_extents(&rects)
                );
                set_shape_rects(&mut s, dest, req.dest_kind, rects);
                Some((dest, req.dest_kind))
            } else {
                debug!("client {} #{} SHAPE::Combine", client_id.0, sequence.0);
                None
            };
            if let Some((window, kind)) = mirror_target {
                mirror_shape_to_host(server, host, window, kind);
            }
            Ok(())
        }
        x11shape::OFFSET => {
            let mirror_target = if let Some(req) = x11shape::parse_offset_request(body) {
                let dest = ResourceId(req.dest);
                let mut s = lock_server(server)?;
                let mut translated = false;
                if let Some(state) = s.shape_windows.get_mut(&dest)
                    && let Some(slot) = state.rects_mut(req.dest_kind)
                    && let Some(rects) = slot.as_mut()
                {
                    translate_region(rects, req.x_off, req.y_off);
                    translated = true;
                }
                translated.then_some((dest, req.dest_kind))
            } else {
                None
            };
            debug!("client {} #{} SHAPE::Offset", client_id.0, sequence.0);
            if let Some((window, kind)) = mirror_target {
                mirror_shape_to_host(server, host, window, kind);
            }
            Ok(())
        }
        x11shape::QUERY_EXTENTS => {
            let window = ResourceId(x11shape::parse_window(body).unwrap_or(ROOT_WINDOW.0));
            let (bounding_shaped, clip_shaped, bounding, clip) = {
                let s = lock_server(server)?;
                let bounding_rects = shape_rects_for(&s, window, x11shape::KIND_BOUNDING);
                let clip_rects = shape_rects_for(&s, window, x11shape::KIND_CLIP);
                (
                    shape_kind_is_set(&s, window, x11shape::KIND_BOUNDING),
                    shape_kind_is_set(&s, window, x11shape::KIND_CLIP),
                    region_extents(&bounding_rects),
                    region_extents(&clip_rects),
                )
            };
            debug!("client {} #{} SHAPE::QueryExtents", client_id.0, sequence.0);
            let reply = x11shape::encode_query_extents_reply(
                sequence,
                bounding_shaped,
                clip_shaped,
                bounding,
                clip,
            );
            lock_writer()?.write_all(&reply)
        }
        x11shape::SELECT_INPUT => {
            if let Some(req) = x11shape::parse_select_input_request(body) {
                let mut s = lock_server(server)?;
                let key = (client_id.0, ResourceId(req.window));
                if req.enable {
                    s.shape_select_masks.insert(key, true);
                } else {
                    s.shape_select_masks.remove(&key);
                }
            }
            debug!("client {} #{} SHAPE::SelectInput", client_id.0, sequence.0);
            Ok(())
        }
        x11shape::INPUT_SELECTED => {
            let window = ResourceId(x11shape::parse_window(body).unwrap_or(ROOT_WINDOW.0));
            let enabled = {
                let s = lock_server(server)?;
                s.shape_select_masks
                    .get(&(client_id.0, window))
                    .copied()
                    .unwrap_or(false)
            };
            debug!(
                "client {} #{} SHAPE::InputSelected",
                client_id.0, sequence.0
            );
            let reply = x11shape::encode_input_selected_reply(sequence, enabled);
            lock_writer()?.write_all(&reply)
        }
        x11shape::GET_RECTANGLES => {
            let (window, kind) = x11shape::parse_get_rectangles_request(body)
                .map(|(w, k)| (ResourceId(w), k))
                .unwrap_or((ROOT_WINDOW, x11shape::KIND_BOUNDING));
            let rects = {
                let s = lock_server(server)?;
                shape_rects_for(&s, window, kind)
            };
            debug!(
                "client {} #{} SHAPE::GetRectangles",
                client_id.0, sequence.0
            );
            let reply = x11shape::encode_get_rectangles_reply(sequence, 0, &rects);
            lock_writer()?.write_all(&reply)
        }
        other => {
            debug!(
                "client {} #{} SHAPE::unknown minor={}",
                client_id.0, sequence.0, other
            );
            Ok(())
        }
    }
}

fn handle_sync_request(
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
        x11sync::INITIALIZE => {
            let (client_major, client_minor) =
                x11sync::parse_initialize(body).unwrap_or((x11sync::MAJOR_VERSION, 0));
            let major = x11sync::MAJOR_VERSION.min(client_major);
            let minor_ver = if major < x11sync::MAJOR_VERSION {
                client_minor
            } else {
                x11sync::MINOR_VERSION
            };
            debug!(
                "client {} #{} SYNC::Initialize -> {}.{}",
                client_id.0, sequence.0, major, minor_ver
            );
            let reply = x11sync::encode_initialize_reply(sequence, major, minor_ver);
            lock_writer()?.write_all(&reply)
        }
        x11sync::LIST_SYSTEM_COUNTERS => {
            debug!(
                "client {} #{} SYNC::ListSystemCounters -> empty",
                client_id.0, sequence.0
            );
            let reply = x11sync::encode_list_system_counters_empty_reply(sequence);
            lock_writer()?.write_all(&reply)
        }
        x11sync::CREATE_COUNTER => {
            if let Some((counter, value)) = x11sync::parse_counter_value(body) {
                lock_server(server)?.sync_counters.insert(
                    counter,
                    SyncCounter {
                        owner: client_id,
                        value,
                    },
                );
            }
            debug!("client {} #{} SYNC::CreateCounter", client_id.0, sequence.0);
            Ok(())
        }
        x11sync::SET_COUNTER => {
            if let Some((counter, value)) = x11sync::parse_counter_value(body)
                && let Some(counter) = lock_server(server)?.sync_counters.get_mut(&counter)
            {
                counter.value = value;
            }
            debug!("client {} #{} SYNC::SetCounter", client_id.0, sequence.0);
            Ok(())
        }
        x11sync::CHANGE_COUNTER => {
            if let Some((counter, delta)) = x11sync::parse_counter_value(body)
                && let Some(counter) = lock_server(server)?.sync_counters.get_mut(&counter)
            {
                counter.value = counter.value.saturating_add(delta);
            }
            debug!("client {} #{} SYNC::ChangeCounter", client_id.0, sequence.0);
            Ok(())
        }
        x11sync::QUERY_COUNTER => {
            let counter = x11sync::parse_resource(body).unwrap_or(0);
            let value = lock_server(server)?
                .sync_counters
                .get(&counter)
                .map_or(0, |counter| counter.value);
            debug!("client {} #{} SYNC::QueryCounter", client_id.0, sequence.0);
            let reply = x11sync::encode_query_counter_reply(sequence, value);
            lock_writer()?.write_all(&reply)
        }
        x11sync::DESTROY_COUNTER => {
            if let Some(counter) = x11sync::parse_resource(body) {
                lock_server(server)?.sync_counters.remove(&counter);
            }
            debug!(
                "client {} #{} SYNC::DestroyCounter",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11sync::AWAIT => {
            debug!(
                "client {} #{} SYNC::Await (non-blocking stub)",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11sync::CREATE_ALARM => {
            if let Some((alarm, _mask)) = x11sync::parse_alarm_with_mask(body) {
                lock_server(server)?.sync_alarms.insert(
                    alarm,
                    SyncAlarm {
                        owner: client_id,
                        ..SyncAlarm::default()
                    },
                );
            }
            debug!("client {} #{} SYNC::CreateAlarm", client_id.0, sequence.0);
            Ok(())
        }
        x11sync::CHANGE_ALARM => {
            if let Some((alarm, _mask)) = x11sync::parse_alarm_with_mask(body)
                && let Some(alarm) = lock_server(server)?.sync_alarms.get_mut(&alarm)
            {
                alarm.state = 0;
            }
            debug!("client {} #{} SYNC::ChangeAlarm", client_id.0, sequence.0);
            Ok(())
        }
        x11sync::QUERY_ALARM => {
            let alarm_id = x11sync::parse_resource(body).unwrap_or(0);
            let alarm = lock_server(server)?
                .sync_alarms
                .get(&alarm_id)
                .cloned()
                .unwrap_or_default();
            debug!("client {} #{} SYNC::QueryAlarm", client_id.0, sequence.0);
            let reply = x11sync::encode_query_alarm_reply(
                sequence,
                alarm.counter,
                alarm.wait_value,
                alarm.delta,
                alarm.events,
                alarm.state,
            );
            lock_writer()?.write_all(&reply)
        }
        x11sync::DESTROY_ALARM => {
            if let Some(alarm) = x11sync::parse_resource(body) {
                lock_server(server)?.sync_alarms.remove(&alarm);
            }
            debug!("client {} #{} SYNC::DestroyAlarm", client_id.0, sequence.0);
            Ok(())
        }
        x11sync::SET_PRIORITY => {
            debug!(
                "client {} #{} SYNC::SetPriority (stub)",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11sync::GET_PRIORITY => {
            debug!(
                "client {} #{} SYNC::GetPriority -> 0",
                client_id.0, sequence.0
            );
            let reply = x11sync::encode_get_priority_reply(sequence, 0);
            lock_writer()?.write_all(&reply)
        }
        other => {
            debug!(
                "client {} #{} SYNC::unknown minor={}",
                client_id.0, sequence.0, other
            );
            Ok(())
        }
    }
}

/// Mark `(x, y, w, h)` of `drawable` as damaged. For every `DamageObject`
/// attached to that drawable (level ≥ 1) emits a `DamageNotify` to the
/// owning client at most once per Subtract cycle, then unions the rect
/// into the damage region.
///
/// Per the Phase 3.5 design, level 0 (RawRectangles, fire-per-op) is
/// deferred; we fire at most one event per cycle for levels 1, 2, and 3.
/// Subtract resets the per-object `pending_notify_fired` flag.
pub fn accumulate_damage(
    server: &Arc<Mutex<ServerState>>,
    drawable: ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) {
    if width == 0 || height == 0 {
        return;
    }
    // Snapshot per-target writers + identity tuples so we can fanout outside
    // the server lock.
    struct Notification {
        writer: Arc<Mutex<UnixStream>>,
        last_sequence: Arc<std::sync::atomic::AtomicU16>,
        damage_id: u32,
        level: u8,
        drawable: u32,
        geometry: yserver_protocol::x11::damage::Rectangle,
    }
    let (timestamp, notifications): (u32, Vec<Notification>) = match server.lock() {
        Ok(mut s) => {
            let timestamp = s.timestamp_now();
            let geom_full = drawable_full_rect(&s, drawable);
            let geom_rect = yserver_protocol::x11::damage::Rectangle {
                x: 0,
                y: 0,
                width: geom_full.width,
                height: geom_full.height,
            };
            let mut out = Vec::new();
            // Walk damage_objects: snapshot client writer for those that
            // need to fire and weren't already fired this cycle.
            let damage_ids: Vec<u32> = s
                .damage_objects
                .iter()
                .filter(|(_, dmg)| dmg.drawable == drawable)
                .map(|(id, _)| *id)
                .collect();
            for damage_id in damage_ids {
                let (level, fired, owner) = {
                    let dmg = s.damage_objects.get(&damage_id).expect("just enumerated");
                    (dmg.level, dmg.pending_notify_fired, dmg.owner)
                };
                // OR the rect into the region. (Stored unconditionally so
                // GetRectangles / Subtract observe accurate state.)
                let rect = yserver_protocol::x11::xfixes::RegionRect {
                    x,
                    y,
                    width,
                    height,
                };
                if let Some(d) = s.damage_objects.get_mut(&damage_id) {
                    d.rects.push(rect);
                }
                if !fired && let Some(client) = s.clients.get(&owner.0) {
                    out.push(Notification {
                        writer: client.writer.clone(),
                        last_sequence: client.last_sequence.clone(),
                        damage_id,
                        level,
                        drawable: drawable.0,
                        geometry: geom_rect,
                    });
                    if let Some(d) = s.damage_objects.get_mut(&damage_id) {
                        d.pending_notify_fired = true;
                    }
                }
            }
            (timestamp, out)
        }
        Err(_) => return,
    };

    for n in notifications {
        let seq = yserver_protocol::x11::SequenceNumber(
            n.last_sequence.load(std::sync::atomic::Ordering::Relaxed),
        );
        let area = yserver_protocol::x11::damage::Rectangle {
            x,
            y,
            width,
            height,
        };
        let evt = yserver_protocol::x11::damage::encode_damage_notify_event(
            DAMAGE_FIRST_EVENT,
            seq,
            n.level,
            n.drawable,
            n.damage_id,
            timestamp,
            area,
            n.geometry,
        );
        if let Ok(mut w) = n.writer.lock() {
            let _ = w.write_all(&evt);
        }
    }
}

/// Apply the GC's effective clip-state to the host shared GC before
/// issuing a draw. Translates the clip's origin into top-level
/// coordinates by `(x_offset, y_offset)`. Replaces the previous
/// `set_clip_rectangles(clip, ...)` call so we also honour
/// `ChangeGC(clip_mask=Pixmap)` (used by wmaker for window-decoration
/// symbols) — without this, depth-1 clip-mask draws fill the entire
/// rect with the foreground color and the X/dot symbols vanish.
fn apply_gc_clip(
    host: &mut HostX11,
    state: &GcClipState,
    x_offset: i16,
    y_offset: i16,
) -> io::Result<()> {
    match state {
        GcClipState::Rectangles(c) => host.set_clip_rectangles(Some(c.clone()), x_offset, y_offset),
        GcClipState::Pixmap {
            host_pixmap,
            clip_x_origin,
            clip_y_origin,
        } => host.set_clip_pixmap(
            *host_pixmap,
            *clip_x_origin,
            *clip_y_origin,
            x_offset,
            y_offset,
        ),
        GcClipState::None => host.clear_clip_rectangles(),
    }
}

/// Free every `Composite::NameWindowPixmap` alias on `window`, clearing
/// the bookkeeping list and `FreePixmap`'ing each host alias. Per the
/// COMPOSITE spec, a resize or destroy invalidates *all* previously
/// named pixmaps on the window simultaneously.
pub fn invalidate_composite_named_pixmaps(
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
    window: ResourceId,
) {
    let aliases: Vec<NamedCompositePixmap> = match server.lock() {
        Ok(mut s) => match s.resources.window_mut(window) {
            Some(w) => std::mem::take(&mut w.composite_named_pixmaps),
            None => return,
        },
        Err(_) => return,
    };
    if aliases.is_empty() {
        return;
    }
    // Drop the local Pixmap resources so subsequent client refs return
    // BadPixmap, matching real X11 behaviour after the alias dies.
    if let Ok(mut s) = server.lock() {
        for a in &aliases {
            let _ = s.resources.free_pixmap(a.client_pixmap);
        }
    }
    // Free host pixmaps. Errors are best-effort — if the host died
    // we'll lose the alias on next reconnect anyway.
    if let Some(host_arc) = host
        && let Ok(mut h) = host_arc.lock()
    {
        for a in &aliases {
            let _ = h.free_pixmap(a.host_pixmap);
        }
    }
}

/// Convenience for drawing ops where computing an exact bounding box is
/// fiddly (`PolyLine`, `PolyArc`, `FillPoly`, text rendering, …). We
/// damage the whole drawable rather than guess at bounds. Conservative
/// but correct: damage consumers may repaint slightly more, never less.
pub fn accumulate_damage_full(server: &Arc<Mutex<ServerState>>, drawable: ResourceId) {
    let (width, height) = match server.lock() {
        Ok(s) => {
            let r = drawable_full_rect(&s, drawable);
            (r.width, r.height)
        }
        Err(_) => return,
    };
    accumulate_damage(server, drawable, 0, 0, width, height);
}

fn drawable_full_rect(server: &ServerState, drawable: ResourceId) -> x11xfixes::RegionRect {
    if let Some(window) = server.resources.window(drawable) {
        return x11xfixes::RegionRect {
            x: 0,
            y: 0,
            width: window.width,
            height: window.height,
        };
    }
    server.resources.pixmap(drawable).map_or(
        x11xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        |pixmap| x11xfixes::RegionRect {
            x: 0,
            y: 0,
            width: pixmap.width,
            height: pixmap.height,
        },
    )
}

fn handle_damage_request(
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
        x11damage::QUERY_VERSION => {
            let (client_major, client_minor) = x11damage::parse_query_version(body)
                .unwrap_or((x11damage::MAJOR_VERSION, x11damage::MINOR_VERSION));
            let major = x11damage::MAJOR_VERSION.min(client_major);
            let minor_ver = if major < x11damage::MAJOR_VERSION {
                client_minor
            } else {
                x11damage::MINOR_VERSION.min(client_minor)
            };
            debug!(
                "client {} #{} DAMAGE::QueryVersion -> {}.{}",
                client_id.0, sequence.0, major, minor_ver
            );
            let reply = x11damage::encode_query_version_reply(sequence, major, minor_ver);
            lock_writer()?.write_all(&reply)
        }
        x11damage::CREATE => {
            if let Some((damage, drawable, level)) = x11damage::parse_create(body) {
                lock_server(server)?.damage_objects.insert(
                    damage,
                    DamageObject {
                        owner: client_id,
                        drawable: ResourceId(drawable),
                        level,
                        rects: Vec::new(),
                        pending_notify_fired: false,
                    },
                );
            }
            debug!("client {} #{} DAMAGE::Create", client_id.0, sequence.0);
            Ok(())
        }
        x11damage::DESTROY => {
            if let Some(damage) = x11damage::parse_resource(body) {
                lock_server(server)?.damage_objects.remove(&damage);
            }
            debug!("client {} #{} DAMAGE::Destroy", client_id.0, sequence.0);
            Ok(())
        }
        x11damage::ADD => {
            if let Some((drawable, region)) = x11damage::parse_add(body) {
                let mut s = lock_server(server)?;
                let rects = if region == 0 {
                    vec![drawable_full_rect(&s, ResourceId(drawable))]
                } else {
                    s.xfixes_regions
                        .get(&region)
                        .map(|region| region.rects.clone())
                        .unwrap_or_default()
                };
                for damage in s.damage_objects.values_mut() {
                    if damage.drawable == ResourceId(drawable) {
                        damage.rects.extend(rects.clone());
                        damage.rects = normalize_region_rects(std::mem::take(&mut damage.rects));
                    }
                }
            }
            debug!("client {} #{} DAMAGE::Add", client_id.0, sequence.0);
            Ok(())
        }
        x11damage::SUBTRACT => {
            if let Some((damage_id, repair, parts)) = x11damage::parse_subtract(body) {
                let mut s = lock_server(server)?;
                let rects = s
                    .damage_objects
                    .get(&damage_id)
                    .map(|damage| damage.rects.clone())
                    .unwrap_or_default();
                if repair != 0 {
                    s.xfixes_regions.insert(
                        repair,
                        XFixesRegion {
                            owner: client_id,
                            rects: rects.clone(),
                        },
                    );
                }
                if parts != 0 {
                    s.xfixes_regions.insert(
                        parts,
                        XFixesRegion {
                            owner: client_id,
                            rects: Vec::new(),
                        },
                    );
                }
                if let Some(damage) = s.damage_objects.get_mut(&damage_id) {
                    damage.rects.clear();
                    // Subtract closes the current cycle; the next damaging op
                    // is allowed to fire DamageNotify again.
                    damage.pending_notify_fired = false;
                }
            }
            debug!("client {} #{} DAMAGE::Subtract", client_id.0, sequence.0);
            Ok(())
        }
        other => {
            debug!(
                "client {} #{} DAMAGE::unknown minor={}",
                client_id.0, sequence.0, other
            );
            Ok(())
        }
    }
}

fn handle_composite_request(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
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
        x11composite::QUERY_VERSION => {
            let _ = x11composite::parse_query_version(body);
            let major = x11composite::MAJOR_VERSION;
            let minor_ver = x11composite::MINOR_VERSION;
            debug!(
                "client {} #{} COMPOSITE::QueryVersion -> {}.{}",
                client_id.0, sequence.0, major, minor_ver
            );
            let reply = x11composite::encode_query_version_reply(sequence, major, minor_ver);
            lock_writer()?.write_all(&reply)
        }
        x11composite::REDIRECT_WINDOW | x11composite::REDIRECT_SUBWINDOWS => {
            if let Some((window, update)) = x11composite::parse_window_update(body) {
                let subwindows = minor == x11composite::REDIRECT_SUBWINDOWS;
                lock_server(server)?
                    .composite_redirects
                    .insert((ResourceId(window), subwindows), update);
            }
            debug!("client {} #{} COMPOSITE::Redirect", client_id.0, sequence.0);
            Ok(())
        }
        x11composite::UNREDIRECT_WINDOW | x11composite::UNREDIRECT_SUBWINDOWS => {
            if let Some((window, _update)) = x11composite::parse_window_update(body) {
                let subwindows = minor == x11composite::UNREDIRECT_SUBWINDOWS;
                lock_server(server)?
                    .composite_redirects
                    .remove(&(ResourceId(window), subwindows));
            }
            debug!(
                "client {} #{} COMPOSITE::Unredirect",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11composite::CREATE_REGION_FROM_BORDER_CLIP => {
            if let Some((region, window)) = x11composite::parse_u32_pair(body) {
                let rects = {
                    let s = lock_server(server)?;
                    vec![drawable_full_rect(&s, ResourceId(window))]
                };
                lock_server(server)?.xfixes_regions.insert(
                    region,
                    XFixesRegion {
                        owner: client_id,
                        rects: normalize_region_rects(rects),
                    },
                );
            }
            debug!(
                "client {} #{} COMPOSITE::CreateRegionFromBorderClip",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11composite::NAME_WINDOW_PIXMAP => {
            let Some((window_raw, pixmap_raw)) = x11composite::parse_u32_pair(body) else {
                return Ok(());
            };
            let window = ResourceId(window_raw);
            let pixmap = ResourceId(pixmap_raw);

            // Snapshot what we need from the server: host xid, geometry,
            // depth, redirection state, and whether this is a sub-window
            // (parent != root). The sub-window block lets Phase 3.6 Step 2
            // ship before Step 5 wires up host-pixmap retention across
            // sub-window DestroyWindow; once Step 5 lands the
            // `is_sub_window` reject below goes away.
            let snapshot = {
                let s = lock_server(server)?;
                s.resources.window(window).map(|w| {
                    let parent_redirected = s
                        .composite_redirects
                        .keys()
                        .any(|(rwid, sub)| *sub && *rwid == w.parent);
                    let self_redirected = s.composite_redirects.contains_key(&(window, false));
                    (
                        w.host_xid,
                        w.width,
                        w.height,
                        w.depth,
                        parent_redirected || self_redirected,
                        w.parent != ROOT_WINDOW,
                    )
                })
            };
            let Some((host_xid, w_width, w_height, w_depth, redirected, is_sub_window)) = snapshot
            else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_WINDOW,
                    window_raw,
                    COMPOSITE_MAJOR_OPCODE,
                );
            };

            // Spec: NameWindowPixmap is only valid on a redirected window.
            if !redirected {
                debug!(
                    "client {} #{} COMPOSITE::NameWindowPixmap -> BadMatch (window not redirected)",
                    client_id.0, sequence.0
                );
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_MATCH,
                    window_raw,
                    COMPOSITE_MAJOR_OPCODE,
                );
            }

            // Phase 3.6 Step 2 block: refuse NameWindowPixmap on
            // mirrored sub-windows until Step 5 wires up host-pixmap
            // retention across sub-window DestroyWindow. Without that
            // retention the host pixmap an aliased sub-window points
            // at would be freed prematurely on destroy. Top-levels are
            // unaffected — they were already mirrored pre-Step-2.
            if is_sub_window {
                debug!(
                    "client {} #{} COMPOSITE::NameWindowPixmap -> BadValue (sub-window mirroring; lifted in Step 5)",
                    client_id.0, sequence.0
                );
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_VALUE,
                    window_raw,
                    COMPOSITE_MAJOR_OPCODE,
                );
            }

            // Without a host or without host COMPOSITE, no backing store
            // exists to alias. Per design, return BadAlloc rather than
            // fake the pixmap (a window-as-pixmap alias breaks downstream
            // pixmap-only requests).
            let Some(host_arc) = host else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_ALLOC,
                    pixmap_raw,
                    COMPOSITE_MAJOR_OPCODE,
                );
            };
            let Some(host_window_xid) = host_xid else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_ALLOC,
                    pixmap_raw,
                    COMPOSITE_MAJOR_OPCODE,
                );
            };

            let host_pixmap_xid = {
                let Ok(mut h) = host_arc.lock() else {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ALLOC,
                        pixmap_raw,
                        COMPOSITE_MAJOR_OPCODE,
                    );
                };
                if h.composite_opcode().is_none() {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ALLOC,
                        pixmap_raw,
                        COMPOSITE_MAJOR_OPCODE,
                    );
                }
                let host_pixmap_xid = h.allocate_xid();
                if h.name_window_pixmap(host_window_xid, host_pixmap_xid)
                    .is_err()
                {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_ALLOC,
                        pixmap_raw,
                        COMPOSITE_MAJOR_OPCODE,
                    );
                }
                host_pixmap_xid
            };

            // Register the local Pixmap resource and link it to the host
            // alias. Future CopyArea/etc. on this xid will use host_xid.
            {
                let mut s = lock_server(server)?;
                s.resources.create_pixmap(
                    client_id,
                    x11::CreatePixmapRequest {
                        pixmap,
                        drawable: window,
                        width: w_width,
                        height: w_height,
                        depth: w_depth,
                    },
                );
                let _ = s.resources.set_pixmap_host_xid(pixmap, host_pixmap_xid);
                if let Some(w) = s.resources.window_mut(window) {
                    w.composite_named_pixmaps.push(NamedCompositePixmap {
                        client_pixmap: pixmap,
                        host_pixmap: host_pixmap_xid,
                        width: w_width,
                        height: w_height,
                    });
                }
            }
            debug!(
                "client {} #{} COMPOSITE::NameWindowPixmap window=0x{:x} pixmap=0x{:x} (host pixmap=0x{:x})",
                client_id.0, sequence.0, window_raw, pixmap_raw, host_pixmap_xid
            );
            Ok(())
        }
        x11composite::GET_OVERLAY_WINDOW => {
            let _window = x11composite::parse_window(body).unwrap_or(ROOT_WINDOW.0);
            debug!(
                "client {} #{} COMPOSITE::GetOverlayWindow",
                client_id.0, sequence.0
            );
            // For the nested compatibility subset, the root window is a stable,
            // viewable server window and is sufficient for capability probes.
            let overlay = ROOT_WINDOW.0;
            let reply = x11composite::encode_get_overlay_window_reply(sequence, overlay);
            lock_writer()?.write_all(&reply)
        }
        x11composite::RELEASE_OVERLAY_WINDOW => {
            debug!(
                "client {} #{} COMPOSITE::ReleaseOverlayWindow",
                client_id.0, sequence.0
            );
            Ok(())
        }
        other => {
            debug!(
                "client {} #{} COMPOSITE::unknown minor={}",
                client_id.0, sequence.0, other
            );
            Ok(())
        }
    }
}

fn handle_present_request(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    minor: u8,
    body: &[u8],
) -> io::Result<()> {
    let lock_writer = || {
        writer
            .lock()
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))
    };

    match minor {
        x11present::QUERY_VERSION => {
            let _ = x11present::parse_query_version(body);
            debug!(
                "client {} #{} PRESENT::QueryVersion -> {}.{}",
                client_id.0,
                sequence.0,
                x11present::MAJOR_VERSION,
                x11present::MINOR_VERSION
            );
            let reply = x11present::encode_query_version_reply(
                sequence,
                x11present::MAJOR_VERSION,
                x11present::MINOR_VERSION,
            );
            lock_writer()?.write_all(&reply)
        }
        x11present::QUERY_CAPABILITIES => {
            let _target = x11present::parse_query_capabilities(body).unwrap_or(0);
            debug!(
                "client {} #{} PRESENT::QueryCapabilities -> none",
                client_id.0, sequence.0
            );
            let reply =
                x11present::encode_query_capabilities_reply(sequence, x11present::CAPABILITY_NONE);
            lock_writer()?.write_all(&reply)
        }
        x11present::SELECT_INPUT => {
            if let Some(req) = x11present::parse_select_input(body) {
                lock_server(server)?.present_event_selections.insert(
                    req.eid,
                    PresentEventSelection {
                        owner: client_id,
                        window: ResourceId(req.window),
                        event_mask: req.event_mask,
                    },
                );
            }
            debug!(
                "client {} #{} PRESENT::SelectInput",
                client_id.0, sequence.0
            );
            Ok(())
        }
        x11present::PIXMAP => {
            let Some(req) = x11present::parse_pixmap(body) else {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    PRESENT_MAJOR_OPCODE,
                );
            };

            if req.wait_fence != 0 || req.idle_fence != 0 {
                let bad = if req.wait_fence != 0 {
                    req.wait_fence
                } else {
                    req.idle_fence
                };
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_IMPLEMENTATION,
                    bad,
                    PRESENT_MAJOR_OPCODE,
                );
            }

            let (window_exists, pixmap_exists, src, dst) = {
                let s = lock_server(server)?;
                (
                    s.resources.window(ResourceId(req.window)).is_some(),
                    s.resources.pixmap(ResourceId(req.pixmap)).is_some(),
                    s.resources.host_drawable_target(ResourceId(req.pixmap)),
                    s.resources.host_drawable_target(ResourceId(req.window)),
                )
            };
            if !window_exists {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_WINDOW,
                    req.window,
                    PRESENT_MAJOR_OPCODE,
                );
            }
            if !pixmap_exists {
                return emit_x11_error(
                    writer,
                    sequence,
                    x11::error::BAD_DRAWABLE,
                    req.pixmap,
                    PRESENT_MAJOR_OPCODE,
                );
            }

            if let (
                Some(HostDrawableTarget::Pixmap {
                    host_xid,
                    width,
                    height,
                    depth: src_depth,
                    ..
                }),
                Some(dst),
            ) = (src, dst)
            {
                if src_depth != dst.depth() {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_MATCH,
                        req.pixmap,
                        PRESENT_MAJOR_OPCODE,
                    );
                }
                if let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    host.copy_area(
                        host_xid,
                        dst.host_xid(),
                        req.x_off,
                        req.y_off,
                        dst.x_offset(),
                        dst.y_offset(),
                        width,
                        height,
                    )?;
                }
            }

            lock_server(server)?
                .present_msc
                .entry(ResourceId(req.window))
                .and_modify(|msc| *msc = msc.saturating_add(1))
                .or_insert(1);
            debug!(
                "client {} #{} PRESENT::Pixmap serial={} notifies={}",
                client_id.0,
                sequence.0,
                req.serial,
                req.notifies.len()
            );
            Ok(())
        }
        x11present::NOTIFY_MSC => {
            if let Some(req) = x11present::parse_notify_msc(body) {
                lock_server(server)?
                    .present_msc
                    .entry(ResourceId(req.window))
                    .and_modify(|msc| *msc = (*msc).max(req.target_msc).saturating_add(1))
                    .or_insert(req.target_msc.saturating_add(1));
            }
            debug!("client {} #{} PRESENT::NotifyMSC", client_id.0, sequence.0);
            Ok(())
        }
        x11present::PIXMAP_SYNCED => emit_x11_error(
            writer,
            sequence,
            x11::error::BAD_IMPLEMENTATION,
            0,
            PRESENT_MAJOR_OPCODE,
        ),
        _ => {
            debug!(
                "client {} #{} PRESENT unsupported minor {} ({} bytes)",
                client_id.0,
                sequence.0,
                minor,
                body.len()
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
    attached_fd: Option<std::os::fd::RawFd>,
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
                    "client {} create window 0x{:x} parent=0x{:x} pos=({},{}) size={}x{} mask=0x{:x}",
                    client_id.0,
                    request.window.0,
                    request.parent.0,
                    request.x,
                    request.y,
                    request.width,
                    request.height,
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
                // Visual validation. CopyFromParent (visual=0) is always
                // legal — the resolver inherits parent.visual. An explicit
                // visual must be in our local table; otherwise reject
                // locally with BadMatch and never reach the host.
                let visual_known = {
                    let s = lock_server(server)?;
                    request.visual.0 == 0 || s.resources.is_known_visual(request.visual)
                };
                if !visual_known {
                    return emit_x11_error(
                        writer,
                        sequence,
                        x11::error::BAD_MATCH,
                        request.visual.0,
                        1,
                    );
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
                // Allocate a host xid for every non-InputOnly window
                // (Phase 3.6 Step 2). The plan's invariant is
                // "host_xid always Some for class != InputOnly", which
                // includes `CopyFromParent` (the common class clients
                // pass; our resource table preserves the protocol
                // value, but logically it inherits the parent's class
                // — InputOutput, for our top-levels). Top-levels are
                // mapped + registered with the input pump; sub-window
                // host children stay dormant — created with
                // event_mask=0 + bit-gravity NW and never mapped — so
                // their bg pixel doesn't paint over the top-level's
                // content. Drawing for sub-windows still routes via
                // `top_level_host_target` until Step 3.
                let needs_host_xid = {
                    let s = lock_server(server)?;
                    s.resources
                        .window(window_id)
                        .is_some_and(|w| w.class != crate::resources::WindowClass::InputOnly)
                };
                if needs_host_xid && let Some(host) = host {
                    let host_visual = resolve_host_subwindow_visual(server, window_id);
                    let host_parent_xid = if parent == ROOT_WINDOW {
                        host.lock().ok().map(|h| h.window_id())
                    } else {
                        let s = lock_server(server)?;
                        s.resources.window(parent).and_then(|w| w.host_xid)
                    };
                    let allocated_xid: Option<u32> = host_parent_xid.and_then(|host_parent| {
                        host.lock().ok().and_then(|mut h| {
                            let xid = h.allocate_xid();
                            if let Err(err) = h.create_subwindow(
                                host_parent,
                                xid,
                                geometry.0,
                                geometry.1,
                                geometry.2,
                                geometry.3,
                                request.border_width,
                                host_visual,
                            ) {
                                warn!(
                                    "client {} create_subwindow for 0x{:x} failed: {err}",
                                    client_id.0, new_id
                                );
                                return None;
                            }
                            Some(xid)
                        })
                    });

                    if let Some(host_xid) = allocated_xid {
                        {
                            let mut s = lock_server(server)?;
                            if let Some(w) = s.resources.window_mut(window_id) {
                                w.host_xid = Some(host_xid);
                                if parent == ROOT_WINDOW {
                                    // Top-level: host pump delivers
                                    // Expose (ExposureMask selected via
                                    // `register_top_level`), so the
                                    // synthetic emitter must stay
                                    // silent.
                                    w.uses_synthetic_expose = false;
                                }
                                // Sub-windows keep
                                // `uses_synthetic_expose = true` (the
                                // default) — Step 4 flips it once host
                                // ExposureMask routing lands.
                            }
                        }
                        if parent == ROOT_WINDOW
                            && let Some(input_handle) = input_handle
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
                let cursor_id = request.cursor;
                let want_focus_check;
                let viewable;
                let root_bg_host_xid = {
                    let mut s = lock_server(server)?;
                    if let Some(event_mask) = request.event_mask {
                        let entry = s.clients.get_mut(&client_id.0).expect("client registered");
                        if event_mask == 0 {
                            entry.event_masks.remove(&target_window);
                        } else {
                            entry.event_masks.insert(target_window, event_mask);
                        }
                    }
                    let previous_bg_host_xid = s.resources.change_window_attributes(request);
                    if let Some(old_host_xid) = previous_bg_host_xid
                        && !s.resources.host_xid_referenced_by_window_bg(old_host_xid)
                        && let Some(host) = host
                        && let Ok(mut h) = host.lock()
                    {
                        let _ = h.free_pixmap(old_host_xid);
                    }
                    want_focus_check = s
                        .clients
                        .get(&client_id.0)
                        .and_then(|c| c.event_masks.get(&target_window).copied())
                        .unwrap_or(0);
                    viewable = s
                        .resources
                        .window(target_window)
                        .is_some_and(|w| w.map_state == MapState::Viewable);

                    // Mirror root's bg-pixmap onto the host container so the
                    // host server auto-fills regions uncovered by nested
                    // top-level moves. Skip if the request didn't touch
                    // background_pixmap or didn't target root.
                    if target_window == ROOT_WINDOW && request.background_pixmap.is_some() {
                        s.resources.window_background_pixmap_host_xid(ROOT_WINDOW)
                    } else {
                        None
                    }
                };
                if target_window == ROOT_WINDOW {
                    debug!(
                        "client {} CWA(root) bg_pixmap={:?} bg_pixel={:?} root_bg_host_xid={:?}",
                        client_id.0,
                        request.background_pixmap,
                        request.background_pixel,
                        root_bg_host_xid,
                    );
                }
                if target_window == ROOT_WINDOW
                    && let Some(host) = host
                {
                    // Mirror the root background onto the host container so
                    // the host auto-clears regions uncovered by nested
                    // top-level moves. bg_pixmap takes precedence over
                    // bg_pixel when both are set in the same request.
                    if request.background_pixmap.is_some()
                        && let Ok(mut h) = host.lock()
                    {
                        let _ = h.set_container_background_pixmap(root_bg_host_xid.unwrap_or(0));
                    } else if let Some(pixel) = request.background_pixel
                        && let Ok(mut h) = host.lock()
                    {
                        let _ = h.set_container_background_pixel(pixel);
                    }
                }
                if viewable && want_focus_check & 0x3 != 0 {
                    set_focused_window(focused_window, server, target_window)?;
                }
                if let Some(cid) = cursor_id {
                    let (host_window_xid, cursor_host_xid) = {
                        let s = lock_server(server)?;
                        let hw = s.resources.window(target_window).and_then(|w| w.host_xid);
                        let ch = s.resources.cursor_host_xid(cid);
                        (hw, ch)
                    };
                    if let (Some(hw), Some(ch)) = (host_window_xid, cursor_host_xid)
                        && let Some(host) = host
                        && let Ok(mut h) = host.lock()
                    {
                        let _ = h.define_cursor(hw, ch);
                    }
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
                let (pending, bg_pixmap_xids, composite_aliases) = {
                    let mut s = lock_server(server)?;
                    let mut order = Vec::new();
                    collect_destroy_order(&s.resources, window, &mut order);
                    let bg = s.resources.collect_bg_pixmap_host_xids(window);
                    let mut composite_aliases: Vec<NamedCompositePixmap> = Vec::new();
                    let mut pending: Vec<PendingDestroy> = Vec::new();
                    for w in &order {
                        if let Some(win) = s.resources.window_mut(*w) {
                            composite_aliases.append(&mut win.composite_named_pixmaps);
                        }
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
                    for a in &composite_aliases {
                        let _ = s.resources.free_pixmap(a.client_pixmap);
                    }
                    let _ = s.resources.destroy_window(window);
                    s.drop_window_subscriptions(&order);
                    (pending, bg, composite_aliases)
                };
                if !bg_pixmap_xids.is_empty()
                    && let Some(host) = host
                    && let Ok(mut h) = host.lock()
                {
                    for xid in &bg_pixmap_xids {
                        let _ = h.free_pixmap(*xid);
                    }
                }
                if !composite_aliases.is_empty()
                    && let Some(host) = host
                    && let Ok(mut h) = host.lock()
                {
                    for a in &composite_aliases {
                        let _ = h.free_pixmap(a.host_pixmap);
                    }
                }
                for pending in pending {
                    if let Some(xid) = pending.host_xid {
                        // Unregister the host_xid → ResourceId mapping
                        // *before* sending host destroy. A late
                        // host-pump event with this xid arriving after
                        // the local Window is gone would otherwise
                        // misroute to a destroyed ResourceId; with the
                        // mapping cleared first, lookup misses and the
                        // event drops silently.
                        if let Some(input_handle) = input_handle {
                            input_handle.unregister_top_level(xid);
                        }
                        if let Some(host) = host
                            && let Ok(mut h) = host.lock()
                        {
                            let _ = h.destroy_subwindow(xid);
                        }
                    }
                    fanout_destroy_sequence(&pending);
                }
            }
            log_void(client_id, sequence, "DestroyWindow")
        }
        5 => {
            // DestroySubwindows: window(4) — destroy each child of the parent.
            if body.len() >= 4 {
                let parent = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let kids: Vec<ResourceId> =
                    lock_server(server)?.resources.children(parent).to_vec();
                for k in kids {
                    let (pending, composite_aliases) = {
                        let mut s = lock_server(server)?;
                        let mut order = Vec::new();
                        collect_destroy_order(&s.resources, k, &mut order);
                        let mut composite_aliases: Vec<NamedCompositePixmap> = Vec::new();
                        let mut pending: Vec<PendingDestroy> = Vec::new();
                        for w in &order {
                            if let Some(win) = s.resources.window_mut(*w) {
                                composite_aliases.append(&mut win.composite_named_pixmaps);
                            }
                            let (kparent, was_mapped, host_xid) =
                                s.resources
                                    .window(*w)
                                    .map_or((ROOT_WINDOW, false, None), |win| {
                                        (
                                            win.parent,
                                            win.map_state != MapState::Unmapped,
                                            win.host_xid,
                                        )
                                    });
                            let on_window = s.subscribers(*w, 0x0002_0000);
                            let on_parent = s.subscribers(kparent, 0x0008_0000);
                            pending.push(PendingDestroy {
                                window: *w,
                                parent: kparent,
                                was_mapped,
                                host_xid,
                                on_window,
                                on_parent,
                            });
                        }
                        for a in &composite_aliases {
                            let _ = s.resources.free_pixmap(a.client_pixmap);
                        }
                        let _ = s.resources.destroy_window(k);
                        s.drop_window_subscriptions(&order);
                        (pending, composite_aliases)
                    };
                    if !composite_aliases.is_empty()
                        && let Some(host) = host
                        && let Ok(mut h) = host.lock()
                    {
                        for a in &composite_aliases {
                            let _ = h.free_pixmap(a.host_pixmap);
                        }
                    }
                    for entry in pending {
                        if let Some(xid) = entry.host_xid {
                            if let Some(host) = host
                                && let Ok(mut h) = host.lock()
                            {
                                let _ = h.destroy_subwindow(xid);
                            }
                            if let Some(input_handle) = input_handle {
                                input_handle.unregister_top_level(xid);
                            }
                        }
                        fanout_destroy_sequence(&entry);
                    }
                }
            }
            log_void(client_id, sequence, "DestroySubwindows")
        }
        6 => {
            // ChangeSaveSet: window(4); header.data = mode (0=Insert, 1=Delete)
            if body.len() >= 4 {
                let win = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let mut s = lock_server(server)?;
                if let Some(c) = s.clients.get_mut(&client_id.0) {
                    match header.data {
                        0 => {
                            c.save_set.insert(win);
                        }
                        1 => {
                            c.save_set.remove(&win);
                        }
                        _ => {}
                    }
                }
            }
            log_void(client_id, sequence, "ChangeSaveSet")
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
                debug!(
                    "client {} #{} ReparentWindow 0x{:x}: 0x{:x}->0x{:x} pos=({},{}) host_xid={:?}",
                    client_id.0,
                    sequence.0,
                    result.window.0,
                    result.old_parent.0,
                    result.new_parent.0,
                    result.x,
                    result.y,
                    result.host_xid
                );
                if let Some(xid) = result.host_xid {
                    if result.new_parent == ROOT_WINDOW {
                        // Window moved back to root: reposition its host subwindow.
                        if let Some(host) = host
                            && let Ok(mut h) = host.lock()
                        {
                            let _ = h.configure_subwindow(
                                xid,
                                HostSubwindowConfig {
                                    x: Some(result.x),
                                    y: Some(result.y),
                                    ..HostSubwindowConfig::default()
                                },
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
                        debug!(
                            "client {} MapWindow 0x{:x} -> MapRequest to WM",
                            client_id.0, window.0
                        );
                        fanout_event(&redirect_targets, |buf, seq, order| {
                            x11::encode_map_request_event(buf, seq, order, parent, window);
                        });
                    } else {
                        let (map_info, host_xid, uses_synthetic_expose) = {
                            let mut s = lock_server(server)?;
                            let _ = s.resources.map_window(window);
                            let w = s.resources.window(window);
                            let host_xid = w.and_then(|w| w.host_xid);
                            let synthetic = w.is_none_or(|w| w.uses_synthetic_expose);
                            let map_info = w.map(|w| {
                                (w.parent, w.override_redirect, w.x, w.y, w.width, w.height)
                            });
                            (map_info, host_xid, synthetic)
                        };
                        if let Some((parent, override_redir, x, y, width, height)) = map_info {
                            debug!(
                                "client {} MapWindow 0x{:x} direct parent=0x{:x} pos=({},{}) size={}x{} override={} host_xid={:?} synthetic_expose={}",
                                client_id.0,
                                window.0,
                                parent.0,
                                x,
                                y,
                                width,
                                height,
                                override_redir,
                                host_xid,
                                uses_synthetic_expose
                            );
                        } else {
                            debug!(
                                "client {} MapWindow 0x{:x} direct host_xid={:?}",
                                client_id.0, window.0, host_xid
                            );
                        }
                        // Map the host child only when it's a non-dormant
                        // window — i.e. a top-level the host server is
                        // driving. Dormant sub-windows (Phase 3.6 Step 2)
                        // keep their host child unmapped so its bg pixel
                        // doesn't paint over the top-level's content.
                        if let Some(xid) = host_xid
                            && !uses_synthetic_expose
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
                        if let Some((_parent, override_redir, _x, _y, width, height)) = map_info {
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
                            // Synthesize Expose for windows we don't yet
                            // get host Expose for — that's everything with
                            // `uses_synthetic_expose = true`. Top-levels
                            // (synthetic = false) get their Expose from
                            // the host pump; sub-windows during Step 2
                            // are dormant on the host so the synthetic
                            // path stays the only Expose source. Step 4
                            // flips the flag for sub-windows once we
                            // start routing host Expose for them.
                            if uses_synthetic_expose {
                                crate::server::emit_window_event(
                                    server,
                                    window,
                                    0x0000_8000,
                                    |buf, seq, order| {
                                        x11::encode_expose_event(
                                            buf, seq, order, window, 0, 0, width, height, 0,
                                        );
                                    },
                                );
                            }
                            // Descendants that were already mapped (e.g. Xt widget children)
                            // are now viewable; send them Expose so they redraw immediately.
                            emit_expose_subtree(server, window);
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
                    let (extents, host_xid, was_unmapped, override_redirect, synthetic) = {
                        let mut s = lock_server(server)?;
                        let was_unmapped = s.resources.map_window(child);
                        let w = s.resources.window(child);
                        let host_xid = w.and_then(|w| w.host_xid);
                        let extents = w.map(|w| (w.width, w.height));
                        let override_redirect = w.is_some_and(|w| w.override_redirect);
                        let synthetic = w.is_none_or(|w| w.uses_synthetic_expose);
                        (
                            extents,
                            host_xid,
                            was_unmapped,
                            override_redirect,
                            synthetic,
                        )
                    };
                    if let Some(xid) = host_xid
                        && !synthetic
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
                                x11::encode_expose_event(
                                    buf, seq, order, child, 0, 0, width, height, 0,
                                );
                            },
                        );
                    }
                }
            }
            log_void(client_id, sequence, "MapSubwindows")
        }
        10 => {
            if let Some(window) = x11::map_window_id(body) {
                let (snapshot, host_xid, synthetic) = {
                    let mut s = lock_server(server)?;
                    let w = s.resources.window(window);
                    let host_xid = w.and_then(|w| w.host_xid);
                    let synthetic = w.is_none_or(|w| w.uses_synthetic_expose);
                    let was_mapped = s.resources.unmap_window(window);
                    let snapshot = if was_mapped {
                        let parent = s.resources.window(window).map_or(ROOT_WINDOW, |w| w.parent);
                        let on_window = s.subscribers(window, 0x0002_0000); // StructureNotify
                        let on_parent = s.subscribers(parent, 0x0008_0000); // SubstructureNotify
                        Some((parent, on_window, on_parent))
                    } else {
                        None
                    };
                    (snapshot, host_xid, synthetic)
                };
                if let Some(xid) = host_xid
                    && !synthetic
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
                    synthetic: bool,
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
                        let w = s.resources.window(child);
                        let host_xid = w.and_then(|w| w.host_xid);
                        let synthetic = w.is_none_or(|w| w.uses_synthetic_expose);
                        if s.resources.unmap_window(child) {
                            pending.push(PendingUnmap {
                                child,
                                host_xid,
                                synthetic,
                                on_child: s.subscribers(child, 0x0002_0000),
                                on_parent: s.subscribers(parent, 0x0008_0000),
                            });
                        }
                    }
                    pending
                };
                for item in pending {
                    if let Some(xid) = item.host_xid
                        && !item.synthetic
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
                    let (configure, host_xid, sibling_host_xid, old_size, parent) = {
                        let mut s = lock_server(server)?;
                        let old_size = s
                            .resources
                            .window(request.window)
                            .map(|w| (w.width, w.height));
                        let sibling_host_xid = request
                            .sibling
                            .and_then(|sibling| s.resources.window(sibling))
                            .and_then(|w| w.host_xid);
                        let configure = s
                            .resources
                            .configure_window(request)
                            .map(|w| (w.id, window_geometry(w), w.override_redirect));
                        let host_xid = configure.as_ref().and_then(|(id, _, _)| {
                            s.resources.window(*id).and_then(|w| w.host_xid)
                        });
                        let parent = configure
                            .as_ref()
                            .and_then(|(id, _, _)| s.resources.window(*id).map(|w| w.parent));
                        (configure, host_xid, sibling_host_xid, old_size, parent)
                    };
                    debug!(
                        "client {} #{} ConfigureWindow 0x{:x} parent={:?} mask=0x{:x} x={:?} y={:?} w={:?} h={:?} sibling={:?}/host={:?} stack={:?} host_xid={:?}",
                        client_id.0,
                        sequence.0,
                        request.window.0,
                        parent.map(|p| format!("0x{:x}", p.0)),
                        request.value_mask,
                        request.x,
                        request.y,
                        request.width,
                        request.height,
                        request.sibling.map(|s| format!("0x{:x}", s.0)),
                        sibling_host_xid,
                        request.stack_mode,
                        host_xid
                    );
                    if let Some(xid) = host_xid
                        && let Some(host) = host
                        && let Ok(mut h) = host.lock()
                    {
                        let _ = h.configure_subwindow(
                            xid,
                            HostSubwindowConfig {
                                x: request.x,
                                y: request.y,
                                width: request.width,
                                height: request.height,
                                sibling: sibling_host_xid,
                                stack_mode: request.stack_mode,
                            },
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
                        // COMPOSITE: a resize invalidates every alias on this
                        // window in one shot. The compositor must re-issue
                        // NameWindowPixmap after the resize.
                        let resized = old_size
                            .is_some_and(|(ow, oh)| geometry.width != ow || geometry.height != oh);
                        if resized {
                            invalidate_composite_named_pixmaps(server, host, window_id);
                        }
                        let grew = old_size
                            .is_some_and(|(ow, oh)| geometry.width > ow || geometry.height > oh);
                        if grew {
                            crate::server::emit_window_event(
                                server,
                                window_id,
                                0x0000_8000,
                                |buf, seq, order| {
                                    x11::encode_expose_event(
                                        buf,
                                        seq,
                                        order,
                                        window_id,
                                        0,
                                        0,
                                        geometry.width,
                                        geometry.height,
                                        0,
                                    );
                                },
                            );
                            emit_expose_subtree(server, window_id);
                        }
                    }
                }
            }
            log_void(client_id, sequence, "ConfigureWindow")
        }
        13 => {
            // CirculateWindow body: container(4); header.data = direction
            // (0=RaiseLowest, 1=LowerHighest). The argument is the container.
            if body.len() >= 4 {
                let container =
                    ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let direction = header.data;
                let chosen_child = {
                    let s = lock_server(server)?;
                    let kids = s.resources.children(container);
                    match (direction, kids.first(), kids.last()) {
                        (0, _, Some(&back)) => Some(back),
                        (1, Some(&front), _) => Some(front),
                        _ => None,
                    }
                };
                if let Some(child) = chosen_child {
                    let redirect_target = lock_server(server)?
                        .subscribers(container, 0x0010_0000)
                        .into_iter()
                        .next();
                    if let Some(target) = redirect_target {
                        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                        let mut buf = Vec::with_capacity(32);
                        let _ = x11::write_circulate_request_event(
                            &mut buf, seq, container, child, direction,
                        );
                        if let Ok(mut w) = target.writer.lock() {
                            let _ = w.write_all(&buf);
                        }
                    } else {
                        let _ = lock_server(server)?
                            .resources
                            .circulate_window(container, direction);
                        let on_child = lock_server(server)?.subscribers(child, 0x0002_0000);
                        let on_container = lock_server(server)?.subscribers(container, 0x0008_0000);
                        for t in on_child.into_iter().chain(on_container) {
                            let seq = SequenceNumber(t.last_sequence.load(Ordering::Relaxed));
                            let mut buf = Vec::with_capacity(32);
                            let _ = x11::write_circulate_notify_event(
                                &mut buf, seq, child, child, direction,
                            );
                            if let Ok(mut w) = t.writer.lock() {
                                let _ = w.write_all(&buf);
                            }
                        }
                    }
                }
            }
            log_void(client_id, sequence, "CirculateWindow")
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
            debug!(
                "client {} #{} GetAtomName {}",
                client_id.0, sequence.0, atom.0,
            );
            handle_get_atom_name(server, host, writer, sequence, atom)
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
        21 => {
            let atoms: Vec<AtomId> = if body.len() >= 4 {
                let window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let s = lock_server(server)?;
                s.resources
                    .window(window)
                    .map(|w| w.properties.keys().copied().collect())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            x11::write_list_properties_reply(&mut *lock_writer()?, sequence, &atoms)
        }
        22 => {
            // SetSelectionOwner: window(4) selection(4) time(4)
            if body.len() >= 8 {
                let window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let selection = AtomId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
                let time_val = if body.len() >= 12 {
                    u32::from_le_bytes([body[8], body[9], body[10], body[11]])
                } else {
                    0u32
                };

                let (old_owner_info, name) = {
                    let mut s = lock_server(server)?;
                    // Capture old owner before modification
                    let old = s.selection_owner_target(selection);
                    let old_window = s.selections.get(&selection).copied();
                    // Perform the update
                    if window.0 == 0 {
                        s.selections.remove(&selection);
                    } else {
                        s.selections.insert(selection, window);
                    }
                    let name = s.atoms.name(selection).map(str::to_owned);
                    // Only send SelectionClear if old owner ≠ new owner
                    let send_clear = old_window.is_some()
                        && old_window != (if window.0 == 0 { None } else { Some(window) });
                    (if send_clear { old } else { None }, name)
                };

                // Send SelectionClear to displaced owner
                if let Some((old_window, old_target)) = old_owner_info {
                    let seq = SequenceNumber(old_target.last_sequence.load(Ordering::Relaxed));
                    let mut buf = Vec::with_capacity(32);
                    x11::encode_selection_clear_event(
                        &mut buf,
                        seq,
                        old_target.byte_order,
                        time_val,
                        old_window,
                        selection,
                    );
                    if let Ok(mut w) = old_target.writer.lock() {
                        let _ = w.write_all(&buf);
                    }
                }

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
        24 => {
            // ConvertSelection: requestor(4) selection(4) target(4) property(4) time(4)
            if body.len() >= 20 {
                let requestor =
                    ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let selection = AtomId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
                let target_atom =
                    AtomId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
                let property = AtomId(u32::from_le_bytes([body[12], body[13], body[14], body[15]]));
                let time_val = u32::from_le_bytes([body[16], body[17], body[18], body[19]]);

                let owner_info = {
                    let s = lock_server(server)?;
                    s.selection_owner_target(selection)
                };

                if let Some((owner_window, owner_target)) = owner_info {
                    // Deliver SelectionRequest to owner
                    let seq = SequenceNumber(owner_target.last_sequence.load(Ordering::Relaxed));
                    let mut buf = Vec::with_capacity(32);
                    x11::encode_selection_request_event(
                        &mut buf,
                        seq,
                        owner_target.byte_order,
                        time_val,
                        owner_window,
                        requestor,
                        selection,
                        target_atom,
                        property,
                    );
                    if let Ok(mut w) = owner_target.writer.lock() {
                        let _ = w.write_all(&buf);
                    }
                    debug!(
                        "client {} #{} ConvertSelection -> owner 0x{:x}",
                        client_id.0, sequence.0, owner_window.0
                    );
                } else {
                    // No owner: send SelectionNotify with property=None to requestor
                    let requestor_target = {
                        let s = lock_server(server)?;
                        s.resources
                            .window_owner(requestor)
                            .and_then(|cid| s.client_target(cid))
                    };
                    if let Some(rt) = requestor_target {
                        let seq = SequenceNumber(rt.last_sequence.load(Ordering::Relaxed));
                        let mut buf = [0u8; 32];
                        buf[0] = 31; // SelectionNotify
                        buf[2] = (seq.0 & 0xff) as u8;
                        buf[3] = ((seq.0 >> 8) & 0xff) as u8;
                        buf[4..8].copy_from_slice(&time_val.to_le_bytes());
                        buf[8..12].copy_from_slice(&requestor.0.to_le_bytes());
                        buf[12..16].copy_from_slice(&selection.0.to_le_bytes());
                        buf[16..20].copy_from_slice(&target_atom.0.to_le_bytes());
                        // property = 0 (None): conversion failed
                        if let Ok(mut w) = rt.writer.lock() {
                            let _ = w.write_all(&buf);
                        }
                    }
                    debug!(
                        "client {} #{} ConvertSelection: no owner, sent SelectionNotify(None)",
                        client_id.0, sequence.0
                    );
                }
            }
            Ok(())
        }
        25 => {
            if let Some(req) = x11::send_event_request(header.data, body) {
                // Set the sent-event bit (bit 7 of first byte)
                let mut event_copy = *req.event;
                event_copy[0] |= 0x80;

                let targets = {
                    let s = lock_server(server)?;
                    if req.destination.0 == 0xffff_ffff {
                        // Broadcast to root subscribers
                        s.subscribers_intersecting(ROOT_WINDOW, req.event_mask)
                    } else {
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
                    }
                };
                fanout_raw_event(&targets, &event_copy);
                debug!(
                    "client {} #{} SendEvent type={} dest=0x{:x}",
                    client_id.0,
                    sequence.0,
                    req.event[0] & 0x7f,
                    req.destination.0
                );
            }
            log_void(client_id, sequence, "SendEvent")
        }
        26 => {
            // GrabPointer body: owner_events(header.data) grab_window(4) event_mask(2)
            //   pointer_mode(1) keyboard_mode(1) confine_to(4) cursor(4) time(4)
            if body.len() >= 20 {
                let grab_window =
                    ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let event_mask = u16::from_le_bytes([body[4], body[5]]);
                let cursor =
                    ResourceId(u32::from_le_bytes([body[12], body[13], body[14], body[15]]));
                let time = u32::from_le_bytes([body[16], body[17], body[18], body[19]]);
                let mut s = lock_server(server)?;
                s.pointer_grab = Some((client_id, grab_window));
                s.pointer_grab_is_passive = false;
                s.active_pointer_grab = Some(crate::server::ActivePointerGrab {
                    owner: client_id,
                    grab_window,
                    event_mask,
                    cursor,
                    time,
                });
            }
            log_reply(client_id, sequence, "GrabPointer");
            x11::write_grab_reply(&mut *lock_writer()?, sequence, 0)
        }
        27 => {
            let mut s = lock_server(server)?;
            s.pointer_grab = None;
            s.pointer_grab_is_passive = false;
            s.active_pointer_grab = None;
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
        30 => {
            // ChangeActivePointerGrab body: cursor(4) time(4) event_mask(2) pad(2)
            if body.len() >= 12 {
                let cursor = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let time = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let event_mask = u16::from_le_bytes([body[8], body[9]]);
                let mut s = lock_server(server)?;
                if let Some(g) = s.active_pointer_grab.as_mut()
                    && g.owner == client_id
                {
                    g.event_mask = event_mask;
                    g.cursor = cursor;
                    g.time = time;
                }
            }
            log_void(client_id, sequence, "ChangeActivePointerGrab")
        }
        31 => {
            // GrabKeyboard body: owner_events(header.data) grab_window(4)
            //   time(4) pointer_mode(1) keyboard_mode(1) pad(2)
            if body.len() >= 12 {
                let grab_window =
                    ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let mut s = lock_server(server)?;
                s.active_keyboard_grab = Some(crate::server::ActiveKeyboardGrab {
                    owner: client_id,
                    grab_window,
                    source: crate::server::ActiveKeyboardGrabSource::Explicit,
                });
            }
            log_reply(client_id, sequence, "GrabKeyboard");
            x11::write_grab_reply(&mut *lock_writer()?, sequence, 0)
        }
        32 => {
            let mut s = lock_server(server)?;
            if let Some(g) = s.active_keyboard_grab
                && g.owner == client_id
            {
                s.active_keyboard_grab = None;
            }
            log_void(client_id, sequence, "UngrabKeyboard")
        }
        33 => {
            if let Some(req) = x11::parse_grab_key(body, header.data != 0) {
                let mut s = lock_server(server)?;
                let grab_window = ResourceId(req.grab_window);
                s.key_grabs.retain(|g| {
                    !(g.owner == client_id
                        && g.grab_window == grab_window
                        && g.keycode == req.keycode
                        && g.modifiers == req.modifiers)
                });
                s.key_grabs.push(crate::server::KeyGrab {
                    owner: client_id,
                    grab_window,
                    keycode: req.keycode,
                    modifiers: req.modifiers,
                    owner_events: req.owner_events,
                    pointer_mode: req.pointer_mode,
                    keyboard_mode: req.keyboard_mode,
                });
                debug!(
                    "client {} GrabKey window=0x{:x} keycode={} modifiers=0x{:x}",
                    client_id.0, req.grab_window, req.keycode, req.modifiers
                );
            }
            log_void(client_id, sequence, "GrabKey")
        }
        34 => {
            if let Some(req) = x11::parse_ungrab_key(body, header.data) {
                let mut s = lock_server(server)?;
                let grab_window = ResourceId(req.grab_window);
                s.key_grabs.retain(|g| {
                    !(g.owner == client_id
                        && g.grab_window == grab_window
                        && (g.keycode == req.keycode || req.keycode == 0)
                        && (g.modifiers == req.modifiers || req.modifiers == 0x8000))
                });
            }
            log_void(client_id, sequence, "UngrabKey")
        }
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
                    root_x: pointer.win_x,
                    root_y: pointer.win_y,
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
            let (child, dst_x, dst_y) = if body.len() >= 12 {
                let src_window =
                    ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let dst_window =
                    ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
                let src_x = i16::from_le_bytes([body[8], body[9]]);
                let src_y = i16::from_le_bytes([body[10], body[11]]);
                let s = lock_server(server)?;
                let (src_abs_x, src_abs_y) = s.resources.window_absolute_position(src_window);
                let abs_x = src_abs_x + i32::from(src_x);
                let abs_y = src_abs_y + i32::from(src_y);
                let (dst_abs_x, dst_abs_y) = s.resources.window_absolute_position(dst_window);
                #[allow(clippy::cast_possible_truncation)]
                let dst_x = (abs_x - dst_abs_x) as i16;
                #[allow(clippy::cast_possible_truncation)]
                let dst_y = (abs_y - dst_abs_y) as i16;
                let child = s
                    .resources
                    .child_containing_point(dst_window, abs_x, abs_y)
                    .unwrap_or(ResourceId(0));
                (child, dst_x, dst_y)
            } else {
                (ResourceId(0), 0i16, 0i16)
            };
            x11::write_translate_coordinates_reply(
                &mut *lock_writer()?,
                sequence,
                child,
                dst_x,
                dst_y,
            )
        }
        41 => {
            if body.len() >= 20 {
                let dst_window =
                    ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
                let dst_x = i16::from_le_bytes([body[16], body[17]]);
                let dst_y = i16::from_le_bytes([body[18], body[19]]);
                let host_target = {
                    let s = lock_server(server)?;
                    if dst_window.0 == 0 {
                        None
                    } else {
                        s.resources.host_drawable_target(dst_window).map(|t| {
                            (
                                t.host_xid(),
                                dst_x.wrapping_add(t.x_offset()),
                                dst_y.wrapping_add(t.y_offset()),
                            )
                        })
                    }
                };
                if let Some((host_xid, x, y)) = host_target
                    && let Some(h) = host
                    && let Ok(mut h) = h.lock()
                {
                    let _ = h.warp_pointer(host_xid, x, y);
                }
            }
            log_void(client_id, sequence, "WarpPointer")
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
                let (removed, still_referenced) = {
                    let mut s = lock_server(server)?;
                    let removed = s.resources.free_pixmap(pixmap);
                    let still_referenced = removed
                        .as_ref()
                        .and_then(|p| p.host_xid)
                        .is_some_and(|xid| s.resources.host_xid_referenced_by_window_bg(xid));
                    (removed, still_referenced)
                };
                if let Some(removed_pixmap) = removed
                    && let Some(xid) = removed_pixmap.host_xid
                    && !still_referenced
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
        57 => {
            if body.len() >= 12 {
                let src_gc = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let dst_gc = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
                let value_mask = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
                let mut s = lock_server(server)?;
                s.resources.copy_gc(src_gc, dst_gc, value_mask);
            }
            log_void(client_id, sequence, "CopyGC")
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
                    if width != 0 && height != 0 {
                        accumulate_damage(
                            server,
                            request.window,
                            request.x,
                            request.y,
                            width,
                            height,
                        );
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
                        s.resources.gc_clip_state(request.gc),
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
                match (src.as_ref(), dst.as_ref()) {
                    (Some(src), Some(dst)) => {
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
                            apply_gc_clip(&mut host, &clip, dst.x_offset(), dst.y_offset())?;
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
                        accumulate_damage(
                            server,
                            request.dst,
                            request.dst_x,
                            request.dst_y,
                            request.width,
                            request.height,
                        );
                    }
                    _ => {}
                }
            }
            log_void(client_id, sequence, "CopyArea")
        }
        63 => {
            // CopyPlane: src(4) dst(4) gc(4) sx(2) sy(2) dx(2) dy(2) w(2) h(2) plane(4) = 28 bytes
            if body.len() >= 28 {
                let src = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let dst = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
                let gc = ResourceId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
                let sx = i16::from_le_bytes([body[12], body[13]]);
                let sy = i16::from_le_bytes([body[14], body[15]]);
                let dx = i16::from_le_bytes([body[16], body[17]]);
                let dy = i16::from_le_bytes([body[18], body[19]]);
                let w = u16::from_le_bytes([body[20], body[21]]);
                let h = u16::from_le_bytes([body[22], body[23]]);
                let plane = u32::from_le_bytes([body[24], body[25], body[26], body[27]]);
                if w != 0 && h != 0 {
                    let (gc_exists, clip, src_target, dst_target) = {
                        let s = lock_server(server)?;
                        (
                            s.resources.gc(gc).is_some(),
                            s.resources.gc_clip_state(gc),
                            s.resources.host_drawable_target(src),
                            s.resources.host_drawable_target(dst),
                        )
                    };
                    if !gc_exists {
                        return emit_x11_error(writer, sequence, x11::error::BAD_GC, gc.0, 63);
                    }
                    if let (Some(srct), Some(dstt)) = (src_target, dst_target)
                        && let Some(host_arc) = host
                        && let Ok(mut hh) = host_arc.lock()
                    {
                        apply_gc_clip(&mut hh, &clip, dstt.x_offset(), dstt.y_offset())?;
                        hh.copy_plane(
                            srct.host_xid(),
                            dstt.host_xid(),
                            translate_i16(sx, srct.x_offset()),
                            translate_i16(sy, srct.y_offset()),
                            translate_i16(dx, dstt.x_offset()),
                            translate_i16(dy, dstt.y_offset()),
                            w,
                            h,
                            plane,
                        )?;
                    }
                    accumulate_damage(server, dst, dx, dy, w, h);
                }
            }
            log_void(client_id, sequence, "CopyPlane")
        }
        64 => {
            if body.len() >= 8 {
                let drawable = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let gc_id = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let points = &body[8..];
                let (foreground, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(ResourceId(gc_id)),
                        s.resources.gc_clip_state(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
                    let translated = translated_points(
                        points,
                        header.data,
                        target.x_offset(),
                        target.y_offset(),
                    );
                    host.poly_point(target.host_xid(), foreground, header.data, &translated)?;
                }
                accumulate_damage_full(server, drawable);
            }
            log_void(client_id, sequence, "PolyPoint")
        }
        65 => {
            if let Some((gc_id, points)) = x11::poly_line_data(body)
                && let Some(drawable) = x11::drawable_request_id(body)
            {
                let (foreground, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(ResourceId(gc_id)),
                        s.resources.gc_clip_state(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
                    let translated = translated_points(
                        points,
                        header.data,
                        target.x_offset(),
                        target.y_offset(),
                    );
                    host.poly_line(target.host_xid(), foreground, header.data, &translated)?;
                }
                accumulate_damage_full(server, drawable);
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
                        s.resources.gc_clip_state(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_segments(segments, target.x_offset(), target.y_offset());
                    host.poly_segment(target.host_xid(), foreground, &translated)?;
                }
                accumulate_damage_full(server, drawable);
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
                        s.resources.gc_clip_state(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_records(rectangles, 8, target.x_offset(), target.y_offset());
                    host.poly_rectangle(target.host_xid(), foreground, &translated)?;
                }
                accumulate_damage_full(server, drawable);
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
                        s.resources.gc_clip_state(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_records(arcs, 12, target.x_offset(), target.y_offset());
                    host.poly_arc(target.host_xid(), foreground, &translated)?;
                }
                accumulate_damage_full(server, drawable);
            }
            log_void(client_id, sequence, "PolyArc")
        }
        69 => {
            if body.len() >= 12 {
                let drawable = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let gc_id = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let coord_mode = body[9];
                let points = &body[12..];
                let (foreground, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(ResourceId(gc_id)),
                        s.resources.gc_clip_state(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_points(points, coord_mode, target.x_offset(), target.y_offset());
                    host.fill_poly(target.host_xid(), foreground, coord_mode, &translated)?;
                }
                accumulate_damage_full(server, drawable);
            }
            log_void(client_id, sequence, "FillPoly")
        }
        70 => {
            if let Some((gc_id, rectangles)) = x11::poly_fill_rectangle_data(body)
                && let Some(drawable) = x11::drawable_request_id(body)
            {
                let (foreground, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(ResourceId(gc_id)),
                        s.resources.gc_clip_state(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_records(rectangles, 8, target.x_offset(), target.y_offset());
                    host.poly_fill_rectangle(target.host_xid(), foreground, &translated)?;
                }
                accumulate_damage_full(server, drawable);
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
                        s.resources.gc_clip_state(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_records(arcs, 12, target.x_offset(), target.y_offset());
                    host.poly_fill_arc(target.host_xid(), foreground, &translated)?;
                }
                accumulate_damage_full(server, drawable);
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
                        s.resources.gc_clip_state(request.gc),
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
                        apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
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
                    accumulate_damage(
                        server,
                        request.drawable,
                        request.dst_x,
                        request.dst_y,
                        request.width,
                        request.height,
                    );
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
                        s.resources.gc_clip_state(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_text_body(text_body, target.x_offset(), target.y_offset());
                    host.poly_text8(target.host_xid(), foreground, &translated)?;
                }
                accumulate_damage_full(server, drawable);
            }
            log_void(client_id, sequence, "PolyText8")
        }
        75 => {
            if let Some((drawable_raw, gc_id, text_body)) = x11::poly_text_data(body) {
                let drawable = ResourceId(drawable_raw);
                let (foreground, clip, target) = {
                    let s = lock_server(server)?;
                    (
                        s.resources.gc_foreground(ResourceId(gc_id)),
                        s.resources.gc_clip_state(ResourceId(gc_id)),
                        s.resources.host_drawable_target(drawable),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
                    let translated =
                        translated_text_body(text_body, target.x_offset(), target.y_offset());
                    host.poly_text16(target.host_xid(), foreground, &translated)?;
                }
                accumulate_damage_full(server, drawable);
            }
            log_void(client_id, sequence, "PolyText16")
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
                        s.resources.gc_clip_state(gc),
                        s.resources.host_drawable_target(ResourceId(drawable)),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
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
                accumulate_damage_full(server, ResourceId(drawable));
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
                        s.resources.gc_clip_state(gc),
                        s.resources.host_drawable_target(ResourceId(drawable)),
                    )
                };
                if let Some(target) = target
                    && let Some(host) = host
                    && let Ok(mut host) = host.lock()
                {
                    apply_gc_clip(&mut host, &clip, target.x_offset(), target.y_offset())?;
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
                accumulate_damage_full(server, ResourceId(drawable));
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
        93 => {
            if body.len() >= 28 {
                let cursor_id =
                    ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let source_id =
                    ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
                let mask_id =
                    ResourceId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
                let fore = (
                    u16::from_le_bytes([body[12], body[13]]),
                    u16::from_le_bytes([body[14], body[15]]),
                    u16::from_le_bytes([body[16], body[17]]),
                );
                let back = (
                    u16::from_le_bytes([body[18], body[19]]),
                    u16::from_le_bytes([body[20], body[21]]),
                    u16::from_le_bytes([body[22], body[23]]),
                );
                let hot_x = u16::from_le_bytes([body[24], body[25]]);
                let hot_y = u16::from_le_bytes([body[26], body[27]]);

                let (src_host, mask_host) = {
                    let s = lock_server(server)?;
                    let src = s
                        .resources
                        .pixmap(source_id)
                        .and_then(|p| p.host_xid)
                        .unwrap_or(0);
                    let mask = if mask_id.0 == 0 {
                        0
                    } else {
                        s.resources
                            .pixmap(mask_id)
                            .and_then(|p| p.host_xid)
                            .unwrap_or(0)
                    };
                    (src, mask)
                };

                {
                    let mut s = lock_server(server)?;
                    s.resources.create_cursor(client_id, cursor_id);
                }

                if src_host != 0
                    && let Some(host) = host
                    && let Ok(mut h) = host.lock()
                {
                    match h.create_cursor(src_host, mask_host, fore, back, hot_x, hot_y) {
                        Ok(host_xid) => {
                            let mut s = lock_server(server)?;
                            s.resources.set_cursor_host_xid(cursor_id, host_xid);
                        }
                        Err(err) => {
                            warn!("client {} CreateCursor failed: {err}", client_id.0);
                        }
                    }
                }
            }
            log_void(client_id, sequence, "CreateCursor")
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
            let (present, major_opcode, first_event, first_error) =
                extension_query_reply(&name, host)
                    .map(|(major_opcode, first_event, first_error)| {
                        (true, major_opcode, first_event, first_error)
                    })
                    .unwrap_or((false, 0, 0, 0));
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
            let names = advertised_extension_names(host);
            x11::write_list_extensions_reply(&mut *lock_writer()?, sequence, &names)
        }
        100 => {
            // ChangeKeyboardMapping: keycode_count in header.data;
            //   body: first_keycode(1) keysyms_per_keycode(1) pad(2) keysyms(...)
            let first_keycode = body.first().copied().unwrap_or(8);
            let count = header.data;
            let targets: Vec<_> = match server.lock() {
                Ok(s) => s
                    .clients
                    .values()
                    .map(|c| crate::server::EventTarget {
                        writer: c.writer.clone(),
                        byte_order: c.byte_order,
                        last_sequence: c.last_sequence.clone(),
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            crate::server::fanout_event(&targets, |buf, seq, _order| {
                let _ = x11::write_mapping_notify_event(buf, seq, 1, first_keycode, count);
            });
            log_void(client_id, sequence, "ChangeKeyboardMapping")
        }
        101 => {
            log_reply(client_id, sequence, "GetKeyboardMapping");
            let first_keycode = body.first().copied().unwrap_or(8);
            let keycode_count = body.get(1).copied().unwrap_or(0);
            let proxied = host
                .and_then(|h| h.lock().ok())
                .and_then(|mut h| h.get_keyboard_mapping(first_keycode, keycode_count).ok());
            if let Some((kpc, keysyms)) = proxied {
                x11::write_get_keyboard_mapping_reply_from_keysyms(
                    &mut *lock_writer()?,
                    sequence,
                    kpc,
                    &keysyms,
                )
            } else {
                x11::write_get_keyboard_mapping_reply(
                    &mut *lock_writer()?,
                    sequence,
                    first_keycode,
                    keycode_count,
                    4,
                )
            }
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
            let proxied = host
                .and_then(|h| h.lock().ok())
                .and_then(|mut h| h.get_modifier_mapping().ok());
            if let Some((kpm, keycodes)) = proxied {
                x11::write_get_modifier_mapping_reply_with_keycodes(
                    &mut *lock_writer()?,
                    sequence,
                    kpm,
                    &keycodes,
                )
            } else {
                x11::write_get_modifier_mapping_reply(&mut *lock_writer()?, sequence)
            }
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
        RENDER_MAJOR_OPCODE => handle_render_request(
            client_id,
            server,
            host,
            writer,
            sequence,
            header.data, // RENDER minor opcode
            body,
        ),
        XFIXES_MAJOR_OPCODE => handle_xfixes_request(
            client_id,
            server,
            host,
            writer,
            sequence,
            header.data, // XFIXES minor opcode
            body,
        ),
        SHAPE_MAJOR_OPCODE => handle_shape_request(
            client_id,
            server,
            host,
            writer,
            sequence,
            header.data, // SHAPE minor opcode
            body,
        ),
        SYNC_MAJOR_OPCODE => handle_sync_request(
            client_id,
            server,
            writer,
            sequence,
            header.data, // SYNC minor opcode
            body,
        ),
        DAMAGE_MAJOR_OPCODE => handle_damage_request(
            client_id,
            server,
            writer,
            sequence,
            header.data, // DAMAGE minor opcode
            body,
        ),
        COMPOSITE_MAJOR_OPCODE => handle_composite_request(
            client_id,
            server,
            host,
            writer,
            sequence,
            header.data, // COMPOSITE minor opcode
            body,
        ),
        PRESENT_MAJOR_OPCODE => handle_present_request(
            client_id,
            server,
            host,
            writer,
            sequence,
            header.data, // PRESENT minor opcode
            body,
        ),
        MIT_SHM_MAJOR_OPCODE => handle_mit_shm_request(
            client_id,
            server,
            host,
            writer,
            sequence,
            header.data, // MIT-SHM minor opcode
            body,
            attached_fd,
        ),
        GE_MAJOR_OPCODE => {
            // Generic Event Extension: only request is GEQueryVersion (minor=0)
            if header.data == 0 {
                log_reply(client_id, sequence, "GEQueryVersion");
                x11::write_ge_query_version_reply(&mut *lock_writer()?, sequence)
            } else {
                Ok(())
            }
        }
        BIG_REQUESTS_MAJOR_OPCODE => {
            let minor = header.data;
            if minor == 0 {
                // Enable
                log_reply(client_id, sequence, "BigRequestsEnable");
                {
                    let mut s = lock_server(server)?;
                    if let Some(client) = s.clients.get_mut(&client_id.0) {
                        client.big_requests_enabled = true;
                    }
                }
                // Max length: 64k units (256 KB) or similar.
                // X11 says max length is in 4-byte units.
                // u16 length field in header is 64k * 4 = 256KB.
                // BIG-REQUESTS allows 4GB.
                // We'll advertise 1MB (256k units) for now.
                x11::write_big_requests_enable_reply(&mut *lock_writer()?, sequence, 256 * 1024)
            } else {
                Ok(())
            }
        }
        XKB_MAJOR_OPCODE => {
            let minor = header.data;
            debug!(
                "client {} #{} XkbProxy minor={}",
                client_id.0, sequence.0, minor
            );
            if minor == 1 && body.len() >= 12 {
                let device_spec = u16::from_le_bytes([body[0], body[1]]);
                let clear = u16::from_le_bytes([body[4], body[5]]);
                let select_all = u16::from_le_bytes([body[6], body[7]]);
                let selected = select_all & !clear;
                let mut s = lock_server(server)?;
                if selected == 0 {
                    s.xkb_select_event_masks.remove(&(client_id.0, device_spec));
                } else {
                    s.xkb_select_event_masks
                        .insert((client_id.0, device_spec), selected);
                }
            }
            let reply = host
                .and_then(|h| h.lock().ok())
                .and_then(|mut h| h.xkb_proxy(minor, body).ok())
                .flatten();
            if let Some(mut bytes) = reply {
                // Patch the sequence number in the reply to match the client's.
                if bytes.len() >= 4 {
                    bytes[2..4].copy_from_slice(&sequence.0.to_le_bytes());
                }
                writer
                    .lock()
                    .map_err(|_| io::Error::other("writer poisoned"))?
                    .write_all(&bytes)
            } else {
                Ok(())
            }
        }
        XI2_MAJOR_OPCODE => {
            let minor = header.data;
            match minor {
                1 => {
                    // GetExtensionVersion (XI1): present=true, major=2, minor=0
                    log_reply(client_id, sequence, "XIGetExtensionVersion");
                    let mut reply = x11::fixed_reply(sequence, 0, 0);
                    x11::write_u16(ClientByteOrder::LittleEndian, &mut reply, 2); // server_major
                    x11::write_u16(ClientByteOrder::LittleEndian, &mut reply, 0); // server_minor
                    reply.push(1); // present=true
                    reply.extend_from_slice(&[0; 19]);
                    writer
                        .lock()
                        .map_err(|_| io::Error::other("writer poisoned"))?
                        .write_all(&reply)
                }
                42 => {
                    // XIChangeCursor: no reply
                    log_void(client_id, sequence, "XIChangeCursor")?;
                    Ok(())
                }
                44 => {
                    // XISetClientPointer
                    log_void(client_id, sequence, "XISetClientPointer")?;
                    Ok(())
                }
                45 => {
                    // XIGetClientPointer
                    log_reply(client_id, sequence, "XIGetClientPointer");
                    let mut reply = x11::fixed_reply(sequence, 0, 0);
                    reply.push(1); // set=true
                    reply.push(0); // pad
                    x11::write_u16(ClientByteOrder::LittleEndian, &mut reply, 2); // deviceid=2 (Master Pointer)
                    reply.extend_from_slice(&[0; 22]);
                    writer
                        .lock()
                        .map_err(|_| io::Error::other("writer poisoned"))?
                        .write_all(&reply)
                }
                46 => {
                    // XISelectEvents: window(4) num_masks(2) pad(2) [deviceid(2) mask_len(2) masks(4*n)]
                    log_void(client_id, sequence, "XISelectEvents")?;
                    if body.len() >= 8 {
                        let window =
                            ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                        let num_masks = u16::from_le_bytes([body[4], body[5]]) as usize;
                        let mut pos = 8;
                        let mut s = lock_server(server)?;
                        if let Some(client) = s.clients.get_mut(&client_id.0) {
                            for _ in 0..num_masks {
                                if pos + 4 > body.len() {
                                    break;
                                }
                                let deviceid = u16::from_le_bytes([body[pos], body[pos + 1]]);
                                let mask_len =
                                    u16::from_le_bytes([body[pos + 2], body[pos + 3]]) as usize;
                                pos += 4;
                                let byte_len = mask_len.saturating_mul(4);
                                if pos + byte_len > body.len() {
                                    break;
                                }
                                let mask = if mask_len > 0 {
                                    u32::from_le_bytes([
                                        body[pos],
                                        body[pos + 1],
                                        body[pos + 2],
                                        body[pos + 3],
                                    ])
                                } else {
                                    0
                                };
                                if mask == 0 {
                                    client.xi2_masks.remove(&(window, deviceid));
                                } else {
                                    client.xi2_masks.insert((window, deviceid), mask);
                                }
                                pos += byte_len;
                            }
                        }
                    }
                    Ok(())
                }
                47 => {
                    // XIQueryVersion
                    log_reply(client_id, sequence, "XIQueryVersion");
                    let mut reply = x11::fixed_reply(sequence, 0, 0);
                    // version 2.2
                    x11::write_u16(ClientByteOrder::LittleEndian, &mut reply, 2);
                    x11::write_u16(ClientByteOrder::LittleEndian, &mut reply, 2);
                    reply.extend_from_slice(&[0; 20]);
                    writer
                        .lock()
                        .map_err(|_| io::Error::other("writer poisoned"))?
                        .write_all(&reply)
                }
                48 => {
                    // XIQueryDevice: deviceid(2) pad(2)
                    log_reply(client_id, sequence, "XIQueryDevice");
                    let mut infos = Vec::new();
                    for (deviceid, use_, attachment, name) in [
                        (2u16, 1u16, 3u16, "Virtual core pointer"),
                        (3u16, 2u16, 2u16, "Virtual core keyboard"),
                    ] {
                        x11::write_u16(ClientByteOrder::LittleEndian, &mut infos, deviceid);
                        x11::write_u16(ClientByteOrder::LittleEndian, &mut infos, use_);
                        x11::write_u16(ClientByteOrder::LittleEndian, &mut infos, attachment);
                        x11::write_u16(ClientByteOrder::LittleEndian, &mut infos, 0); // classes
                        x11::write_u16(
                            ClientByteOrder::LittleEndian,
                            &mut infos,
                            name.len() as u16,
                        );
                        infos.push(1); // enabled
                        infos.push(0);
                        infos.extend_from_slice(name.as_bytes());
                        x11::pad_vec4(&mut infos);
                    }

                    let mut reply =
                        x11::fixed_reply(sequence, 0, x11::checked_units(infos.len())? as u32);
                    x11::write_u16(ClientByteOrder::LittleEndian, &mut reply, 2); // num_devices
                    reply.extend_from_slice(&[0; 22]);
                    reply.extend_from_slice(&infos);
                    writer
                        .lock()
                        .map_err(|_| io::Error::other("writer poisoned"))?
                        .write_all(&reply)
                }
                59 => {
                    // XIGetProperty: return "no such property" (format=0, type=None, num_items=0)
                    log_reply(client_id, sequence, "XIGetProperty -> not found");
                    let mut reply = x11::fixed_reply(sequence, 0, 0);
                    // type(4) + bytes_after(4) + num_items(4) + format(1) + pad(11) = 24 bytes
                    reply.extend_from_slice(&[0u8; 24]);
                    writer
                        .lock()
                        .map_err(|_| io::Error::other("writer poisoned"))?
                        .write_all(&reply)
                }
                60 => {
                    // XIGetSelectedEvents: window(4)
                    log_reply(client_id, sequence, "XIGetSelectedEvents");
                    if body.len() < 4 {
                        return Ok(());
                    }
                    let window =
                        ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                    let mut masks = Vec::new();
                    if let Ok(s) = server.lock()
                        && let Some(client) = s.clients.get(&client_id.0)
                    {
                        for (&(win, dev), &mask) in &client.xi2_masks {
                            if win == window {
                                x11::write_u16(ClientByteOrder::LittleEndian, &mut masks, dev);
                                x11::write_u16(ClientByteOrder::LittleEndian, &mut masks, 1); // mask_len=1
                                x11::write_u32(ClientByteOrder::LittleEndian, &mut masks, mask);
                            }
                        }
                    }
                    let num_masks = (masks.len() / 8) as u16;
                    let mut reply =
                        x11::fixed_reply(sequence, 0, x11::checked_units(masks.len())? as u32);
                    x11::write_u16(ClientByteOrder::LittleEndian, &mut reply, num_masks);
                    reply.extend_from_slice(&[0; 22]);
                    reply.extend_from_slice(&masks);
                    writer
                        .lock()
                        .map_err(|_| io::Error::other("writer poisoned"))?
                        .write_all(&reply)
                }
                40 => {
                    // XIQueryPointer: deviceid(2) pad(2) window(4)
                    // Fixed 32 bytes: same_screen(1) pad(1) seq(2) length(4)
                    //   root(4) child(4) root_x(4) root_y(4) win_x(4) win_y(4)
                    // Extra (length*4 bytes): buttons_len(2) pad(2)
                    //   ModifierInfo: base(4) latched(4) locked(4) effective(4) = 16 bytes
                    //   GroupInfo: base(1) latched(1) locked(1) effective(1) = 4 bytes
                    //   buttons: buttons_len*4 bytes = 0 bytes (buttons_len=0)
                    // Extra = 2+2+16+4 = 24 bytes = 6 units
                    log_reply(client_id, sequence, "XIQueryPointer");
                    let mut reply = x11::fixed_reply(sequence, 1 /* same_screen */, 6);
                    x11::write_u32(ClientByteOrder::LittleEndian, &mut reply, ROOT_WINDOW.0); // root
                    x11::write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // child=None
                    x11::write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // root_x (FP1616)
                    x11::write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // root_y
                    x11::write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // win_x
                    x11::write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // win_y
                    x11::write_u16(ClientByteOrder::LittleEndian, &mut reply, 0); // buttons_len=0
                    x11::write_u16(ClientByteOrder::LittleEndian, &mut reply, 0); // pad
                    reply.extend_from_slice(&[0u8; 16]); // ModifierInfo
                    reply.extend_from_slice(&[0u8; 4]); // GroupInfo (4×CARD8)
                    writer
                        .lock()
                        .map_err(|_| io::Error::other("writer poisoned"))?
                        .write_all(&reply)
                }
                _ => {
                    debug!("unhandled XI2 request minor={}", minor);
                    Ok(())
                }
            }
        }
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
        97 => {
            // QueryBestSize — return the requested dimensions unchanged
            let width = if body.len() >= 8 {
                u16::from_le_bytes([body[4], body[5]])
            } else {
                0
            };
            let height = if body.len() >= 8 {
                u16::from_le_bytes([body[6], body[7]])
            } else {
                0
            };
            log_reply(client_id, sequence, "QueryBestSize");
            x11::write_query_best_size_reply(&mut *lock_writer()?, sequence, width, height)
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
    matches!(depth, 1 | 4 | 8 | 24 | 32)
}

fn zpixmap_expected_len(width: u16, height: u16, depth: u8) -> Option<usize> {
    let stride_bytes: usize = match depth {
        24 | 32 => {
            let stride_bits = usize::from(width).checked_mul(32)?;
            stride_bits.div_ceil(32).checked_mul(4)?
        }
        8 => usize::from(width).div_ceil(4).checked_mul(4)?,
        4 => usize::from(width).div_ceil(8).checked_mul(4)?,
        // 1 bit per pixel: width bits per row, padded to 32-bit boundary.
        // wmaker (libwraster) uses depth-1 ZPixmap masks via MIT-SHM
        // when compositing app icons — without this they fail with
        // BadValue and the icon never renders.
        1 => usize::from(width).div_ceil(32).checked_mul(4)?,
        _ => return None,
    };
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
    use super::{
        EXTENSIONS, UnsupportedMinorPolicy, advertised_extension_names, extension_metadata,
        zpixmap_expected_len,
    };

    #[test]
    fn extension_registry_major_opcodes_are_unique() {
        let major_opcodes = EXTENSIONS
            .iter()
            .map(|ext| ext.major_opcode)
            .collect::<Vec<_>>();
        let mut sorted = major_opcodes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), major_opcodes.len());
    }

    #[test]
    fn extension_registry_non_zero_bases_are_unique() {
        let non_zero_event_bases = EXTENSIONS
            .into_iter()
            .map(|ext| ext.first_event)
            .filter(|base| *base != 0)
            .collect::<Vec<_>>();
        let mut sorted = non_zero_event_bases.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), non_zero_event_bases.len());

        let non_zero_error_bases = EXTENSIONS
            .into_iter()
            .map(|ext| ext.first_error)
            .filter(|base| *base != 0)
            .collect::<Vec<_>>();
        let mut sorted = non_zero_error_bases.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), non_zero_error_bases.len());
    }

    #[test]
    fn phase3_2_extensions_are_not_advertised_until_implemented() {
        let names = advertised_extension_names(None);
        assert!(names.contains(&"RANDR"));
        assert!(names.contains(&"BIG-REQUESTS"));
        assert!(names.contains(&"Generic Event Extension"));
        assert!(names.contains(&"XInputExtension"));
        assert!(names.contains(&"XFIXES"));
        assert!(names.contains(&"SHAPE"));
        assert!(names.contains(&"SYNC"));
        assert!(names.contains(&"DAMAGE"));
        assert!(names.contains(&"Composite"));
        assert!(names.contains(&"Present"));
    }

    #[test]
    fn phase3_2_extensions_use_inline_handlers() {
        for name in ["XFIXES", "SHAPE", "SYNC", "DAMAGE", "Composite", "Present"] {
            let ext = extension_metadata(name).expect("extension metadata");
            assert_eq!(
                ext.unsupported_minor_policy,
                UnsupportedMinorPolicy::HandledInline
            );
        }
    }

    #[test]
    fn zpixmap_expected_len_depth24_2x3() {
        assert_eq!(zpixmap_expected_len(2, 3, 24), Some(24));
    }

    #[test]
    fn zpixmap_expected_len_depth32_2x3() {
        assert_eq!(zpixmap_expected_len(2, 3, 32), Some(24));
    }

    #[test]
    fn zpixmap_expected_len_depth8_4x3() {
        // 4 pixels * 8bpp = 32 bits = 4 bytes/row (already 32-bit aligned)
        assert_eq!(zpixmap_expected_len(4, 3, 8), Some(12));
    }

    #[test]
    fn zpixmap_expected_len_depth8_padding() {
        // 5 pixels * 8bpp = 40 bits → padded to 64 bits = 8 bytes/row
        assert_eq!(zpixmap_expected_len(5, 2, 8), Some(16));
    }

    #[test]
    fn zpixmap_expected_len_depth4_4x3() {
        // 4 pixels * 4bpp = 16 bits → padded to 32 bits = 4 bytes/row
        assert_eq!(zpixmap_expected_len(4, 3, 4), Some(12));
    }

    #[test]
    fn zpixmap_expected_len_depth4_padding() {
        // 9 pixels * 4bpp = 36 bits → padded to 64 bits = 8 bytes/row
        assert_eq!(zpixmap_expected_len(9, 2, 4), Some(16));
    }

    #[test]
    fn zpixmap_expected_len_depth1_24x24() {
        // wmaker uploads 24x24 d1 alpha masks via MIT-SHM. 24 bits per
        // row → padded to 32 bits = 4 bytes/row × 24 rows = 96 bytes.
        assert_eq!(zpixmap_expected_len(24, 24, 1), Some(96));
    }

    #[test]
    fn zpixmap_expected_len_depth1_padding() {
        // 33 bits per row → padded to 64 bits = 8 bytes/row × 2 = 16
        assert_eq!(zpixmap_expected_len(33, 2, 1), Some(16));
    }

    #[test]
    fn zpixmap_expected_len_unsupported_depth_returns_none() {
        // 16- and 15-bit ZPixmap aren't on a real path for ynest yet.
        // (Depth 1 is supported — see depth1 tests above.)
        assert_eq!(zpixmap_expected_len(2, 3, 16), None);
        assert_eq!(zpixmap_expected_len(2, 3, 15), None);
    }

    #[test]
    fn zpixmap_expected_len_zero_width_returns_zero() {
        assert_eq!(zpixmap_expected_len(0, 3, 24), Some(0));
    }

    mod key_routing {
        use super::super::{KeyTarget, WriterTag, route_key_event};
        use crate::{
            resources::ROOT_WINDOW,
            server::{ActiveKeyboardGrab, ActiveKeyboardGrabSource, KeyGrab, ServerState},
        };
        use yserver_protocol::x11::{ClientId, ResourceId};

        #[test]
        fn focus_when_no_grab() {
            let mut s = ServerState::new();
            let focus = ResourceId(0x200);
            let t = route_key_event(&mut s, ClientId(1), focus, 24, 0, true);
            assert_eq!(t, KeyTarget::Focus(focus));
        }

        #[test]
        fn drop_when_focus_is_root() {
            let mut s = ServerState::new();
            let t = route_key_event(&mut s, ClientId(1), ROOT_WINDOW, 24, 0, true);
            assert_eq!(t, KeyTarget::Drop);
        }

        #[test]
        fn active_grab_pre_empts() {
            let mut s = ServerState::new();
            s.active_keyboard_grab = Some(ActiveKeyboardGrab {
                owner: ClientId(3),
                grab_window: ResourceId(0x200),
                source: ActiveKeyboardGrabSource::Explicit,
            });
            let t = route_key_event(&mut s, ClientId(1), ResourceId(0x200), 24, 0, true);
            assert_eq!(
                t,
                KeyTarget::Grab {
                    client_id: ClientId(3),
                    grab_window: ResourceId(0x200),
                    writer: WriterTag::Other(ClientId(3)),
                }
            );
        }

        #[test]
        fn passive_grab_on_root_fires_for_descendant_focus() {
            let mut s = ServerState::new();
            // Set up a focus window whose ancestor walk reaches root.
            let req = yserver_protocol::x11::CreateWindowRequest {
                window: ResourceId(0x200),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                depth: 24,
                visual: ResourceId(0),
                class: 0,
                background_pixel: None,
                event_mask: None,
                override_redirect: None,
            };
            s.resources.create_window(ClientId(1), req);

            s.key_grabs.push(KeyGrab {
                owner: ClientId(2),
                grab_window: ROOT_WINDOW,
                keycode: 24,
                modifiers: 0x0040,
                owner_events: false,
                pointer_mode: 1,
                keyboard_mode: 1,
            });

            let t = route_key_event(
                &mut s,
                /*self_client=*/ ClientId(1),
                /*focus=*/ ResourceId(0x200),
                /*keycode=*/ 24,
                /*state_mask=*/ 0x0040,
                /*pressed=*/ true,
            );
            assert_eq!(
                t,
                KeyTarget::Grab {
                    client_id: ClientId(2),
                    grab_window: ROOT_WINDOW,
                    writer: WriterTag::Other(ClientId(2)),
                }
            );
            // Press should have installed an active passive-key grab.
            assert!(matches!(
                s.active_keyboard_grab.unwrap().source,
                ActiveKeyboardGrabSource::PassiveKey { keycode: 24 }
            ));
        }

        #[test]
        fn passive_release_clears_active_grab() {
            let mut s = ServerState::new();
            s.active_keyboard_grab = Some(ActiveKeyboardGrab {
                owner: ClientId(2),
                grab_window: ROOT_WINDOW,
                source: ActiveKeyboardGrabSource::PassiveKey { keycode: 24 },
            });
            let _ = route_key_event(
                &mut s,
                ClientId(1),
                ResourceId(0x200),
                24,
                0,
                /*pressed=*/ false,
            );
            assert!(s.active_keyboard_grab.is_none());
        }

        #[test]
        fn passive_release_of_other_keycode_keeps_grab() {
            let mut s = ServerState::new();
            s.active_keyboard_grab = Some(ActiveKeyboardGrab {
                owner: ClientId(2),
                grab_window: ROOT_WINDOW,
                source: ActiveKeyboardGrabSource::PassiveKey { keycode: 24 },
            });
            let _ = route_key_event(&mut s, ClientId(1), ResourceId(0x200), 25, 0, false);
            assert!(s.active_keyboard_grab.is_some());
        }
    }

    mod render {
        use super::super::change_picture_translate_xids;

        // Helper to build a ChangePicture values slice with one CARD32 value.
        fn one_val(v: u32) -> [u8; 4] {
            v.to_le_bytes()
        }

        fn two_vals(a: u32, b: u32) -> [u8; 8] {
            let mut buf = [0u8; 8];
            buf[0..4].copy_from_slice(&a.to_le_bytes());
            buf[4..8].copy_from_slice(&b.to_le_bytes());
            buf
        }

        // ── ChangePicture XID translation ──────────────────────────────────

        #[test]
        fn translate_xids_passes_scalar_attrs_through() {
            // CPRepeat (bit 0) only — translator must never be invoked.
            let mut translator_called = false;
            let out = change_picture_translate_xids(0x01, &one_val(7), |_, _| {
                translator_called = true;
                Some(0)
            });
            assert_eq!(out, Some(one_val(7).to_vec()));
            assert!(!translator_called);
        }

        #[test]
        fn translate_xids_leaves_none_value_unchanged() {
            // CPClipMask=None (value=0) — no translation needed, just forward as-is.
            let out = change_picture_translate_xids(0x40, &one_val(0), |_, _| {
                panic!("translator should not be called for None XID")
            });
            assert_eq!(out, Some(one_val(0).to_vec()));
        }

        #[test]
        fn translate_xids_swaps_clip_mask_pixmap_to_host() {
            // CPClipMask = client pixmap 0x1234; translator returns host 0x4242.
            // The patched values slice should carry 0x4242 in the same slot.
            let out = change_picture_translate_xids(0x40, &one_val(0x1234), |attr, v| {
                assert!(matches!(attr, super::super::ChangePictureAttr::ClipMask));
                assert_eq!(v, 0x1234);
                Some(0x4242)
            });
            assert_eq!(out, Some(one_val(0x4242).to_vec()));
        }

        #[test]
        fn translate_xids_swaps_alpha_map_picture_to_host() {
            let out = change_picture_translate_xids(0x02, &one_val(0xdead), |attr, _| {
                assert!(matches!(attr, super::super::ChangePictureAttr::AlphaMap));
                Some(0xbeef)
            });
            assert_eq!(out, Some(one_val(0xbeef).to_vec()));
        }

        #[test]
        fn translate_xids_drops_when_translator_returns_none() {
            // Unknown XID → drop the request rather than forwarding a stale value.
            let out = change_picture_translate_xids(0x40, &one_val(0x9999), |_, _| None::<u32>);
            assert_eq!(out, None);
        }

        #[test]
        fn translate_xids_handles_repeat_plus_clip_mask_pixmap() {
            // CPRepeat (bit 0) + CPClipMask (bit 6): values in bit order are
            // [repeat, clip]. Translation must hit only the clip slot.
            let out = change_picture_translate_xids(0x41, &two_vals(1, 0x1234), |attr, _| {
                if matches!(attr, super::super::ChangePictureAttr::ClipMask) {
                    Some(0xbeef)
                } else {
                    panic!("only ClipMask should hit translator")
                }
            });
            assert_eq!(out, Some(two_vals(1, 0xbeef).to_vec()));
        }

        #[test]
        fn translate_xids_handles_alpha_map_and_clip_mask_together() {
            // CPAlphaMap (bit 1) + CPClipMask (bit 6): values in bit order:
            // [alpha_map, clip_mask]. Both XIDs should be translated.
            let out = change_picture_translate_xids(
                (1 << 1) | (1 << 6),
                &two_vals(0xa1, 0xc1),
                |attr, v| match attr {
                    super::super::ChangePictureAttr::AlphaMap => {
                        assert_eq!(v, 0xa1);
                        Some(0xa2)
                    }
                    super::super::ChangePictureAttr::ClipMask => {
                        assert_eq!(v, 0xc1);
                        Some(0xc2)
                    }
                },
            );
            assert_eq!(out, Some(two_vals(0xa2, 0xc2).to_vec()));
        }

        #[test]
        fn translate_xids_returns_none_on_short_values_with_xid_bit() {
            // value_mask has CPClipMask (bit 6) but values slice is empty.
            let out = change_picture_translate_xids(0x40, &[], |_, _| Some(0));
            assert_eq!(out, None);
        }

        // ── XIQueryPointer reply length ────────────────────────────────────────

        #[test]
        fn xi_query_pointer_extra_bytes_fit_6_length_units() {
            // GroupInfo is 4×CARD8 = 4 bytes (NOT 16 like ModifierInfo).
            // Extra payload: buttons_len(2) + pad(2) + ModifierInfo(16) + GroupInfo(4) = 24 bytes.
            // 24 bytes / 4 = 6 length units.
            let buttons_len_field = 2usize;
            let pad = 2usize;
            let modifier_info = 16usize; // base(4)+latched(4)+locked(4)+effective(4)
            let group_info = 4usize; // base(1)+latched(1)+locked(1)+effective(1)
            let extra = buttons_len_field + pad + modifier_info + group_info;
            assert_eq!(extra, 24, "extra payload must be 24 bytes");
            assert_eq!(extra % 4, 0, "must be 4-byte aligned");
            assert_eq!(extra / 4, 6, "length field must be 6");
        }

        // ── SetPictureClipRectangles offset adjustment ────────────────────────

        #[test]
        fn clip_origin_adjusted_by_window_offset() {
            // When the host picture sits at (x_off, y_off) inside the host container,
            // clip_x_origin and clip_y_origin must be adjusted so the clip aligns with
            // Composite's dst_x/dst_y which are also shifted by (x_off, y_off).
            let x_off: i16 = 100;
            let y_off: i16 = 50;
            let mut body = vec![0u8; 16];
            body[4..6].copy_from_slice(&10i16.to_le_bytes());
            body[6..8].copy_from_slice(&20i16.to_le_bytes());
            let adj_x = i16::from_le_bytes([body[4], body[5]]).wrapping_add(x_off);
            let adj_y = i16::from_le_bytes([body[6], body[7]]).wrapping_add(y_off);
            assert_eq!(adj_x, 110);
            assert_eq!(adj_y, 70);
        }

        #[test]
        fn clip_origin_zero_offset_unchanged() {
            // Pixmap-backed pictures have x_off=y_off=0; clip must pass through unmodified.
            let x_off: i16 = 0;
            let y_off: i16 = 0;
            let mut body = vec![0u8; 16];
            body[4..6].copy_from_slice(&(-5i16).to_le_bytes());
            body[6..8].copy_from_slice(&30i16.to_le_bytes());
            let adj_x = i16::from_le_bytes([body[4], body[5]]).wrapping_add(x_off);
            let adj_y = i16::from_le_bytes([body[6], body[7]]).wrapping_add(y_off);
            assert_eq!(adj_x, -5);
            assert_eq!(adj_y, 30);
        }
    }

    mod xfixes_ops {
        use super::super::{
            clear_shape_rects, intersect_regions, normalize_region_rects, region_extents,
            shape_kind_is_set, shape_mask_source_rects, shape_rects_for, translate_region,
        };
        use crate::{resources::ROOT_WINDOW, server::ServerState};
        use yserver_protocol::x11::{
            ClientId, CreatePixmapRequest, ResourceId, shape, xfixes::RegionRect,
        };

        fn r(x: i16, y: i16, w: u16, h: u16) -> RegionRect {
            RegionRect {
                x,
                y,
                width: w,
                height: h,
            }
        }

        #[test]
        fn normalize_removes_empty_rects() {
            let input = vec![r(0, 0, 0, 5), r(1, 2, 3, 4), r(5, 5, 1, 0)];
            assert_eq!(normalize_region_rects(input), vec![r(1, 2, 3, 4)]);
        }

        #[test]
        fn normalize_truncates_at_cap() {
            let rects: Vec<RegionRect> = (0..4097).map(|i| r(i as i16, 0, 1, 1)).collect();
            assert_eq!(normalize_region_rects(rects).len(), 4096);
        }

        #[test]
        fn region_extents_empty_returns_zero() {
            assert_eq!(region_extents(&[]), r(0, 0, 0, 0));
        }

        #[test]
        fn region_extents_single_passthrough() {
            let rect = r(3, 4, 10, 20);
            assert_eq!(region_extents(&[rect]), rect);
        }

        #[test]
        fn region_extents_bounding_box() {
            let rects = vec![r(0, 0, 10, 10), r(5, 5, 10, 10)];
            assert_eq!(region_extents(&rects), r(0, 0, 15, 15));
        }

        #[test]
        fn intersect_overlapping() {
            let a = vec![r(0, 0, 10, 10)];
            let b = vec![r(5, 5, 10, 10)];
            assert_eq!(intersect_regions(&a, &b), vec![r(5, 5, 5, 5)]);
        }

        #[test]
        fn intersect_non_overlapping_is_empty() {
            let a = vec![r(0, 0, 5, 5)];
            let b = vec![r(10, 10, 5, 5)];
            assert!(intersect_regions(&a, &b).is_empty());
        }

        #[test]
        fn intersect_with_empty_region_is_empty() {
            let empty: Vec<RegionRect> = vec![];
            let nonempty = vec![r(0, 0, 10, 10)];
            assert!(intersect_regions(&empty, &nonempty).is_empty());
            assert!(intersect_regions(&nonempty, &empty).is_empty());
        }

        #[test]
        fn translate_shifts_coords() {
            let mut rects = vec![r(10, 20, 5, 5)];
            translate_region(&mut rects, 3, -5);
            assert_eq!(rects[0], r(13, 15, 5, 5));
        }

        #[test]
        fn translate_saturates_at_bounds() {
            let mut rects = vec![r(i16::MAX, i16::MIN, 1, 1)];
            translate_region(&mut rects, 100, -100);
            assert_eq!(rects[0].x, i16::MAX);
            assert_eq!(rects[0].y, i16::MIN);
        }

        #[test]
        fn shape_mask_source_uses_pixmap_geometry() {
            let mut server = ServerState::new();
            let pixmap = ResourceId(0x200);
            server.resources.create_pixmap(
                ClientId(1),
                CreatePixmapRequest {
                    depth: 1,
                    pixmap,
                    drawable: ROOT_WINDOW,
                    width: 17,
                    height: 23,
                },
            );

            assert_eq!(
                shape_mask_source_rects(&server, pixmap),
                vec![r(0, 0, 17, 23)]
            );
        }

        #[test]
        fn clear_shape_rects_reverts_to_default_region() {
            let mut server = ServerState::new();
            let window = ROOT_WINDOW;
            server.shape_windows.entry(window).or_default().bounding = Some(vec![r(1, 2, 3, 4)]);

            assert!(shape_kind_is_set(&server, window, shape::KIND_BOUNDING));
            clear_shape_rects(&mut server, window, shape::KIND_BOUNDING);

            assert!(!shape_kind_is_set(&server, window, shape::KIND_BOUNDING));
            assert_eq!(
                shape_rects_for(&server, window, shape::KIND_BOUNDING),
                vec![r(0, 0, 800, 600)]
            );
        }
    }

    mod xfixes_requests {
        use std::{
            os::unix::net::UnixStream,
            sync::{Arc, Mutex},
        };

        use super::super::handle_xfixes_request;
        use crate::server::ServerState;
        use yserver_protocol::x11::{
            AtomId, ClientId, ResourceId, SequenceNumber, xfixes as x11xfixes,
        };

        fn make_writer() -> Arc<Mutex<UnixStream>> {
            let (w, _r) = UnixStream::pair().unwrap();
            Arc::new(Mutex::new(w))
        }

        fn make_server() -> Arc<Mutex<ServerState>> {
            Arc::new(Mutex::new(ServerState::new()))
        }

        fn create_region_body(xid: u32, x: i16, y: i16, w: u16, h: u16) -> Vec<u8> {
            let mut body = vec![0u8; 12];
            body[0..4].copy_from_slice(&xid.to_le_bytes());
            body[4..6].copy_from_slice(&x.to_le_bytes());
            body[6..8].copy_from_slice(&y.to_le_bytes());
            body[8..10].copy_from_slice(&w.to_le_bytes());
            body[10..12].copy_from_slice(&h.to_le_bytes());
            body
        }

        #[test]
        fn hide_show_cursor_return_ok_no_reply() {
            let writer = make_writer();
            let server = make_server();
            for minor in [x11xfixes::HIDE_CURSOR, x11xfixes::SHOW_CURSOR] {
                assert!(
                    handle_xfixes_request(
                        ClientId(1),
                        &server,
                        None,
                        &writer,
                        SequenceNumber(1),
                        minor,
                        &[0u8; 4]
                    )
                    .is_ok()
                );
            }
        }

        #[test]
        fn selection_mask_stored_and_cleared() {
            let writer = make_writer();
            let server = make_server();
            let mut body = [0u8; 12];
            body[0..4].copy_from_slice(&0x100u32.to_le_bytes());
            body[4..8].copy_from_slice(&1u32.to_le_bytes());
            body[8..12].copy_from_slice(&7u32.to_le_bytes());
            handle_xfixes_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(1),
                x11xfixes::SELECT_SELECTION_INPUT,
                &body,
            )
            .unwrap();
            {
                let s = server.lock().unwrap();
                let key = (1u32, ResourceId(0x100), AtomId(1));
                assert_eq!(s.xfixes_selection_masks.get(&key), Some(&7u32));
            }
            body[8..12].copy_from_slice(&0u32.to_le_bytes());
            handle_xfixes_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(2),
                x11xfixes::SELECT_SELECTION_INPUT,
                &body,
            )
            .unwrap();
            {
                let s = server.lock().unwrap();
                let key = (1u32, ResourceId(0x100), AtomId(1));
                assert!(s.xfixes_selection_masks.get(&key).is_none());
            }
        }

        #[test]
        fn cursor_mask_stored_and_cleared() {
            let writer = make_writer();
            let server = make_server();
            let mut body = [0u8; 8];
            body[0..4].copy_from_slice(&0x200u32.to_le_bytes());
            body[4..8].copy_from_slice(&3u32.to_le_bytes());
            handle_xfixes_request(
                ClientId(2),
                &server,
                None,
                &writer,
                SequenceNumber(1),
                x11xfixes::SELECT_CURSOR_INPUT,
                &body,
            )
            .unwrap();
            {
                let s = server.lock().unwrap();
                assert_eq!(
                    s.xfixes_cursor_masks.get(&(2u32, ResourceId(0x200))),
                    Some(&3u32)
                );
            }
            body[4..8].copy_from_slice(&0u32.to_le_bytes());
            handle_xfixes_request(
                ClientId(2),
                &server,
                None,
                &writer,
                SequenceNumber(2),
                x11xfixes::SELECT_CURSOR_INPUT,
                &body,
            )
            .unwrap();
            {
                let s = server.lock().unwrap();
                assert!(
                    s.xfixes_cursor_masks
                        .get(&(2u32, ResourceId(0x200)))
                        .is_none()
                );
            }
        }

        #[test]
        fn region_create_and_destroy() {
            let writer = make_writer();
            let server = make_server();
            handle_xfixes_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(1),
                x11xfixes::CREATE_REGION,
                &create_region_body(0x300, 0, 0, 10, 10),
            )
            .unwrap();
            assert!(server.lock().unwrap().xfixes_regions.contains_key(&0x300));
            let mut body2 = [0u8; 4];
            body2[0..4].copy_from_slice(&0x300u32.to_le_bytes());
            handle_xfixes_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(2),
                x11xfixes::DESTROY_REGION,
                &body2,
            )
            .unwrap();
            assert!(!server.lock().unwrap().xfixes_regions.contains_key(&0x300));
        }

        #[test]
        fn region_duplicate_xid_overwrites() {
            let writer = make_writer();
            let server = make_server();
            handle_xfixes_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(1),
                x11xfixes::CREATE_REGION,
                &create_region_body(0x400, 1, 0, 5, 5),
            )
            .unwrap();
            handle_xfixes_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(2),
                x11xfixes::CREATE_REGION,
                &create_region_body(0x400, 9, 0, 5, 5),
            )
            .unwrap();
            let s = server.lock().unwrap();
            assert_eq!(s.xfixes_regions.get(&0x400).unwrap().rects[0].x, 9);
        }

        #[test]
        fn destroy_unknown_region_is_silent() {
            let writer = make_writer();
            let server = make_server();
            let mut body = [0u8; 4];
            body[0..4].copy_from_slice(&0xdeadbeefu32.to_le_bytes());
            assert!(
                handle_xfixes_request(
                    ClientId(1),
                    &server,
                    None,
                    &writer,
                    SequenceNumber(1),
                    x11xfixes::DESTROY_REGION,
                    &body,
                )
                .is_ok()
            );
            assert!(server.lock().unwrap().xfixes_regions.is_empty());
        }

        #[test]
        fn region_client_disconnect_cleanup() {
            let writer = make_writer();
            let server = make_server();
            handle_xfixes_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(1),
                x11xfixes::CREATE_REGION,
                &create_region_body(0x500, 0, 0, 5, 5),
            )
            .unwrap();
            handle_xfixes_request(
                ClientId(2),
                &server,
                None,
                &writer,
                SequenceNumber(2),
                x11xfixes::CREATE_REGION,
                &create_region_body(0x501, 0, 0, 5, 5),
            )
            .unwrap();
            server
                .lock()
                .unwrap()
                .xfixes_regions
                .retain(|_, r| r.owner != ClientId(1));
            let s = server.lock().unwrap();
            assert!(!s.xfixes_regions.contains_key(&0x500));
            assert!(s.xfixes_regions.contains_key(&0x501));
        }
    }

    mod shape_requests {
        //! Integration tests for `handle_shape_request`'s resolved-rect path.
        //!
        //! The host-mirror leg is intentionally exercised with `host = None`:
        //! we already byte-test `build_shape_rectangles` over in
        //! `host_x11::tests::shape_rectangles_*`. What we want to lock down
        //! here is that `shape_windows` ends up holding the right resolved
        //! rectangle list after each handler invocation, since that is what
        //! `mirror_shape_to_host` would pass on to the host.
        use std::{
            os::unix::net::UnixStream,
            sync::{Arc, Mutex},
        };

        use super::super::{handle_shape_request, shape_rects_for};
        use crate::{resources::ROOT_WINDOW, server::ServerState};
        use yserver_protocol::x11::{
            ClientId, CreateWindowRequest, ResourceId, SequenceNumber, shape as x11shape,
            xfixes::RegionRect,
        };

        fn make_writer() -> Arc<Mutex<UnixStream>> {
            let (w, _r) = UnixStream::pair().unwrap();
            Arc::new(Mutex::new(w))
        }

        fn make_server_with_window(
            window: ResourceId,
            host_xid: Option<u32>,
        ) -> Arc<Mutex<ServerState>> {
            let server = Arc::new(Mutex::new(ServerState::new()));
            let mut s = server.lock().unwrap();
            let req = CreateWindowRequest {
                window,
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                depth: 24,
                visual: ResourceId(0),
                class: 0,
                background_pixel: None,
                event_mask: None,
                override_redirect: None,
            };
            s.resources.create_window(ClientId(1), req);
            if let Some(host_xid) = host_xid
                && let Some(w) = s.resources.window_mut(window)
            {
                w.host_xid = Some(host_xid);
            }
            drop(s);
            server
        }

        fn rectangles_body(dest: u32, rects: &[RegionRect]) -> Vec<u8> {
            let mut body = Vec::with_capacity(12 + rects.len() * 8);
            body.push(x11shape::OP_SET);
            body.push(x11shape::KIND_BOUNDING);
            body.push(0); // ordering = Unsorted
            body.push(0); // pad
            body.extend_from_slice(&dest.to_le_bytes());
            body.extend_from_slice(&0i16.to_le_bytes()); // x_off
            body.extend_from_slice(&0i16.to_le_bytes()); // y_off
            for rect in rects {
                body.extend_from_slice(&rect.x.to_le_bytes());
                body.extend_from_slice(&rect.y.to_le_bytes());
                body.extend_from_slice(&rect.width.to_le_bytes());
                body.extend_from_slice(&rect.height.to_le_bytes());
            }
            body
        }

        fn combine_body(
            op: u8,
            dest_kind: u8,
            src_kind: u8,
            dest: u32,
            x_off: i16,
            y_off: i16,
            src: u32,
        ) -> Vec<u8> {
            let mut body = Vec::with_capacity(16);
            body.push(op);
            body.push(dest_kind);
            body.push(src_kind);
            body.push(0); // pad
            body.extend_from_slice(&dest.to_le_bytes());
            body.extend_from_slice(&x_off.to_le_bytes());
            body.extend_from_slice(&y_off.to_le_bytes());
            body.extend_from_slice(&src.to_le_bytes());
            body
        }

        #[test]
        fn rectangles_set_records_resolved_bounding_list() {
            let dest = ResourceId(0x300);
            let server = make_server_with_window(dest, Some(0xdead_beef));
            let writer = make_writer();
            let rects = vec![RegionRect {
                x: 5,
                y: 6,
                width: 30,
                height: 40,
            }];
            let body = rectangles_body(dest.0, &rects);
            handle_shape_request(
                ClientId(1),
                &server,
                None, // host mirror is no-op here; we test local resolved state
                &writer,
                SequenceNumber(1),
                x11shape::RECTANGLES,
                &body,
            )
            .unwrap();
            let s = server.lock().unwrap();
            assert_eq!(shape_rects_for(&s, dest, x11shape::KIND_BOUNDING), rects);
        }

        #[test]
        fn combine_with_local_only_source_merges_into_dest() {
            // Simulates a WM titlebar mask combined into a frame: the source
            // window has no host_xid, so a per-opcode mirror would silently
            // drop. Resolved-rect mirroring instead snapshots the merged
            // destination list — this test asserts that snapshot is correct.
            let dest = ResourceId(0x301);
            let src = ResourceId(0x302);
            let server = make_server_with_window(dest, Some(0xdead_beef));

            // Give source its own bounding state (local-only, no host_xid).
            let mut s = server.lock().unwrap();
            let req = CreateWindowRequest {
                window: src,
                parent: dest,
                x: 0,
                y: 0,
                width: 50,
                height: 10,
                border_width: 0,
                depth: 24,
                visual: ResourceId(0),
                class: 0,
                background_pixel: None,
                event_mask: None,
                override_redirect: None,
            };
            s.resources.create_window(ClientId(1), req);
            drop(s);

            let writer = make_writer();
            let src_rect = RegionRect {
                x: 0,
                y: 0,
                width: 50,
                height: 10,
            };
            handle_shape_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(1),
                x11shape::RECTANGLES,
                &rectangles_body(src.0, &[src_rect]),
            )
            .unwrap();

            // Seed dest with a rect, then COMBINE union from src.
            let dest_rect = RegionRect {
                x: 0,
                y: 20,
                width: 100,
                height: 60,
            };
            handle_shape_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(2),
                x11shape::RECTANGLES,
                &rectangles_body(dest.0, &[dest_rect]),
            )
            .unwrap();
            handle_shape_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(3),
                x11shape::COMBINE,
                &combine_body(
                    x11shape::OP_UNION,
                    x11shape::KIND_BOUNDING,
                    x11shape::KIND_BOUNDING,
                    dest.0,
                    0,
                    0,
                    src.0,
                ),
            )
            .unwrap();

            let s = server.lock().unwrap();
            let merged = shape_rects_for(&s, dest, x11shape::KIND_BOUNDING);
            // Both rects are disjoint, so normalize_region_rects keeps both.
            assert_eq!(merged.len(), 2);
            assert!(merged.contains(&dest_rect));
            assert!(merged.contains(&src_rect));
        }
    }

    mod root_resize {
        //! `handle_host_container_resize` post-conditions, including
        //! `ConfigureNotify` delivery to clients that selected
        //! `StructureNotify` on root *without* `RRSelectInput`.
        use std::{
            collections::{HashMap, HashSet},
            io::Read,
            os::unix::net::UnixStream,
            sync::{Arc, Mutex, atomic::AtomicU16},
        };

        use super::super::handle_host_container_resize;
        use crate::{
            host_x11::HostConfigureEvent,
            resources::ROOT_WINDOW,
            server::{ClientHandle, ServerState},
        };
        use yserver_protocol::x11::ClientByteOrder;

        const STRUCTURE_NOTIFY_MASK: u32 = 0x0002_0000;

        fn server_with_root_listener() -> (Arc<Mutex<ServerState>>, UnixStream) {
            let server = Arc::new(Mutex::new(ServerState::new()));
            let (writer_local, reader_remote) = UnixStream::pair().expect("socketpair");
            let mut s = server.lock().unwrap();
            s.clients.insert(
                1,
                ClientHandle {
                    writer: Arc::new(Mutex::new(writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0010_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(ROOT_WINDOW, STRUCTURE_NOTIFY_MASK)]),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                },
            );
            drop(s);
            (server, reader_remote)
        }

        #[test]
        fn resize_updates_state_and_root_geometry() {
            let (server, _reader) = server_with_root_listener();
            handle_host_container_resize(
                &server,
                HostConfigureEvent {
                    host_xid: 0xdead_beef,
                    x: 0,
                    y: 0,
                    width: 1024,
                    height: 768,
                },
            );
            let s = server.lock().unwrap();
            assert_eq!(s.randr.screen_width, 1024);
            assert_eq!(s.randr.screen_height, 768);
            let root = s.resources.window(ROOT_WINDOW).expect("root window");
            assert_eq!(root.width, 1024);
            assert_eq!(root.height, 768);
        }

        #[test]
        fn structure_notify_listener_gets_configure_notify() {
            let (server, mut reader) = server_with_root_listener();

            handle_host_container_resize(
                &server,
                HostConfigureEvent {
                    host_xid: 0xdead_beef,
                    x: 0,
                    y: 0,
                    width: 1024,
                    height: 768,
                },
            );

            // Drain everything currently buffered. The first 32 bytes must be a
            // ConfigureNotify (event type 22) on root with the new dimensions.
            reader.set_nonblocking(true).expect("set non-blocking");
            let mut buf = [0u8; 32];
            reader.read_exact(&mut buf).expect("event byte block");
            assert_eq!(buf[0], 22, "event type 22 = ConfigureNotify");
            // Bytes 4..8 = event_window, 8..12 = window. Both must be ROOT_WINDOW.
            let event_window = u32::from_le_bytes(buf[4..8].try_into().unwrap());
            let window = u32::from_le_bytes(buf[8..12].try_into().unwrap());
            assert_eq!(event_window, ROOT_WINDOW.0);
            assert_eq!(window, ROOT_WINDOW.0);
            // Width @ bytes 20..22, height @ bytes 22..24 (after above_sibling
            // u32 + x i16 + y i16).
            let width = u16::from_le_bytes(buf[20..22].try_into().unwrap());
            let height = u16::from_le_bytes(buf[22..24].try_into().unwrap());
            assert_eq!(width, 1024);
            assert_eq!(height, 768);
        }
    }

    mod atom_name {
        //! `GetAtomName` (opcode 17) — atom IDs in our protocol stream can
        //! come from host-proxied replies (most notably the FONTPROP atoms
        //! in `ListFontsWithInfo`), so a client may legitimately ask us for
        //! the name of an atom we never interned ourselves. Falling back to
        //! the host keeps atom IDs consistent across our layer; without it
        //! e16 sees a `BadAtom` and exits during startup.
        use std::{
            io::Read,
            os::unix::net::UnixStream,
            sync::{Arc, Mutex},
        };

        use super::super::handle_get_atom_name_with_host_lookup;
        use crate::server::ServerState;
        use yserver_protocol::x11::{AtomId, SequenceNumber};

        fn pair() -> (Arc<Mutex<UnixStream>>, UnixStream) {
            let (w, r) = UnixStream::pair().unwrap();
            (Arc::new(Mutex::new(w)), r)
        }

        #[test]
        fn predefined_atom_returns_reply_without_host_lookup() {
            // Predefined atom 1 = PRIMARY. Local-only path; host lookup must
            // not be invoked.
            let server = Arc::new(Mutex::new(ServerState::new()));
            let (writer, mut reader) = pair();
            let host_called = std::cell::Cell::new(false);
            handle_get_atom_name_with_host_lookup(
                &server,
                &writer,
                SequenceNumber(1),
                AtomId(1),
                |_atom| {
                    host_called.set(true);
                    Some("not used".into())
                },
            )
            .unwrap();
            assert!(!host_called.get(), "predefined atom should not hit host");
            let mut header = [0u8; 32];
            reader.read_exact(&mut header).expect("reply header");
            assert_eq!(header[0], 1, "expected reply, got error");
        }

        #[test]
        fn unknown_atom_falls_through_to_host_lookup() {
            let server = Arc::new(Mutex::new(ServerState::new()));
            let (writer, mut reader) = pair();
            handle_get_atom_name_with_host_lookup(
                &server,
                &writer,
                SequenceNumber(7),
                AtomId(117),
                |atom| {
                    assert_eq!(atom, 117);
                    Some("Button Wheel Up".into())
                },
            )
            .unwrap();

            // Drain the 32-byte fixed reply header.
            let mut header = [0u8; 32];
            reader.read_exact(&mut header).expect("reply header");
            assert_eq!(header[0], 1, "expected successful reply");
            let name_len = u16::from_le_bytes([header[8], header[9]]) as usize;
            assert_eq!(name_len, "Button Wheel Up".len());
            // Drain the padded name body.
            let padded = (name_len + 3) & !3;
            let mut body = vec![0u8; padded];
            reader.read_exact(&mut body).expect("reply body");
            assert_eq!(&body[..name_len], b"Button Wheel Up");
        }

        #[test]
        fn unknown_atom_with_no_host_fallback_emits_bad_atom() {
            // No host (or host has no answer either). Spec-correct response
            // is BadAtom — the previous "UNKNOWN" placeholder reply was
            // wrong and would fool clients into believing the atom exists.
            let server = Arc::new(Mutex::new(ServerState::new()));
            let (writer, mut reader) = pair();
            handle_get_atom_name_with_host_lookup(
                &server,
                &writer,
                SequenceNumber(9),
                AtomId(117),
                |_| None,
            )
            .unwrap();
            let mut buf = [0u8; 32];
            reader.read_exact(&mut buf).expect("error reply block");
            assert_eq!(buf[0], 0, "expected error response");
            assert_eq!(buf[1], 5, "BadAtom = 5");
            assert_eq!(
                u32::from_le_bytes(buf[4..8].try_into().unwrap()),
                117,
                "bad value = the offending atom",
            );
            assert_eq!(buf[10], 17, "major opcode for GetAtomName");
        }
    }

    mod damage {
        //! Auto-accumulation: when a draw op modifies a drawable that has
        //! one or more `DamageObject`s attached, the server must fire
        //! `DamageNotify` events to the owning clients per the level
        //! contract. Phase 3.5 implements levels 2 (BoundingBox) and 3
        //! (NonEmpty) as "at most one event per Subtract cycle"; level 1
        //! (DeltaRectangles) is included via the same path for now (one
        //! event when region transitions empty → non-empty), and level 0
        //! (RawRectangles) is deferred.
        use std::{
            collections::{HashMap, HashSet},
            io::Read,
            os::unix::net::UnixStream,
            sync::{Arc, Mutex, atomic::AtomicU16},
        };

        use super::super::accumulate_damage;
        use crate::server::{ClientHandle, DamageObject, ServerState};
        use yserver_protocol::x11::{ClientByteOrder, ClientId, ResourceId};

        const DAMAGE_FIRST_EVENT: u8 = 94;

        fn server_with_client_owning_damage(
            damage_id: u32,
            drawable: ResourceId,
            level: u8,
        ) -> (Arc<Mutex<ServerState>>, UnixStream) {
            let server = Arc::new(Mutex::new(ServerState::new()));
            let (writer_local, reader_remote) = UnixStream::pair().expect("socketpair");
            let mut s = server.lock().unwrap();
            s.clients.insert(
                1,
                ClientHandle {
                    writer: Arc::new(Mutex::new(writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0010_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::new(),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                },
            );
            s.damage_objects.insert(
                damage_id,
                DamageObject {
                    owner: ClientId(1),
                    drawable,
                    level,
                    rects: Vec::new(),
                    pending_notify_fired: false,
                },
            );
            drop(s);
            (server, reader_remote)
        }

        #[test]
        fn first_draw_on_a_damaged_drawable_fires_damage_notify() {
            let drawable = ResourceId(0x10_0200);
            let (server, mut reader) = server_with_client_owning_damage(0x10_0301, drawable, 3);
            accumulate_damage(&server, drawable, 0, 0, 32, 32);
            reader.set_nonblocking(true).expect("set non-blocking");
            let mut buf = [0u8; 32];
            reader.read_exact(&mut buf).expect("DamageNotify event");
            assert_eq!(buf[0], DAMAGE_FIRST_EVENT, "type = first_event");
            assert_eq!(buf[1] & 0x7F, 3, "level = NonEmpty (3)");
            // drawable @ bytes 4..8
            let dwbl = u32::from_le_bytes(buf[4..8].try_into().unwrap());
            assert_eq!(dwbl, drawable.0);
            // damage @ bytes 8..12
            let dmg = u32::from_le_bytes(buf[8..12].try_into().unwrap());
            assert_eq!(dmg, 0x10_0301);
            // area.width @ bytes 20..22
            assert_eq!(u16::from_le_bytes([buf[20], buf[21]]), 32);
        }

        #[test]
        fn second_draw_in_same_cycle_does_not_fire_for_non_empty_level() {
            let drawable = ResourceId(0x10_0200);
            let (server, mut reader) = server_with_client_owning_damage(0x10_0301, drawable, 3);
            accumulate_damage(&server, drawable, 0, 0, 32, 32);
            accumulate_damage(&server, drawable, 0, 0, 64, 64);
            reader.set_nonblocking(true).expect("non-blocking");
            // Drain any first event first.
            let mut buf = [0u8; 32];
            reader.read_exact(&mut buf).expect("first event");
            // Second event must not be present.
            let mut more = [0u8; 32];
            assert!(
                reader.read_exact(&mut more).is_err(),
                "no second event in same cycle"
            );
        }

        #[test]
        fn accumulate_does_nothing_when_drawable_does_not_match_any_damage() {
            let damaged = ResourceId(0x10_0200);
            let (server, mut reader) = server_with_client_owning_damage(0x10_0301, damaged, 3);
            // Draw on a different drawable — no event should fire.
            accumulate_damage(&server, ResourceId(0x10_0999), 0, 0, 32, 32);
            reader.set_nonblocking(true).expect("non-blocking");
            let mut buf = [0u8; 32];
            assert!(
                reader.read_exact(&mut buf).is_err(),
                "no event for unrelated drawable",
            );
        }

        /// After the client issues `DamageSubtract`, the cycle ends and the
        /// next damaging op must fire `DamageNotify` again.
        #[test]
        fn subtract_reopens_the_cycle_for_next_damage_notify() {
            let drawable = ResourceId(0x10_0200);
            let damage_id = 0x10_0301;
            let (server, mut reader) = server_with_client_owning_damage(damage_id, drawable, 3);
            accumulate_damage(&server, drawable, 0, 0, 32, 32);

            // Simulate Subtract: clear region + reset pending_notify_fired.
            // We poke the field directly here rather than going through the
            // wire decoder; the production code path is one line in
            // handle_damage_request.
            {
                let mut s = server.lock().unwrap();
                let dmg = s.damage_objects.get_mut(&damage_id).unwrap();
                dmg.rects.clear();
                dmg.pending_notify_fired = false;
            }
            accumulate_damage(&server, drawable, 0, 0, 64, 64);

            reader.set_nonblocking(true).expect("non-blocking");
            let mut first = [0u8; 32];
            reader.read_exact(&mut first).expect("first event");
            let mut second = [0u8; 32];
            reader
                .read_exact(&mut second)
                .expect("second event after subtract");
            assert_eq!(u16::from_le_bytes([second[20], second[21]]), 64);
        }
    }

    mod composite {
        //! NameWindowPixmap path. With a real host these would forward
        //! `Composite::NameWindowPixmap`; in unit tests we exercise the
        //! preconditions that don't need a host (BadMatch when not
        //! redirected, BadAlloc when no host backing). Success-path
        //! integration is covered by the picom manual smoke described in
        //! the Phase 3.5 design.
        use std::{
            io::Read,
            os::unix::net::UnixStream,
            sync::{Arc, Mutex},
        };

        use super::super::handle_composite_request;
        use crate::{resources::ROOT_WINDOW, server::ServerState};
        use yserver_protocol::x11::{
            ClientId, CreateWindowRequest, ResourceId, SequenceNumber, composite as x11composite,
            error as x11error,
        };

        fn make_server_with_window(
            window: ResourceId,
            host_xid: Option<u32>,
        ) -> Arc<Mutex<ServerState>> {
            let server = Arc::new(Mutex::new(ServerState::new()));
            let mut s = server.lock().unwrap();
            s.resources.create_window(
                ClientId(1),
                CreateWindowRequest {
                    window,
                    parent: ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                    border_width: 0,
                    depth: 24,
                    visual: ResourceId(0),
                    class: 0,
                    background_pixel: None,
                    event_mask: None,
                    override_redirect: None,
                },
            );
            if let Some(xid) = host_xid
                && let Some(w) = s.resources.window_mut(window)
            {
                w.host_xid = Some(xid);
            }
            drop(s);
            server
        }

        fn name_window_pixmap_body(window: u32, pixmap: u32) -> Vec<u8> {
            let mut body = Vec::with_capacity(8);
            body.extend_from_slice(&window.to_le_bytes());
            body.extend_from_slice(&pixmap.to_le_bytes());
            body
        }

        fn read_error(reader: &mut UnixStream) -> [u8; 32] {
            let mut buf = [0u8; 32];
            reader.read_exact(&mut buf).expect("error reply");
            assert_eq!(buf[0], 0, "expected error response, got opcode {}", buf[0]);
            buf
        }

        #[test]
        fn name_window_pixmap_on_unredirected_window_returns_bad_match() {
            let window = ResourceId(0x10_0500);
            let server = make_server_with_window(window, Some(0xdead_beef));
            let (writer_local, mut reader_remote) = UnixStream::pair().unwrap();
            let writer = Arc::new(Mutex::new(writer_local));
            handle_composite_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(1),
                x11composite::NAME_WINDOW_PIXMAP,
                &name_window_pixmap_body(window.0, 0x10_0501),
            )
            .unwrap();
            let buf = read_error(&mut reader_remote);
            assert_eq!(buf[1], x11error::BAD_MATCH);
        }

        #[test]
        fn name_window_pixmap_on_mirrored_sub_window_returns_bad_value() {
            // Phase 3.6 Step 2 block: even with the parent redirected via
            // SUBWINDOWS, calling NameWindowPixmap on a sub-window must
            // reject locally with BadValue (lifted in Step 5 once host
            // pixmap retention across sub-window destroy is wired up).
            let top_level = ResourceId(0x10_0500);
            let sub_window = ResourceId(0x10_0501);
            let server = make_server_with_window(top_level, Some(0xdead_beef));
            {
                let mut s = server.lock().unwrap();
                s.resources.create_window(
                    ClientId(1),
                    CreateWindowRequest {
                        window: sub_window,
                        parent: top_level,
                        x: 0,
                        y: 0,
                        width: 50,
                        height: 50,
                        border_width: 0,
                        depth: 24,
                        visual: ResourceId(0),
                        class: 0,
                        background_pixel: None,
                        event_mask: None,
                        override_redirect: None,
                    },
                );
                if let Some(w) = s.resources.window_mut(sub_window) {
                    w.host_xid = Some(0xface_face);
                }
                // Redirect sub-windows of the top-level so the
                // BadMatch-not-redirected guard doesn't fire.
                s.composite_redirects.insert((top_level, true), 0);
            }
            let (writer_local, mut reader_remote) = UnixStream::pair().unwrap();
            let writer = Arc::new(Mutex::new(writer_local));
            handle_composite_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(1),
                x11composite::NAME_WINDOW_PIXMAP,
                &name_window_pixmap_body(sub_window.0, 0x10_0701),
            )
            .unwrap();
            let buf = read_error(&mut reader_remote);
            assert_eq!(buf[1], x11error::BAD_VALUE);
        }

        #[test]
        fn name_window_pixmap_with_no_host_returns_bad_alloc() {
            let window = ResourceId(0x10_0500);
            let server = make_server_with_window(window, Some(0xdead_beef));
            // Mark redirected so we get past the BadMatch check.
            {
                let mut s = server.lock().unwrap();
                s.composite_redirects.insert((window, false), 0);
            }
            let (writer_local, mut reader_remote) = UnixStream::pair().unwrap();
            let writer = Arc::new(Mutex::new(writer_local));
            handle_composite_request(
                ClientId(1),
                &server,
                None, // no host -> cannot satisfy NameWindowPixmap
                &writer,
                SequenceNumber(1),
                x11composite::NAME_WINDOW_PIXMAP,
                &name_window_pixmap_body(window.0, 0x10_0501),
            )
            .unwrap();
            let buf = read_error(&mut reader_remote);
            assert_eq!(buf[1], x11error::BAD_ALLOC);
        }

        #[test]
        fn invalidate_drops_local_pixmaps_and_clears_window_list() {
            use super::super::invalidate_composite_named_pixmaps;
            use crate::resources::NamedCompositePixmap;
            let window = ResourceId(0x10_0500);
            let server = make_server_with_window(window, Some(0xdead_beef));
            let p1 = ResourceId(0x10_0601);
            let p2 = ResourceId(0x10_0602);
            {
                let mut s = server.lock().unwrap();
                s.resources.create_pixmap(
                    ClientId(1),
                    yserver_protocol::x11::CreatePixmapRequest {
                        pixmap: p1,
                        drawable: window,
                        width: 100,
                        height: 100,
                        depth: 24,
                    },
                );
                s.resources.create_pixmap(
                    ClientId(1),
                    yserver_protocol::x11::CreatePixmapRequest {
                        pixmap: p2,
                        drawable: window,
                        width: 100,
                        height: 100,
                        depth: 24,
                    },
                );
                let w = s.resources.window_mut(window).unwrap();
                w.composite_named_pixmaps.push(NamedCompositePixmap {
                    client_pixmap: p1,
                    host_pixmap: 0xa,
                    width: 100,
                    height: 100,
                });
                w.composite_named_pixmaps.push(NamedCompositePixmap {
                    client_pixmap: p2,
                    host_pixmap: 0xb,
                    width: 100,
                    height: 100,
                });
            }
            invalidate_composite_named_pixmaps(&server, None, window);
            let s = server.lock().unwrap();
            assert!(s.resources.pixmap(p1).is_none(), "p1 freed locally");
            assert!(s.resources.pixmap(p2).is_none(), "p2 freed locally");
            assert!(
                s.resources
                    .window(window)
                    .unwrap()
                    .composite_named_pixmaps
                    .is_empty(),
                "window's alias list cleared",
            );
        }

        #[test]
        fn name_window_pixmap_on_unknown_window_returns_bad_window() {
            let server = Arc::new(Mutex::new(ServerState::new()));
            let (writer_local, mut reader_remote) = UnixStream::pair().unwrap();
            let writer = Arc::new(Mutex::new(writer_local));
            handle_composite_request(
                ClientId(1),
                &server,
                None,
                &writer,
                SequenceNumber(1),
                x11composite::NAME_WINDOW_PIXMAP,
                &name_window_pixmap_body(0x9999_9999, 0x10_0501),
            )
            .unwrap();
            let buf = read_error(&mut reader_remote);
            assert_eq!(buf[1], x11error::BAD_WINDOW);
        }
    }
}
