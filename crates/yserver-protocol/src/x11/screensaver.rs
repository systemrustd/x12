//! MIT-SCREEN-SAVER extension wire codecs.
//!
//! Spec: `/usr/include/X11/extensions/saver.h` + `saverproto.h`.
//! Behaviour reference: `/home/jos/Projects/xserver/Xext/saver.c`.

use super::{
    ClientByteOrder, SequenceNumber,
    wire::{fixed_reply, write_u16, write_u32},
};

// Minor opcodes.
pub const QUERY_VERSION: u8 = 0;
pub const QUERY_INFO: u8 = 1;
pub const SELECT_INPUT: u8 = 2;
pub const SET_ATTRIBUTES: u8 = 3;
pub const UNSET_ATTRIBUTES: u8 = 4;
pub const SUSPEND: u8 = 5;

// Protocol version reported by QueryVersion.
pub const SERVER_MAJOR_VERSION: u16 = 1;
pub const SERVER_MINOR_VERSION: u16 = 1;

// SelectInput event-mask bits.
pub const SCREEN_SAVER_NOTIFY_MASK: u32 = 0x0000_0001;
pub const SCREEN_SAVER_CYCLE_MASK: u32 = 0x0000_0002;

// Notify state values.
pub const SCREEN_SAVER_OFF: u8 = 0;
pub const SCREEN_SAVER_ON: u8 = 1;
pub const SCREEN_SAVER_CYCLE: u8 = 2;
pub const SCREEN_SAVER_DISABLED: u8 = 3;

// Kind values.
pub const SCREEN_SAVER_BLANKED: u8 = 0;
pub const SCREEN_SAVER_INTERNAL: u8 = 1;
pub const SCREEN_SAVER_EXTERNAL: u8 = 2;

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[must_use]
pub fn parse_query_info_request(body: &[u8]) -> Option<u32> {
    // Layout: drawable:u32
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

#[must_use]
pub fn parse_select_input_request(body: &[u8]) -> Option<(u32, u32)> {
    // Layout: drawable:u32 event_mask:u32
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..])))
}

#[must_use]
pub fn parse_unset_attributes_request(body: &[u8]) -> Option<u32> {
    // Layout: drawable:u32
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

#[must_use]
pub fn parse_suspend_request(body: &[u8]) -> Option<bool> {
    // Layout: suspend:BOOL pad[3]
    if body.is_empty() {
        return None;
    }
    Some(body[0] != 0)
}

#[must_use]
pub fn encode_query_version_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    server_major: u16,
    server_minor: u16,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0, 0);
    write_u16(byte_order, &mut out, server_major);
    write_u16(byte_order, &mut out, server_minor);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// QueryInfo reply: state(1) window(8) til_or_since(12) idle(16)
