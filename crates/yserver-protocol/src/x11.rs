use std::io::{self, ErrorKind, Read, Write};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientByteOrder {
    LittleEndian,
    BigEndian,
}

#[derive(Clone, Copy, Debug)]
pub struct ClientId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ResourceId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AtomId(pub u32);

#[derive(Clone, Copy, Debug)]
pub struct SequenceNumber(pub u16);

impl SequenceNumber {
    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Rgb16 {
    pub red: u16,
    pub green: u16,
    pub blue: u16,
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

#[derive(Clone, Copy, Debug)]
pub struct KeyEvent {
    pub pressed: bool,
    pub keycode: u8,
    pub sequence: SequenceNumber,
    pub time: u32,
    pub root: ResourceId,
    pub event: ResourceId,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct CreateWindowRequest {
    pub depth: u8,
    pub window: ResourceId,
    pub parent: ResourceId,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
    pub class: u16,
    pub visual: ResourceId,
    pub background_pixel: Option<u32>,
    pub event_mask: Option<u32>,
    pub override_redirect: Option<bool>,
}

#[derive(Clone, Copy, Debug)]
pub struct ChangeWindowAttributesRequest {
    pub window: ResourceId,
    pub background_pixel: Option<u32>,
    pub event_mask: Option<u32>,
    pub cursor: Option<ResourceId>,
}

#[derive(Clone, Copy, Debug)]
pub struct ConfigureWindowRequest {
    pub window: ResourceId,
    pub x: Option<i16>,
    pub y: Option<i16>,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub border_width: Option<u16>,
}

#[derive(Clone, Copy, Debug)]
pub struct CreatePixmapRequest {
    pub depth: u8,
    pub pixmap: ResourceId,
    pub drawable: ResourceId,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct CreateGcRequest {
    pub gc: ResourceId,
    pub drawable: ResourceId,
    pub foreground: Option<u32>,
    pub background: Option<u32>,
    pub line_width: Option<u16>,
}

#[derive(Clone, Copy, Debug)]
pub struct GcChange {
    pub gc: ResourceId,
    pub foreground: Option<u32>,
    pub background: Option<u32>,
    pub line_width: Option<u16>,
}

#[derive(Clone, Copy, Debug)]
pub struct ClearAreaRequest {
    pub window: ResourceId,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Debug)]
pub struct OpenFontRequest {
    pub font: ResourceId,
    pub name: String,
}

#[derive(Clone, Copy, Debug)]
pub struct FontInfo {
    pub ascent: i16,
    pub descent: i16,
    pub min_bounds_width: i16,
    pub max_bounds_width: i16,
}

#[derive(Clone, Copy, Debug)]
pub struct WindowAttributes {
    pub visual: ResourceId,
    pub class: u16,
    pub bit_gravity: u8,
    pub win_gravity: u8,
    pub backing_planes: u32,
    pub backing_pixel: u32,
    pub save_under: bool,
    pub map_is_installed: bool,
    pub map_state: u8,
    pub override_redirect: bool,
    pub colormap: ResourceId,
    pub all_event_masks: u32,
    pub your_event_mask: u32,
    pub do_not_propagate_mask: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub root: ResourceId,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
    pub depth: u8,
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

pub fn write_error(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    error_code: u8,
    bad_value: u32,
    minor_opcode: u16,
    major_opcode: u8,
) -> io::Result<()> {
    let mut error = Vec::with_capacity(32);
    error.push(0);
    error.push(error_code);
    write_u16(ClientByteOrder::LittleEndian, &mut error, sequence.0);
    write_u32(ClientByteOrder::LittleEndian, &mut error, bad_value);
    write_u16(ClientByteOrder::LittleEndian, &mut error, minor_opcode);
    error.push(major_opcode);
    error.extend_from_slice(&[0; 21]);
    writer.write_all(&error)
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

pub fn create_window_request(depth: u8, body: &[u8]) -> Option<CreateWindowRequest> {
    let value_mask = read_u32_le(body.get(24..28)?);
    let values = value_list(value_mask, body.get(28..)?);
    Some(CreateWindowRequest {
        depth,
        window: ResourceId(read_u32_le(body.get(0..4)?)),
        parent: ResourceId(read_u32_le(body.get(4..8)?)),
        x: read_i16_le(body.get(8..10)?),
        y: read_i16_le(body.get(10..12)?),
        width: read_u16_le(body.get(12..14)?),
        height: read_u16_le(body.get(14..16)?),
        border_width: read_u16_le(body.get(16..18)?),
        class: read_u16_le(body.get(18..20)?),
        visual: ResourceId(read_u32_le(body.get(20..24)?)),
        background_pixel: values.value(1),
        event_mask: values.value(11),
        override_redirect: values.value(9).map(|value| value != 0),
    })
}

pub fn change_window_attributes_request(body: &[u8]) -> Option<ChangeWindowAttributesRequest> {
    let value_mask = read_u32_le(body.get(4..8)?);
    let values = value_list(value_mask, body.get(8..)?);
    Some(ChangeWindowAttributesRequest {
        window: ResourceId(read_u32_le(body.get(0..4)?)),
        background_pixel: values.value(1),
        event_mask: values.value(11),
        cursor: values.value(14).map(ResourceId),
    })
}

pub fn configure_window_request(body: &[u8]) -> Option<ConfigureWindowRequest> {
    let window = ResourceId(read_u32_le(body.get(0..4)?));
    let value_mask = read_u16_le(body.get(4..6)?) as u32;
    let values = value_list(value_mask, body.get(8..)?);
    Some(ConfigureWindowRequest {
        window,
        x: values.value(0).map(|value| value as i16),
        y: values.value(1).map(|value| value as i16),
        width: values.value(2).map(|value| value as u16),
        height: values.value(3).map(|value| value as u16),
        border_width: values.value(4).map(|value| value as u16),
    })
}

pub fn create_pixmap_request(depth: u8, body: &[u8]) -> Option<CreatePixmapRequest> {
    Some(CreatePixmapRequest {
        depth,
        pixmap: ResourceId(read_u32_le(body.get(0..4)?)),
        drawable: ResourceId(read_u32_le(body.get(4..8)?)),
        width: read_u16_le(body.get(8..10)?),
        height: read_u16_le(body.get(10..12)?),
    })
}

pub fn free_resource_id(body: &[u8]) -> Option<ResourceId> {
    Some(ResourceId(read_u32_le(body.get(0..4)?)))
}

pub fn create_gc_request(body: &[u8]) -> Option<CreateGcRequest> {
    let value_mask = read_u32_le(body.get(8..12)?);
    let values = value_list(value_mask, body.get(12..)?);
    Some(CreateGcRequest {
        gc: ResourceId(read_u32_le(body.get(0..4)?)),
        drawable: ResourceId(read_u32_le(body.get(4..8)?)),
        foreground: values.value(2),
        background: values.value(3),
        line_width: values.value(4).map(|value| value as u16),
    })
}

pub fn change_gc_request(body: &[u8]) -> Option<GcChange> {
    let value_mask = read_u32_le(body.get(4..8)?);
    let values = value_list(value_mask, body.get(8..)?);
    Some(GcChange {
        gc: ResourceId(read_u32_le(body.get(0..4)?)),
        foreground: values.value(2),
        background: values.value(3),
        line_width: values.value(4).map(|value| value as u16),
    })
}

pub fn drawable_request_id(body: &[u8]) -> Option<ResourceId> {
    Some(ResourceId(read_u32_le(body.get(0..4)?)))
}

pub fn clear_area_request(body: &[u8]) -> Option<ClearAreaRequest> {
    Some(ClearAreaRequest {
        window: ResourceId(read_u32_le(body.get(0..4)?)),
        x: read_i16_le(body.get(4..6)?),
        y: read_i16_le(body.get(6..8)?),
        width: read_u16_le(body.get(8..10)?),
        height: read_u16_le(body.get(10..12)?),
    })
}

pub fn open_font_request(body: &[u8]) -> Option<OpenFontRequest> {
    let font = ResourceId(read_u32_le(body.get(0..4)?));
    let name_len = read_u16_le(body.get(4..6)?) as usize;
    let name = body.get(8..8 + name_len)?;
    Some(OpenFontRequest {
        font,
        name: String::from_utf8_lossy(name).into_owned(),
    })
}

pub fn create_glyph_cursor_id(body: &[u8]) -> Option<ResourceId> {
    Some(ResourceId(read_u32_le(body.get(0..4)?)))
}

pub fn poly_fill_arc_data(body: &[u8]) -> Option<(u32, &[u8])> {
    arc_request_data(body)
}

pub fn poly_arc_data(body: &[u8]) -> Option<(u32, &[u8])> {
    arc_request_data(body)
}

pub fn poly_fill_rectangle_data(body: &[u8]) -> Option<(u32, &[u8])> {
    let gc_id = read_u32_le(body.get(4..8)?);
    let rectangles = body.get(8..)?;
    if rectangles.len() % 8 != 0 {
        return None;
    }
    Some((gc_id, rectangles))
}

pub fn poly_line_data(body: &[u8]) -> Option<(u32, &[u8])> {
    let gc_id = read_u32_le(body.get(4..8)?);
    let points = body.get(8..)?;
    if points.len() % 4 != 0 {
        return None;
    }
    Some((gc_id, points))
}

pub fn image_text8_data(body: &[u8]) -> Option<(u32, u32, &[u8])> {
    let drawable = read_u32_le(body.get(0..4)?);
    let gc_id = read_u32_le(body.get(4..8)?);
    Some((drawable, gc_id, body))
}

pub fn poly_text_data(body: &[u8]) -> Option<(u32, u32, &[u8])> {
    let drawable = read_u32_le(body.get(0..4)?);
    let gc_id = read_u32_le(body.get(4..8)?);
    Some((drawable, gc_id, body))
}

pub fn map_window_id(body: &[u8]) -> Option<ResourceId> {
    Some(ResourceId(read_u32_le(body.get(0..4)?)))
}

pub fn input_focus_window(body: &[u8]) -> Option<ResourceId> {
    Some(ResourceId(read_u32_le(body.get(0..4)?)))
}

pub fn write_key_event(writer: &mut impl Write, event: KeyEvent) -> io::Result<()> {
    let mut out = Vec::with_capacity(32);
    out.push(if event.pressed { 2 } else { 3 });
    out.push(event.keycode);
    write_u16(ClientByteOrder::LittleEndian, &mut out, event.sequence.0);
    write_u32(ClientByteOrder::LittleEndian, &mut out, event.time);
    write_u32(ClientByteOrder::LittleEndian, &mut out, event.root.0);
    write_u32(ClientByteOrder::LittleEndian, &mut out, event.event.0);
    write_u32(ClientByteOrder::LittleEndian, &mut out, 0); // child
    write_i16(ClientByteOrder::LittleEndian, &mut out, event.root_x);
    write_i16(ClientByteOrder::LittleEndian, &mut out, event.root_y);
    write_i16(ClientByteOrder::LittleEndian, &mut out, event.event_x);
    write_i16(ClientByteOrder::LittleEndian, &mut out, event.event_y);
    write_u16(ClientByteOrder::LittleEndian, &mut out, event.state);
    out.push(1); // same-screen
    out.push(0);
    writer.write_all(&out)
}

pub fn write_focus_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    focus_in: bool,
    window: ResourceId,
) -> io::Result<()> {
    let mut event = Vec::with_capacity(32);
    event.push(if focus_in { 9 } else { 10 });
    event.push(0); // NotifyAncestor
    write_u16(ClientByteOrder::LittleEndian, &mut event, sequence.0);
    write_u32(ClientByteOrder::LittleEndian, &mut event, window.0);
    event.push(0); // NotifyNormal
    event.extend_from_slice(&[0; 23]);
    writer.write_all(&event)
}

pub fn write_expose_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    window: ResourceId,
    width: u16,
    height: u16,
) -> io::Result<()> {
    let mut event = Vec::with_capacity(32);
    event.push(12); // Expose
    event.push(0);
    write_u16(ClientByteOrder::LittleEndian, &mut event, sequence.0);
    write_u32(ClientByteOrder::LittleEndian, &mut event, window.0);
    write_u16(ClientByteOrder::LittleEndian, &mut event, 0);
    write_u16(ClientByteOrder::LittleEndian, &mut event, 0);
    write_u16(ClientByteOrder::LittleEndian, &mut event, width);
    write_u16(ClientByteOrder::LittleEndian, &mut event, height);
    write_u16(ClientByteOrder::LittleEndian, &mut event, 0); // count
    event.extend_from_slice(&[0; 14]);
    writer.write_all(&event)
}

pub fn write_map_notify_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    event_window: ResourceId,
    window: ResourceId,
    override_redirect: bool,
) -> io::Result<()> {
    let mut event = Vec::with_capacity(32);
    event.push(19); // MapNotify
    event.push(0);
    write_u16(ClientByteOrder::LittleEndian, &mut event, sequence.0);
    write_u32(ClientByteOrder::LittleEndian, &mut event, event_window.0);
    write_u32(ClientByteOrder::LittleEndian, &mut event, window.0);
    event.push(u8::from(override_redirect));
    event.extend_from_slice(&[0; 19]);
    writer.write_all(&event)
}

pub fn write_configure_notify_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    event_window: ResourceId,
    window: ResourceId,
    geometry: Geometry,
    override_redirect: bool,
) -> io::Result<()> {
    let mut event = Vec::with_capacity(32);
    event.push(22); // ConfigureNotify
    event.push(0);
    write_u16(ClientByteOrder::LittleEndian, &mut event, sequence.0);
    write_u32(ClientByteOrder::LittleEndian, &mut event, event_window.0);
    write_u32(ClientByteOrder::LittleEndian, &mut event, window.0);
    write_u32(ClientByteOrder::LittleEndian, &mut event, 0); // above-sibling
    write_i16(ClientByteOrder::LittleEndian, &mut event, geometry.x);
    write_i16(ClientByteOrder::LittleEndian, &mut event, geometry.y);
    write_u16(ClientByteOrder::LittleEndian, &mut event, geometry.width);
    write_u16(ClientByteOrder::LittleEndian, &mut event, geometry.height);
    write_u16(
        ClientByteOrder::LittleEndian,
        &mut event,
        geometry.border_width,
    );
    event.push(u8::from(override_redirect));
    event.extend_from_slice(&[0; 5]);
    writer.write_all(&event)
}

fn arc_request_data(body: &[u8]) -> Option<(u32, &[u8])> {
    let gc_id = read_u32_le(body.get(4..8)?);
    let arcs = body.get(8..)?;
    if arcs.len() % 12 != 0 {
        return None;
    }
    Some((gc_id, arcs))
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

pub fn alloc_color_request(body: &[u8]) -> Option<Rgb16> {
    Some(Rgb16 {
        red: read_u16_le(body.get(4..6)?),
        green: read_u16_le(body.get(6..8)?),
        blue: read_u16_le(body.get(8..10)?),
    })
}

pub fn alloc_named_color_name(body: &[u8]) -> String {
    let Some(name_len) = body.get(4..6).map(read_u16_le) else {
        return String::new();
    };
    let name = body.get(8..8 + name_len as usize).unwrap_or_default();
    String::from_utf8_lossy(name).into_owned()
}

pub fn lookup_color_name(name: &str) -> Option<Rgb16> {
    let normalized: String = name
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();

    let gray_n = normalized
        .strip_prefix("gray")
        .or_else(|| normalized.strip_prefix("grey"));
    if let Some(rest) = gray_n {
        if rest.is_empty() {
            return Some(rgb8(190, 190, 190));
        }
        if let Ok(percent) = rest.parse::<u32>() {
            let value = u8::try_from(percent.min(100) * 255 / 100).unwrap_or(u8::MAX);
            return Some(rgb8(value, value, value));
        }
    }

    let (r, g, b) = match normalized.as_str() {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" | "red1" => (255, 0, 0),
        "red2" => (238, 0, 0),
        "red3" => (205, 0, 0),
        "red4" => (139, 0, 0),
        "green" | "green1" => (0, 255, 0),
        "green2" => (0, 238, 0),
        "green3" => (0, 205, 0),
        "green4" => (0, 139, 0),
        "blue" | "blue1" => (0, 0, 255),
        "blue2" => (0, 0, 238),
        "blue3" => (0, 0, 205),
        "blue4" => (0, 0, 139),
        "yellow" | "yellow1" => (255, 255, 0),
        "yellow2" => (238, 238, 0),
        "yellow3" => (205, 205, 0),
        "yellow4" => (139, 139, 0),
        "cyan" | "cyan1" => (0, 255, 255),
        "cyan2" => (0, 238, 238),
        "cyan3" => (0, 205, 205),
        "cyan4" => (0, 139, 139),
        "magenta" | "magenta1" => (255, 0, 255),
        "magenta2" => (238, 0, 238),
        "magenta3" => (205, 0, 205),
        "magenta4" => (139, 0, 139),
        "orange" => (255, 165, 0),
        "pink" => (255, 192, 203),
        "brown" => (165, 42, 42),
        "purple" => (160, 32, 240),
        "navy" | "navyblue" => (0, 0, 128),
        "gold" => (255, 215, 0),
        "lightgray" | "lightgrey" => (211, 211, 211),
        "darkgray" | "darkgrey" => (169, 169, 169),
        _ => return None,
    };

    Some(rgb8(r, g, b))
}

fn rgb8(r: u8, g: u8, b: u8) -> Rgb16 {
    Rgb16 {
        red: u16::from(r) * 257,
        green: u16::from(g) * 257,
        blue: u16::from(b) * 257,
    }
}

#[derive(Clone, Copy, Debug)]
struct ValueList<'a> {
    value_mask: u32,
    values: &'a [u8],
}

impl ValueList<'_> {
    fn value(self, target_bit: u8) -> Option<u32> {
        let mut offset = 0;
        for bit in 0..32 {
            if self.value_mask & (1 << bit) == 0 {
                continue;
            }

            let value = read_u32_le(self.values.get(offset..offset + 4)?);
            if bit == target_bit {
                return Some(value);
            }
            offset += 4;
        }
        None
    }
}

