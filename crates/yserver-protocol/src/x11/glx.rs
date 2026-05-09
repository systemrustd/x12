//! GLX wire-protocol decoders/encoders (Phase 4.2.5).
//!
//! Server-side GLX scope is *identification + bookkeeping*. libGLX_mesa
//! loads, hits these requests, gets enough metadata to identify our
//! DRI3 backend, then renders client-side direct-to-GPU. Indirect
//! rendering (server-side GL execution) is explicitly out of scope —
//! design §1 "Out of scope".
//!
//! Opcode numbering follows `glxproto.h`. Indirect-rendering opcodes
//! 1..=198 are stubbed with `GLXBadRequest` at the dispatcher layer.

use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_u16, write_u32},
};

// Identification + setup (always-handled minor opcodes).
pub const RENDER: u8 = 1;
pub const QUERY_VERSION: u8 = 7;
pub const WAIT_GL: u8 = 8;
pub const WAIT_X: u8 = 9;
pub const QUERY_SERVER_STRING: u8 = 19;
pub const CLIENT_INFO: u8 = 20;

// Vendor-private dispatch (legacy).
pub const VENDOR_PRIVATE: u8 = 16;
pub const VENDOR_PRIVATE_WITH_REPLY: u8 = 17;

// Visual / FBConfig enumeration.
pub const GET_VISUAL_CONFIGS: u8 = 14;
pub const GET_FB_CONFIGS: u8 = 21;

// Context lifecycle.
pub const CREATE_CONTEXT: u8 = 3;
pub const DESTROY_CONTEXT: u8 = 4;
pub const MAKE_CURRENT: u8 = 5;
pub const IS_DIRECT: u8 = 6;
pub const COPY_CONTEXT: u8 = 11;
pub const SWAP_BUFFERS: u8 = 11; // alias used pre-1.3; clients can use either form
pub const CREATE_NEW_CONTEXT: u8 = 24;
pub const CREATE_CONTEXT_ATTRIBS_ARB: u8 = 34; // GLX_ARB_create_context
pub const QUERY_EXTENSIONS_STRING: u8 = 18;
pub const QUERY_CONTEXT: u8 = 25;
pub const MAKE_CONTEXT_CURRENT: u8 = 26;
pub const SET_CLIENT_INFO_ARB: u8 = 33;
pub const SET_CLIENT_INFO_2_ARB: u8 = 35;

pub const MAJOR_VERSION: u32 = 1;
pub const MINOR_VERSION: u32 = 4;

/// `GLXBadRequest` error code for indirect-rendering opcodes that we
/// don't implement. Per glxproto: error codes start at the extension's
/// `first_error` allocation; the dispatcher resolves the absolute
/// number from `nested.rs::GLX_FIRST_ERROR`.
pub const ERROR_GLX_BAD_REQUEST: u8 = 0;
pub const ERROR_GLX_BAD_CONTEXT: u8 = 1;
pub const ERROR_GLX_UNSUPPORTED_PRIVATE_REQUEST: u8 = 11;

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[must_use]
pub fn parse_query_version(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..])))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryServerStringRequest {
    pub screen: u32,
    pub name: u32,
}

/// `GLX_VENDOR` / `GLX_VERSION` / `GLX_EXTENSIONS` constants per
/// `glxtokens.h`. Used by `QueryServerString` to identify which
/// string the client wants.
pub const STRING_VENDOR: u32 = 1;
pub const STRING_VERSION: u32 = 2;
pub const STRING_EXTENSIONS: u32 = 3;

#[must_use]
pub fn parse_query_server_string(body: &[u8]) -> Option<QueryServerStringRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(QueryServerStringRequest {
        screen: read_u32_le(body),
        name: read_u32_le(&body[4..]),
    })
}

