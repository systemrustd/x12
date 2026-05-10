use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_u16, write_u32},
};

pub const INITIALIZE: u8 = 0;
pub const LIST_SYSTEM_COUNTERS: u8 = 1;
pub const CREATE_COUNTER: u8 = 2;
pub const SET_COUNTER: u8 = 3;
pub const CHANGE_COUNTER: u8 = 4;
pub const QUERY_COUNTER: u8 = 5;
pub const DESTROY_COUNTER: u8 = 6;
pub const AWAIT: u8 = 7;
pub const CREATE_ALARM: u8 = 8;
pub const CHANGE_ALARM: u8 = 9;
pub const QUERY_ALARM: u8 = 10;
pub const DESTROY_ALARM: u8 = 11;
pub const SET_PRIORITY: u8 = 12;
pub const GET_PRIORITY: u8 = 13;
pub const CREATE_FENCE: u8 = 14;
pub const TRIGGER_FENCE: u8 = 15;
pub const RESET_FENCE: u8 = 16;
pub const DESTROY_FENCE: u8 = 17;
pub const QUERY_FENCE: u8 = 18;
pub const AWAIT_FENCE: u8 = 19;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateFenceRequest {
    pub drawable: u32,
    pub fence: u32,
    pub initially_triggered: bool,
}

pub const MAJOR_VERSION: u8 = 3;
pub const MINOR_VERSION: u8 = 0;

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_i32_le(bytes: &[u8]) -> i32 {
    i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_i64(hi: &[u8], lo: &[u8]) -> i64 {
    (i64::from(read_i32_le(hi)) << 32) | i64::from(read_u32_le(lo))
}

fn write_i64(byte_order: ClientByteOrder, out: &mut Vec<u8>, value: i64) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hi = (value >> 32) as u32;
    #[allow(clippy::cast_possible_truncation)]
    let lo = value as u32;
    write_u32(byte_order, out, hi);
    write_u32(byte_order, out, lo);
}

fn fixed_reply(byte_order: ClientByteOrder, sequence: SequenceNumber, length: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length);
    out
}

#[must_use]
pub fn parse_initialize(body: &[u8]) -> Option<(u8, u8)> {
    if body.len() < 4 {
        return None;
    }
    Some((body[0], body[1]))
}

#[must_use]
pub fn parse_counter_value(body: &[u8]) -> Option<(u32, i64)> {
    if body.len() < 12 {
        return None;
    }
    Some((read_u32_le(body), read_i64(&body[4..], &body[8..])))
}

#[must_use]
pub fn parse_resource(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

#[must_use]
pub fn parse_alarm_with_mask(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..])))
}

#[must_use]
pub fn parse_create_fence(body: &[u8]) -> Option<CreateFenceRequest> {
    // Body: drawable(4) fence(4) initially_triggered(1) pad(3) = 12B.
    if body.len() < 12 {
        return None;
    }
    Some(CreateFenceRequest {
        drawable: read_u32_le(body),
        fence: read_u32_le(&body[4..]),
        initially_triggered: body[8] != 0,
    })
}

#[must_use]
pub fn parse_await_fence(body: &[u8]) -> Option<Vec<u32>> {
    // Body: u32[n] fences. n is implicit from the body length /4.
    if !body.len().is_multiple_of(4) {
        return None;
    }
    Some(body.chunks_exact(4).map(read_u32_le).collect())
}

