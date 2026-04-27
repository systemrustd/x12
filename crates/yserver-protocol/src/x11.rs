use std::io::{self, ErrorKind, Read, Write};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientByteOrder {
    LittleEndian,
    BigEndian,
}

#[derive(Clone, Copy, Debug)]
pub struct ClientId(pub u32);

#[derive(Clone, Copy, Debug)]
pub struct ResourceId(pub u32);

#[derive(Clone, Copy, Debug)]
pub struct AtomId(pub u32);

#[derive(Clone, Copy, Debug)]
pub struct SequenceNumber(pub u16);

impl SequenceNumber {
    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

#[derive(Debug)]
pub struct SetupRequest {
    pub byte_order: ClientByteOrder,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub auth_protocol_name: Vec<u8>,
    pub auth_protocol_data: Vec<u8>,
}

#[derive(Debug)]
pub struct SetupSuccess<'a> {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub release_number: u32,
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    pub motion_buffer_size: u32,
    pub maximum_request_length: u16,
    pub image_byte_order: ClientByteOrder,
    pub bitmap_format_bit_order: ClientByteOrder,
    pub bitmap_format_scanline_unit: u8,
    pub bitmap_format_scanline_pad: u8,
    pub min_keycode: u8,
    pub max_keycode: u8,
    pub vendor: &'a str,
    pub root: Screen,
}

#[derive(Clone, Copy, Debug)]
pub struct Screen {
    pub root: ResourceId,
    pub default_colormap: ResourceId,
    pub white_pixel: u32,
    pub black_pixel: u32,
    pub current_input_masks: u32,
    pub width_px: u16,
    pub height_px: u16,
    pub width_mm: u16,
    pub height_mm: u16,
    pub min_installed_maps: u16,
    pub max_installed_maps: u16,
    pub root_visual: ResourceId,
    pub root_depth: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct RequestHeader {
    pub opcode: u8,
    pub data: u8,
    pub length_units: u16,
}

pub fn read_setup_request(reader: &mut impl Read) -> io::Result<SetupRequest> {
    let mut header = [0; 12];
    reader.read_exact(&mut header)?;

    let byte_order = match header[0] {
        b'l' => ClientByteOrder::LittleEndian,
        b'B' => ClientByteOrder::BigEndian,
        byte => {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("invalid X11 byte order marker {byte}"),
            ));
        }
    };

    let protocol_major = read_u16(byte_order, &header[2..4]);
    let protocol_minor = read_u16(byte_order, &header[4..6]);
    let auth_name_len = read_u16(byte_order, &header[6..8]) as usize;
    let auth_data_len = read_u16(byte_order, &header[8..10]) as usize;

    let mut auth_protocol_name = vec![0; pad4(auth_name_len)];
    reader.read_exact(&mut auth_protocol_name)?;
    auth_protocol_name.truncate(auth_name_len);

    let mut auth_protocol_data = vec![0; pad4(auth_data_len)];
    reader.read_exact(&mut auth_protocol_data)?;
    auth_protocol_data.truncate(auth_data_len);

    Ok(SetupRequest {
        byte_order,
        protocol_major,
        protocol_minor,
        auth_protocol_name,
        auth_protocol_data,
    })
}

pub fn write_setup_failed(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    reason: &str,
) -> io::Result<()> {
    let reason_len = reason.len().min(u8::MAX as usize);
    let mut body = Vec::new();
    body.push(0);
    body.push(reason_len as u8);
    write_u16(byte_order, &mut body, 11);
    write_u16(byte_order, &mut body, 0);
    body.extend_from_slice(&reason.as_bytes()[..reason_len]);
    pad_vec4(&mut body);
    writer.write_all(&body)
}

