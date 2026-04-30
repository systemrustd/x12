use super::SequenceNumber;

// ── Version ──────────────────────────────────────────────────────────────────

pub const MAJOR_VERSION: u32 = 1;
pub const MINOR_VERSION: u32 = 5;

// ── Minor opcode constants ────────────────────────────────────────────────────

pub const RR_QUERY_VERSION: u8 = 0;
pub const RR_SET_SCREEN_CONFIG: u8 = 2;
pub const RR_SELECT_INPUT: u8 = 4;
pub const RR_GET_SCREEN_INFO: u8 = 5;
pub const RR_GET_SCREEN_SIZE_RANGE: u8 = 6;
pub const RR_GET_SCREEN_RESOURCES: u8 = 8;
pub const RR_GET_OUTPUT_INFO: u8 = 9;
pub const RR_LIST_OUTPUT_PROPERTIES: u8 = 10;
pub const RR_GET_OUTPUT_PROPERTY: u8 = 15;
pub const RR_GET_CRTC_INFO: u8 = 20;
pub const RR_SET_CRTC_CONFIG: u8 = 21;
pub const RR_GET_CRTC_GAMMA_SIZE: u8 = 22;
pub const RR_GET_CRTC_GAMMA: u8 = 23;
pub const RR_GET_SCREEN_RESOURCES_CURRENT: u8 = 25;
pub const RR_GET_CRTC_TRANSFORM: u8 = 27;
pub const RR_GET_PANNING: u8 = 28;
pub const RR_GET_OUTPUT_PRIMARY: u8 = 31;
pub const RR_GET_PROVIDERS: u8 = 32;
pub const RR_GET_MONITORS: u8 = 42;

// ── Local wire helpers (mirrors of wire.rs helpers, private to this module) ──

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

/// Round `len` up to the nearest multiple of 4.
fn pad4(len: usize) -> usize {
    (len + 3) & !3
}

/// Pad `out` with zero bytes until its length is a multiple of 4.
fn pad_vec4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// Create the standard 8-byte prefix for an X11 reply:
/// `[1, data, seq_lo, seq_hi, length_bytes…]` (little-endian u32 `length`).
fn fixed_reply(sequence: SequenceNumber, data: u8, length: u32) -> Vec<u8> {
    let mut reply = Vec::with_capacity(32);
    reply.push(1u8); // reply type
    reply.push(data);
    reply.extend_from_slice(&sequence.0.to_le_bytes());
    reply.extend_from_slice(&length.to_le_bytes());
    reply
}

// ── Request structs ───────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub struct QueryVersionRequest {
    pub major: u32,
    pub minor: u32,
}

#[derive(Debug, PartialEq)]
pub struct ScreenRequest {
    pub window: u32,
}

#[derive(Debug, PartialEq)]
pub struct OutputRequest {
    pub output: u32,
    pub config_timestamp: u32,
}

#[derive(Debug, PartialEq)]
pub struct CrtcRequest {
    pub crtc: u32,
    pub config_timestamp: u32,
}

#[derive(Debug, PartialEq)]
pub struct SelectInputRequest {
    pub window: u32,
    pub enable: u16,
}

// ── Request parsers ───────────────────────────────────────────────────────────

pub fn parse_query_version(body: &[u8]) -> Option<QueryVersionRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(QueryVersionRequest {
        major: read_u32_le(body),
        minor: read_u32_le(&body[4..]),
    })
}

pub fn parse_screen_request(body: &[u8]) -> Option<ScreenRequest> {
    if body.len() < 4 {
        return None;
    }
    Some(ScreenRequest {
        window: read_u32_le(body),
    })
}

pub fn parse_output_request(body: &[u8]) -> Option<OutputRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(OutputRequest {
        output: read_u32_le(body),
        config_timestamp: read_u32_le(&body[4..]),
    })
}

pub fn parse_crtc_request(body: &[u8]) -> Option<CrtcRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(CrtcRequest {
        crtc: read_u32_le(body),
        config_timestamp: read_u32_le(&body[4..]),
    })
}

