use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_i16, write_u16, write_u32},
};

pub const QUERY_VERSION: u8 = 0;
pub const CREATE: u8 = 1;
pub const DESTROY: u8 = 2;
pub const SUBTRACT: u8 = 3;
pub const ADD: u8 = 4;

pub const MAJOR_VERSION: u32 = 1;
pub const MINOR_VERSION: u32 = 1;

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
pub fn parse_create(body: &[u8]) -> Option<(u32, u32, u8)> {
    if body.len() < 12 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..]), body[8]))
}

#[must_use]
pub fn parse_resource(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

#[must_use]
pub fn parse_subtract(body: &[u8]) -> Option<(u32, u32, u32)> {
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
pub fn parse_add(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..])))
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

/// Damage report level a client requested when creating the damage object.
/// Spec values: 0 = RawRectangles, 1 = DeltaRectangles, 2 = BoundingBox,
/// 3 = NonEmpty.
pub mod report_level {
    pub const RAW_RECTANGLES: u8 = 0;
    pub const DELTA_RECTANGLES: u8 = 1;
    pub const BOUNDING_BOX: u8 = 2;
    pub const NON_EMPTY: u8 = 3;
}

/// Bit 7 of the `level` byte signals that more `DamageNotify` events follow
/// inside the same Subtract cycle. Last event in a cycle has it cleared.
pub const MORE_FLAG: u8 = 0x80;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rectangle {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// Encode a `DamageNotify` event (event type = `first_event + 0`). The
/// `level` byte's high bit must already include the `MORE_FLAG` if more
/// events follow this one in the current Subtract cycle.
#[must_use]
#[allow(clippy::too_many_arguments)] // wire encoder — fields are part of the protocol
pub fn encode_damage_notify_event(
    byte_order: ClientByteOrder,
    first_event: u8,
    sequence: SequenceNumber,
    level: u8,
    drawable: u32,
    damage: u32,
    timestamp: u32,
    area: Rectangle,
    geometry: Rectangle,
) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[0] = first_event;
    out[1] = level;
    let mut tmp: Vec<u8> = Vec::with_capacity(2);
    write_u16(byte_order, &mut tmp, sequence.0);
    out[2..4].copy_from_slice(&tmp);
    let mut tmp4: Vec<u8> = Vec::with_capacity(4);
    write_u32(byte_order, &mut tmp4, drawable);
    out[4..8].copy_from_slice(&tmp4);
    tmp4.clear();
    write_u32(byte_order, &mut tmp4, damage);
    out[8..12].copy_from_slice(&tmp4);
    tmp4.clear();
    write_u32(byte_order, &mut tmp4, timestamp);
    out[12..16].copy_from_slice(&tmp4);
    tmp.clear();
    write_i16(byte_order, &mut tmp, area.x);
    out[16..18].copy_from_slice(&tmp);
    tmp.clear();
    write_i16(byte_order, &mut tmp, area.y);
    out[18..20].copy_from_slice(&tmp);
    tmp.clear();
    write_u16(byte_order, &mut tmp, area.width);
    out[20..22].copy_from_slice(&tmp);
    tmp.clear();
    write_u16(byte_order, &mut tmp, area.height);
    out[22..24].copy_from_slice(&tmp);
    tmp.clear();
    write_i16(byte_order, &mut tmp, geometry.x);
    out[24..26].copy_from_slice(&tmp);
    tmp.clear();
    write_i16(byte_order, &mut tmp, geometry.y);
    out[26..28].copy_from_slice(&tmp);
    tmp.clear();
    write_u16(byte_order, &mut tmp, geometry.width);
    out[28..30].copy_from_slice(&tmp);
    tmp.clear();
    write_u16(byte_order, &mut tmp, geometry.height);
    out[30..32].copy_from_slice(&tmp);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_version_reply_shape() {
        let reply =
            encode_query_version_reply(ClientByteOrder::LittleEndian, SequenceNumber(6), 1, 1);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 1);
    }

    #[test]
    fn damage_notify_event_wire_layout() {
        let area = Rectangle {
            x: 5,
            y: 6,
            width: 7,
            height: 8,
        };
        let geometry = Rectangle {
            x: 0,
            y: 0,
            width: 100,
            height: 200,
        };
        let evt = encode_damage_notify_event(
            ClientByteOrder::LittleEndian,
            94,
            SequenceNumber(0xabcd),
            report_level::NON_EMPTY,
            0xdead_beef,
            0xcafe_babe,
            0x12345678,
            area,
            geometry,
        );
        assert_eq!(evt[0], 94, "type = first_event");
        assert_eq!(evt[1], report_level::NON_EMPTY);
        assert_eq!(u16::from_le_bytes([evt[2], evt[3]]), 0xabcd);
        assert_eq!(
            u32::from_le_bytes(evt[4..8].try_into().unwrap()),
            0xdead_beef
        );
        assert_eq!(
            u32::from_le_bytes(evt[8..12].try_into().unwrap()),
            0xcafe_babe
        );
        assert_eq!(
            u32::from_le_bytes(evt[12..16].try_into().unwrap()),
            0x12345678
        );
        assert_eq!(i16::from_le_bytes([evt[16], evt[17]]), 5);
        assert_eq!(u16::from_le_bytes([evt[20], evt[21]]), 7);
        assert_eq!(u16::from_le_bytes([evt[28], evt[29]]), 100);
    }

    #[test]
    fn damage_notify_more_flag_distinguishes_intermediate_from_final() {
        let r = Rectangle {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let intermediate = encode_damage_notify_event(
            ClientByteOrder::LittleEndian,
            94,
            SequenceNumber(1),
            report_level::DELTA_RECTANGLES | MORE_FLAG,
            0,
            0,
            0,
            r,
            r,
        );
        let last = encode_damage_notify_event(
            ClientByteOrder::LittleEndian,
            94,
            SequenceNumber(1),
            report_level::DELTA_RECTANGLES,
            0,
            0,
            0,
            r,
            r,
        );
        assert_eq!(intermediate[1] & MORE_FLAG, MORE_FLAG);
        assert_eq!(last[1] & MORE_FLAG, 0);
    }
}
