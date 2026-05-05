use super::SequenceNumber;

pub const QUERY_VERSION: u8 = 0;
pub const PIXMAP: u8 = 1;
pub const NOTIFY_MSC: u8 = 2;
pub const SELECT_INPUT: u8 = 3;
pub const QUERY_CAPABILITIES: u8 = 4;
pub const PIXMAP_SYNCED: u8 = 5;

pub const MAJOR_VERSION: u32 = 1;
pub const MINOR_VERSION: u32 = 0;

pub const CAPABILITY_NONE: u32 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Notify {
    pub window: u32,
    pub serial: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PixmapRequest {
    pub window: u32,
    pub pixmap: u32,
    pub serial: u32,
    pub valid: u32,
    pub update: u32,
    pub x_off: i16,
    pub y_off: i16,
    pub target_crtc: u32,
    pub wait_fence: u32,
    pub idle_fence: u32,
    pub options: u32,
    pub target_msc: u64,
    pub divisor: u64,
    pub remainder: u64,
    pub notifies: Vec<Notify>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotifyMscRequest {
    pub window: u32,
    pub serial: u32,
    pub target_msc: u64,
    pub divisor: u64,
    pub remainder: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectInputRequest {
    pub eid: u32,
    pub window: u32,
    pub event_mask: u32,
}

fn read_i16_le(bytes: &[u8]) -> i16 {
    i16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u64_le(bytes: &[u8]) -> u64 {
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

#[must_use]
pub fn parse_query_version(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..])))
}

#[must_use]
pub fn parse_pixmap(body: &[u8]) -> Option<PixmapRequest> {
    if body.len() < 68 || !(body.len() - 68).is_multiple_of(8) {
        return None;
    }

    let notifies = body[68..]
        .chunks_exact(8)
        .map(|chunk| Notify {
            window: read_u32_le(chunk),
            serial: read_u32_le(&chunk[4..]),
        })
        .collect();

    Some(PixmapRequest {
        window: read_u32_le(body),
        pixmap: read_u32_le(&body[4..]),
        serial: read_u32_le(&body[8..]),
        valid: read_u32_le(&body[12..]),
        update: read_u32_le(&body[16..]),
        x_off: read_i16_le(&body[20..]),
        y_off: read_i16_le(&body[22..]),
        target_crtc: read_u32_le(&body[24..]),
        wait_fence: read_u32_le(&body[28..]),
        idle_fence: read_u32_le(&body[32..]),
        options: read_u32_le(&body[36..]),
        target_msc: read_u64_le(&body[44..]),
        divisor: read_u64_le(&body[52..]),
        remainder: read_u64_le(&body[60..]),
        notifies,
    })
}

#[must_use]
pub fn parse_notify_msc(body: &[u8]) -> Option<NotifyMscRequest> {
    if body.len() < 36 {
        return None;
    }
    Some(NotifyMscRequest {
        window: read_u32_le(body),
        serial: read_u32_le(&body[4..]),
        target_msc: read_u64_le(&body[12..]),
        divisor: read_u64_le(&body[20..]),
        remainder: read_u64_le(&body[28..]),
    })
}

#[must_use]
pub fn parse_select_input(body: &[u8]) -> Option<SelectInputRequest> {
    if body.len() < 12 {
        return None;
    }
    Some(SelectInputRequest {
        eid: read_u32_le(body),
        window: read_u32_le(&body[4..]),
        event_mask: read_u32_le(&body[8..]),
    })
}

#[must_use]
pub fn parse_query_capabilities(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
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

#[must_use]
pub fn encode_query_capabilities_reply(sequence: SequenceNumber, capabilities: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    out.extend_from_slice(&sequence.0.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&capabilities.to_le_bytes());
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_version_reply_shape() {
        let reply = encode_query_version_reply(SequenceNumber(3), 1, 0);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 0);
    }

    #[test]
    fn query_capabilities_reply_shape() {
        let reply = encode_query_capabilities_reply(SequenceNumber(4), CAPABILITY_NONE);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 0);
    }

    #[test]
    fn pixmap_request_parses_fixed_fields_and_no_fences() {
        let mut body = vec![0u8; 68];
        body[0..4].copy_from_slice(&0x100u32.to_le_bytes());
        body[4..8].copy_from_slice(&0x200u32.to_le_bytes());
        body[8..12].copy_from_slice(&7u32.to_le_bytes());
        body[20..22].copy_from_slice(&(-2i16).to_le_bytes());
        body[22..24].copy_from_slice(&3i16.to_le_bytes());
        body[44..52].copy_from_slice(&11u64.to_le_bytes());
        body[52..60].copy_from_slice(&13u64.to_le_bytes());
        body[60..68].copy_from_slice(&17u64.to_le_bytes());

        let req = parse_pixmap(&body).unwrap();
        assert_eq!(req.window, 0x100);
        assert_eq!(req.pixmap, 0x200);
        assert_eq!(req.serial, 7);
        assert_eq!(req.x_off, -2);
        assert_eq!(req.y_off, 3);
        assert_eq!(req.wait_fence, 0);
        assert_eq!(req.idle_fence, 0);
        assert_eq!(req.target_msc, 11);
        assert_eq!(req.divisor, 13);
        assert_eq!(req.remainder, 17);
        assert!(req.notifies.is_empty());
    }

    #[test]
    fn pixmap_request_parses_non_zero_fences_and_notifies() {
        let mut body = vec![0u8; 76];
        body[28..32].copy_from_slice(&0x44u32.to_le_bytes());
        body[32..36].copy_from_slice(&0x55u32.to_le_bytes());
        body[68..72].copy_from_slice(&0x100u32.to_le_bytes());
        body[72..76].copy_from_slice(&9u32.to_le_bytes());

        let req = parse_pixmap(&body).unwrap();
        assert_eq!(req.wait_fence, 0x44);
        assert_eq!(req.idle_fence, 0x55);
        assert_eq!(
            req.notifies,
            vec![Notify {
                window: 0x100,
                serial: 9
            }]
        );
    }

    #[test]
    fn pixmap_request_rejects_misaligned_notify_tail() {
        assert!(parse_pixmap(&[0u8; 72]).is_none());
    }

    #[test]
    fn notify_msc_request_shape() {
        let mut body = vec![0u8; 36];
        body[0..4].copy_from_slice(&0x100u32.to_le_bytes());
        body[4..8].copy_from_slice(&2u32.to_le_bytes());
        body[12..20].copy_from_slice(&3u64.to_le_bytes());
        body[20..28].copy_from_slice(&4u64.to_le_bytes());
        body[28..36].copy_from_slice(&5u64.to_le_bytes());

        let req = parse_notify_msc(&body).unwrap();
        assert_eq!(req.window, 0x100);
        assert_eq!(req.serial, 2);
        assert_eq!(req.target_msc, 3);
        assert_eq!(req.divisor, 4);
        assert_eq!(req.remainder, 5);
    }

    #[test]
    fn select_input_request_shape() {
        let mut body = vec![0u8; 12];
        body[0..4].copy_from_slice(&0x500u32.to_le_bytes());
        body[4..8].copy_from_slice(&0x100u32.to_le_bytes());
        body[8..12].copy_from_slice(&7u32.to_le_bytes());

        let req = parse_select_input(&body).unwrap();
        assert_eq!(req.eid, 0x500);
        assert_eq!(req.window, 0x100);
        assert_eq!(req.event_mask, 7);
    }
}