pub fn parse_select_input(body: &[u8]) -> Option<SelectInputRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(SelectInputRequest {
        window: read_u32_le(body),
        enable: read_u16_le(&body[4..]),
        // bytes 6-7: padding, ignored
    })
}

// ── Reply data structs ────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ModeInfo {
    pub id: u32,
    pub width: u16,
    pub height: u16,
    pub dot_clock: u32,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    /// Length of the mode name in bytes.
    pub name_len: u16,
    pub mode_flags: u32,
}

#[derive(Debug)]
pub struct ScreenResources {
    pub timestamp: u32,
    pub config_timestamp: u32,
    pub crtcs: Vec<u32>,
    pub outputs: Vec<u32>,
    pub modes: Vec<ModeInfo>,
    /// Concatenated mode name bytes.
    pub mode_names: Vec<u8>,
}

// ── Reply encoders ────────────────────────────────────────────────────────────

/// Encodes a `QueryVersion` reply (32 bytes total).
pub fn encode_query_version_reply(sequence: SequenceNumber, major: u32, minor: u32) -> Vec<u8> {
    let mut out = fixed_reply(sequence, 0, 0);
    // out is now 8 bytes; add major + minor (8 bytes) then pad to 32
    out.extend_from_slice(&major.to_le_bytes());
    out.extend_from_slice(&minor.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encodes a `GetScreenSizeRange` reply (32 bytes total).
pub fn encode_get_screen_size_range_reply(
    sequence: SequenceNumber,
    min_width: u16,
    min_height: u16,
    max_width: u16,
    max_height: u16,
) -> Vec<u8> {
    let mut out = fixed_reply(sequence, 0, 0);
    out.extend_from_slice(&min_width.to_le_bytes());
    out.extend_from_slice(&min_height.to_le_bytes());
    out.extend_from_slice(&max_width.to_le_bytes());
    out.extend_from_slice(&max_height.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encodes a `GetScreenResourcesCurrent` reply.
pub fn encode_get_screen_resources_current_reply(
    sequence: SequenceNumber,
    resources: &ScreenResources,
) -> Vec<u8> {
    let num_crtcs = resources.crtcs.len();
    let num_outputs = resources.outputs.len();
    let num_modes = resources.modes.len();
    let names_len = resources.mode_names.len();
    let names_padded = pad4(names_len);

    // Extra bytes after the 32-byte header
    let extra = num_crtcs * 4 + num_outputs * 4 + num_modes * 32 + names_padded;
    #[allow(clippy::cast_possible_truncation)]
    let length = (extra / 4) as u32;

    let mut out = fixed_reply(sequence, 0, length);
    // bytes 8-11: timestamp
    out.extend_from_slice(&resources.timestamp.to_le_bytes());
    // bytes 12-15: config_timestamp
    out.extend_from_slice(&resources.config_timestamp.to_le_bytes());
    // bytes 16-17: num_crtcs
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(num_crtcs as u16).to_le_bytes());
    // bytes 18-19: num_outputs
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(num_outputs as u16).to_le_bytes());
    // bytes 20-21: num_modes
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(num_modes as u16).to_le_bytes());
    // bytes 22-23: names_len
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(names_len as u16).to_le_bytes());
    // bytes 24-31: 8 bytes padding
    out.extend_from_slice(&[0u8; 8]);

    // crtcs array
    for &crtc in &resources.crtcs {
        out.extend_from_slice(&crtc.to_le_bytes());
    }
    // outputs array
    for &output in &resources.outputs {
        out.extend_from_slice(&output.to_le_bytes());
    }
    // mode info structs (xRRModeInfo, each 32 bytes)
    for mode in &resources.modes {
        out.extend_from_slice(&mode.id.to_le_bytes());
        out.extend_from_slice(&mode.width.to_le_bytes());
        out.extend_from_slice(&mode.height.to_le_bytes());
        out.extend_from_slice(&mode.dot_clock.to_le_bytes());
        out.extend_from_slice(&mode.hsync_start.to_le_bytes());
        out.extend_from_slice(&mode.hsync_end.to_le_bytes());
        out.extend_from_slice(&mode.htotal.to_le_bytes());
        out.extend_from_slice(&mode.hskew.to_le_bytes());
        out.extend_from_slice(&mode.vsync_start.to_le_bytes());
        out.extend_from_slice(&mode.vsync_end.to_le_bytes());
        out.extend_from_slice(&mode.vtotal.to_le_bytes());
        out.extend_from_slice(&mode.name_len.to_le_bytes());
        out.extend_from_slice(&mode.mode_flags.to_le_bytes());
    }
    // mode names (padded to 4)
    out.extend_from_slice(&resources.mode_names);
    pad_vec4(&mut out);

    out
}

