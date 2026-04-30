use super::{SequenceNumber, xfixes::RegionRect};

pub const QUERY_VERSION: u8 = 0;
pub const RECTANGLES: u8 = 1;
pub const MASK: u8 = 2;
pub const COMBINE: u8 = 3;
pub const OFFSET: u8 = 4;
pub const QUERY_EXTENTS: u8 = 5;
pub const SELECT_INPUT: u8 = 6;
pub const INPUT_SELECTED: u8 = 7;
pub const GET_RECTANGLES: u8 = 8;

pub const MAJOR_VERSION: u16 = 1;
pub const MINOR_VERSION: u16 = 1;

pub const OP_SET: u8 = 0;
pub const OP_UNION: u8 = 1;
pub const OP_INTERSECT: u8 = 2;
pub const OP_SUBTRACT: u8 = 3;
pub const OP_INVERT: u8 = 4;

pub const KIND_BOUNDING: u8 = 0;
pub const KIND_CLIP: u8 = 1;
pub const KIND_INPUT: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RectanglesRequest {
    pub op: u8,
    pub dest_kind: u8,
    pub ordering: u8,
    pub dest: u32,
    pub x_off: i16,
    pub y_off: i16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaskRequest {
    pub op: u8,
    pub dest_kind: u8,
    pub dest: u32,
    pub x_off: i16,
    pub y_off: i16,
    pub src: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CombineRequest {
    pub op: u8,
    pub dest_kind: u8,
    pub src_kind: u8,
    pub dest: u32,
    pub x_off: i16,
    pub y_off: i16,
    pub src: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OffsetRequest {
    pub dest_kind: u8,
    pub dest: u32,
    pub x_off: i16,
    pub y_off: i16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectInputRequest {
    pub window: u32,
    pub enable: bool,
}

fn read_u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_i16_le(bytes: &[u8]) -> i16 {
    i16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn fixed_reply(sequence: SequenceNumber, data: u8, length: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(data);
    out.extend_from_slice(&sequence.0.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out
}

#[must_use]
pub fn parse_rectangles_request(body: &[u8]) -> Option<(RectanglesRequest, Vec<RegionRect>)> {
    if body.len() < 12 {
        return None;
    }
    let req = RectanglesRequest {
        op: body[0],
        dest_kind: body[1],
        ordering: body[2],
        dest: read_u32_le(&body[4..]),
        x_off: read_i16_le(&body[8..]),
        y_off: read_i16_le(&body[10..]),
    };
    Some((req, parse_rectangles(&body[12..])))
}

#[must_use]
pub fn parse_mask_request(body: &[u8]) -> Option<MaskRequest> {
    if body.len() < 16 {
        return None;
    }
    Some(MaskRequest {
        op: body[0],
        dest_kind: body[1],
        dest: read_u32_le(&body[4..]),
        x_off: read_i16_le(&body[8..]),
        y_off: read_i16_le(&body[10..]),
        src: read_u32_le(&body[12..]),
    })
}

#[must_use]
pub fn parse_combine_request(body: &[u8]) -> Option<CombineRequest> {
    if body.len() < 16 {
        return None;
    }
    Some(CombineRequest {
        op: body[0],
        dest_kind: body[1],
        src_kind: body[2],
        dest: read_u32_le(&body[4..]),
        x_off: read_i16_le(&body[8..]),
        y_off: read_i16_le(&body[10..]),
        src: read_u32_le(&body[12..]),
    })
}

#[must_use]
pub fn parse_offset_request(body: &[u8]) -> Option<OffsetRequest> {
    if body.len() < 12 {
        return None;
    }
    Some(OffsetRequest {
        dest_kind: body[0],
        dest: read_u32_le(&body[4..]),
        x_off: read_i16_le(&body[8..]),
        y_off: read_i16_le(&body[10..]),
    })
}

#[must_use]
pub fn parse_select_input_request(body: &[u8]) -> Option<SelectInputRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(SelectInputRequest {
        window: read_u32_le(body),
        enable: body[4] != 0,
    })
}

#[must_use]
pub fn parse_window(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

#[must_use]
pub fn parse_get_rectangles_request(body: &[u8]) -> Option<(u32, u8)> {
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), body[4]))
}

#[must_use]
pub fn parse_rectangles(mut bytes: &[u8]) -> Vec<RegionRect> {
    let mut rects = Vec::new();
    while bytes.len() >= 8 {
        let rect = RegionRect {
            x: read_i16_le(bytes),
            y: read_i16_le(&bytes[2..]),
            width: read_u16_le(&bytes[4..]),
            height: read_u16_le(&bytes[6..]),
        };
        if !rect.is_empty() {
            rects.push(rect);
        }
        bytes = &bytes[8..];
    }
    rects
}

#[must_use]
pub fn encode_query_version_reply(sequence: SequenceNumber) -> Vec<u8> {
    let mut out = fixed_reply(sequence, 0, 0);
    out.extend_from_slice(&MAJOR_VERSION.to_le_bytes());
    out.extend_from_slice(&MINOR_VERSION.to_le_bytes());
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_query_extents_reply(
    sequence: SequenceNumber,
    bounding_shaped: bool,
    clip_shaped: bool,
    bounding: RegionRect,
    clip: RegionRect,
) -> Vec<u8> {
    let mut out = fixed_reply(sequence, 0, 0);
    out.push(u8::from(bounding_shaped));
    out.push(u8::from(clip_shaped));
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&bounding.x.to_le_bytes());
    out.extend_from_slice(&bounding.y.to_le_bytes());
    out.extend_from_slice(&bounding.width.to_le_bytes());
    out.extend_from_slice(&bounding.height.to_le_bytes());
    out.extend_from_slice(&clip.x.to_le_bytes());
    out.extend_from_slice(&clip.y.to_le_bytes());
    out.extend_from_slice(&clip.width.to_le_bytes());
    out.extend_from_slice(&clip.height.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_input_selected_reply(sequence: SequenceNumber, enabled: bool) -> Vec<u8> {
    let mut out = fixed_reply(sequence, u8::from(enabled), 0);
    out.extend_from_slice(&[0u8; 24]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_get_rectangles_reply(
    sequence: SequenceNumber,
    ordering: u8,
    rects: &[RegionRect],
) -> Vec<u8> {
    #[allow(clippy::cast_possible_truncation)]
    let length = (rects.len() * 2) as u32;
    let mut out = fixed_reply(sequence, ordering, length);
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(rects.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 20]);
    for rect in rects {
        out.extend_from_slice(&rect.x.to_le_bytes());
        out.extend_from_slice(&rect.y.to_le_bytes());
        out.extend_from_slice(&rect.width.to_le_bytes());
        out.extend_from_slice(&rect.height.to_le_bytes());
    }
    debug_assert_eq!(out.len(), 32 + rects.len() * 8);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_version_reply_shape() {
        let reply = encode_query_version_reply(SequenceNumber(4));
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(reply[8..10].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(reply[10..12].try_into().unwrap()), 1);
    }

    #[test]
    fn query_extents_reply_shape() {
        let rect = RegionRect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        };
        let reply = encode_query_extents_reply(SequenceNumber(8), true, false, rect, rect);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[8], 1);
        assert_eq!(reply[9], 0);
        assert_eq!(i16::from_le_bytes(reply[12..14].try_into().unwrap()), 1);
    }

    #[test]
    fn input_selected_reply_shape() {
        let reply = encode_input_selected_reply(SequenceNumber(8), true);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[1], 1);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
    }

    #[test]
    fn get_rectangles_reply_shape() {
        let rects = [RegionRect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        }];
        let reply = encode_get_rectangles_reply(SequenceNumber(9), 0, &rects);
        assert_eq!(reply.len(), 40);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 1);
        assert_eq!(i16::from_le_bytes(reply[32..34].try_into().unwrap()), 1);
    }
}