#[must_use]
pub fn encode_query_version_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major: u32,
    minor: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    write_u32(byte_order, &mut out, major);
    write_u32(byte_order, &mut out, minor);
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encode a string-list reply (`QueryServerString` /
/// `QueryExtensionsString`). Layout per glxproto:
///
/// ```text
/// 1   Reply
/// 1   pad
/// 2   sequence
/// 4   length (4-byte units past the 32-byte header)
/// 4   string length n (bytes)
/// 16  pad
/// n   string bytes (null-terminated, padded to 4-byte boundary)
/// ```
#[must_use]
pub fn encode_string_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    string: &str,
) -> Vec<u8> {
    let bytes = string.as_bytes();
    let n = bytes.len() + 1; // include null terminator
    let padded = n.div_ceil(4) * 4;
    let length_units = u32::try_from(padded / 4).unwrap_or(0);
    let mut out = Vec::with_capacity(32 + padded);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length_units);
    write_u32(byte_order, &mut out, u32::try_from(n).unwrap_or(0));
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out.extend_from_slice(bytes);
    out.push(0); // null terminator
    while out.len() < 32 + padded {
        out.push(0);
    }
    out
}

/// Encode an `IsDirect` reply.
#[must_use]
pub fn encode_is_direct_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    is_direct: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    out.push(u8::from(is_direct));
    out.extend_from_slice(&[0u8; 23]);
    debug_assert_eq!(out.len(), 32);
    out
}

// GLX FBConfig attribute IDs per `glxtokens.h`. Used as the keys in
// the (attrib, value) pairs that follow the `GetFBConfigsReply`
// header.
pub const GLX_BUFFER_SIZE: u32 = 2;
pub const GLX_LEVEL: u32 = 3;
pub const GLX_DOUBLEBUFFER: u32 = 5;
pub const GLX_STEREO: u32 = 6;
pub const GLX_AUX_BUFFERS: u32 = 7;
pub const GLX_RED_SIZE: u32 = 8;
pub const GLX_GREEN_SIZE: u32 = 9;
pub const GLX_BLUE_SIZE: u32 = 10;
pub const GLX_ALPHA_SIZE: u32 = 11;
pub const GLX_DEPTH_SIZE: u32 = 12;
pub const GLX_STENCIL_SIZE: u32 = 13;
pub const GLX_ACCUM_RED_SIZE: u32 = 14;
pub const GLX_ACCUM_GREEN_SIZE: u32 = 15;
pub const GLX_ACCUM_BLUE_SIZE: u32 = 16;
pub const GLX_ACCUM_ALPHA_SIZE: u32 = 17;

pub const GLX_CONFIG_CAVEAT: u32 = 0x20;
pub const GLX_X_VISUAL_TYPE: u32 = 0x22;
pub const GLX_TRANSPARENT_TYPE: u32 = 0x23;

pub const GLX_VISUAL_ID: u32 = 0x800B;
pub const GLX_DRAWABLE_TYPE: u32 = 0x8010;
pub const GLX_RENDER_TYPE: u32 = 0x8011;
pub const GLX_X_RENDERABLE: u32 = 0x8012;
pub const GLX_FBCONFIG_ID: u32 = 0x8013;

pub const GLX_SAMPLE_BUFFERS: u32 = 100_000;
pub const GLX_SAMPLES: u32 = 100_001;

// Values
pub const GLX_TRUE_COLOR: u32 = 0x8002;
pub const GLX_NONE: u32 = 0x8000;
pub const GLX_WINDOW_BIT: u32 = 0x1;
pub const GLX_PIXMAP_BIT: u32 = 0x2;
pub const GLX_RGBA_BIT: u32 = 0x1;

/// Encode `GetFBConfigsReply`. `configs` is a slice of (attrib,
/// value) pair lists — one list per FBConfig. All lists must have
/// the same length (`num_properties` is shared in the wire format).
///
/// Layout per glxproto `GetFBConfigsReply`:
///
/// ```text
/// 1   Reply
/// 1   pad
/// 2   sequence
/// 4   length = num_FB_configs * num_properties * 2 (in 4-byte units)
/// 4   num_FB_configs
/// 4   num_properties
/// 16  pad
/// (num_FB_configs * num_properties) (CARD32 attrib, CARD32 value) pairs
/// ```
///
/// Falls back to `encode_get_fb_configs_empty_reply` when `configs`
/// is empty.
#[must_use]
pub fn encode_get_fb_configs_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    configs: &[&[(u32, u32)]],
) -> Vec<u8> {
    if configs.is_empty() {
        return encode_get_fb_configs_empty_reply(byte_order, sequence);
    }
    let num_properties = configs[0].len();
    debug_assert!(
        configs.iter().all(|c| c.len() == num_properties),
        "every FBConfig in a GetFBConfigsReply must declare the same property count"
    );
    let num_configs = u32::try_from(configs.len()).unwrap_or(0);
    let num_props = u32::try_from(num_properties).unwrap_or(0);
    let length_units = num_configs.saturating_mul(num_props).saturating_mul(2);
    let mut out = Vec::with_capacity(32 + (length_units as usize) * 4);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length_units);
    write_u32(byte_order, &mut out, num_configs);
    write_u32(byte_order, &mut out, num_props);
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    for config in configs {
        for &(attrib, value) in *config {
            write_u32(byte_order, &mut out, attrib);
            write_u32(byte_order, &mut out, value);
        }
    }
    out
}