pub fn write_setup_success(writer: &mut impl Write, setup: SetupSuccess<'_>) -> io::Result<()> {
    let vendor = setup.vendor.as_bytes();

    let mut extra = Vec::new();
    write_u32(
        ClientByteOrder::LittleEndian,
        &mut extra,
        setup.release_number,
    );
    write_u32(
        ClientByteOrder::LittleEndian,
        &mut extra,
        setup.resource_id_base,
    );
    write_u32(
        ClientByteOrder::LittleEndian,
        &mut extra,
        setup.resource_id_mask,
    );
    write_u32(
        ClientByteOrder::LittleEndian,
        &mut extra,
        setup.motion_buffer_size,
    );
    write_u16(
        ClientByteOrder::LittleEndian,
        &mut extra,
        vendor.len() as u16,
    );
    write_u16(
        ClientByteOrder::LittleEndian,
        &mut extra,
        setup.maximum_request_length,
    );
    extra.push(1); // roots
    extra.push(1); // pixmap formats
    extra.push(byte_order_value(setup.image_byte_order));
    extra.push(byte_order_value(setup.bitmap_format_bit_order));
    extra.push(setup.bitmap_format_scanline_unit);
    extra.push(setup.bitmap_format_scanline_pad);
    extra.push(setup.min_keycode);
    extra.push(setup.max_keycode);
    extra.extend_from_slice(&[0; 4]);

    extra.extend_from_slice(vendor);
    pad_vec4(&mut extra);

    extra.push(24); // depth
    extra.push(32); // bits per pixel
    extra.push(32); // scanline pad
    extra.extend_from_slice(&[0; 5]);

    write_screen(&mut extra, setup.root);

    let length_units = checked_units(extra.len())?;
    let mut reply = Vec::with_capacity(8 + extra.len());
    reply.push(1);
    reply.push(0);
    write_u16(
        ClientByteOrder::LittleEndian,
        &mut reply,
        setup.protocol_major,
    );
    write_u16(
        ClientByteOrder::LittleEndian,
        &mut reply,
        setup.protocol_minor,
    );
    write_u16(ClientByteOrder::LittleEndian, &mut reply, length_units);
    reply.extend_from_slice(&extra);
    writer.write_all(&reply)
}

fn write_screen(out: &mut Vec<u8>, screen: Screen) {
    write_u32(ClientByteOrder::LittleEndian, out, screen.root.0);
    write_u32(
        ClientByteOrder::LittleEndian,
        out,
        screen.default_colormap.0,
    );
    write_u32(ClientByteOrder::LittleEndian, out, screen.white_pixel);
    write_u32(ClientByteOrder::LittleEndian, out, screen.black_pixel);
    write_u32(
        ClientByteOrder::LittleEndian,
        out,
        screen.current_input_masks,
    );
    write_u16(ClientByteOrder::LittleEndian, out, screen.width_px);
    write_u16(ClientByteOrder::LittleEndian, out, screen.height_px);
    write_u16(ClientByteOrder::LittleEndian, out, screen.width_mm);
    write_u16(ClientByteOrder::LittleEndian, out, screen.height_mm);
    write_u16(
        ClientByteOrder::LittleEndian,
        out,
        screen.min_installed_maps,
    );
    write_u16(
        ClientByteOrder::LittleEndian,
        out,
        screen.max_installed_maps,
    );
    write_u32(ClientByteOrder::LittleEndian, out, screen.root_visual.0);
    out.push(0); // backing stores: Never
    out.push(0); // save unders: false
    out.push(screen.root_depth);
    out.push(1); // allowed depths

    out.push(screen.root_depth);
    out.push(0);
    write_u16(ClientByteOrder::LittleEndian, out, 1); // visuals
    write_u32(ClientByteOrder::LittleEndian, out, 0);

    write_u32(ClientByteOrder::LittleEndian, out, screen.root_visual.0);
    out.push(4); // TrueColor
    out.push(8); // bits per rgb
    write_u16(ClientByteOrder::LittleEndian, out, 256);
    write_u32(ClientByteOrder::LittleEndian, out, 0x00ff_0000);
    write_u32(ClientByteOrder::LittleEndian, out, 0x0000_ff00);
    write_u32(ClientByteOrder::LittleEndian, out, 0x0000_00ff);
    write_u32(ClientByteOrder::LittleEndian, out, 0);
}

