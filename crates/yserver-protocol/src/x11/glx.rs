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
pub const SWAP_BUFFERS: u8 = 11;
pub const COPY_CONTEXT: u8 = 10;
pub const CREATE_NEW_CONTEXT: u8 = 24;
pub const CREATE_CONTEXT_ATTRIBS_ARB: u8 = 34; // GLX_ARB_create_context
pub const QUERY_EXTENSIONS_STRING: u8 = 18;
pub const QUERY_CONTEXT: u8 = 25;
pub const MAKE_CONTEXT_CURRENT: u8 = 26;
pub const SET_CLIENT_INFO_ARB: u8 = 33;
pub const SET_CLIENT_INFO_2_ARB: u8 = 35;

// GLX 1.3 drawable lifecycle (CreateGLXWindow / DestroyGLXWindow /
// CreateGLXPbuffer / DestroyGLXPbuffer / GetDrawableAttributes /
// ChangeDrawableAttributes).
pub const CREATE_PIXMAP: u8 = 22;
pub const DESTROY_PIXMAP: u8 = 23;
pub const CREATE_PBUFFER: u8 = 27;
pub const DESTROY_PBUFFER: u8 = 28;
pub const GET_DRAWABLE_ATTRIBUTES: u8 = 29;
pub const CHANGE_DRAWABLE_ATTRIBUTES: u8 = 30;
pub const CREATE_WINDOW: u8 = 31;
pub const DELETE_WINDOW: u8 = 32;

pub const MAJOR_VERSION: u32 = 1;
pub const MINOR_VERSION: u32 = 4;

/// GLX error codes per /usr/share/xcb/glx.xml `errorcopy` entries.
/// Numbers are extension-relative; the dispatcher resolves the
/// absolute code via `nested.rs::GLX_FIRST_ERROR + N`.
///
/// `BadRenderRequest` is the canonical reply for an indirect-rendering
/// minor opcode the server doesn't implement; `UnsupportedPrivateRequest`
/// is for `VendorPrivate`/`VendorPrivateWithReply` we don't handle.
pub const ERROR_GLX_BAD_RENDER_REQUEST: u8 = 6;
pub const ERROR_GLX_UNSUPPORTED_PRIVATE_REQUEST: u8 = 8;

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

/// The GLX extensions yserver advertises. Returned for BOTH
/// `glXQueryServerString(GLX_EXTENSIONS)` (opcode 19, `QUERY_SERVER_STRING`,
/// `STRING_EXTENSIONS`) and `glXQueryExtensionsString` (opcode 18,
/// `QUERY_EXTENSIONS_STRING`). They must stay identical: ANGLE/Chromium
/// queries the *server* string (opcode 19) and refuses to create a GLES
/// context if `GLX_ARB_create_context` is absent — an empty server string
/// there breaks Chromium even though opcode 18 carried the list.
pub const SERVER_EXTENSIONS: &str = "GLX_ARB_create_context GLX_ARB_create_context_profile \
    GLX_EXT_create_context_es2_profile GLX_EXT_buffer_age GLX_EXT_swap_control \
    GLX_INTEL_swap_event GLX_ARB_fbconfig_float GLX_EXT_visual_info \
    GLX_EXT_visual_rating GLX_EXT_import_context";

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
/// `QueryExtensionsString`). Layout per `glxproto.h`
/// `xGLXQueryServerStringReply` (sz=32):
///
/// ```text
/// 1   Reply
/// 1   pad
/// 2   sequence
/// 4   length (4-byte units past the 32-byte header)
/// 4   pad1                         ← reserved
/// 4   n        (string byte count, including \0)
/// 16  pad3..pad6
/// n   string bytes (null-terminated, padded to 4-byte boundary)
/// ```
///
/// The `pad1` slot is load-bearing: it's where Mesa's xcb stub
/// expects four bytes of *padding*, not the string length. Putting
/// `n` at offset 8 instead of 12 is the bug that crashed
/// libepoxy's `epoxy_glx_version` (`sscanf` of the resulting
/// "string" returned ret != 2).
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
    out.extend_from_slice(&[0u8; 4]); // pad1
    write_u32(byte_order, &mut out, u32::try_from(n).unwrap_or(0)); // n
    out.extend_from_slice(&[0u8; 16]); // pad3..pad6
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