/// Encode an empty `GetFBConfigs` reply (zero configs). Used by
/// callers that want to advertise "no FBConfigs" — Mesa survives this
/// by falling back to GetVisualConfigs.
#[must_use]
pub fn encode_get_fb_configs_empty_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    write_u32(byte_order, &mut out, 0); // num_FB_configs
    write_u32(byte_order, &mut out, 0); // num_properties
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcodes_match_glxproto() {
        assert_eq!(QUERY_VERSION, 7);
        assert_eq!(QUERY_SERVER_STRING, 19);
        assert_eq!(QUERY_EXTENSIONS_STRING, 18);
        assert_eq!(IS_DIRECT, 6);
        assert_eq!(MAKE_CURRENT, 5);
        assert_eq!(MAKE_CONTEXT_CURRENT, 26);
        assert_eq!(CREATE_NEW_CONTEXT, 24);
        assert_eq!(CREATE_CONTEXT_ATTRIBS_ARB, 34);
        assert_eq!(SET_CLIENT_INFO_ARB, 33);
        assert_eq!(SET_CLIENT_INFO_2_ARB, 35);
        assert_eq!(GET_FB_CONFIGS, 21);
        assert_eq!(GET_VISUAL_CONFIGS, 14);
    }

    #[test]
    fn query_version_reply_shape() {
        let reply =
            encode_query_version_reply(ClientByteOrder::LittleEndian, SequenceNumber(7), 1, 4);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 4);
    }

    #[test]
    fn string_reply_round_trip() {
        let reply =
            encode_string_reply(ClientByteOrder::LittleEndian, SequenceNumber(3), "yserver");
        // n = 7 + 1 = 8 → padded = 8 → length_units = 2.
        assert_eq!(reply.len(), 32 + 8);
        let length_units = u32::from_le_bytes(reply[4..8].try_into().unwrap());
        assert_eq!(length_units, 2);
        let n = u32::from_le_bytes(reply[8..12].try_into().unwrap());
        assert_eq!(n, 8);
        assert_eq!(&reply[32..39], b"yserver");
        assert_eq!(reply[39], 0);
    }

    #[test]
    fn is_direct_reply_true() {
        let reply = encode_is_direct_reply(ClientByteOrder::LittleEndian, SequenceNumber(2), true);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[8], 1);
    }

    #[test]
    fn empty_fb_configs_reply() {
        let reply =
            encode_get_fb_configs_empty_reply(ClientByteOrder::LittleEndian, SequenceNumber(1));
        assert_eq!(reply.len(), 32);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 0);
    }

    #[test]
    fn fb_configs_reply_layout() {
        let cfg_a: &[(u32, u32)] = &[(GLX_VISUAL_ID, 0x102), (GLX_DOUBLEBUFFER, 1)];
        let cfg_b: &[(u32, u32)] = &[(GLX_VISUAL_ID, 0x103), (GLX_DOUBLEBUFFER, 0)];
        let reply = encode_get_fb_configs_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(2),
            &[cfg_a, cfg_b],
        );
        // Header (32) + 2 configs × 2 props × 8 bytes/pair = 32 → total 64.
        assert_eq!(reply.len(), 64);
        // length field: 2 configs × 2 props × 2 (4-byte units per pair) = 8.
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 8);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(reply[32..36].try_into().unwrap()),
            GLX_VISUAL_ID
        );
        assert_eq!(u32::from_le_bytes(reply[36..40].try_into().unwrap()), 0x102);
    }
}