pub fn read_request(reader: &mut impl Read) -> io::Result<Option<(RequestHeader, Vec<u8>)>> {
    let mut header = [0; 4];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err),
    }

    let request = RequestHeader {
        opcode: header[0],
        data: header[1],
        length_units: u16::from_le_bytes([header[2], header[3]]),
    };

    if request.length_units == 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "BIG-REQUESTS length form is not supported yet",
        ));
    }

    let request_len = usize::from(request.length_units) * 4;
    if request_len < 4 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("invalid request length {}", request.length_units),
        ));
    }

    let mut body = vec![0; request_len - 4];
    reader.read_exact(&mut body)?;
    Ok(Some((request, body)))
}

pub fn intern_atom_name(body: &[u8]) -> String {
    if body.len() < 4 {
        return String::new();
    }
    let len = u16::from_le_bytes([body[0], body[1]]) as usize;
    let name = body.get(4..4 + len).unwrap_or_default();
    String::from_utf8_lossy(name).into_owned()
}

pub fn request_atom(body: &[u8]) -> AtomId {
    if body.len() < 4 {
        return AtomId(0);
    }
    AtomId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]))
}

pub fn query_colors_pixels(body: &[u8]) -> Vec<u32> {
    if body.len() <= 4 {
        return Vec::new();
    }

    body[4..]
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

pub fn query_extension_name(body: &[u8]) -> String {
    if body.len() < 4 {
        return String::new();
    }
    let len = u16::from_le_bytes([body[0], body[1]]) as usize;
    let name = body.get(4..4 + len).unwrap_or_default();
    String::from_utf8_lossy(name).into_owned()
}

pub fn well_known_atom(name: &str) -> Option<AtomId> {
    let id = match name {
        "PRIMARY" => 1,
        "SECONDARY" => 2,
        "ARC" => 3,
        "ATOM" => 4,
        "BITMAP" => 5,
        "CARDINAL" => 6,
        "COLORMAP" => 7,
        "CURSOR" => 8,
        "CUT_BUFFER0" => 9,
        "CUT_BUFFER1" => 10,
        "CUT_BUFFER2" => 11,
        "CUT_BUFFER3" => 12,
        "CUT_BUFFER4" => 13,
        "CUT_BUFFER5" => 14,
        "CUT_BUFFER6" => 15,
        "CUT_BUFFER7" => 16,
        "DRAWABLE" => 17,
        "FONT" => 18,
        "INTEGER" => 19,
        "PIXMAP" => 20,
        "POINT" => 21,
        "RECTANGLE" => 22,
        "RESOURCE_MANAGER" => 23,
        "RGB_COLOR_MAP" => 24,
        "RGB_BEST_MAP" => 25,
        "RGB_BLUE_MAP" => 26,
        "RGB_DEFAULT_MAP" => 27,
        "RGB_GRAY_MAP" => 28,
        "RGB_GREEN_MAP" => 29,
        "RGB_RED_MAP" => 30,
        "STRING" => 31,
        "VISUALID" => 32,
        "WINDOW" => 33,
        "WM_COMMAND" => 34,
        "WM_HINTS" => 35,
        "WM_CLIENT_MACHINE" => 36,
        "WM_ICON_NAME" => 37,
        "WM_ICON_SIZE" => 38,
        "WM_NAME" => 39,
        "WM_NORMAL_HINTS" => 40,
        "WM_SIZE_HINTS" => 41,
        "WM_ZOOM_HINTS" => 42,
        "MIN_SPACE" => 43,
        "NORM_SPACE" => 44,
        "MAX_SPACE" => 45,
        "END_SPACE" => 46,
        "SUPERSCRIPT_X" => 47,
        "SUPERSCRIPT_Y" => 48,
        "SUBSCRIPT_X" => 49,
        "SUBSCRIPT_Y" => 50,
        "UNDERLINE_POSITION" => 51,
        "UNDERLINE_THICKNESS" => 52,
        "STRIKEOUT_ASCENT" => 53,
        "STRIKEOUT_DESCENT" => 54,
        "ITALIC_ANGLE" => 55,
        "X_HEIGHT" => 56,
        "QUAD_WIDTH" => 57,
        "WEIGHT" => 58,
        "POINT_SIZE" => 59,
        "RESOLUTION" => 60,
        "COPYRIGHT" => 61,
        "NOTICE" => 62,
        "FONT_NAME" => 63,
        "FAMILY_NAME" => 64,
        "FULL_NAME" => 65,
        "CAP_HEIGHT" => 66,
        "WM_CLASS" => 67,
        "WM_TRANSIENT_FOR" => 68,
        _ => return None,
    };
    Some(AtomId(id))
}

pub fn well_known_atom_name(atom: AtomId) -> Option<&'static str> {
    let name = match atom.0 {
        1 => "PRIMARY",
        2 => "SECONDARY",
        3 => "ARC",
        4 => "ATOM",
        5 => "BITMAP",
        6 => "CARDINAL",
        7 => "COLORMAP",
        8 => "CURSOR",
        9 => "CUT_BUFFER0",
        10 => "CUT_BUFFER1",
        11 => "CUT_BUFFER2",
        12 => "CUT_BUFFER3",
        13 => "CUT_BUFFER4",
        14 => "CUT_BUFFER5",
        15 => "CUT_BUFFER6",
        16 => "CUT_BUFFER7",
        17 => "DRAWABLE",
        18 => "FONT",
        19 => "INTEGER",
        20 => "PIXMAP",
        21 => "POINT",
        22 => "RECTANGLE",
        23 => "RESOURCE_MANAGER",
        24 => "RGB_COLOR_MAP",
        25 => "RGB_BEST_MAP",
        26 => "RGB_BLUE_MAP",
        27 => "RGB_DEFAULT_MAP",
        28 => "RGB_GRAY_MAP",
        29 => "RGB_GREEN_MAP",
        30 => "RGB_RED_MAP",
        31 => "STRING",
        32 => "VISUALID",
        33 => "WINDOW",
        34 => "WM_COMMAND",
        35 => "WM_HINTS",
        36 => "WM_CLIENT_MACHINE",
        37 => "WM_ICON_NAME",
        38 => "WM_ICON_SIZE",
        39 => "WM_NAME",
        40 => "WM_NORMAL_HINTS",
        41 => "WM_SIZE_HINTS",
        42 => "WM_ZOOM_HINTS",
        43 => "MIN_SPACE",
        44 => "NORM_SPACE",
        45 => "MAX_SPACE",
        46 => "END_SPACE",
        47 => "SUPERSCRIPT_X",
        48 => "SUPERSCRIPT_Y",
        49 => "SUBSCRIPT_X",
        50 => "SUBSCRIPT_Y",
        51 => "UNDERLINE_POSITION",
        52 => "UNDERLINE_THICKNESS",
        53 => "STRIKEOUT_ASCENT",
        54 => "STRIKEOUT_DESCENT",
        55 => "ITALIC_ANGLE",
        56 => "X_HEIGHT",
        57 => "QUAD_WIDTH",
        58 => "WEIGHT",
        59 => "POINT_SIZE",
        60 => "RESOLUTION",
        61 => "COPYRIGHT",
        62 => "NOTICE",
        63 => "FONT_NAME",
        64 => "FAMILY_NAME",
        65 => "FULL_NAME",
        66 => "CAP_HEIGHT",
        67 => "WM_CLASS",
        68 => "WM_TRANSIENT_FOR",
        _ => return None,
    };
    Some(name)
}