/// One GLX 1.2 visual config entry.
///
/// `GetVisualConfigs` (opcode 14) returns the legacy untagged layout:
/// 18 leading `INT32` fields followed by 2*N tagged (attrib, value)
/// pairs. We emit only the 18 leading fields — `num_properties = 18`.
/// Mesa's `__glXInitializeVisualConfigFromTags(!tagged_only)` expects
/// exactly this prefix, in this order.
#[derive(Clone, Copy, Debug)]
pub struct VisualConfig {
    pub visual_id: u32,
    pub visual_class: u32,
    pub rgba: bool,
    pub red_bits: u32,
    pub green_bits: u32,
    pub blue_bits: u32,
    pub alpha_bits: u32,
    pub double_buffer: bool,
    pub stereo: bool,
    pub rgb_bits: u32,
    pub depth_bits: u32,
    pub stencil_bits: u32,
    pub aux_buffers: u32,
    pub level: u32,
}

/// Encode a `GetVisualConfigs` reply. Layout per glxproto:
///
/// ```text
/// 1   Reply (=1)
/// 1   pad
/// 2   sequence
/// 4   length (in 4-byte units beyond 32)
/// 4   numVisuals
/// 4   numProps  (INT32 count per visual)
/// 16  pad
/// numVisuals * numProps INT32 fields
/// ```
///
/// Mesa's `createConfigsFromProperties` rejects `numProps == 0` (and
/// anything less than `__GLX_MIN_CONFIG_PROPS == 18`), so even an
/// "empty" client-visible visual list must ship a non-zero numProps;
/// we just emit zero visuals in that case.
#[must_use]
pub fn encode_get_visual_configs_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    visuals: &[VisualConfig],
) -> Vec<u8> {
    const PROPS_PER_VISUAL: u32 = 18;
    let num_visuals = u32::try_from(visuals.len()).unwrap_or(0);
    let length_units = num_visuals.saturating_mul(PROPS_PER_VISUAL);
    let mut out = Vec::with_capacity(32 + (length_units as usize) * 4);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length_units);
    write_u32(byte_order, &mut out, num_visuals);
    write_u32(byte_order, &mut out, PROPS_PER_VISUAL);
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    for v in visuals {
        write_u32(byte_order, &mut out, v.visual_id);
        write_u32(byte_order, &mut out, v.visual_class);
        write_u32(byte_order, &mut out, u32::from(v.rgba));
        write_u32(byte_order, &mut out, v.red_bits);
        write_u32(byte_order, &mut out, v.green_bits);
        write_u32(byte_order, &mut out, v.blue_bits);
        write_u32(byte_order, &mut out, v.alpha_bits);
        write_u32(byte_order, &mut out, 0); // accum_red_bits
        write_u32(byte_order, &mut out, 0); // accum_green_bits
        write_u32(byte_order, &mut out, 0); // accum_blue_bits
        write_u32(byte_order, &mut out, 0); // accum_alpha_bits
        write_u32(byte_order, &mut out, u32::from(v.double_buffer));
        write_u32(byte_order, &mut out, u32::from(v.stereo));
        write_u32(byte_order, &mut out, v.rgb_bits);
        write_u32(byte_order, &mut out, v.depth_bits);
        write_u32(byte_order, &mut out, v.stencil_bits);
        write_u32(byte_order, &mut out, v.aux_buffers);
        write_u32(byte_order, &mut out, v.level);
    }
    out
}

