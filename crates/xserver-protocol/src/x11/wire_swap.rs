//! Shared byte-swap field-table primitives used by:
//! - Phase D2: per-recipient re-encoding of raw event templates that flow
//!   through `fanout_raw_event_to_clients`.
//! - Phase E: per-opcode in-place body swap of inbound BE-client requests
//!   before dispatch, so the rest of the dispatch code can decode bytes
//!   as little-endian.
//!
//! The module is byte-order-agnostic at construction time: each
//! `FieldEntry` describes *where* typed fields live within a 32-byte
//! event template or a request body; the actual swap direction is
//! supplied at call time via `ClientByteOrder`.

use super::ClientByteOrder;

/// Width of one typed field — used both for in-place swaps and for the
/// length-prefix arithmetic in `LengthPrefixedBytes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    U16,
    I16,
    U32,
    I32,
}

impl FieldKind {
    #[must_use]
    pub fn size(self) -> usize {
        match self {
            FieldKind::U16 | FieldKind::I16 => 2,
            FieldKind::U32 | FieldKind::I32 => 4,
        }
    }
}

/// One entry in a per-opcode (or per-event) field table.
#[derive(Clone, Copy, Debug)]
pub enum FieldEntry {
    /// A typed CARD16/CARD32/INT16/INT32 field at a known byte offset.
    Fixed { offset: u16, kind: FieldKind },
    /// Bytes from `from` (inclusive) to end-of-buffer are opaque
    /// (font names, glyph blobs, image data) — leave alone.
    OpaqueTail { from: u16 },
    /// From `from` to end-of-buffer is a uniform array of typed
    /// elements (e.g., u32[] for `FreeColors` pixels). Each element is
    /// swapped in place.
    ElementArrayTail { from: u16, kind: FieldKind },
    /// A typed length field at `length_offset` followed (after
    /// padding to `data_offset`) by `length_kind.size() == 1`-style
    /// opaque bytes (string). Used for InternAtom name and similar.
    LengthPrefixedBytes {
        length_offset: u16,
        length_kind: FieldKind,
        data_offset: u16,
    },
    /// Per-opcode irregular layout — caller owns the swap.
    /// The custom handler operates on the raw buffer and may read
    /// `u8` discriminants (which are byte-order-agnostic) before
    /// mutating typed fields.
    Custom(fn(byte_order: ClientByteOrder, buf: &mut [u8])),
}

/// Walk `entries` and byte-swap each typed field in place. If
/// `byte_order` is `LittleEndian` this is a no-op for native LE
/// hosts (Rust's primary target), so callers don't need to gate the
/// call themselves.
pub fn swap_in_place(entries: &[FieldEntry], byte_order: ClientByteOrder, buf: &mut [u8]) {
    if matches!(byte_order, ClientByteOrder::LittleEndian) {
        return;
    }
    for entry in entries {
        apply_entry(*entry, byte_order, buf);
    }
}

fn apply_entry(entry: FieldEntry, byte_order: ClientByteOrder, buf: &mut [u8]) {
    match entry {
        FieldEntry::Fixed { offset, kind } => swap_typed(buf, offset as usize, kind),
        FieldEntry::OpaqueTail { .. } => { /* nothing to swap */ }
        FieldEntry::ElementArrayTail { from, kind } => {
            let start = from as usize;
            if start >= buf.len() {
                return;
            }
            let element_size = kind.size();
            let mut off = start;
            while off + element_size <= buf.len() {
                swap_typed(buf, off, kind);
                off += element_size;
            }
        }
        FieldEntry::LengthPrefixedBytes {
            length_offset,
            length_kind,
            ..
        } => {
            // Swap the length field; the trailing bytes are opaque.
            swap_typed(buf, length_offset as usize, length_kind);
        }
        FieldEntry::Custom(f) => f(byte_order, buf),
    }
}

fn swap_typed(buf: &mut [u8], offset: usize, kind: FieldKind) {
    let size = kind.size();
    if offset + size > buf.len() {
        return;
    }
    match kind {
        FieldKind::U16 | FieldKind::I16 => buf[offset..offset + 2].reverse(),
        FieldKind::U32 | FieldKind::I32 => buf[offset..offset + 4].reverse(),
    }
}

