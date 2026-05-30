//! DPMS extension wire codecs.
//!
//! Spec: `/usr/include/X11/extensions/dpmsproto.h`.
//! Behaviour reference: `/home/jos/Projects/xserver/Xext/dpms.c`.

use super::{
    ClientByteOrder, SequenceNumber,
    wire::{fixed_reply, write_u16, write_u32},
};

// Minor opcodes (xDPMSGetVersion=0 … xDPMSSelectInput=8).
pub const GET_VERSION: u8 = 0;
pub const CAPABLE: u8 = 1;
pub const GET_TIMEOUTS: u8 = 2;
pub const SET_TIMEOUTS: u8 = 3;
pub const ENABLE: u8 = 4;
pub const DISABLE: u8 = 5;
pub const FORCE_LEVEL: u8 = 6;
pub const INFO: u8 = 7;
pub const SELECT_INPUT: u8 = 8;

// Protocol version reported by GetVersion.
pub const MAJOR_VERSION: u16 = 1;
pub const MINOR_VERSION: u16 = 2;

// Power levels (xDPMSInfo `power_level`).
pub const DPMS_MODE_ON: u16 = 0;
pub const DPMS_MODE_STANDBY: u16 = 1;
pub const DPMS_MODE_SUSPEND: u16 = 2;
pub const DPMS_MODE_OFF: u16 = 3;

// SelectInput event-mask bit.
pub const DPMS_INFO_NOTIFY_MASK: u32 = 0x0000_0001;

// XGE event type for DPMSInfoNotify (only one).
pub const EVENT_INFO_NOTIFY: u16 = 0;

fn read_u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[must_use]
pub fn parse_set_timeouts_request(body: &[u8]) -> Option<(u16, u16, u16)> {
    // Layout: standby:u16 suspend:u16 off:u16 pad:u16
    if body.len() < 8 {
        return None;
    }
    Some((
        read_u16_le(body),
        read_u16_le(&body[2..]),
        read_u16_le(&body[4..]),
    ))
}

#[must_use]
pub fn parse_force_level_request(body: &[u8]) -> Option<u16> {
    // Layout: power_level:u16 pad:u16
    if body.len() < 4 {
        return None;
    }
    Some(read_u16_le(body))
}