/// Encode a `MakeCurrent` / `MakeContextCurrent` reply. Fixed 32-byte
/// reply carrying a server-assigned `contextTag` that the client uses
/// to identify subsequent indirect-rendering requests.
///
/// Per `glxproto.h` `xGLXMakeCurrentReply`:
/// ```text
/// 1   Reply
/// 1   pad
/// 2   sequence
/// 4   length = 0
/// 4   contextTag
/// 20  pad
/// ```
#[must_use]
pub fn encode_make_current_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    context_tag: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    write_u32(byte_order, &mut out, context_tag);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encode a `GetDrawableAttributes` reply. Layout per glxproto:
///
/// ```text
/// 1   Reply
/// 1   pad
/// 2   sequence
/// 4   length = 2 * numAttribs (in 4-byte units)
/// 4   numAttribs
/// 20  pad
/// numAttribs * (CARD32 attrib, CARD32 value) pairs
/// ```
///
/// Mesa's `loader_dri3` reads `GLX_FBCONFIG_ID`, `GLX_TEXTURE_TARGET_EXT`
/// and `GLX_Y_INVERTED_EXT` to set up swap-chain orientation for
/// `texture_from_pixmap`. Direct-rendering glxgears doesn't actually
/// consult any of these for rendering, but it does roundtrip on the
/// reply, so we ship the canonical defaults.
#[must_use]
pub fn encode_get_drawable_attributes_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    attribs: &[(u32, u32)],
) -> Vec<u8> {
    let num_attribs = u32::try_from(attribs.len()).unwrap_or(0);
    let length_units = num_attribs.saturating_mul(2);
    let mut out = Vec::with_capacity(32 + (length_units as usize) * 4);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length_units);
    write_u32(byte_order, &mut out, num_attribs);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    for &(attrib, value) in attribs {
        write_u32(byte_order, &mut out, attrib);
        write_u32(byte_order, &mut out, value);
    }
    out
}

/// Parse a `CreateGLXWindow` (minor 31) request body — fbconfig +
/// X drawable + GLX drawable XID + property list.
#[must_use]
pub fn parse_create_glx_window(body: &[u8]) -> Option<CreateGlxWindowRequest> {
    if body.len() < 16 {
        return None;
    }
    Some(CreateGlxWindowRequest {
        screen: read_u32_le(body),
        fbconfig: read_u32_le(&body[4..]),
        x_window: read_u32_le(&body[8..]),
        glx_window: read_u32_le(&body[12..]),
    })
}