fn value_list(value_mask: u32, values: &[u8]) -> ValueList<'_> {
    ValueList { value_mask, values }
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
    attributes: WindowAttributes,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, 3);
    write_u32(
        ClientByteOrder::LittleEndian,
        &mut reply,
        attributes.visual.0,
    );
    write_u16(ClientByteOrder::LittleEndian, &mut reply, attributes.class);
    reply.push(attributes.bit_gravity);
    reply.push(attributes.win_gravity);
    write_u32(
        ClientByteOrder::LittleEndian,
        &mut reply,
        attributes.backing_planes,
    );
    write_u32(
        ClientByteOrder::LittleEndian,
        &mut reply,
        attributes.backing_pixel,
    );
    reply.push(u8::from(attributes.save_under));
    reply.push(u8::from(attributes.map_is_installed));
    reply.push(attributes.map_state);
    reply.push(u8::from(attributes.override_redirect));
    write_u32(
        ClientByteOrder::LittleEndian,
        &mut reply,
        attributes.colormap.0,
    );
    write_u32(
        ClientByteOrder::LittleEndian,
        &mut reply,
        attributes.all_event_masks,
    );
    write_u32(
        ClientByteOrder::LittleEndian,
        &mut reply,
        attributes.your_event_mask,
    );
    write_u16(
        ClientByteOrder::LittleEndian,
        &mut reply,
        attributes.do_not_propagate_mask,
    );
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0);
    writer.write_all(&reply)
}