/// Parameters for encoding a `GetOutputInfo` reply.
pub struct OutputInfoReply<'a> {
    pub timestamp: u32,
    /// CRTC currently driving this output (0 if none).
    pub crtc: u32,
    pub width_mm: u32,
    pub height_mm: u32,
    /// 0 = Connected, 1 = Disconnected, 2 = Unknown.
    pub connection: u8,
    pub subpixel_order: u8,
    pub crtcs: &'a [u32],
    pub modes: &'a [u32],
    pub clones: &'a [u32],
    pub name: &'a [u8],
}

/// Encodes a `GetOutputInfo` reply.
pub fn encode_get_output_info_reply(
    sequence: SequenceNumber,
    info: &OutputInfoReply<'_>,
) -> Vec<u8> {
    let timestamp = info.timestamp;
    let crtc = info.crtc;
    let width_mm = info.width_mm;
    let height_mm = info.height_mm;
    let connection = info.connection;
    let subpixel_order = info.subpixel_order;
    let crtcs = info.crtcs;
    let modes = info.modes;
    let clones = info.clones;
    let name = info.name;
    let num_crtcs = crtcs.len();
    let num_modes = modes.len();
    let num_clones = clones.len();
    let name_len = name.len();
    let name_padded = pad4(name_len);

    // xRRGetOutputInfoReply (sz=36): connection and subpixelOrder are CARD8 (1 byte each).
    // bytes 24-25: u8+u8, then nCrtcs at 26, nModes at 28, nPreferred at 30 (all in first 32 bytes),
    // nClones at 32, nameLen at 34 (the one extra 4-byte word beyond byte 31).
    // Arrays start at byte 36 (4-byte aligned, no pad needed).
    // length = (4 + crtcs*4 + modes*4 + clones*4 + pad4(name)) / 4
    let extra = 4 + num_crtcs * 4 + num_modes * 4 + num_clones * 4 + name_padded;
    #[allow(clippy::cast_possible_truncation)]
    let length = (extra / 4) as u32;

    let mut out = fixed_reply(sequence, 0, length);
    // bytes 8-11: timestamp
    out.extend_from_slice(&timestamp.to_le_bytes());
    // bytes 12-15: crtc
    out.extend_from_slice(&crtc.to_le_bytes());
    // bytes 16-19: mm_width
    out.extend_from_slice(&width_mm.to_le_bytes());
    // bytes 20-23: mm_height
    out.extend_from_slice(&height_mm.to_le_bytes());
    // byte 24: connection (CARD8)
    out.push(connection);
    // byte 25: subpixel_order (CARD8)
    out.push(subpixel_order);
    // bytes 26-27: num_crtcs
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(num_crtcs as u16).to_le_bytes());
    // bytes 28-29: num_modes
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(num_modes as u16).to_le_bytes());
    // bytes 30-31: num_preferred (all modes are preferred in this stub)
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(num_modes as u16).to_le_bytes());
    // bytes 32-33: num_clones  (extra word read by _XReply with extra=1)
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(num_clones as u16).to_le_bytes());
    // bytes 34-35: name_len
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(name_len as u16).to_le_bytes());
    // no pad: byte 36 is 4-byte aligned, arrays follow immediately

    // crtcs
    for &c in crtcs {
        out.extend_from_slice(&c.to_le_bytes());
    }
    // modes
    for &m in modes {
        out.extend_from_slice(&m.to_le_bytes());
    }
    // clones
    for &cl in clones {
        out.extend_from_slice(&cl.to_le_bytes());
    }
    // name (padded to 4)
    out.extend_from_slice(name);
    pad_vec4(&mut out);

    out
}

