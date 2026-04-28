use std::io;
use std::io::ErrorKind;

use super::{ClientByteOrder, SequenceNumber};

pub(super) fn read_u16(byte_order: ClientByteOrder, bytes: &[u8]) -> u16 {
    match byte_order {
        ClientByteOrder::LittleEndian => u16::from_le_bytes([bytes[0], bytes[1]]),
        ClientByteOrder::BigEndian => u16::from_be_bytes([bytes[0], bytes[1]]),
    }
}

pub(super) fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub(super) fn read_u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

pub(super) fn read_i16_le(bytes: &[u8]) -> i16 {
    i16::from_le_bytes([bytes[0], bytes[1]])
}

pub(super) fn write_u16(byte_order: ClientByteOrder, out: &mut Vec<u8>, value: u16) {
    match byte_order {
        ClientByteOrder::LittleEndian => out.extend_from_slice(&value.to_le_bytes()),
        ClientByteOrder::BigEndian => out.extend_from_slice(&value.to_be_bytes()),
    }
}

pub(super) fn write_i16(byte_order: ClientByteOrder, out: &mut Vec<u8>, value: i16) {
    match byte_order {
        ClientByteOrder::LittleEndian => out.extend_from_slice(&value.to_le_bytes()),
        ClientByteOrder::BigEndian => out.extend_from_slice(&value.to_be_bytes()),
    }
}

pub(super) fn write_u32(byte_order: ClientByteOrder, out: &mut Vec<u8>, value: u32) {
    match byte_order {
        ClientByteOrder::LittleEndian => out.extend_from_slice(&value.to_le_bytes()),
        ClientByteOrder::BigEndian => out.extend_from_slice(&value.to_be_bytes()),
    }
}

pub(super) fn byte_order_value(byte_order: ClientByteOrder) -> u8 {
    match byte_order {
        ClientByteOrder::LittleEndian => 0,
        ClientByteOrder::BigEndian => 1,
    }
}

pub(super) fn pad4(len: usize) -> usize {
    (len + 3) & !3
}

pub(super) fn pad_vec4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

pub(super) fn fixed_reply(sequence: SequenceNumber, data: u8, length: u32) -> Vec<u8> {
    let mut reply = Vec::with_capacity(32);
    reply.push(1);
    reply.push(data);
    write_u16(ClientByteOrder::LittleEndian, &mut reply, sequence.0);
    write_u32(ClientByteOrder::LittleEndian, &mut reply, length);
    reply
}

pub(super) fn checked_units(byte_len: usize) -> io::Result<u16> {
    if !byte_len.is_multiple_of(4) {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("length {byte_len} is not 4-byte aligned"),
        ));
    }
    u16::try_from(byte_len / 4).map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("length {byte_len} is too large"),
        )
    })
}