#[must_use]
pub fn encode_query_fence_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    triggered: bool,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0);
    out.push(u8::from(triggered));
    out.extend_from_slice(&[0u8; 23]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_initialize_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major: u8,
    minor: u8,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0);
    out.push(major);
    out.push(minor);
    write_u16(byte_order, &mut out, 0);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_list_system_counters_empty_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0);
    write_u32(byte_order, &mut out, 0); // counters_len = 0
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_query_counter_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    value: i64,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0);
    write_i64(byte_order, &mut out, value);
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_query_alarm_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    counter: u32,
    wait_value: i64,
    delta: i64,
    events: bool,
    state: u8,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 2);
    write_u32(byte_order, &mut out, counter);
    write_u32(byte_order, &mut out, 0); // absolute value
    write_i64(byte_order, &mut out, wait_value);
    write_u32(byte_order, &mut out, 0); // positive transition
    write_i64(byte_order, &mut out, delta);
    out.push(u8::from(events));
    out.push(state);
    out.extend_from_slice(&[0u8; 2]);
    debug_assert_eq!(out.len(), 40);
    out
}

#[must_use]
pub fn encode_get_priority_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    priority: i32,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0);
    #[allow(clippy::cast_sign_loss)]
    let p = priority as u32;
    write_u32(byte_order, &mut out, p);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_reply_shape() {
        let reply = encode_initialize_reply(ClientByteOrder::LittleEndian, SequenceNumber(2), 3, 0);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(reply[8], 3);
        assert_eq!(reply[9], 0);
    }

    #[test]
    fn query_counter_reply_shape() {
        let reply = encode_query_counter_reply(ClientByteOrder::LittleEndian, SequenceNumber(2), 5);
        assert_eq!(reply.len(), 32);
        assert_eq!(i32::from_le_bytes(reply[8..12].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 5);
    }

    #[test]
    fn create_fence_parses() {
        let mut body = vec![0u8; 12];
        body[0..4].copy_from_slice(&0x100u32.to_le_bytes());
        body[4..8].copy_from_slice(&0x500u32.to_le_bytes());
        body[8] = 1;
        let req = parse_create_fence(&body).unwrap();
        assert_eq!(req.drawable, 0x100);
        assert_eq!(req.fence, 0x500);
        assert!(req.initially_triggered);
    }

    #[test]
    fn await_fence_parses_list() {
        let mut body = vec![0u8; 12];
        body[0..4].copy_from_slice(&0x100u32.to_le_bytes());
        body[4..8].copy_from_slice(&0x200u32.to_le_bytes());
        body[8..12].copy_from_slice(&0x300u32.to_le_bytes());
        let list = parse_await_fence(&body).unwrap();
        assert_eq!(list, vec![0x100, 0x200, 0x300]);
    }

    #[test]
    fn await_fence_rejects_misaligned() {
        assert!(parse_await_fence(&[0u8; 7]).is_none());
    }

    #[test]
    fn query_fence_reply_shape() {
        let reply =
            encode_query_fence_reply(ClientByteOrder::LittleEndian, SequenceNumber(8), true);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1, "Reply opcode");
        assert_eq!(reply[8], 1, "triggered byte");
    }

    #[test]
    fn query_alarm_reply_shape() {
        let reply = encode_query_alarm_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(2),
            7,
            0,
            0,
            false,
            0,
        );
        assert_eq!(reply.len(), 40);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 7);
    }

    // Canonical SYNC minor-opcode values, sourced from
    // `/usr/include/X11/extensions/syncproto.h` and the xcbproto
    // `sync.xml` registry. A prior numbering bug shipped these as
    // 14/15/18/19/20/21, which made Mesa's `xcb_sync_trigger_fence`
    // route to our DESTROY_FENCE handler and hung clients in
    // `xshmfence_await`. Pin against the canonical table so any
    // future drift is caught at unit-test time.
    #[test]
    fn sync_fence_opcodes_match_canonical_table() {
        assert_eq!(CREATE_FENCE, 14, "X_SyncCreateFence");
        assert_eq!(TRIGGER_FENCE, 15, "X_SyncTriggerFence");
        assert_eq!(RESET_FENCE, 16, "X_SyncResetFence");
        assert_eq!(DESTROY_FENCE, 17, "X_SyncDestroyFence");
        assert_eq!(QUERY_FENCE, 18, "X_SyncQueryFence");
        assert_eq!(AWAIT_FENCE, 19, "X_SyncAwaitFence");
    }
}
