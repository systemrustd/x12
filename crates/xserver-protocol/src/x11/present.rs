use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_u16, write_u32},
};

pub const QUERY_VERSION: u8 = 0;
pub const PIXMAP: u8 = 1;
pub const NOTIFY_MSC: u8 = 2;
pub const SELECT_INPUT: u8 = 3;
pub const QUERY_CAPABILITIES: u8 = 4;
pub const PIXMAP_SYNCED: u8 = 5;

pub const MAJOR_VERSION: u32 = 1;
pub const MINOR_VERSION: u32 = 4;

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

/// `PresentPixmapSynced` (v1.4): timeline-syncobj-driven Present.
/// Identical to `PresentPixmap` except the `(wait_fence, idle_fence)`
/// XID pair is replaced by `(acquire_syncobj, release_syncobj,
/// acquire_value, release_value)` — both syncobjs are timeline
/// `Sync::Syncobj` resources (DRI3 1.4) and the values are the
/// timeline points to wait/signal at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PixmapSyncedRequest {
    pub window: u32,
    pub pixmap: u32,
    pub serial: u32,
    pub valid: u32,
    pub update: u32,
    pub x_off: i16,
    pub y_off: i16,
    pub target_crtc: u32,
    pub acquire_syncobj: u32,
    pub release_syncobj: u32,
    pub acquire_value: u64,
    pub release_value: u64,
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
pub fn parse_pixmap_synced(body: &[u8]) -> Option<PixmapSyncedRequest> {
    // Layout per /usr/share/xcb/present.xml `PixmapSynced` (opcode 5):
    //   window(4) pixmap(4) serial(4) valid(4) update(4) x_off(2)
    //   y_off(2) target_crtc(4) acquire_syncobj(4) release_syncobj(4)
    //   acquire_point(8) release_point(8) options(4) pad(4)
    //   target_msc(8) divisor(8) remainder(8) notifies[N](8 each).
    // Fixed prefix = 84 bytes; trailing notifies are 8 each.
    if body.len() < 84 || !(body.len() - 84).is_multiple_of(8) {
        return None;
    }
    let notifies = body[84..]
        .chunks_exact(8)
        .map(|chunk| Notify {
            window: read_u32_le(chunk),
            serial: read_u32_le(&chunk[4..]),
        })
        .collect();
    Some(PixmapSyncedRequest {
        window: read_u32_le(body),
        pixmap: read_u32_le(&body[4..]),
        serial: read_u32_le(&body[8..]),
        valid: read_u32_le(&body[12..]),
        update: read_u32_le(&body[16..]),
        x_off: read_i16_le(&body[20..]),
        y_off: read_i16_le(&body[22..]),
        target_crtc: read_u32_le(&body[24..]),
        acquire_syncobj: read_u32_le(&body[28..]),
        release_syncobj: read_u32_le(&body[32..]),
        acquire_value: read_u64_le(&body[36..]),
        release_value: read_u64_le(&body[44..]),
        options: read_u32_le(&body[52..]),
        // 4 bytes pad at offset 56
        target_msc: read_u64_le(&body[60..]),
        divisor: read_u64_le(&body[68..]),
        remainder: read_u64_le(&body[76..]),
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

/// `presentproto` event-type constants for GE-mode dispatch.
pub const EVENT_CONFIGURE_NOTIFY: u8 = 0;
pub const EVENT_COMPLETE_NOTIFY: u8 = 1;
pub const EVENT_IDLE_NOTIFY: u8 = 2;
pub const EVENT_REDIRECT_NOTIFY: u8 = 3;

/// `CompleteNotify.mode` per `presentproto`.
pub const COMPLETE_MODE_COPY: u8 = 0;
pub const COMPLETE_MODE_FLIP: u8 = 1;
pub const COMPLETE_MODE_SKIP: u8 = 2;
pub const COMPLETE_MODE_SUBOPTIMAL_COPY: u8 = 3;

/// `CompleteNotify.kind` per `presentproto`.
pub const COMPLETE_KIND_PIXMAP: u8 = 0;
pub const COMPLETE_KIND_NOTIFY_MSC: u8 = 1;

/// Encode a `CompleteNotify` GE event. Phase 4.2 design §3.3. Per
/// `presentproto` the event is 40 bytes total — 32-byte fixed slot
/// + 8 bytes extra (so `length` = `(40-32)/4` = 2).
///
/// ```text
/// 1   GenericEvent (35)
/// 1   extension major opcode
/// 2   sequence number
/// 4   length = 2
/// 2   evtype (1 = CompleteNotify)
/// 1   kind
/// 1   mode
/// 4   eid (event id from SelectInput)
/// 4   window
/// 4   serial
/// 8   ust (microseconds; 0 if unknown)
/// 8   msc
/// ```
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn encode_complete_notify(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    extension_major: u8,
    eid: u32,
    window: u32,
    serial: u32,
    kind: u8,
    mode: u8,
    ust: u64,
    msc: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(40);
    out.push(35);
    out.push(extension_major);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 2); // length = 2 (8 extra bytes)
    write_u16(byte_order, &mut out, EVENT_COMPLETE_NOTIFY.into());
    out.push(kind);
    out.push(mode);
    write_u32(byte_order, &mut out, eid);
    write_u32(byte_order, &mut out, window);
    write_u32(byte_order, &mut out, serial);
    #[allow(clippy::cast_possible_truncation)]
    {
        write_u32(byte_order, &mut out, (ust & 0xFFFF_FFFF) as u32);
        write_u32(byte_order, &mut out, (ust >> 32) as u32);
        write_u32(byte_order, &mut out, (msc & 0xFFFF_FFFF) as u32);
        write_u32(byte_order, &mut out, (msc >> 32) as u32);
    }
    debug_assert_eq!(out.len(), 40);
    out
}

/// Encode an `IdleNotify` GE event. Layout per `presentproto`:
///
/// ```text
/// 1   GenericEvent (35)
/// 1   extension major opcode
/// 2   sequence number
/// 4   length (in 4-byte units past the 32-byte header)
/// 2   evtype (2 = IdleNotify)
/// 2   pad
/// 4   eid
/// 4   window
/// 4   serial
/// 4   pixmap
/// 4   idle_fence
/// ```
///
/// Total 32 bytes.
#[must_use]
pub fn encode_idle_notify(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    extension_major: u8,
    eid: u32,
    window: u32,
    serial: u32,
    pixmap: u32,
    idle_fence: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(35);
    out.push(extension_major);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    write_u16(byte_order, &mut out, EVENT_IDLE_NOTIFY.into());
    write_u16(byte_order, &mut out, 0); // pad
    write_u32(byte_order, &mut out, eid);
    write_u32(byte_order, &mut out, window);
    write_u32(byte_order, &mut out, serial);
    write_u32(byte_order, &mut out, pixmap);
    write_u32(byte_order, &mut out, idle_fence);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encode a `ConfigureNotify` GE event. Layout per `presentproto`:
///
/// ```text
/// 1   GenericEvent (35)
/// 1   extension major opcode
/// 2   sequence number
/// 4   length = 2  (8 extra bytes past the 32-byte slot)
/// 2   evtype (0 = ConfigureNotify)
/// 2   pad
/// 4   eid
/// 4   window
/// 2   x
/// 2   y
/// 2   width
/// 2   height
/// 2   off_x
/// 2   off_y
/// 2   pixmap_width
/// 2   pixmap_height
/// 4   pixmap_flags
/// ```
///
/// Total 40 bytes. Mesa's `loader_dri3_helper` consumes this to know it
/// must reallocate the EGL/DRI3 swap buffer at the new window size — without
/// it, Mesa keeps presenting whatever size it allocated initially, which
/// for a profile-chooser window first created at 1×1 means 1×1 presents
/// over an 800×800 window → blank content (Firefox empty-shadow on bee).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn encode_configure_notify(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    extension_major: u8,
    eid: u32,
    window: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    off_x: i16,
    off_y: i16,
    pixmap_width: u16,
    pixmap_height: u16,
    pixmap_flags: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(40);
    out.push(35);
    out.push(extension_major);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 2);
    write_u16(byte_order, &mut out, EVENT_CONFIGURE_NOTIFY.into());
    write_u16(byte_order, &mut out, 0); // pad
    write_u32(byte_order, &mut out, eid);
    write_u32(byte_order, &mut out, window);
    #[allow(clippy::cast_sign_loss)]
    {
        write_u16(byte_order, &mut out, x as u16);
        write_u16(byte_order, &mut out, y as u16);
    }
    write_u16(byte_order, &mut out, width);
    write_u16(byte_order, &mut out, height);
    #[allow(clippy::cast_sign_loss)]
    {
        write_u16(byte_order, &mut out, off_x as u16);
        write_u16(byte_order, &mut out, off_y as u16);
    }
    write_u16(byte_order, &mut out, pixmap_width);
    write_u16(byte_order, &mut out, pixmap_height);
    write_u32(byte_order, &mut out, pixmap_flags);
    debug_assert_eq!(out.len(), 40);
    out
}

/// `Present::SelectInput` event mask bits per `presentproto`.
pub const EVENT_MASK_CONFIGURE_NOTIFY: u32 = 1;
pub const EVENT_MASK_COMPLETE_NOTIFY: u32 = 1 << 1;
pub const EVENT_MASK_IDLE_NOTIFY: u32 = 1 << 2;
pub const EVENT_MASK_REDIRECT_NOTIFY: u32 = 1 << 3;

#[must_use]
pub fn encode_query_capabilities_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    capabilities: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    write_u32(byte_order, &mut out, capabilities);
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
            encode_query_version_reply(ClientByteOrder::LittleEndian, SequenceNumber(3), 1, 0);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 0);
    }

    #[test]
    fn query_capabilities_reply_shape() {
        let reply = encode_query_capabilities_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(4),
            CAPABILITY_NONE,
        );
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
    fn complete_notify_event_shape() {
        let ev = encode_complete_notify(
            ClientByteOrder::LittleEndian,
            SequenceNumber(7),
            145,
            0xEEE,
            0xBEEF,
            42,
            COMPLETE_KIND_PIXMAP,
            COMPLETE_MODE_COPY,
            0xFEED_FACE,
            12345,
        );
        assert_eq!(ev.len(), 40);
        assert_eq!(ev[0], 35); // GenericEvent
        assert_eq!(ev[1], 145);
        assert_eq!(u16::from_le_bytes(ev[2..4].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(ev[4..8].try_into().unwrap()), 2); // length
        assert_eq!(u16::from_le_bytes(ev[8..10].try_into().unwrap()), 1);
        assert_eq!(ev[10], COMPLETE_KIND_PIXMAP);
        assert_eq!(ev[11], COMPLETE_MODE_COPY);
        assert_eq!(u32::from_le_bytes(ev[12..16].try_into().unwrap()), 0xEEE);
        assert_eq!(u32::from_le_bytes(ev[16..20].try_into().unwrap()), 0xBEEF);
        assert_eq!(u32::from_le_bytes(ev[20..24].try_into().unwrap()), 42);
        assert_eq!(
            u64::from_le_bytes(ev[24..32].try_into().unwrap()),
            0xFEED_FACE
        );
        assert_eq!(u64::from_le_bytes(ev[32..40].try_into().unwrap()), 12345);
    }

    #[test]
    fn idle_notify_event_shape() {
        let ev = encode_idle_notify(
            ClientByteOrder::LittleEndian,
            SequenceNumber(3),
            145,
            0xEE0,
            0xBEEF,
            7,
            0xCAFE,
            0,
        );
        assert_eq!(ev.len(), 32);
        assert_eq!(ev[0], 35);
        assert_eq!(ev[1], 145);
        assert_eq!(u16::from_le_bytes(ev[8..10].try_into().unwrap()), 2); // evtype
        assert_eq!(u32::from_le_bytes(ev[12..16].try_into().unwrap()), 0xEE0);
        assert_eq!(u32::from_le_bytes(ev[16..20].try_into().unwrap()), 0xBEEF);
        assert_eq!(u32::from_le_bytes(ev[20..24].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(ev[24..28].try_into().unwrap()), 0xCAFE);
        assert_eq!(u32::from_le_bytes(ev[28..32].try_into().unwrap()), 0);
    }

    #[test]
    fn pixmap_synced_parses_minimum_body() {
        // 84-byte fixed prefix per xcb present.xml; no notifies.
        let mut body = vec![0u8; 84];
        body[0..4].copy_from_slice(&0x100u32.to_le_bytes()); // window
        body[4..8].copy_from_slice(&0x200u32.to_le_bytes()); // pixmap
        body[8..12].copy_from_slice(&7u32.to_le_bytes()); // serial
        body[28..32].copy_from_slice(&0x300u32.to_le_bytes()); // acquire_syncobj
        body[32..36].copy_from_slice(&0x400u32.to_le_bytes()); // release_syncobj
        body[36..44].copy_from_slice(&42u64.to_le_bytes()); // acquire_value
        body[44..52].copy_from_slice(&43u64.to_le_bytes()); // release_value
        body[60..68].copy_from_slice(&100u64.to_le_bytes()); // target_msc
        let req = parse_pixmap_synced(&body).unwrap();
        assert_eq!(req.window, 0x100);
        assert_eq!(req.pixmap, 0x200);
        assert_eq!(req.serial, 7);
        assert_eq!(req.acquire_syncobj, 0x300);
        assert_eq!(req.release_syncobj, 0x400);
        assert_eq!(req.acquire_value, 42);
        assert_eq!(req.release_value, 43);
        assert_eq!(req.target_msc, 100);
        assert!(req.notifies.is_empty());
    }

    #[test]
    fn pixmap_synced_rejects_misaligned_notifies() {
        assert!(parse_pixmap_synced(&[0u8; 87]).is_none());
    }

    #[test]
    fn pixmap_synced_rejects_short_body() {
        assert!(parse_pixmap_synced(&[0u8; 83]).is_none());
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