pub fn write_get_window_attributes_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 3, 0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // visual
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 1); // class InputOutput
    reply.push(1); // bit gravity NorthWest
    reply.push(1); // win gravity NorthWest
    write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // backing planes
    write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // backing pixel
    reply.push(0); // save under
    reply.push(1); // map installed
    reply.push(2); // map state viewable
    reply.push(0); // override redirect
    write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // colormap
    write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // all event masks
    write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // your event mask
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0); // do not propagate
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0);
    writer.write_all(&reply)
}

pub fn write_get_geometry_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    root: ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
    depth: u8,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, depth, 0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, root.0);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, x);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, y);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, width);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, height);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, border_width);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0);
    reply.extend_from_slice(&[0; 10]);
    writer.write_all(&reply)
}

pub fn write_query_tree_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    root: ResourceId,
    parent: ResourceId,
    children: &[ResourceId],
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, children.len() as u32);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, root.0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, parent.0);
    write_u16(
        ClientByteOrder::LittleEndian,
        &mut reply,
        children.len() as u16,
    );
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0);
    reply.extend_from_slice(&[0; 14]);
    for child in children {
        write_u32(ClientByteOrder::LittleEndian, &mut reply, child.0);
    }
    writer.write_all(&reply)
}

pub fn write_intern_atom_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    atom: AtomId,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, 0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, atom.0);
    reply.extend_from_slice(&[0; 20]);
    writer.write_all(&reply)
}