/// Parameters for encoding a `GetCrtcInfo` reply.
pub struct CrtcInfoReply<'a> {
    pub timestamp: u32,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    /// Active mode ID (0 if CRTC is disabled).
    pub mode: u32,
    pub rotation: u16,
    pub rotations: u16,
    pub outputs: &'a [u32],
    pub possible: &'a [u32],
}

/// Encodes a `GetCrtcInfo` reply.
pub fn encode_get_crtc_info_reply(sequence: SequenceNumber, info: &CrtcInfoReply<'_>) -> Vec<u8> {
    let timestamp = info.timestamp;
    let x = info.x;
    let y = info.y;
    let width = info.width;
    let height = info.height;
    let mode = info.mode;
    let rotation = info.rotation;
    let rotations = info.rotations;
    let outputs = info.outputs;
    let possible = info.possible;
    let num_outputs = outputs.len();
    let num_possible = possible.len();

    // Extra bytes after 32-byte header
    let extra = num_outputs * 4 + num_possible * 4;
    #[allow(clippy::cast_possible_truncation)]
    let length = (extra / 4) as u32;

    let mut out = fixed_reply(sequence, 0, length);
    // bytes 8-11: timestamp
    out.extend_from_slice(&timestamp.to_le_bytes());
    // bytes 12-13: x (i16)
    out.extend_from_slice(&x.to_le_bytes());
    // bytes 14-15: y (i16)
    out.extend_from_slice(&y.to_le_bytes());
    // bytes 16-17: width
    out.extend_from_slice(&width.to_le_bytes());
    // bytes 18-19: height
    out.extend_from_slice(&height.to_le_bytes());
    // bytes 20-23: mode
    out.extend_from_slice(&mode.to_le_bytes());
    // bytes 24-25: rotation
    out.extend_from_slice(&rotation.to_le_bytes());
    // bytes 26-27: rotations
    out.extend_from_slice(&rotations.to_le_bytes());
    // bytes 28-29: num_outputs
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(num_outputs as u16).to_le_bytes());
    // bytes 30-31: num_possible
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(num_possible as u16).to_le_bytes());

    // outputs
    for &o in outputs {
        out.extend_from_slice(&o.to_le_bytes());
    }
    // possible outputs
    for &p in possible {
        out.extend_from_slice(&p.to_le_bytes());
    }

    out
}

/// Encodes a `GetCrtcTransform` reply (96 bytes) with identity transforms and no filter.
///
/// Wire layout: standard 8-byte header + pendingTransform(36) + hasTransforms(1)+pad(3) +
/// currentTransform(36) + pad(4) + four u16 filter-length fields.
/// Identity matrix in 16.16 fixed-point: diagonal = 0x0001_0000, off-diagonal = 0.
pub fn encode_get_crtc_transform_reply(sequence: SequenceNumber) -> Vec<u8> {
    const IDENTITY: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x0001_0000];
    let mut out = fixed_reply(sequence, 0, 16); // 64 extra bytes = 16 CARD32s
    for &v in &IDENTITY {
        out.extend_from_slice(&v.to_le_bytes()); // bytes 8-43: pendingTransform
    }
    out.push(0); // byte 44: hasTransforms = false
    out.extend_from_slice(&[0u8; 3]); // bytes 45-47: pad
    for &v in &IDENTITY {
        out.extend_from_slice(&v.to_le_bytes()); // bytes 48-83: currentTransform
    }
    out.extend_from_slice(&[0u8; 4]); // bytes 84-87: pad
    out.extend_from_slice(&[0u8; 8]); // bytes 88-95: four u16 filter lengths (all 0)
    debug_assert_eq!(out.len(), 96);
    out
}

