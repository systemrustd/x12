//! MIT-SHM (extension v1.2) — wire encoding and request parsing.
//!
//! Spec reference: <https://www.x.org/releases/X11R7.7/doc/xextproto/shm.html>
//! plus xcb-proto's `xcb-proto/src/shm.xml` for v1.2 (`AttachFd`,
//! `CreateSegment`, `shared_pixmaps` flag).
//!
//! ynest only implements the fd-mode subset: `QueryVersion`, `AttachFd`,
//! `Detach`, `CreatePixmap`, `PutImage`, `GetImage`. Legacy SysV `Attach`
//! (minor 1) is rejected with `BadValue` since shared SysV memory across
//! sandbox boundaries is unreliable.

use super::SequenceNumber;

pub const QUERY_VERSION: u8 = 0;
pub const ATTACH: u8 = 1;
pub const DETACH: u8 = 2;
pub const PUT_IMAGE: u8 = 3;
pub const GET_IMAGE: u8 = 4;
pub const CREATE_PIXMAP: u8 = 5;
pub const ATTACH_FD: u8 = 6;
pub const CREATE_SEGMENT: u8 = 7;

/// Advertise version 1.2 — the `*Fd` opcodes are v1.2 features.
pub const MAJOR_VERSION: u16 = 1;
pub const MINOR_VERSION: u16 = 2;

/// Pixmap format byte the server returns from `QueryVersion`. ZPixmap (=2)
/// is the only format we know how to read out of a shm segment.
pub const PIXMAP_FORMAT_Z_PIXMAP: u8 = 2;

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_i16_le(bytes: &[u8]) -> i16 {
    i16::from_le_bytes([bytes[0], bytes[1]])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttachFdRequest {
    pub shmseg: u32,
    pub read_only: bool,
}

/// `CreateSegment` (minor 7) — server-allocates a shm segment and
/// passes the descriptor back to the client via `SCM_RIGHTS` in the
/// reply. The `nfd` field of the reply is fixed at 1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateSegmentRequest {
    pub shmseg: u32,
    pub size: u32,
    pub read_only: bool,
}