pub fn write_get_geometry_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    geometry: Geometry,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, geometry.depth, 0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, geometry.root.0);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, geometry.x);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, geometry.y);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, geometry.width);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, geometry.height);
    write_u16(
        ClientByteOrder::LittleEndian,
        &mut reply,
        geometry.border_width,
    );
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
    root_x: i16,
    root_y: i16,
    win_x: i16,
    win_y: i16,
    mask: u16,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 1, 0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, root.0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, child.0);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, root_x);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, root_y);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, win_x);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, win_y);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, mask);
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
    color: Rgb16,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, 0);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.red);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.green);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.blue);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0);
    write_u32(
        ClientByteOrder::LittleEndian,
        &mut reply,
        rgb16_to_pixel(color),
    );
    reply.extend_from_slice(&[0; 12]);
    writer.write_all(&reply)
}

pub fn write_lookup_color_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    color: Rgb16,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, 0);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.red);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.green);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.blue);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.red);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.green);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.blue);
    reply.extend_from_slice(&[0; 12]);
    writer.write_all(&reply)
}

pub fn write_alloc_named_color_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    color: Rgb16,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, 0);
    write_u32(
        ClientByteOrder::LittleEndian,
        &mut reply,
        rgb16_to_pixel(color),
    );
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.red);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.green);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.blue);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.red);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.green);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, color.blue);
    reply.extend_from_slice(&[0; 8]);
    writer.write_all(&reply)
}