/// Encodes a `ListOutputProperties` reply with zero properties (32 bytes).
pub fn encode_list_output_properties_reply(sequence: SequenceNumber) -> Vec<u8> {
    let mut out = fixed_reply(sequence, 0, 0);
    out.extend_from_slice(&[0u8; 24]); // nAtoms=0 + pad
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encodes a `GetOutputProperty` reply indicating the property does not exist (format=0,
/// type=None, bytes_after=0, num_items=0, no data).
pub fn encode_get_output_property_reply(sequence: SequenceNumber) -> Vec<u8> {
    let mut out = fixed_reply(sequence, 0 /* format=0 */, 0);
    out.extend_from_slice(&[0u8; 24]); // type=None(4) + bytes_after=0(4) + num_items=0(4) + pad(12)
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encodes a `GetPanning` reply (36 bytes) with all-zero panning (no panning configured).
///
/// Wire layout: `status(1) seq(2) length=1(4) timestamp(4) left top width height
/// trackLeft trackTop trackWidth trackHeight borderLeft borderTop borderRight borderBottom`
/// (each of the 12 panning fields is u16/i16).
pub fn encode_get_panning_reply(sequence: SequenceNumber, timestamp: u32) -> Vec<u8> {
    let mut out = fixed_reply(sequence, 0 /* status=Success */, 1);
    out.extend_from_slice(&timestamp.to_le_bytes()); // bytes 8-11
    out.extend_from_slice(&[0u8; 24]); // 12 × u16 fields, all zero
    debug_assert_eq!(out.len(), 36);
    out
}

/// Encodes a `GetOutputPrimary` reply (32 bytes), returning no primary output.
pub fn encode_get_output_primary_reply(sequence: SequenceNumber, output: u32) -> Vec<u8> {
    let mut out = fixed_reply(sequence, 0, 0);
    out.extend_from_slice(&output.to_le_bytes()); // bytes 8-11: primary output XID (0 = none)
    out.extend_from_slice(&[0u8; 20]); // pad
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encodes a `GetProviders` reply (32 bytes) with zero providers.
pub fn encode_get_providers_reply(sequence: SequenceNumber, timestamp: u32) -> Vec<u8> {
    let mut out = fixed_reply(sequence, 0, 0);
    out.extend_from_slice(&timestamp.to_le_bytes()); // bytes 8-11
    out.extend_from_slice(&[0u8; 20]); // nProviders=0 + pad
    debug_assert_eq!(out.len(), 32);
    out
}

/// One monitor descriptor inside a `GetMonitors` reply.
pub struct MonitorInfo<'a> {
    /// Atom ID for the monitor name (0 = anonymous).
    pub name: u32,
    pub primary: bool,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub width_mm: u32,
    pub height_mm: u32,
    pub outputs: &'a [u32],
}

/// Encodes a `GetMonitors` reply (RANDR 1.5).
///
/// Wire format per monitor (`xRRMonitorInfo`, 24 bytes fixed + 4*nOutput):
/// `name(4) primary(1) automatic(1) nOutput(2) x(2) y(2) width(2) height(2)
///  widthMM(4) heightMM(4)` followed by output XIDs.
pub fn encode_get_monitors_reply(
    sequence: SequenceNumber,
    timestamp: u32,
    monitors: &[MonitorInfo<'_>],
) -> Vec<u8> {
    let n_monitors = monitors.len();
    let n_outputs: usize = monitors.iter().map(|m| m.outputs.len()).sum();

    // Total extra bytes after the 32-byte header.
    let extra: usize = monitors.iter().map(|m| 24 + m.outputs.len() * 4).sum();
    #[allow(clippy::cast_possible_truncation)]
    let length = (extra / 4) as u32;

    let mut out = fixed_reply(sequence, 0, length);
    // bytes 8-11: timestamp
    out.extend_from_slice(&timestamp.to_le_bytes());
    // bytes 12-15: nMonitors
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(n_monitors as u32).to_le_bytes());
    // bytes 16-19: nOutputs (total across all monitors)
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(n_outputs as u32).to_le_bytes());
    // bytes 20-31: pad
    out.extend_from_slice(&[0u8; 12]);
    debug_assert_eq!(out.len(), 32);

    for m in monitors {
        #[allow(clippy::cast_possible_truncation)]
        let n_out = m.outputs.len() as u16;
        out.extend_from_slice(&m.name.to_le_bytes()); // 4: name (Atom)
        out.push(u8::from(m.primary)); // 1: primary
        out.push(0); // 1: automatic = false
        out.extend_from_slice(&n_out.to_le_bytes()); // 2: nOutput
        out.extend_from_slice(&m.x.to_le_bytes()); // 2: x
        out.extend_from_slice(&m.y.to_le_bytes()); // 2: y
        out.extend_from_slice(&m.width.to_le_bytes()); // 2: width
        out.extend_from_slice(&m.height.to_le_bytes()); // 2: height
        out.extend_from_slice(&m.width_mm.to_le_bytes()); // 4: widthInMillimeters
        out.extend_from_slice(&m.height_mm.to_le_bytes()); // 4: heightInMillimeters
        for &oid in m.outputs {
            out.extend_from_slice(&oid.to_le_bytes());
        }
    }

    out
}

/// Encodes a `GetCrtcGammaSize` reply (32 bytes, `size` = 0 means no gamma support).
pub fn encode_get_crtc_gamma_size_reply(sequence: SequenceNumber, size: u16) -> Vec<u8> {
    let mut out = fixed_reply(sequence, 0, 0);
    out.extend_from_slice(&size.to_le_bytes()); // bytes 8-9: size
    out.extend_from_slice(&[0u8; 22]); // bytes 10-31: pad
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encodes a `GetCrtcGamma` reply (32 bytes when `size` = 0; no gamma arrays).
pub fn encode_get_crtc_gamma_reply(sequence: SequenceNumber, size: u16) -> Vec<u8> {
    // When size=0 the three channel arrays are empty, so length=0.
    // Xlib reads the size field from the fixed header to know array length.
    let mut out = fixed_reply(sequence, 0, 0);
    out.extend_from_slice(&size.to_le_bytes()); // bytes 8-9: size
    out.extend_from_slice(&[0u8; 22]); // bytes 10-31: pad
    debug_assert_eq!(out.len(), 32);
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Parser tests ──────────────────────────────────────────────────────────

    #[test]
    fn parse_query_version_short_body_returns_none() {
        assert!(parse_query_version(&[]).is_none());
        assert!(parse_query_version(&[0u8; 7]).is_none());
    }

    #[test]
    fn parse_query_version_round_trip() {
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes()); // major
        body.extend_from_slice(&2u32.to_le_bytes()); // minor
        let req = parse_query_version(&body).unwrap();
        assert_eq!(req, QueryVersionRequest { major: 1, minor: 2 });
    }

    #[test]
    fn parse_output_request_round_trip() {
        let mut body = Vec::new();
        body.extend_from_slice(&42u32.to_le_bytes()); // output
        body.extend_from_slice(&1000u32.to_le_bytes()); // config_timestamp
        let req = parse_output_request(&body).unwrap();
        assert_eq!(
            req,
            OutputRequest {
                output: 42,
                config_timestamp: 1000,
            }
        );
    }

    #[test]
    fn parse_output_request_short_body_returns_none() {
        assert!(parse_output_request(&[]).is_none());
        assert!(parse_output_request(&[0u8; 7]).is_none());
    }

    // ── Reply size tests ──────────────────────────────────────────────────────

    #[test]
    fn encode_query_version_reply_shape() {
        let buf = encode_query_version_reply(SequenceNumber(0xABCD), 1, 2);
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 1); // reply code
        assert_eq!(&buf[2..4], &0xABCDu16.to_le_bytes()); // sequence
        assert_eq!(&buf[8..12], &1u32.to_le_bytes()); // major
        assert_eq!(&buf[12..16], &2u32.to_le_bytes()); // minor
    }

    #[test]
    fn encode_get_screen_size_range_reply_shape() {
        let buf = encode_get_screen_size_range_reply(SequenceNumber(1), 320, 240, 3840, 2160);
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 1);
        assert_eq!(&buf[8..10], &320u16.to_le_bytes());
        assert_eq!(&buf[10..12], &240u16.to_le_bytes());
        assert_eq!(&buf[12..14], &3840u16.to_le_bytes());
        assert_eq!(&buf[14..16], &2160u16.to_le_bytes());
    }

    #[test]
    fn encode_get_screen_resources_current_reply_shape() {
        let mode_name = b"800x600";
        let resources = ScreenResources {
            timestamp: 100,
            config_timestamp: 200,
            crtcs: vec![0x10],
            outputs: vec![0x20],
            modes: vec![ModeInfo {
                id: 1,
                width: 800,
                height: 600,
                dot_clock: 40_000_000,
                hsync_start: 840,
                hsync_end: 968,
                htotal: 1056,
                hskew: 0,
                vsync_start: 601,
                vsync_end: 605,
                vtotal: 628,
                name_len: mode_name.len() as u16,
                mode_flags: 0,
            }],
            mode_names: mode_name.to_vec(),
        };

        let buf = encode_get_screen_resources_current_reply(SequenceNumber(5), &resources);

        // 32 header + 4 (1 crtc) + 4 (1 output) + 32 (1 mode info) + 8 ("800x600" = 7 bytes, padded to 8)
        let expected_len = 32 + 4 + 4 + 32 + 8;
        assert_eq!(buf.len(), expected_len);
        assert_eq!(buf[0], 1);
        // length field in 4-byte units after first 32 bytes
        let length_field = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(length_field, ((expected_len - 32) / 4) as u32);
        // timestamp
        assert_eq!(&buf[8..12], &100u32.to_le_bytes());
        // config_timestamp
        assert_eq!(&buf[12..16], &200u32.to_le_bytes());
        // num_crtcs
        assert_eq!(&buf[16..18], &1u16.to_le_bytes());
        // num_outputs
        assert_eq!(&buf[18..20], &1u16.to_le_bytes());
        // num_modes
        assert_eq!(&buf[20..22], &1u16.to_le_bytes());
    }

    #[test]
    fn encode_get_output_info_reply_shape() {
        let crtcs = [2u32];
        let modes = [3u32];
        let name = b"ynest-0";
        let buf = encode_get_output_info_reply(
            SequenceNumber(7),
            &OutputInfoReply {
                timestamp: 42,
                crtc: 2,
                width_mm: 211,
                height_mm: 158,
                connection: 0,
                subpixel_order: 0,
                crtcs: &crtcs,
                modes: &modes,
                clones: &[],
                name,
            },
        );
        // 32 header + 4 (nClones+nameLen extra word) + 4 (1 crtc) + 4 (1 mode) + 8 (7-byte name padded to 8) = 52
        let expected_len = 32 + 4 + 4 + 4 + 8;
        assert_eq!(buf.len(), expected_len);
        assert_eq!(buf[0], 1);
        let length_field = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(length_field, ((expected_len - 32) / 4) as u32);
        // crtc XID in fixed header at bytes 12-15
        assert_eq!(&buf[12..16], &2u32.to_le_bytes());
        // connection (CARD8) at byte 24, subpixelOrder at byte 25
        assert_eq!(buf[24], 0u8); // Connected
        assert_eq!(buf[25], 0u8); // subpixel Unknown
        // num_crtcs at bytes 26-27
        assert_eq!(&buf[26..28], &1u16.to_le_bytes());
        // num_clones at bytes 32-33
        assert_eq!(&buf[32..34], &0u16.to_le_bytes());
        // name_len at bytes 34-35
        assert_eq!(&buf[34..36], &7u16.to_le_bytes());
        // CRTCs array starts at byte 36
        assert_eq!(&buf[36..40], &2u32.to_le_bytes());
        // modes array at byte 40
        assert_eq!(&buf[40..44], &3u32.to_le_bytes());
        // name at byte 44
        assert_eq!(&buf[44..51], b"ynest-0");
    }
}