#[must_use]
pub fn parse_select_input_request(body: &[u8]) -> Option<u32> {
    // Layout: event_mask:u32
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

#[must_use]
pub fn encode_get_version_reply(
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

#[must_use]
pub fn encode_capable_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    capable: bool,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0, 0);
    out.push(u8::from(capable));
    out.extend_from_slice(&[0u8; 23]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_get_timeouts_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    standby: u16,
    suspend: u16,
    off: u16,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0, 0);
    write_u16(byte_order, &mut out, standby);
    write_u16(byte_order, &mut out, suspend);
    write_u16(byte_order, &mut out, off);
    out.extend_from_slice(&[0u8; 18]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_info_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    power_level: u16,
    state: bool,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0, 0);
    write_u16(byte_order, &mut out, power_level);
    out.push(u8::from(state));
    out.extend_from_slice(&[0u8; 21]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encode `DPMSInfoNotify` as a GenericEvent (XGE). 32 bytes total:
/// 12-byte GE header, then timestamp:u32 (offset 12), power_level:u16
/// (offset 16), state:u8 (offset 18), 13 bytes pad.
pub fn encode_dpms_info_notify_event(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    extension_opcode: u8,
    timestamp: u32,
    power_level: u16,
    state: bool,
) {
    let start = out.len();
    out.push(35); // GenericEvent
    out.push(extension_opcode);
    write_u16(byte_order, out, sequence.0);
    write_u32(byte_order, out, 0); // length = 0 (no overflow past 32-byte slot)
    write_u16(byte_order, out, EVENT_INFO_NOTIFY);
    write_u16(byte_order, out, 0); // pad
    write_u32(byte_order, out, timestamp);
    write_u16(byte_order, out, power_level);
    out.push(u8::from(state));
    out.extend_from_slice(&[0u8; 13]);
    debug_assert_eq!(out.len() - start, 32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x11::{ClientByteOrder::LittleEndian, SequenceNumber};

    #[test]
    fn parse_set_timeouts_round_trip() {
        // body: standby:u16 | suspend:u16 | off:u16 | pad:u16
        let body = [0x2c, 0x01, 0x58, 0x02, 0x84, 0x03, 0, 0];
        let parsed = parse_set_timeouts_request(&body).expect("parse");
        assert_eq!(parsed, (300, 600, 900));
    }

    #[test]
    fn parse_set_timeouts_rejects_short_body() {
        assert!(parse_set_timeouts_request(&[0; 6]).is_none());
    }

    #[test]
    fn parse_force_level_extracts_card16() {
        // body: power_level:u16 | pad:u16
        let body = [3, 0, 0, 0];
        assert_eq!(parse_force_level_request(&body), Some(3));
    }

    #[test]
    fn parse_select_input_extracts_card32() {
        let body = [1, 0, 0, 0];
        assert_eq!(parse_select_input_request(&body), Some(1));
    }

    #[test]
    fn parse_force_level_rejects_short_body() {
        assert!(parse_force_level_request(&[0; 2]).is_none());
    }

    #[test]
    fn parse_select_input_rejects_short_body() {
        assert!(parse_select_input_request(&[0; 2]).is_none());
    }

    #[test]
    fn get_version_reply_shape() {
        let buf = encode_get_version_reply(LittleEndian, SequenceNumber(0x1234), 1, 2);
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 1, "reply tag");
        assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0x1234, "sequence");
        assert_eq!(
            u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            0,
            "length"
        );
        assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), 1, "server_major");
        assert_eq!(u16::from_le_bytes([buf[10], buf[11]]), 2, "server_minor");
    }

    #[test]
    fn capable_reply_carries_bool_at_byte_8() {
        let yes = encode_capable_reply(LittleEndian, SequenceNumber(7), true);
        assert_eq!(yes.len(), 32);
        assert_eq!(yes[8], 1);
        let no = encode_capable_reply(LittleEndian, SequenceNumber(7), false);
        assert_eq!(no[8], 0);
    }

    #[test]
    fn get_timeouts_reply_shape() {
        let buf = encode_get_timeouts_reply(LittleEndian, SequenceNumber(9), 300, 600, 900);
        assert_eq!(buf.len(), 32);
        assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), 300, "standby");
        assert_eq!(u16::from_le_bytes([buf[10], buf[11]]), 600, "suspend");
        assert_eq!(u16::from_le_bytes([buf[12], buf[13]]), 900, "off");
    }

    #[test]
    fn info_reply_shape() {
        let buf = encode_info_reply(LittleEndian, SequenceNumber(11), DPMS_MODE_OFF, true);
        assert_eq!(buf.len(), 32);
        assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), DPMS_MODE_OFF);
        assert_eq!(buf[10], 1, "state byte = enabled");
    }

    #[test]
    fn info_notify_event_shape() {
        let mut buf = Vec::new();
        encode_dpms_info_notify_event(
            &mut buf,
            LittleEndian,
            SequenceNumber(0xabcd),
            134, // extension major
            0x1234_5678,
            DPMS_MODE_STANDBY,
            true,
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 35, "GenericEvent tag");
        assert_eq!(buf[1], 134, "extension opcode");
        assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0xabcd, "sequence");
        assert_eq!(
            u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            0,
            "length=0"
        );
        assert_eq!(
            u16::from_le_bytes([buf[8], buf[9]]),
            EVENT_INFO_NOTIFY,
            "evtype"
        );
        assert_eq!(
            u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            0x1234_5678
        );
        assert_eq!(u16::from_le_bytes([buf[16], buf[17]]), DPMS_MODE_STANDBY);
        assert_eq!(buf[18], 1, "state");
    }
}