fn rgb16_to_pixel(color: Rgb16) -> u32 {
    ((u32::from(color.red) >> 8) << 16)
        | ((u32::from(color.green) >> 8) << 8)
        | (u32::from(color.blue) >> 8)
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
    first_keycode: u8,
    keycode_count: u8,
    keysyms_per_keycode: u8,
) -> io::Result<()> {
    let keysym_count = u32::from(keycode_count) * u32::from(keysyms_per_keycode);
    let mut reply = fixed_reply(sequence, keysyms_per_keycode, keysyms_per_keycode as u32);
    reply.extend_from_slice(&[0; 24]);
    reply.truncate(32);
    reply[4..8].copy_from_slice(&keysym_count.to_le_bytes());

    for offset in 0..keycode_count {
        let keycode = first_keycode.wrapping_add(offset);
        let (base, shifted) = keysyms_for_keycode(keycode);
        write_u32(ClientByteOrder::LittleEndian, &mut reply, base);
        if keysyms_per_keycode > 1 {
            write_u32(ClientByteOrder::LittleEndian, &mut reply, shifted);
        }
        for _ in 2..keysyms_per_keycode {
            write_u32(ClientByteOrder::LittleEndian, &mut reply, 0);
        }
    }
    writer.write_all(&reply)
}

fn keysyms_for_keycode(keycode: u8) -> (u32, u32) {
    match keycode {
        9 => (0xff1b, 0xff1b), // Escape
        10 => (b'1' as u32, b'!' as u32),
        11 => (b'2' as u32, b'@' as u32),
        12 => (b'3' as u32, b'#' as u32),
        13 => (b'4' as u32, b'$' as u32),
        14 => (b'5' as u32, b'%' as u32),
        15 => (b'6' as u32, b'^' as u32),
        16 => (b'7' as u32, b'&' as u32),
        17 => (b'8' as u32, b'*' as u32),
        18 => (b'9' as u32, b'(' as u32),
        19 => (b'0' as u32, b')' as u32),
        20 => (b'-' as u32, b'_' as u32),
        21 => (b'=' as u32, b'+' as u32),
        22 => (0xff08, 0xff08), // Backspace
        23 => (0xff09, 0xff09), // Tab
        24 => (b'q' as u32, b'Q' as u32),
        25 => (b'w' as u32, b'W' as u32),
        26 => (b'e' as u32, b'E' as u32),
        27 => (b'r' as u32, b'R' as u32),
        28 => (b't' as u32, b'T' as u32),
        29 => (b'y' as u32, b'Y' as u32),
        30 => (b'u' as u32, b'U' as u32),
        31 => (b'i' as u32, b'I' as u32),
        32 => (b'o' as u32, b'O' as u32),
        33 => (b'p' as u32, b'P' as u32),
        34 => (b'[' as u32, b'{' as u32),
        35 => (b']' as u32, b'}' as u32),
        36 => (0xff0d, 0xff0d), // Return
        38 => (b'a' as u32, b'A' as u32),
        39 => (b's' as u32, b'S' as u32),
        40 => (b'd' as u32, b'D' as u32),
        41 => (b'f' as u32, b'F' as u32),
        42 => (b'g' as u32, b'G' as u32),
        43 => (b'h' as u32, b'H' as u32),
        44 => (b'j' as u32, b'J' as u32),
        45 => (b'k' as u32, b'K' as u32),
        46 => (b'l' as u32, b'L' as u32),
        47 => (b';' as u32, b':' as u32),
        48 => (b'\'' as u32, b'"' as u32),
        49 => (b'`' as u32, b'~' as u32),
        50 => (0xffe1, 0xffe1), // Shift_L
        51 => (b'\\' as u32, b'|' as u32),
        52 => (b'z' as u32, b'Z' as u32),
        53 => (b'x' as u32, b'X' as u32),
        54 => (b'c' as u32, b'C' as u32),
        55 => (b'v' as u32, b'V' as u32),
        56 => (b'b' as u32, b'B' as u32),
        57 => (b'n' as u32, b'N' as u32),
        58 => (b'm' as u32, b'M' as u32),
        59 => (b',' as u32, b'<' as u32),
        60 => (b'.' as u32, b'>' as u32),
        61 => (b'/' as u32, b'?' as u32),
        62 => (0xffe2, 0xffe2), // Shift_R
        65 => (b' ' as u32, b' ' as u32),
        66 => (0xffe5, 0xffe5),  // Caps_Lock
        37 => (0xffe3, 0xffe3),  // Control_L
        64 => (0xffe9, 0xffe9),  // Alt_L
        105 => (0xffe4, 0xffe4), // Control_R
        108 => (0xffea, 0xffea), // Alt_R
        113 => (0xff51, 0xff51), // Left
        114 => (0xff53, 0xff53), // Right
        111 => (0xff52, 0xff52), // Up
        116 => (0xff54, 0xff54), // Down
        119 => (0xffff, 0xffff), // Delete
        133 => (0xffeb, 0xffeb), // Super_L
        134 => (0xffec, 0xffec), // Super_R
        _ => (0, 0),
    }
}

