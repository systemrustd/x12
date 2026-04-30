use super::SequenceNumber;

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
pub fn encode_query_version_reply(sequence: SequenceNumber, major: u32, minor: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    out.extend_from_slice(&sequence.0.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&major.to_le_bytes());
    out.extend_from_slice(&minor.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_version_reply_shape() {
        let reply = encode_query_version_reply(SequenceNumber(6), 1, 1);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 1);
    }
}
