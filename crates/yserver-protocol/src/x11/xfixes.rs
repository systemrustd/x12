use super::SequenceNumber;

pub const QUERY_VERSION: u8 = 0;
pub const SELECT_SELECTION_INPUT: u8 = 2;
pub const SELECT_CURSOR_INPUT: u8 = 3;
pub const GET_CURSOR_IMAGE: u8 = 4;
pub const CREATE_REGION: u8 = 5;
pub const CREATE_REGION_FROM_BITMAP: u8 = 6;
pub const CREATE_REGION_FROM_WINDOW: u8 = 7;
pub const CREATE_REGION_FROM_GC: u8 = 8;
pub const DESTROY_REGION: u8 = 10;
pub const SET_REGION: u8 = 11;
pub const COPY_REGION: u8 = 12;
pub const UNION_REGION: u8 = 13;
pub const INTERSECT_REGION: u8 = 14;
pub const SUBTRACT_REGION: u8 = 15;
pub const INVERT_REGION: u8 = 16;
pub const TRANSLATE_REGION: u8 = 17;
pub const REGION_EXTENTS: u8 = 18;
pub const FETCH_REGION: u8 = 19;
pub const HIDE_CURSOR: u8 = 29;
pub const SHOW_CURSOR: u8 = 30;

pub const MAJOR_VERSION: u32 = 2;
pub const MINOR_VERSION: u32 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionRect {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

impl RegionRect {
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectSelectionInputRequest {
    pub window: u32,
    pub selection: u32,
    pub event_mask: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectCursorInputRequest {
    pub window: u32,
    pub event_mask: u32,
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

fn fixed_reply(sequence: SequenceNumber, length: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    out.extend_from_slice(&sequence.0.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out
}

#[must_use]
pub fn parse_u32_pair(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..])))
}

#[must_use]
pub fn parse_u32_triplet(body: &[u8]) -> Option<(u32, u32, u32)> {
    if body.len() < 12 {
        return None;
    }
    Some((
        read_u32_le(body),
        read_u32_le(&body[4..]),
        read_u32_le(&body[8..]),
    ))
}

#[must_use]
pub fn parse_select_selection_input(body: &[u8]) -> Option<SelectSelectionInputRequest> {
    if body.len() < 12 {
        return None;
    }
    Some(SelectSelectionInputRequest {
        window: read_u32_le(body),
        selection: read_u32_le(&body[4..]),
        event_mask: read_u32_le(&body[8..]),
    })
}

#[must_use]
pub fn parse_select_cursor_input(body: &[u8]) -> Option<SelectCursorInputRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(SelectCursorInputRequest {
        window: read_u32_le(body),
        event_mask: read_u32_le(&body[4..]),
    })
}

#[must_use]
pub fn parse_create_region(body: &[u8]) -> Option<(u32, Vec<RegionRect>)> {
    if body.len() < 4 {
        return None;
    }
    Some((read_u32_le(body), parse_rectangles(&body[4..])))
}

#[must_use]
pub fn parse_translate_region(body: &[u8]) -> Option<(u32, i16, i16)> {
    if body.len() < 8 {
        return None;
    }
    Some((
        read_u32_le(body),
        read_i16_le(&body[4..]),
        read_i16_le(&body[6..]),
    ))
}

#[must_use]
pub fn parse_invert_region(body: &[u8]) -> Option<(u32, RegionRect, u32)> {
    if body.len() < 16 {
        return None;
    }
    Some((
        read_u32_le(body),
        RegionRect {
            x: read_i16_le(&body[4..]),
            y: read_i16_le(&body[6..]),
            width: read_u16_le(&body[8..]),
            height: read_u16_le(&body[10..]),
        },
        read_u32_le(&body[12..]),
    ))
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
pub fn encode_query_version_reply(sequence: SequenceNumber, major: u32, minor: u32) -> Vec<u8> {
    let mut out = fixed_reply(sequence, 0);
    out.extend_from_slice(&major.to_le_bytes());
    out.extend_from_slice(&minor.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_get_cursor_image_empty_reply(sequence: SequenceNumber) -> Vec<u8> {
    let mut out = fixed_reply(sequence, 0);
    out.extend_from_slice(&0i16.to_le_bytes()); // x
    out.extend_from_slice(&0i16.to_le_bytes()); // y
    out.extend_from_slice(&0u16.to_le_bytes()); // width
    out.extend_from_slice(&0u16.to_le_bytes()); // height
    out.extend_from_slice(&0u16.to_le_bytes()); // xhot
    out.extend_from_slice(&0u16.to_le_bytes()); // yhot
    out.extend_from_slice(&0u32.to_le_bytes()); // cursor serial
    out.extend_from_slice(&[0u8; 8]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_fetch_region_reply(
    sequence: SequenceNumber,
    extents: RegionRect,
    rects: &[RegionRect],
) -> Vec<u8> {
    #[allow(clippy::cast_possible_truncation)]
    let length = (rects.len() * 2) as u32;
    let mut out = fixed_reply(sequence, length);
    out.extend_from_slice(&extents.x.to_le_bytes());
    out.extend_from_slice(&extents.y.to_le_bytes());
    out.extend_from_slice(&extents.width.to_le_bytes());
    out.extend_from_slice(&extents.height.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
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
        let reply = encode_query_version_reply(SequenceNumber(7), MAJOR_VERSION, MINOR_VERSION);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(u16::from_le_bytes([reply[2], reply[3]]), 7);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(
            u32::from_le_bytes(reply[8..12].try_into().unwrap()),
            MAJOR_VERSION
        );
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 0);
    }

    #[test]
    fn cursor_image_empty_reply_shape() {
        let reply = encode_get_cursor_image_empty_reply(SequenceNumber(9));
        assert_eq!(reply.len(), 32);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(reply[12..14].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(reply[14..16].try_into().unwrap()), 0);
    }

    #[test]
    fn fetch_region_reply_shape() {
        let rects = [RegionRect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        }];
        let reply = encode_fetch_region_reply(SequenceNumber(3), rects[0], &rects);
        assert_eq!(reply.len(), 40);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 2);
        assert_eq!(i16::from_le_bytes(reply[8..10].try_into().unwrap()), 1);
        assert_eq!(i16::from_le_bytes(reply[32..34].try_into().unwrap()), 1);
    }
}