/// Per-event-type swap table. Used by `fanout_raw_event_to_clients`
/// to re-encode a 32-byte event template into each recipient's byte
/// order. The lookup key is the event-type byte (`buf[0] & 0x7f`,
/// stripping the send-event bit).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn core_event_swap_table(event_type: u8) -> &'static [FieldEntry] {
    use FieldEntry::Fixed;
    use FieldKind::{U16, U32};

    macro_rules! u32f {
        ($off:expr) => {
            Fixed {
                offset: $off,
                kind: U32,
            }
        };
    }
    macro_rules! u16f {
        ($off:expr) => {
            Fixed {
                offset: $off,
                kind: U16,
            }
        };
    }

    // Common event prefix: response_type(u8) detail(u8) sequence(u16).
    // Most events have time(u32) at offset 4, root(u32) at 8, etc.
    match event_type {
        // 2 KeyPress, 3 KeyRelease, 4 ButtonPress, 5 ButtonRelease, 6 MotionNotify
        2..=6 => &[
            u16f!(2),  // sequence
            u32f!(4),  // time
            u32f!(8),  // root
            u32f!(12), // event window
            u32f!(16), // child
            u16f!(20), // root_x
            u16f!(22), // root_y
            u16f!(24), // event_x
            u16f!(26), // event_y
            u16f!(28), // state
        ],
        // 7 EnterNotify, 8 LeaveNotify
        7 | 8 => &[
            u16f!(2),
            u32f!(4),
            u32f!(8),
            u32f!(12),
            u32f!(16),
            u16f!(20),
            u16f!(22),
            u16f!(24),
            u16f!(26),
            u16f!(28),
        ],
        // 9 FocusIn, 10 FocusOut: sequence(u16) event(u32)
        9 | 10 => &[u16f!(2), u32f!(4)],
        // 11 KeymapNotify: 32-byte bitmap, opaque after byte 0.
        11 => &[],
        // 12 Expose: sequence(u16) window(u32) x(u16) y(u16) w(u16) h(u16) count(u16)
        12 => &[
            u16f!(2),
            u32f!(4),
            u16f!(8),
            u16f!(10),
            u16f!(12),
            u16f!(14),
            u16f!(16),
        ],
        // 13 GraphicsExposure: sequence(u16) drawable(u32) x/y/w/h(u16) minor(u16)
        //                     count(u16) major(u8 byte 24)
        13 => &[
            u16f!(2),
            u32f!(4),
            u16f!(8),
            u16f!(10),
            u16f!(12),
            u16f!(14),
            u16f!(16),
            u16f!(18),
        ],
        // 14 NoExposure: sequence(u16) drawable(u32) minor(u16) major(u8)
        14 => &[u16f!(2), u32f!(4), u16f!(8)],
        // 15 VisibilityNotify: sequence(u16) window(u32)
        15 => &[u16f!(2), u32f!(4)],
        // 16 CreateNotify: sequence(u16) parent(u32) window(u32) x(i16) y(i16)
        //                 w(u16) h(u16) border_w(u16)
        16 => &[
            u16f!(2),
            u32f!(4),
            u32f!(8),
            u16f!(12),
            u16f!(14),
            u16f!(16),
            u16f!(18),
            u16f!(20),
        ],
        // 17 DestroyNotify: sequence(u16) event(u32) window(u32)
        17 => &[u16f!(2), u32f!(4), u32f!(8)],
        // 18 UnmapNotify: sequence(u16) event(u32) window(u32)
        18 => &[u16f!(2), u32f!(4), u32f!(8)],
        // 19 MapNotify: sequence(u16) event(u32) window(u32)
        19 => &[u16f!(2), u32f!(4), u32f!(8)],
        // 20 MapRequest: sequence(u16) parent(u32) window(u32)
        20 => &[u16f!(2), u32f!(4), u32f!(8)],
        // 21 ReparentNotify: sequence(u16) event(u32) window(u32) parent(u32)
        //                  x(i16) y(i16)
        21 => &[
            u16f!(2),
            u32f!(4),
            u32f!(8),
            u32f!(12),
            u16f!(16),
            u16f!(18),
        ],
        // 22 ConfigureNotify: sequence(u16) event(u32) window(u32) above_sibling(u32)
        //                    x(i16) y(i16) w(u16) h(u16) border_w(u16)
        22 => &[
            u16f!(2),
            u32f!(4),
            u32f!(8),
            u32f!(12),
            u16f!(16),
            u16f!(18),
            u16f!(20),
            u16f!(22),
            u16f!(24),
        ],
        // 23 ConfigureRequest: sequence(u16) parent(u32) window(u32)
        //                     sibling(u32) x(i16) y(i16) w(u16) h(u16)
        //                     border_w(u16) value_mask(u16)
        23 => &[
            u16f!(2),
            u32f!(4),
            u32f!(8),
            u32f!(12),
            u16f!(16),
            u16f!(18),
            u16f!(20),
            u16f!(22),
            u16f!(24),
            u16f!(26),
        ],
        // 24 GravityNotify: sequence(u16) event(u32) window(u32) x(i16) y(i16)
        24 => &[u16f!(2), u32f!(4), u32f!(8), u16f!(12), u16f!(14)],
        // 25 ResizeRequest: sequence(u16) window(u32) w(u16) h(u16)
        25 => &[u16f!(2), u32f!(4), u16f!(8), u16f!(10)],
        // 26 CirculateNotify: sequence(u16) event(u32) window(u32)
        26 => &[u16f!(2), u32f!(4), u32f!(8)],
        // 27 CirculateRequest: sequence(u16) parent(u32) window(u32)
        27 => &[u16f!(2), u32f!(4), u32f!(8)],
        // 28 PropertyNotify: sequence(u16) window(u32) atom(u32) time(u32)
        28 => &[u16f!(2), u32f!(4), u32f!(8), u32f!(12)],
        // 29 SelectionClear: sequence(u16) time(u32) owner(u32) selection(u32)
        29 => &[u16f!(2), u32f!(4), u32f!(8), u32f!(12)],
        // 30 SelectionRequest: sequence(u16) time(u32) owner(u32) requestor(u32)
        //                     selection(u32) target(u32) property(u32)
        30 => &[
            u16f!(2),
            u32f!(4),
            u32f!(8),
            u32f!(12),
            u32f!(16),
            u32f!(20),
            u32f!(24),
        ],
        // 31 SelectionNotify: sequence(u16) time(u32) requestor(u32)
        //                    selection(u32) target(u32) property(u32)
        31 => &[
            u16f!(2),
            u32f!(4),
            u32f!(8),
            u32f!(12),
            u32f!(16),
            u32f!(20),
        ],
        // 32 ColormapNotify: sequence(u16) window(u32) colormap(u32)
        32 => &[u16f!(2), u32f!(4), u32f!(8)],
        // 33 ClientMessage: sequence(u16) window(u32) type(u32). The data
        //                  payload (20 bytes from offset 12) is format-
        //                  dependent and treated as opaque here — the X
        //                  spec lets the sender encode it; we don't
        //                  re-encode (the client knew its byte order).
        33 => &[u16f!(2), u32f!(4), u32f!(8)],
        // 34 MappingNotify: sequence(u16) only — rest is u8 fields.
        34 => &[u16f!(2)],
        // GenericEvent (35): only sequence is fixed; extension owns the rest.
        35 => &[u16f!(2)],
        // KeymapNotify and unknown: nothing to swap.
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_u16_round_trip() {
        let entries = [FieldEntry::Fixed {
            offset: 0,
            kind: FieldKind::U16,
        }];
        let mut buf = [0xab, 0xcd, 0xef, 0x12];
        swap_in_place(&entries, ClientByteOrder::BigEndian, &mut buf);
        assert_eq!(buf, [0xcd, 0xab, 0xef, 0x12]);
        // No-op for LE source.
        let mut buf2 = [0xab, 0xcd, 0xef, 0x12];
        swap_in_place(&entries, ClientByteOrder::LittleEndian, &mut buf2);
        assert_eq!(buf2, [0xab, 0xcd, 0xef, 0x12]);
    }

    #[test]
    fn fixed_u32_swap() {
        let entries = [FieldEntry::Fixed {
            offset: 4,
            kind: FieldKind::U32,
        }];
        let mut buf = [0; 12];
        buf[4..8].copy_from_slice(&0xdead_beef_u32.to_le_bytes());
        swap_in_place(&entries, ClientByteOrder::BigEndian, &mut buf);
        assert_eq!(
            u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            0xdead_beef
        );
    }

    #[test]
    fn element_array_tail_swaps_each_u32() {
        let entries = [FieldEntry::ElementArrayTail {
            from: 4,
            kind: FieldKind::U32,
        }];
        let mut buf = [0u8; 12];
        // Start with LE encoding of two u32s.
        buf[4..8].copy_from_slice(&0x0011_2233_u32.to_le_bytes());
        buf[8..12].copy_from_slice(&0x4455_6677_u32.to_le_bytes());
        // After swap, each element is in BE byte order.
        swap_in_place(&entries, ClientByteOrder::BigEndian, &mut buf);
        assert_eq!(buf[4..8], [0x00, 0x11, 0x22, 0x33]);
        assert_eq!(buf[8..12], [0x44, 0x55, 0x66, 0x77]);
    }

    #[test]
    fn opaque_tail_is_left_alone() {
        let entries = [FieldEntry::OpaqueTail { from: 0 }];
        let mut buf = [1, 2, 3, 4, 5, 6, 7, 8];
        let original = buf;
        swap_in_place(&entries, ClientByteOrder::BigEndian, &mut buf);
        assert_eq!(buf, original);
    }

    #[test]
    fn custom_handler_runs() {
        fn handler(_bo: ClientByteOrder, buf: &mut [u8]) {
            buf[0] = 0xff;
        }
        let entries = [FieldEntry::Custom(handler)];
        let mut buf = [0u8; 4];
        swap_in_place(&entries, ClientByteOrder::BigEndian, &mut buf);
        assert_eq!(buf[0], 0xff);
    }

    #[test]
    fn out_of_bounds_offset_does_not_panic() {
        let entries = [FieldEntry::Fixed {
            offset: 100,
            kind: FieldKind::U32,
        }];
        let mut buf = [0u8; 4];
        swap_in_place(&entries, ClientByteOrder::BigEndian, &mut buf);
        // No panic, no swap.
        assert_eq!(buf, [0u8; 4]);
    }
}