/// `CreateGLXWindow` request fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateGlxWindowRequest {
    pub screen: u32,
    pub fbconfig: u32,
    pub x_window: u32,
    pub glx_window: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcodes_match_glxproto() {
        // Pinned against /usr/share/xcb/glx.xml.
        assert_eq!(RENDER, 1);
        assert_eq!(CREATE_CONTEXT, 3);
        assert_eq!(DESTROY_CONTEXT, 4);
        assert_eq!(MAKE_CURRENT, 5);
        assert_eq!(IS_DIRECT, 6);
        assert_eq!(QUERY_VERSION, 7);
        assert_eq!(WAIT_GL, 8);
        assert_eq!(WAIT_X, 9);
        assert_eq!(COPY_CONTEXT, 10);
        assert_eq!(SWAP_BUFFERS, 11);
        // 12 is UseXFont — we don't model it; ensure we didn't shadow it.
        assert_eq!(GET_VISUAL_CONFIGS, 14);
        assert_eq!(VENDOR_PRIVATE, 16);
        assert_eq!(VENDOR_PRIVATE_WITH_REPLY, 17);
        assert_eq!(QUERY_EXTENSIONS_STRING, 18);
        assert_eq!(QUERY_SERVER_STRING, 19);
        assert_eq!(CLIENT_INFO, 20);
        assert_eq!(GET_FB_CONFIGS, 21);
        assert_eq!(CREATE_PIXMAP, 22);
        assert_eq!(DESTROY_PIXMAP, 23);
        assert_eq!(CREATE_NEW_CONTEXT, 24);
        assert_eq!(QUERY_CONTEXT, 25);
        assert_eq!(MAKE_CONTEXT_CURRENT, 26);
        assert_eq!(CREATE_PBUFFER, 27);
        assert_eq!(DESTROY_PBUFFER, 28);
        assert_eq!(GET_DRAWABLE_ATTRIBUTES, 29);
        assert_eq!(CHANGE_DRAWABLE_ATTRIBUTES, 30);
        assert_eq!(CREATE_WINDOW, 31);
        assert_eq!(DELETE_WINDOW, 32);
        assert_eq!(SET_CLIENT_INFO_ARB, 33);
        assert_eq!(CREATE_CONTEXT_ATTRIBS_ARB, 34);
        assert_eq!(SET_CLIENT_INFO_2_ARB, 35);
    }

    #[test]
    fn error_codes_match_glxproto() {
        // Pinned against /usr/share/xcb/glx.xml `errorcopy` entries.
        // Shipped wrong all of today: BAD_REQUEST=0 was actually
        // BadContext, UNSUPPORTED_PRIVATE_REQUEST=11 was
        // BadCurrentDrawable.
        assert_eq!(ERROR_GLX_BAD_RENDER_REQUEST, 6);
        assert_eq!(ERROR_GLX_UNSUPPORTED_PRIVATE_REQUEST, 8);
    }

    #[test]
    fn server_extensions_advertise_create_context() {
        // ANGLE/Chromium calls glXQueryServerString(GLX_EXTENSIONS)
        // (opcode 19) and refuses a GLES context if GLX_ARB_create_context
        // is missing. The server string must be non-empty and advertise it
        // (regression: STRING_EXTENSIONS used to return "").
        assert!(!SERVER_EXTENSIONS.is_empty());
        assert!(SERVER_EXTENSIONS.contains("GLX_ARB_create_context"));
        // The string reply for STRING_EXTENSIONS must carry the same list.
        let reply = encode_string_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(1),
            SERVER_EXTENSIONS,
        );
        let body = String::from_utf8_lossy(&reply);
        assert!(body.contains("GLX_ARB_create_context"));
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
        // n = 7 + 1 = 8 → padded = 8 → length_units = 2.
        let reply =
            encode_string_reply(ClientByteOrder::LittleEndian, SequenceNumber(3), "yserver");
        assert_eq!(reply.len(), 32 + 8);
        let length_units = u32::from_le_bytes(reply[4..8].try_into().unwrap());
        assert_eq!(length_units, 2);
        // Bytes 8..12 are pad1 — MUST be zero, not the string length.
        // Putting `n` here used to crash libepoxy's epoxy_glx_version
        // because Mesa reads `n` at offset 12 per glxproto.h
        // xGLXQueryServerStringReply.
        assert_eq!(&reply[8..12], &[0u8; 4], "pad1 must be zero");
        let n = u32::from_le_bytes(reply[12..16].try_into().unwrap());
        assert_eq!(n, 8, "n at offset 12 (after pad1)");
        // Bytes 16..32 are pad3..pad6.
        assert_eq!(&reply[16..32], &[0u8; 16], "pad3..pad6 must be zero");
        assert_eq!(&reply[32..39], b"yserver");
        assert_eq!(reply[39], 0);
    }

    /// Regression for the libepoxy `epoxy_glx_version` assertion
    /// failure that crashed xfce4-session: the version reply must
    /// `sscanf` as `M.N`. With `n` mis-located at offset 8, Mesa's
    /// xcb stub read the X11 reply length (1) instead of strLen,
    /// truncating the version string to a single byte.
    #[test]
    fn version_reply_parses_as_dotted_number() {
        let reply = encode_string_reply(ClientByteOrder::LittleEndian, SequenceNumber(1), "1.4");
        // Header (32) + "1.4\0" (4 bytes, already 4-aligned) = 36.
        assert_eq!(reply.len(), 36);
        let length_units = u32::from_le_bytes(reply[4..8].try_into().unwrap());
        assert_eq!(length_units, 1);
        let n = u32::from_le_bytes(reply[12..16].try_into().unwrap());
        assert_eq!(n, 4);
        assert_eq!(&reply[32..35], b"1.4");
        assert_eq!(reply[35], 0);
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
