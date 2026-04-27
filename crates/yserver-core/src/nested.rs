use std::collections::HashMap;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use yserver_protocol::x11::{
    self, AtomId, ClientByteOrder, ClientId, RequestHeader, ResourceId, SequenceNumber,
};

use crate::host_x11::HostX11;

const ROOT_WINDOW: ResourceId = ResourceId(0x100);
const ROOT_COLORMAP: ResourceId = ResourceId(0x101);
const ROOT_VISUAL: ResourceId = ResourceId(0x102);
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
    println!("ynest listening on DISPLAY=:{display}");

    let mut host = match HostX11::open_from_env() {
        Ok(host) => {
            println!("ynest host X11 container window: 0x{:x}", host.window_id());
            Some(host)
        }
        Err(err) => {
            eprintln!("ynest: could not open host X11 window: {err}");
            None
        }
    };
    if let Some(host) = host.as_mut() {
        let _ = host.ping();
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let client_id = ClientId(NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed));
                thread::spawn(move || {
                    if let Err(err) = handle_client(client_id, stream) {
                        eprintln!("client {} disconnected: {err}", client_id.0);
                    }
                });
            }
            Err(err) => eprintln!("accept failed: {err}"),
        }
    }

    Ok(())
}

fn handle_client(client_id: ClientId, mut stream: UnixStream) -> io::Result<()> {
    let setup = x11::read_setup_request(&mut stream)?;
    if setup.byte_order != ClientByteOrder::LittleEndian {
        x11::write_setup_failed(
            &mut stream,
            setup.byte_order,
            "ynest currently supports only little-endian clients",
        )?;
        return Ok(());
    }

    println!(
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

    let mut state = ClientState::new();
    let mut sequence = SequenceNumber(0);
    loop {
        let Some((header, body)) = x11::read_request(&mut stream)? else {
            return Ok(());
        };
        sequence = sequence.next();
        handle_request(client_id, &mut state, &mut stream, sequence, header, &body)?;
    }
}

struct ClientState {
    atoms_by_name: HashMap<String, AtomId>,
    atom_names: HashMap<u32, String>,
    next_atom_id: u32,
}

impl ClientState {
    fn new() -> Self {
        Self {
            atoms_by_name: HashMap::new(),
            atom_names: HashMap::new(),
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

fn handle_request(
    client_id: ClientId,
    state: &mut ClientState,
    stream: &mut UnixStream,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<()> {
    match header.opcode {
        1 => log_void(client_id, sequence, "CreateWindow"),
        2 => log_void(client_id, sequence, "ChangeWindowAttributes"),
        3 => {
            log_reply(client_id, sequence, "GetWindowAttributes");
            x11::write_get_window_attributes_reply(stream, sequence)
        }
        4 => log_void(client_id, sequence, "DestroyWindow"),
        7 => log_void(client_id, sequence, "ReparentWindow"),
        8 => log_void(client_id, sequence, "MapWindow"),
        9 => log_void(client_id, sequence, "MapSubwindows"),
        10 => log_void(client_id, sequence, "UnmapWindow"),
        12 => log_void(client_id, sequence, "ConfigureWindow"),
        14 => {
            log_reply(client_id, sequence, "GetGeometry");
            x11::write_get_geometry_reply(stream, sequence, ROOT_WINDOW, 0, 0, 800, 600, 0, 24)
        }
        15 => {
            log_reply(client_id, sequence, "QueryTree");
            x11::write_query_tree_reply(stream, sequence, ROOT_WINDOW, ROOT_WINDOW, &[])
        }
        16 => {
            let name = x11::intern_atom_name(body);
            let atom = state.intern_atom(&name, header.data != 0);
            println!(
                "client {} #{} InternAtom {:?} -> {}",
                client_id.0, sequence.0, name, atom.0
            );
            x11::write_intern_atom_reply(stream, sequence, atom)
        }
        17 => {
            let atom = x11::request_atom(body);
            let name = state.atom_name(atom).unwrap_or("UNKNOWN");
            println!(
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
            x11::write_query_pointer_reply(stream, sequence, ROOT_WINDOW, ROOT_WINDOW)
        }
        40 => {
            log_reply(client_id, sequence, "TranslateCoordinates");
            x11::write_translate_coordinates_reply(stream, sequence, ResourceId(0), 0, 0)
        }
        42 => log_void(client_id, sequence, "SetInputFocus"),
        43 => {
            log_reply(client_id, sequence, "GetInputFocus");
            x11::write_get_input_focus_reply(stream, sequence, ROOT_WINDOW)
        }
        44 => {
            log_reply(client_id, sequence, "QueryKeymap");
            x11::write_query_keymap_reply(stream, sequence)
        }
        45 => log_void(client_id, sequence, "OpenFont"),
        46 => log_void(client_id, sequence, "CloseFont"),
        53 => log_void(client_id, sequence, "CreatePixmap"),
        54 => log_void(client_id, sequence, "FreePixmap"),
        55 => log_void(client_id, sequence, "CreateGC"),
        56 => log_void(client_id, sequence, "ChangeGC"),
        60 => log_void(client_id, sequence, "FreeGC"),
        61 => log_void(client_id, sequence, "ClearArea"),
        62 => log_void(client_id, sequence, "CopyArea"),
        64 => log_void(client_id, sequence, "PolyPoint"),
        65 => log_void(client_id, sequence, "PolyLine"),
        66 => log_void(client_id, sequence, "PolySegment"),
        67 => log_void(client_id, sequence, "PolyRectangle"),
        68 => log_void(client_id, sequence, "PolyArc"),
        69 => log_void(client_id, sequence, "FillPoly"),
        70 => log_void(client_id, sequence, "PolyFillRectangle"),
        71 => log_void(client_id, sequence, "PolyFillArc"),
        72 => log_void(client_id, sequence, "PutImage"),
        76 => log_void(client_id, sequence, "ImageText8"),
        78 => log_void(client_id, sequence, "CreateColormap"),
        84 => {
            log_reply(client_id, sequence, "AllocColor");
            x11::write_alloc_color_reply(stream, sequence)
        }
        91 => {
            let pixels = x11::query_colors_pixels(body);
            println!(
                "client {} #{} QueryColors {} pixels",
                client_id.0,
                sequence.0,
                pixels.len()
            );
            x11::write_query_colors_reply(stream, sequence, &pixels)
        }
        98 => {
            let name = x11::query_extension_name(body);
            println!(
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
            x11::write_get_keyboard_mapping_reply(stream, sequence, 1)
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
            println!(
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

fn log_void(client_id: ClientId, sequence: SequenceNumber, name: &str) -> io::Result<()> {
    println!("client {} #{} {name}", client_id.0, sequence.0);
    Ok(())
}

fn log_reply(client_id: ClientId, sequence: SequenceNumber, name: &str) {
    println!("client {} #{} {name}", client_id.0, sequence.0);
}