pub fn write_query_font_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    info: FontInfo,
) -> io::Result<()> {
    let mut reply = fixed_reply(sequence, 0, 7);
    write_char_info(&mut reply, info.min_bounds_width, info.ascent, info.descent);
    reply.extend_from_slice(&[0; 4]); // min-bounds padding
    write_char_info(&mut reply, info.max_bounds_width, info.ascent, info.descent);
    reply.extend_from_slice(&[0; 4]); // max-bounds padding
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 1); // min-char-or-byte2
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 255); // max-char-or-byte2
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0); // default-char
    write_u16(ClientByteOrder::LittleEndian, &mut reply, 0); // properties
    reply.push(0); // draw-direction LeftToRight
    reply.push(0); // min-byte1
    reply.push(0); // max-byte1
    reply.push(0); // all-chars-exist
    write_i16(ClientByteOrder::LittleEndian, &mut reply, info.ascent);
    write_i16(ClientByteOrder::LittleEndian, &mut reply, info.descent);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, 0); // char-infos
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
    reply.extend_from_slice(&[50, 66, 37, 64, 0, 0, 0, 133]);
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

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_i16_le(bytes: &[u8]) -> i16 {
    i16::from_le_bytes([bytes[0], bytes[1]])
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

fn write_char_info(out: &mut Vec<u8>, width: i16, ascent: i16, descent: i16) {
    write_i16(ClientByteOrder::LittleEndian, out, 0); // left-side-bearing
    write_i16(ClientByteOrder::LittleEndian, out, width); // right-side-bearing
    write_i16(ClientByteOrder::LittleEndian, out, width); // character-width
    write_i16(ClientByteOrder::LittleEndian, out, ascent);
    write_i16(ClientByteOrder::LittleEndian, out, descent);
    write_u16(ClientByteOrder::LittleEndian, out, 0); // attributes
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