pub fn write_get_atom_name_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    name: &str,
) -> io::Result<()> {
    let mut extra = name.as_bytes().to_vec();
    pad_vec4(&mut extra);
    let mut reply = fixed_reply(sequence, 0, checked_units(extra.len())? as u32);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, name.len() as u16);
    reply.extend_from_slice(&[0; 22]);
    reply.extend_from_slice(&extra);
    writer.write_all(&reply)
}

pub fn write_get_property_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 8, 0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // type None
    write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // bytes after
    write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // value len
    reply.extend_from_slice(&[0; 12]);
    writer.write_all(&reply)
}

pub fn write_get_selection_owner_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    owner: ResourceId,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, 0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, owner.0);
    reply.extend_from_slice(&[0; 20]);
    writer.write_all(&reply)
}

pub fn write_grab_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    status: u8,
) -> io::Result<()> {
    let reply = fixed_reply(sequence, status, 0);
    writer.write_all(&reply)
}

pub fn write_query_pointer_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    root: ResourceId,
    child: ResourceId,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 1, 0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, root.0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, child.0);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, 0);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, 0);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, 0);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, 0);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0);
    reply.extend_from_slice(&[0; 6]);
    writer.write_all(&reply)
}

pub fn write_translate_coordinates_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    child: ResourceId,
    dst_x: i16,
    dst_y: i16,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 1, 0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, child.0);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, dst_x);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, dst_y);
    reply.extend_from_slice(&[0; 16]);
    writer.write_all(&reply)
}

pub fn write_get_input_focus_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    focus: ResourceId,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 1, 0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, focus.0);
    reply.extend_from_slice(&[0; 20]);
    writer.write_all(&reply)
}

pub fn write_query_keymap_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, 2);
    reply.extend_from_slice(&[0; 32]);
    writer.write_all(&reply)
}

pub fn write_alloc_color_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, 0);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0xffff);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0xffff);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0xffff);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, 0x00ff_ffff);
    reply.extend_from_slice(&[0; 12]);
    writer.write_all(&reply)
}

pub fn write_query_colors_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    pixels: &[u32],
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, (pixels.len() * 2) as u32);
    write_u16(
        ClientByteOrder::LittleEndian,
        &mut reply,
        pixels.len() as u16,
    );
    reply.extend_from_slice(&[0; 22]);

    for pixel in pixels {
        let red = (((pixel >> 16) & 0xff) as u16) * 257;
        let green = (((pixel >> 8) & 0xff) as u16) * 257;
        let blue = ((pixel & 0xff) as u16) * 257;
        write_u16(ClientByteOrder::LittleEndian, &mut reply, red);
        write_u16(ClientByteOrder::LittleEndian, &mut reply, green);
        write_u16(ClientByteOrder::LittleEndian, &mut reply, blue);
        write_u16(ClientByteOrder::LittleEndian, &mut reply, 0);
    }

    writer.write_all(&reply)
}