/// event_mask(20) kind(24) pads to 32. `state` lands in the
/// fixed_reply's per-request byte slot (`data` field at offset 1).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn encode_query_info_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    state: u8,
    window: u32,
    til_or_since: u32,
    idle: u32,
    event_mask: u32,
    kind: u8,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, state, 0);
    write_u32(byte_order, &mut out, window);
    write_u32(byte_order, &mut out, til_or_since);
    write_u32(byte_order, &mut out, idle);
    write_u32(byte_order, &mut out, event_mask);
    out.push(kind);
    out.extend_from_slice(&[0u8; 7]); // pad to 32
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encode `ScreenSaverNotify` as a sequential event (NOT XGE).
/// 32 bytes total: type(0) state(1) seq(2-3) timestamp(4-7) root(8-11)
/// window(12-15) kind(16) forced(17) pad to 32.
#[allow(clippy::too_many_arguments)]
pub fn encode_screen_saver_notify_event(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    first_event: u8,
    state: u8,
    timestamp: u32,
    root: u32,
    window: u32,
    kind: u8,
    forced: bool,
) {
    let start = out.len();
    out.push(first_event);
    out.push(state);
    write_u16(byte_order, out, sequence.0);
    write_u32(byte_order, out, timestamp);
    write_u32(byte_order, out, root);
    write_u32(byte_order, out, window);
    out.push(kind);
    out.push(u8::from(forced));
    out.extend_from_slice(&[0u8; 14]); // pad to 32
    debug_assert_eq!(out.len() - start, 32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x11::{ClientByteOrder::LittleEndian, SequenceNumber};

    #[test]
    fn parse_query_info_extracts_drawable() {
        let body = [0xab, 0xcd, 0xef, 0x12];
        assert_eq!(parse_query_info_request(&body), Some(0x12ef_cdab));
    }

    #[test]
    fn parse_select_input_extracts_drawable_and_mask() {
        // drawable:u32, event_mask:u32
        let body = [0x44, 0x33, 0x22, 0x11, 0x03, 0x00, 0x00, 0x00];
        assert_eq!(
            parse_select_input_request(&body),
            Some((0x1122_3344, 0x0000_0003))
        );
    }

    #[test]
    fn parse_suspend_extracts_bool() {
        assert_eq!(parse_suspend_request(&[1, 0, 0, 0]), Some(true));
        assert_eq!(parse_suspend_request(&[0, 0, 0, 0]), Some(false));
        assert_eq!(parse_suspend_request(&[]), None);
    }

    #[test]
    fn encode_query_info_reply_shape() {
        // saverproto.h: state(1) window(8) til_or_since(12) idle(16)
        // event_mask(20) kind(24) pads to 32.
        let buf = encode_query_info_reply(
            LittleEndian,
            SequenceNumber(0x5555),
            SCREEN_SAVER_ON,
            0xdead_beef,
            12345,
            67890,
            SCREEN_SAVER_NOTIFY_MASK | SCREEN_SAVER_CYCLE_MASK,
            SCREEN_SAVER_BLANKED,
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 1, "reply tag");
        assert_eq!(buf[1], SCREEN_SAVER_ON, "state at offset 1");
        assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0x5555, "sequence");
        assert_eq!(
            u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            0,
            "length"
        );
        assert_eq!(
            u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            0xdead_beef,
            "window at offset 8"
        );
        assert_eq!(
            u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            12345,
            "til_or_since at offset 12"
        );
        assert_eq!(
            u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
            67890,
            "idle at offset 16"
        );
        assert_eq!(
            u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]),
            SCREEN_SAVER_NOTIFY_MASK | SCREEN_SAVER_CYCLE_MASK,
            "event_mask at offset 20"
        );
        assert_eq!(buf[24], SCREEN_SAVER_BLANKED, "kind at offset 24");
    }

    #[test]
    fn encode_screen_saver_notify_event_shape() {
        // saverproto.h: type(0) state(1) seq(2-3) timestamp(4-7)
        // root(8-11) window(12-15) kind(16) forced(17) pads to 32.
        let mut buf = Vec::new();
        encode_screen_saver_notify_event(
            &mut buf,
            LittleEndian,
            SequenceNumber(0xabcd),
            162,
            SCREEN_SAVER_ON,
            0x1234_5678,
            0xcafe_f00d,
            0,
            SCREEN_SAVER_BLANKED,
            true,
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 162, "event type = first_event + 0");
        assert_eq!(buf[1], SCREEN_SAVER_ON, "state at offset 1");
        assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0xabcd, "sequence");
        assert_eq!(
            u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            0x1234_5678,
            "timestamp at offset 4"
        );
        assert_eq!(
            u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            0xcafe_f00d,
            "root at offset 8"
        );
        assert_eq!(
            u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            0,
            "window at offset 12 (always 0 — no SetAttributes path)"
        );
        assert_eq!(buf[16], SCREEN_SAVER_BLANKED, "kind at offset 16");
        assert_eq!(buf[17], 1, "forced at offset 17");
    }
}
