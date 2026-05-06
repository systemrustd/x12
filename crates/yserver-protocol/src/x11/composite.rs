use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_u16, write_u32},
};

pub const QUERY_VERSION: u8 = 0;
pub const REDIRECT_WINDOW: u8 = 1;
pub const REDIRECT_SUBWINDOWS: u8 = 2;
pub const UNREDIRECT_WINDOW: u8 = 3;
pub const UNREDIRECT_SUBWINDOWS: u8 = 4;
pub const CREATE_REGION_FROM_BORDER_CLIP: u8 = 5;
pub const NAME_WINDOW_PIXMAP: u8 = 6;
pub const GET_OVERLAY_WINDOW: u8 = 7;
pub const RELEASE_OVERLAY_WINDOW: u8 = 8;

pub const MAJOR_VERSION: u32 = 0;
pub const MINOR_VERSION: u32 = 4;

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

#[must_use]
pub fn parse_window_update(body: &[u8]) -> Option<(u32, u8)> {
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), body[4]))
}

#[must_use]
pub fn parse_u32_pair(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..])))
}

#[must_use]
pub fn parse_window(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
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
pub fn encode_get_overlay_window_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    overlay: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    write_u32(byte_order, &mut out, overlay);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_version_reply_shape() {
        let reply =
            encode_query_version_reply(ClientByteOrder::LittleEndian, SequenceNumber(1), 0, 4);
        assert_eq!(reply.len(), 32);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 4);
    }

    #[test]
    fn overlay_window_reply_shape() {
        let reply = encode_get_overlay_window_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(1),
            0x100,
        );
        assert_eq!(reply.len(), 32);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 0x100);
    }
}