/// Legacy SysV `Attach` (minor 1). The client passes a `shmid` from
/// `shmget(2)`; the server `shmat`s it to obtain a mapping. Used by
/// wmaker (libwraster) and many older toolkits that haven't migrated to
/// `AttachFd`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttachRequest {
    pub shmseg: u32,
    pub shmid: u32,
    pub read_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreatePixmapRequest {
    pub pid: u32,
    pub drawable: u32,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
    pub shmseg: u32,
    pub offset: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PutImageRequest {
    pub drawable: u32,
    pub gc: u32,
    pub total_width: u16,
    pub total_height: u16,
    pub src_x: i16,
    pub src_y: i16,
    pub src_width: u16,
    pub src_height: u16,
    pub dst_x: i16,
    pub dst_y: i16,
    pub depth: u8,
    pub format: u8,
    pub send_event: bool,
    pub shmseg: u32,
    pub offset: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GetImageRequest {
    pub drawable: u32,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub plane_mask: u32,
    pub format: u8,
    pub shmseg: u32,
    pub offset: u32,
}

#[must_use]
pub fn parse_detach(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

#[must_use]
pub fn parse_attach_fd(body: &[u8]) -> Option<AttachFdRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(AttachFdRequest {
        shmseg: read_u32_le(body),
        read_only: body[4] != 0,
    })
}

#[must_use]
pub fn parse_create_segment(body: &[u8]) -> Option<CreateSegmentRequest> {
    if body.len() < 12 {
        return None;
    }
    Some(CreateSegmentRequest {
        shmseg: read_u32_le(body),
        size: read_u32_le(&body[4..]),
        read_only: body[8] != 0,
    })
}

#[must_use]
pub fn parse_attach(body: &[u8]) -> Option<AttachRequest> {
    if body.len() < 12 {
        return None;
    }
    Some(AttachRequest {
        shmseg: read_u32_le(body),
        shmid: read_u32_le(&body[4..]),
        read_only: body[8] != 0,
    })
}

#[must_use]
pub fn parse_create_pixmap(body: &[u8]) -> Option<CreatePixmapRequest> {
    if body.len() < 24 {
        return None;
    }
    Some(CreatePixmapRequest {
        pid: read_u32_le(body),
        drawable: read_u32_le(&body[4..]),
        width: read_u16_le(&body[8..]),
        height: read_u16_le(&body[10..]),
        depth: body[12],
        // body[13..16] is pad
        shmseg: read_u32_le(&body[16..]),
        offset: read_u32_le(&body[20..]),
    })
}

#[must_use]
pub fn parse_put_image(body: &[u8]) -> Option<PutImageRequest> {
    // Wire layout (after the 4-byte request header): drawable(4) + gc(4) +
    // total_w/h(4) + src_xy(4) + src_w/h(4) + dst_xy(4) + depth/format/
    // send_event/pad(4) + shmseg(4) + offset(4) = 36 bytes.
    if body.len() < 36 {
        return None;
    }
    Some(PutImageRequest {
        drawable: read_u32_le(body),
        gc: read_u32_le(&body[4..]),
        total_width: read_u16_le(&body[8..]),
        total_height: read_u16_le(&body[10..]),
        src_x: read_i16_le(&body[12..]),
        src_y: read_i16_le(&body[14..]),
        src_width: read_u16_le(&body[16..]),
        src_height: read_u16_le(&body[18..]),
        dst_x: read_i16_le(&body[20..]),
        dst_y: read_i16_le(&body[22..]),
        depth: body[24],
        format: body[25],
        send_event: body[26] != 0,
        // body[27] is pad
        shmseg: read_u32_le(&body[28..]),
        offset: read_u32_le(&body[32..]),
    })
}

#[must_use]
pub fn parse_get_image(body: &[u8]) -> Option<GetImageRequest> {
    if body.len() < 28 {
        return None;
    }
    Some(GetImageRequest {
        drawable: read_u32_le(body),
        x: read_i16_le(&body[4..]),
        y: read_i16_le(&body[6..]),
        width: read_u16_le(&body[8..]),
        height: read_u16_le(&body[10..]),
        plane_mask: read_u32_le(&body[12..]),
        format: body[16],
        // body[17..20] is pad
        shmseg: read_u32_le(&body[20..]),
        offset: read_u32_le(&body[24..]),
    })
}

/// Encode the `CreateSegment` reply. The reply itself carries
/// `nfd = 1` and a length-zero body; the actual file descriptor is
/// delivered alongside via `SCM_RIGHTS` in the same `sendmsg`.
#[must_use]
pub fn encode_create_segment_reply(sequence: SequenceNumber) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1); // reply
    out.push(1); // nfd = 1
    out.extend_from_slice(&sequence.0.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // length = 0
    out.extend_from_slice(&[0u8; 24]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encode the `QueryVersion` reply.
///
/// Per the v1.2 spec, the `data` byte (offset 1) carries
/// `shared_pixmaps`: when `true` the server promises that pixmaps
/// created from a shm segment will reflect later writes into the
/// segment. ynest answers `false` — see the design doc — because we
/// snapshot the segment at `CreatePixmap` time.
#[must_use]
pub fn encode_query_version_reply(sequence: SequenceNumber, shared_pixmaps: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1); // reply
    out.push(u8::from(shared_pixmaps));
    out.extend_from_slice(&sequence.0.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // length = 0
    out.extend_from_slice(&MAJOR_VERSION.to_le_bytes());
    out.extend_from_slice(&MINOR_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // uid
    out.extend_from_slice(&0u16.to_le_bytes()); // gid
    out.push(PIXMAP_FORMAT_Z_PIXMAP);
    out.extend_from_slice(&[0u8; 15]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encode a `GetImage` reply. The reply carries the geometry of the
/// drawable and the visual id; the actual pixel bytes go straight to
/// the shm segment (we wrote them there before sending the reply).
#[must_use]
pub fn encode_get_image_reply(
    sequence: SequenceNumber,
    depth: u8,
    visual: u32,
    size: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1); // reply
    out.push(depth);
    out.extend_from_slice(&sequence.0.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // length (no inline data)
    out.extend_from_slice(&visual.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_version_reply_advertises_v1_2_with_shared_pixmaps_false() {
        let reply = encode_query_version_reply(SequenceNumber(7), false);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1, "reply byte");
        assert_eq!(reply[1], 0, "shared_pixmaps = false");
        assert_eq!(u16::from_le_bytes([reply[2], reply[3]]), 7);
        assert_eq!(u16::from_le_bytes([reply[8], reply[9]]), MAJOR_VERSION);
        assert_eq!(u16::from_le_bytes([reply[10], reply[11]]), MINOR_VERSION);
        assert_eq!(reply[16], PIXMAP_FORMAT_Z_PIXMAP);
    }

    #[test]
    fn query_version_reply_can_advertise_shared_pixmaps_true() {
        let reply = encode_query_version_reply(SequenceNumber(1), true);
        assert_eq!(reply[1], 1);
    }

    #[test]
    fn parse_attach_fd_decodes_shmseg_and_read_only() {
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&0xdead_beef_u32.to_le_bytes());
        body.push(1); // read_only = true
        body.extend_from_slice(&[0u8; 3]); // pad
        let req = parse_attach_fd(&body).unwrap();
        assert_eq!(req.shmseg, 0xdead_beef);
        assert!(req.read_only);
    }

    #[test]
    fn parse_attach_fd_rejects_short_body() {
        assert!(parse_attach_fd(&[1, 2, 3]).is_none());
    }

    #[test]
    fn parse_create_pixmap_extracts_all_fields() {
        let mut body = Vec::with_capacity(24);
        body.extend_from_slice(&0x100_u32.to_le_bytes()); // pid
        body.extend_from_slice(&0x200_u32.to_le_bytes()); // drawable
        body.extend_from_slice(&64_u16.to_le_bytes()); // width
        body.extend_from_slice(&48_u16.to_le_bytes()); // height
        body.push(24); // depth
        body.extend_from_slice(&[0u8; 3]); // pad
        body.extend_from_slice(&0x300_u32.to_le_bytes()); // shmseg
        body.extend_from_slice(&0_u32.to_le_bytes()); // offset
        let req = parse_create_pixmap(&body).unwrap();
        assert_eq!(req.pid, 0x100);
        assert_eq!(req.drawable, 0x200);
        assert_eq!(req.width, 64);
        assert_eq!(req.height, 48);
        assert_eq!(req.depth, 24);
        assert_eq!(req.shmseg, 0x300);
        assert_eq!(req.offset, 0);
    }

    #[test]
    fn parse_detach_extracts_shmseg() {
        let body = 0xcafe_babe_u32.to_le_bytes();
        assert_eq!(parse_detach(&body).unwrap(), 0xcafe_babe);
    }
}