pub fn write_query_extension_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    present: bool,
    major_opcode: u8,
    first_event: u8,
    first_error: u8,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, 0);
    reply.push(u8::from(present));
    reply.push(major_opcode);
    reply.push(first_event);
    reply.push(first_error);
    reply.extend_from_slice(&[0; 20]);
    writer.write_all(&reply)
}

pub fn write_list_extensions_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
) -> io::Result<()> {
    let reply = fixed_reply(sequence, 0, 0);
    writer.write_all(&reply)
}

pub fn write_get_keyboard_mapping_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    keysyms_per_keycode: u8,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, keysyms_per_keycode, 0);
    reply.extend_from_slice(&[0; 24]);
    writer.write_all(&reply)
}

pub fn write_list_hosts_reply(writer: &mut impl Write, sequence: SequenceNumber) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, 0);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0);
    reply.extend_from_slice(&[0; 22]);
    writer.write_all(&reply)
}

pub fn write_get_pointer_mapping_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 3, 1);
    reply.extend_from_slice(&[0; 24]);
    reply.extend_from_slice(&[1, 2, 3, 0]);
    writer.write_all(&reply)
}

pub fn write_get_modifier_mapping_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 1, 2);
    reply.extend_from_slice(&[0; 24]);
    reply.extend_from_slice(&[0; 8]);
    writer.write_all(&reply)
}

fn fixed_reply(sequence: SequenceNumber, data: u8, length: u32) -> Vec<u8> {
    let mut reply = Vec::with_capacity(32);
    reply.push(1);
    reply.push(data);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, sequence.0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, length);
    reply
}

fn checked_units(byte_len: usize) -> io::Result<u16> {
    if byte_len % 4 != 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("length {byte_len} is not 4-byte aligned"),
        ));
    }
    u16::try_from(byte_len / 4).map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("length {byte_len} is too large"),
        )
    })
}

fn read_u16(byte_order: ClientByteOrder, bytes: &[u8]) -> u16 {
    match byte_order {
        ClientByteOrder::LittleEndian => u16::from_le_bytes([bytes[0], bytes[1]]),
        ClientByteOrder::BigEndian => u16::from_be_bytes([bytes[0], bytes[1]]),
    }
}

fn write_u16(byte_order: ClientByteOrder, out: &mut Vec<u8>, value: u16) {
    match byte_order {
        ClientByteOrder::LittleEndian => out.extend_from_slice(&value.to_le_bytes()),
        ClientByteOrder::BigEndian => out.extend_from_slice(&value.to_be_bytes()),
    }
}

fn write_i16(byte_order: ClientByteOrder, out: &mut Vec<u8>, value: i16) {
    match byte_order {
        ClientByteOrder::LittleEndian => out.extend_from_slice(&value.to_le_bytes()),
        ClientByteOrder::BigEndian => out.extend_from_slice(&value.to_be_bytes()),
    }
}

fn write_u32(byte_order: ClientByteOrder, out: &mut Vec<u8>, value: u32) {
    match byte_order {
        ClientByteOrder::LittleEndian => out.extend_from_slice(&value.to_le_bytes()),
        ClientByteOrder::BigEndian => out.extend_from_slice(&value.to_be_bytes()),
    }
}

fn byte_order_value(byte_order: ClientByteOrder) -> u8 {
    match byte_order {
        ClientByteOrder::LittleEndian => 0,
        ClientByteOrder::BigEndian => 1,
    }
}

fn pad4(len: usize) -> usize {
    (len + 3) & !3
}

fn pad_vec4(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}
