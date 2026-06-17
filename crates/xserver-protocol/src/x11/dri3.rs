//! DRI3 v1.4 wire-protocol decoders/encoders.
//!
//! Request opcode numbering follows `dri3proto`. The protocol has one
//! `Open` request (since 1.0); later versions add new requests around
//! it rather than versioning `Open` itself.

use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_u16, write_u32},
};

pub const QUERY_VERSION: u8 = 0;
pub const OPEN: u8 = 1;
pub const PIXMAP_FROM_BUFFER: u8 = 2;
pub const BUFFER_FROM_PIXMAP: u8 = 3;
pub const FENCE_FROM_FD: u8 = 4;
pub const FD_FROM_FENCE: u8 = 5;
pub const GET_SUPPORTED_MODIFIERS: u8 = 6;
pub const PIXMAP_FROM_BUFFERS: u8 = 7;
pub const BUFFERS_FROM_PIXMAP: u8 = 8;
pub const SET_DRM_DEVICE_IN_USE: u8 = 9;
pub const IMPORT_SYNCOBJ: u8 = 10;
pub const FREE_SYNCOBJ: u8 = 11;

pub const MAJOR_VERSION: u32 = 1;
pub const MINOR_VERSION: u32 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenRequest {
    pub drawable: u32,
    pub provider: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixmapFromBufferRequest {
    pub pixmap: u32,
    pub drawable: u32,
    pub size: u32,
    pub width: u16,
    pub height: u16,
    pub stride: u16,
    pub depth: u8,
    pub bpp: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FenceFromFdRequest {
    pub drawable: u32,
    pub fence: u32,
    pub initially_triggered: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FdFromFenceRequest {
    pub drawable: u32,
    pub fence: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GetSupportedModifiersRequest {
    pub window: u32,
    pub depth: u8,
    pub bpp: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetDrmDeviceInUseRequest {
    pub window: u32,
    pub drm_major: u32,
    pub drm_minor: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportSyncobjRequest {
    pub syncobj: u32,
    pub drawable: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixmapFromBuffersRequest {
    pub pixmap: u32,
    pub window: u32,
    pub num_buffers: u8,
    pub strides: [u32; 4],
    pub offsets: [u32; 4],
    pub width: u16,
    pub height: u16,
    pub depth: u8,
    pub bpp: u8,
    pub modifier: u64,
}

fn read_u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u64_le(bytes: &[u8]) -> u64 {
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

#[must_use]
pub fn parse_query_version(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..])))
}

#[must_use]
pub fn parse_open(body: &[u8]) -> Option<OpenRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(OpenRequest {
        drawable: read_u32_le(body),
        provider: read_u32_le(&body[4..]),
    })
}

#[must_use]
pub fn parse_pixmap_from_buffer(body: &[u8]) -> Option<PixmapFromBufferRequest> {
    // Wire layout (20-byte body, dri3proto sz_xDRI3PixmapFromBufferReq=24
    // including the 4-byte X11 request header):
    //   pixmap(4) drawable(4) size(4) width(2) height(2) stride(2)
    //   depth(1) bpp(1)
    if body.len() < 20 {
        return None;
    }
    Some(PixmapFromBufferRequest {
        pixmap: read_u32_le(body),
        drawable: read_u32_le(&body[4..]),
        size: read_u32_le(&body[8..]),
        width: read_u16_le(&body[12..]),
        height: read_u16_le(&body[14..]),
        stride: read_u16_le(&body[16..]),
        depth: body[18],
        bpp: body[19],
    })
}

#[must_use]
pub fn parse_pixmap_from_buffers(body: &[u8]) -> Option<PixmapFromBuffersRequest> {
    // Wire layout per xcbproto dri3.xml (60-byte body —
    // sz_xDRI3PixmapFromBuffersReq = 64 including the 4-byte X11
    // request header):
    //   pixmap(4) window(4) num_buffers(1) pad(3)
    //   width(2) height(2)
    //   stride0(4) offset0(4) stride1(4) offset1(4)
    //   stride2(4) offset2(4) stride3(4) offset3(4)
    //   depth(1) bpp(1) pad(2)
    //   modifier(8)
    if body.len() < 60 {
        return None;
    }
    let num_buffers = body[8];
    if !(1..=4).contains(&num_buffers) {
        return None;
    }
    let mut strides = [0u32; 4];
    let mut offsets = [0u32; 4];
    for i in 0..4 {
        strides[i] = read_u32_le(&body[16 + i * 8..]);
        offsets[i] = read_u32_le(&body[20 + i * 8..]);
    }
    Some(PixmapFromBuffersRequest {
        pixmap: read_u32_le(body),
        window: read_u32_le(&body[4..]),
        num_buffers,
        strides,
        offsets,
        width: read_u16_le(&body[12..]),
        height: read_u16_le(&body[14..]),
        depth: body[48],
        bpp: body[49],
        modifier: read_u64_le(&body[52..]),
    })
}

#[must_use]
pub fn parse_buffer_from_pixmap(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

#[must_use]
pub fn parse_fence_from_fd(body: &[u8]) -> Option<FenceFromFdRequest> {
    // Body: drawable(4) fence(4) initially_triggered(1) pad(3) = 12B.
    if body.len() < 12 {
        return None;
    }
    Some(FenceFromFdRequest {
        drawable: read_u32_le(body),
        fence: read_u32_le(&body[4..]),
        initially_triggered: body[8] != 0,
    })
}

#[must_use]
pub fn parse_fd_from_fence(body: &[u8]) -> Option<FdFromFenceRequest> {
    // Body: drawable(4) fence(4) = 8B.
    if body.len() < 8 {
        return None;
    }
    Some(FdFromFenceRequest {
        drawable: read_u32_le(body),
        fence: read_u32_le(&body[4..]),
    })
}

#[must_use]
pub fn parse_get_supported_modifiers(body: &[u8]) -> Option<GetSupportedModifiersRequest> {
    // Body: window(4) depth(1) bpp(1) pad(2) = 8B.
    if body.len() < 8 {
        return None;
    }
    Some(GetSupportedModifiersRequest {
        window: read_u32_le(body),
        depth: body[4],
        bpp: body[5],
    })
}

#[must_use]
pub fn parse_buffers_from_pixmap(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

#[must_use]
pub fn parse_set_drm_device_in_use(body: &[u8]) -> Option<SetDrmDeviceInUseRequest> {
    // Body: window(4) drm_major(4) drm_minor(4) = 12B. Phase 4.2
    // accepts but ignores this — single-GPU only.
    if body.len() < 12 {
        return None;
    }
    Some(SetDrmDeviceInUseRequest {
        window: read_u32_le(body),
        drm_major: read_u32_le(&body[4..]),
        drm_minor: read_u32_le(&body[8..]),
    })
}

#[must_use]
pub fn parse_import_syncobj(body: &[u8]) -> Option<ImportSyncobjRequest> {
    // Body: syncobj(4) drawable(4) = 8B. fd via SCM_RIGHTS.
    if body.len() < 8 {
        return None;
    }
    Some(ImportSyncobjRequest {
        syncobj: read_u32_le(body),
        drawable: read_u32_le(&body[4..]),
    })
}

#[must_use]
pub fn parse_free_syncobj(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

fn write_u64(byte_order: ClientByteOrder, out: &mut Vec<u8>, value: u64) {
    #[allow(clippy::cast_possible_truncation)]
    let lo = value as u32;
    #[allow(clippy::cast_possible_truncation)]
    let hi = (value >> 32) as u32;
    write_u32(byte_order, out, lo);
    write_u32(byte_order, out, hi);
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

#[must_use]
pub fn encode_open_reply(byte_order: ClientByteOrder, sequence: SequenceNumber) -> Vec<u8> {
    // nfd = 1; fd is delivered out-of-band via SCM_RIGHTS by the
    // dispatcher (see `send_reply_with_fd`).
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(1); // nfd
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    out.extend_from_slice(&[0u8; 24]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_buffer_from_pixmap_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    size: u32,
    width: u16,
    height: u16,
    stride: u16,
    depth: u8,
    bpp: u8,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(1); // nfd
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    write_u32(byte_order, &mut out, size);
    write_u16(byte_order, &mut out, width);
    write_u16(byte_order, &mut out, height);
    write_u16(byte_order, &mut out, stride);
    out.push(depth);
    out.push(bpp);
    out.extend_from_slice(&[0u8; 12]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_fd_from_fence_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    // Same shape as Open reply: nfd=1, fd via SCM_RIGHTS.
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(1);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    out.extend_from_slice(&[0u8; 24]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_get_supported_modifiers_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    window_modifiers: &[u64],
    screen_modifiers: &[u64],
) -> Vec<u8> {
    // Reply length is in 4-byte units past the 32-byte header. Each
    // modifier is 8 bytes, so 2 units per modifier.
    let nwin = u32::try_from(window_modifiers.len()).expect("modifier list fits in u32");
    let nscr = u32::try_from(screen_modifiers.len()).expect("modifier list fits in u32");
    let payload_units = 2 * (nwin + nscr);
    let mut out = Vec::with_capacity(32 + 8 * (window_modifiers.len() + screen_modifiers.len()));
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, payload_units);
    write_u32(byte_order, &mut out, nwin);
    write_u32(byte_order, &mut out, nscr);
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    for &m in window_modifiers {
        write_u64(byte_order, &mut out, m);
    }
    for &m in screen_modifiers {
        write_u64(byte_order, &mut out, m);
    }
    out
}

/// Encode an `xDRI3BuffersFromPixmapReply` (op 8).
///
/// Wire layout (dri3proto.h `xDRI3BuffersFromPixmapReply`, xcbproto
/// `dri3.xml` BuffersFromPixmap reply — 32-byte header then the
/// variable arrays):
/// ```text
///   type(1) nfd(1) sequence(2) length(4)        // length = 2*nfd 4-byte units
///   width(2) height(2) pad(4)
///   modifier(8)
///   depth(1) bpp(1) pad(6)                       // -> 32 bytes
///   strides: CARD32 * nfd
///   offsets: CARD32 * nfd
/// ```
/// The `nfd` file descriptors travel out-of-band via SCM_RIGHTS; this
/// encodes only the inline bytes. `strides` and `offsets` must be the
/// same length (one entry per plane); `nfd` is taken from that length.
#[must_use]
pub fn encode_buffers_from_pixmap_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    width: u16,
    height: u16,
    modifier: u64,
    depth: u8,
    bpp: u8,
    strides: &[u32],
    offsets: &[u32],
) -> Vec<u8> {
    debug_assert_eq!(
        strides.len(),
        offsets.len(),
        "BuffersFromPixmap: one stride and one offset per plane"
    );
    let nfd = u8::try_from(strides.len()).expect("DRI3 plane count fits in u8");
    // length counts the payload in 4-byte units: nfd strides + nfd offsets.
    let length = 2 * u32::from(nfd);

    let mut out = Vec::with_capacity(32 + 8 * strides.len());
    out.push(1);
    out.push(nfd);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length);
    write_u16(byte_order, &mut out, width);
    write_u16(byte_order, &mut out, height);
    out.extend_from_slice(&[0u8; 4]); // pad
    write_u64(byte_order, &mut out, modifier);
    out.push(depth);
    out.push(bpp);
    out.extend_from_slice(&[0u8; 6]); // pad
    debug_assert_eq!(out.len(), 32);
    for &s in strides {
        write_u32(byte_order, &mut out, s);
    }
    for &o in offsets {
        write_u32(byte_order, &mut out, o);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcodes_match_dri3proto() {
        assert_eq!(QUERY_VERSION, 0);
        assert_eq!(OPEN, 1);
        assert_eq!(PIXMAP_FROM_BUFFER, 2);
        assert_eq!(BUFFER_FROM_PIXMAP, 3);
        assert_eq!(FENCE_FROM_FD, 4);
        assert_eq!(FD_FROM_FENCE, 5);
        assert_eq!(GET_SUPPORTED_MODIFIERS, 6);
        assert_eq!(PIXMAP_FROM_BUFFERS, 7);
        assert_eq!(BUFFERS_FROM_PIXMAP, 8);
        assert_eq!(SET_DRM_DEVICE_IN_USE, 9);
        assert_eq!(IMPORT_SYNCOBJ, 10);
        assert_eq!(FREE_SYNCOBJ, 11);
    }

    #[test]
    fn version_constants() {
        assert_eq!(MAJOR_VERSION, 1);
        assert_eq!(MINOR_VERSION, 4);
    }

    #[test]
    fn query_version_parses_minor() {
        let mut body = vec![0u8; 8];
        body[0..4].copy_from_slice(&1u32.to_le_bytes());
        body[4..8].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(parse_query_version(&body), Some((1, 4)));
    }

    #[test]
    fn query_version_rejects_short_body() {
        assert_eq!(parse_query_version(&[0u8; 7]), None);
    }

    #[test]
    fn open_parses() {
        let mut body = vec![0u8; 8];
        body[0..4].copy_from_slice(&0x100u32.to_le_bytes());
        body[4..8].copy_from_slice(&0x42u32.to_le_bytes());
        let req = parse_open(&body).unwrap();
        assert_eq!(req.drawable, 0x100);
        assert_eq!(req.provider, 0x42);
    }

    #[test]
    fn open_rejects_short_body() {
        assert_eq!(parse_open(&[0u8; 7]), None);
    }

    #[test]
    fn pixmap_from_buffer_parses() {
        // body layout: pixmap(4) drawable(4) size(4) w(2) h(2) stride(2)
        // depth(1) bpp(1) = 20 bytes.
        let mut body = vec![0u8; 20];
        body[0..4].copy_from_slice(&0x200u32.to_le_bytes());
        body[4..8].copy_from_slice(&0x300u32.to_le_bytes());
        body[8..12].copy_from_slice(&65536u32.to_le_bytes());
        body[12..14].copy_from_slice(&256u16.to_le_bytes());
        body[14..16].copy_from_slice(&128u16.to_le_bytes());
        body[16..18].copy_from_slice(&1024u16.to_le_bytes());
        body[18] = 24;
        body[19] = 32;
        let req = parse_pixmap_from_buffer(&body).unwrap();
        assert_eq!(req.pixmap, 0x200);
        assert_eq!(req.drawable, 0x300);
        assert_eq!(req.size, 65536);
        assert_eq!(req.width, 256);
        assert_eq!(req.height, 128);
        assert_eq!(req.stride, 1024);
        assert_eq!(req.depth, 24);
        assert_eq!(req.bpp, 32);
    }

    #[test]
    fn pixmap_from_buffer_rejects_short_body() {
        assert_eq!(parse_pixmap_from_buffer(&[0u8; 19]), None);
    }

    #[test]
    fn pixmap_from_buffers_parses_single_plane() {
        // 60-byte body. See spec layout in parse_pixmap_from_buffers.
        let mut body = vec![0u8; 60];
        body[0..4].copy_from_slice(&0x400u32.to_le_bytes());
        body[4..8].copy_from_slice(&0x500u32.to_le_bytes());
        body[8] = 1;
        body[12..14].copy_from_slice(&64u16.to_le_bytes()); // width
        body[14..16].copy_from_slice(&64u16.to_le_bytes()); // height
        body[16..20].copy_from_slice(&512u32.to_le_bytes()); // stride0
        body[20..24].copy_from_slice(&0u32.to_le_bytes()); // offset0
        body[48] = 24;
        body[49] = 32;
        body[52..60].copy_from_slice(&0xFFFF_FFFFu64.to_le_bytes());
        let req = parse_pixmap_from_buffers(&body).unwrap();
        assert_eq!(req.pixmap, 0x400);
        assert_eq!(req.window, 0x500);
        assert_eq!(req.num_buffers, 1);
        assert_eq!(req.strides[0], 512);
        assert_eq!(req.offsets[0], 0);
        assert_eq!(req.width, 64);
        assert_eq!(req.height, 64);
        assert_eq!(req.depth, 24);
        assert_eq!(req.bpp, 32);
        assert_eq!(req.modifier, 0xFFFF_FFFF);
    }

    #[test]
    fn pixmap_from_buffers_accepts_num_buffers_4() {
        let mut body = vec![0u8; 60];
        body[8] = 4;
        body[16..20].copy_from_slice(&1u32.to_le_bytes()); // stride0
        body[24..28].copy_from_slice(&2u32.to_le_bytes()); // stride1
        body[32..36].copy_from_slice(&3u32.to_le_bytes()); // stride2
        body[40..44].copy_from_slice(&4u32.to_le_bytes()); // stride3
        let req = parse_pixmap_from_buffers(&body).unwrap();
        assert_eq!(req.num_buffers, 4);
        assert_eq!(req.strides, [1, 2, 3, 4]);
    }

    #[test]
    fn pixmap_from_buffers_rejects_zero_num_buffers() {
        let body = vec![0u8; 60];
        assert!(parse_pixmap_from_buffers(&body).is_none());
    }

    #[test]
    fn pixmap_from_buffers_rejects_num_buffers_5() {
        let mut body = vec![0u8; 60];
        body[8] = 5;
        assert!(parse_pixmap_from_buffers(&body).is_none());
    }

    #[test]
    fn pixmap_from_buffers_rejects_short_body() {
        assert_eq!(parse_pixmap_from_buffers(&[0u8; 59]), None);
    }

    #[test]
    fn buffer_from_pixmap_parses() {
        let mut body = vec![0u8; 4];
        body[0..4].copy_from_slice(&0xCAFEu32.to_le_bytes());
        assert_eq!(parse_buffer_from_pixmap(&body), Some(0xCAFE));
        assert_eq!(parse_buffer_from_pixmap(&[0u8; 3]), None);
    }

    #[test]
    fn fence_from_fd_parses() {
        let mut body = vec![0u8; 12];
        body[0..4].copy_from_slice(&0x100u32.to_le_bytes());
        body[4..8].copy_from_slice(&0x200u32.to_le_bytes());
        body[8] = 1;
        let req = parse_fence_from_fd(&body).unwrap();
        assert_eq!(req.drawable, 0x100);
        assert_eq!(req.fence, 0x200);
        assert!(req.initially_triggered);
    }

    #[test]
    fn fence_from_fd_rejects_short_body() {
        assert_eq!(parse_fence_from_fd(&[0u8; 11]), None);
    }

    #[test]
    fn fd_from_fence_parses() {
        let mut body = vec![0u8; 8];
        body[0..4].copy_from_slice(&0x300u32.to_le_bytes());
        body[4..8].copy_from_slice(&0x400u32.to_le_bytes());
        let req = parse_fd_from_fence(&body).unwrap();
        assert_eq!(req.drawable, 0x300);
        assert_eq!(req.fence, 0x400);
    }

    #[test]
    fn get_supported_modifiers_parses() {
        let mut body = vec![0u8; 8];
        body[0..4].copy_from_slice(&0x500u32.to_le_bytes());
        body[4] = 24;
        body[5] = 32;
        let req = parse_get_supported_modifiers(&body).unwrap();
        assert_eq!(req.window, 0x500);
        assert_eq!(req.depth, 24);
        assert_eq!(req.bpp, 32);
    }

    #[test]
    fn buffers_from_pixmap_parses() {
        let mut body = vec![0u8; 4];
        body[0..4].copy_from_slice(&0xBEEFu32.to_le_bytes());
        assert_eq!(parse_buffers_from_pixmap(&body), Some(0xBEEF));
    }

    #[test]
    fn set_drm_device_in_use_parses() {
        let mut body = vec![0u8; 12];
        body[0..4].copy_from_slice(&0x100u32.to_le_bytes());
        body[4..8].copy_from_slice(&226u32.to_le_bytes());
        body[8..12].copy_from_slice(&128u32.to_le_bytes());
        let req = parse_set_drm_device_in_use(&body).unwrap();
        assert_eq!(req.window, 0x100);
        assert_eq!(req.drm_major, 226);
        assert_eq!(req.drm_minor, 128);
    }

    #[test]
    fn import_syncobj_parses() {
        let mut body = vec![0u8; 8];
        body[0..4].copy_from_slice(&0x600u32.to_le_bytes());
        body[4..8].copy_from_slice(&0x700u32.to_le_bytes());
        let req = parse_import_syncobj(&body).unwrap();
        assert_eq!(req.syncobj, 0x600);
        assert_eq!(req.drawable, 0x700);
    }

    #[test]
    fn free_syncobj_parses() {
        let mut body = vec![0u8; 4];
        body[0..4].copy_from_slice(&0x800u32.to_le_bytes());
        assert_eq!(parse_free_syncobj(&body), Some(0x800));
    }

    #[test]
    fn query_version_reply_shape() {
        let reply =
            encode_query_version_reply(ClientByteOrder::LittleEndian, SequenceNumber(7), 1, 4);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(u16::from_le_bytes(reply[2..4].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 4);
    }

    #[test]
    fn open_reply_shape() {
        let reply = encode_open_reply(ClientByteOrder::LittleEndian, SequenceNumber(3));
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(reply[1], 1, "nfd should be 1");
    }

    #[test]
    fn buffer_from_pixmap_reply_shape() {
        let reply = encode_buffer_from_pixmap_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(2),
            65536,
            128,
            64,
            512,
            24,
            32,
        );
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[1], 1, "nfd should be 1");
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 65536);
        assert_eq!(u16::from_le_bytes(reply[12..14].try_into().unwrap()), 128);
        assert_eq!(u16::from_le_bytes(reply[14..16].try_into().unwrap()), 64);
        assert_eq!(u16::from_le_bytes(reply[16..18].try_into().unwrap()), 512);
        assert_eq!(reply[18], 24);
        assert_eq!(reply[19], 32);
    }

    #[test]
    fn fd_from_fence_reply_shape() {
        let reply = encode_fd_from_fence_reply(ClientByteOrder::LittleEndian, SequenceNumber(5));
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[1], 1);
    }

    #[test]
    fn buffers_from_pixmap_reply_single_plane_layout() {
        // Byte offsets per dri3proto.h xDRI3BuffersFromPixmapReply /
        // xcbproto dri3.xml. Single plane: nfd=1, length=2 units.
        let reply = encode_buffers_from_pixmap_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(11),
            /* width */ 256,
            /* height */ 128,
            /* modifier */ 0x0123_4567_89ab_cdef,
            /* depth */ 24,
            /* bpp */ 32,
            /* strides */ &[1024],
            /* offsets */ &[0],
        );
        // 32-byte header + 1 stride (4) + 1 offset (4) = 40.
        assert_eq!(reply.len(), 40);
        assert_eq!(reply[0], 1, "X_Reply");
        assert_eq!(reply[1], 1, "nfd == 1 plane");
        assert_eq!(u16::from_le_bytes(reply[2..4].try_into().unwrap()), 11);
        assert_eq!(
            u32::from_le_bytes(reply[4..8].try_into().unwrap()),
            2,
            "length = 2*nfd 4-byte units"
        );
        assert_eq!(u16::from_le_bytes(reply[8..10].try_into().unwrap()), 256);
        assert_eq!(u16::from_le_bytes(reply[10..12].try_into().unwrap()), 128);
        assert_eq!(
            u64::from_le_bytes(reply[16..24].try_into().unwrap()),
            0x0123_4567_89ab_cdef,
            "modifier at byte 16"
        );
        assert_eq!(reply[24], 24, "depth");
        assert_eq!(reply[25], 32, "bpp");
        // strides[0] at byte 32, offsets[0] at byte 36.
        assert_eq!(u32::from_le_bytes(reply[32..36].try_into().unwrap()), 1024);
        assert_eq!(u32::from_le_bytes(reply[36..40].try_into().unwrap()), 0);
    }

    #[test]
    fn buffers_from_pixmap_reply_multi_plane_length() {
        let reply = encode_buffers_from_pixmap_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(1),
            64,
            64,
            0,
            24,
            32,
            &[10, 20, 30],
            &[0, 100, 200],
        );
        assert_eq!(reply[1], 3, "nfd == 3 planes");
        assert_eq!(
            u32::from_le_bytes(reply[4..8].try_into().unwrap()),
            6,
            "length = 2*3"
        );
        assert_eq!(reply.len(), 32 + 3 * 4 + 3 * 4);
        // strides then offsets.
        assert_eq!(u32::from_le_bytes(reply[32..36].try_into().unwrap()), 10);
        assert_eq!(u32::from_le_bytes(reply[44..48].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[48..52].try_into().unwrap()), 100);
    }

    #[test]
    fn get_supported_modifiers_reply_lengths() {
        let win = [0u64, 1u64];
        let scr = [0u64, 1u64, 2u64];
        let reply = encode_get_supported_modifiers_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(9),
            &win,
            &scr,
        );
        assert_eq!(reply.len(), 32 + 8 * 5);
        // Reply length field counts payload 4-byte units past the 32-byte header.
        let units = u32::from_le_bytes(reply[4..8].try_into().unwrap());
        assert_eq!(units, 2 * (win.len() as u32 + scr.len() as u32));
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 3);
        // First window modifier follows the 32-byte header.
        let m0 = u64::from_le_bytes(reply[32..40].try_into().unwrap());
        assert_eq!(m0, 0);
    }
}
