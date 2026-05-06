//! Per-opcode request-body swap tables for inbound BE-client requests.
//!
//! After `read_request` returns the body, the per-client reader thread
//! calls `swap_request_body(opcode, byte_order, &mut body)` which looks
//! up the opcode in this module's table and byte-swaps each typed
//! field in place. After the swap the body is in LE form and the
//! existing dispatch / decoder code (which uses `read_u16_le` /
//! `read_u32_le` everywhere) runs unchanged.
//!
//! Tables are conservative: when an opcode is missing or unknown the
//! body passes through unchanged. xts coverage will guide which
//! opcodes need entries first.
//!
//! Field offsets are relative to the start of the body — i.e., the
//! 4-byte request header is **not** included.

use super::{
    ClientByteOrder,
    wire_swap::{FieldEntry, FieldKind, swap_in_place},
};

/// Apply the per-opcode swap table for `opcode` to `body`. No-op for
/// little-endian clients, or for opcodes without a registered table.
pub fn swap_request_body(opcode: u8, byte_order: ClientByteOrder, body: &mut [u8]) {
    if matches!(byte_order, ClientByteOrder::LittleEndian) {
        return;
    }
    if let Some(entries) = core_request_swap_table(opcode) {
        swap_in_place(entries, byte_order, body);
    }
}

#[allow(clippy::too_many_lines)]
const fn core_request_swap_table(opcode: u8) -> Option<&'static [FieldEntry]> {
    use FieldEntry::{ElementArrayTail, Fixed, OpaqueTail};
    use FieldKind::{I16, U16, U32};

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
    macro_rules! i16f {
        ($off:expr) => {
            Fixed {
                offset: $off,
                kind: I16,
            }
        };
    }

    Some(match opcode {
        // 1 CreateWindow
        // body: window(u32) parent(u32) x(i16) y(i16) w(u16) h(u16)
        //       border_w(u16) class(u16) visual(u32) value-mask(u32) values(u32[])
        1 => &[
            u32f!(0),
            u32f!(4),
            i16f!(8),
            i16f!(10),
            u16f!(12),
            u16f!(14),
            u16f!(16),
            u16f!(18),
            u32f!(20),
            u32f!(24),
            ElementArrayTail {
                from: 28,
                kind: U32,
            },
        ],
        // 2 ChangeWindowAttributes: window(u32) mask(u32) values(u32[])
        2 => &[u32f!(0), u32f!(4), ElementArrayTail { from: 8, kind: U32 }],
        // 3-15: most are window-only u32 at offset 0
        3 | 4 | 5 | 6 | 8 | 9 | 10 | 11 | 13 | 14 | 15 | 21 | 23 | 38 | 79 | 81 | 82 | 83 | 95
        | 113 => &[u32f!(0)],
        // 7 ReparentWindow: window(u32) parent(u32) x(i16) y(i16)
        7 => &[u32f!(0), u32f!(4), i16f!(8), i16f!(10)],
        // 12 ConfigureWindow: window(u32) mask(u16) pad(u16) values(u32[])
        12 => &[u32f!(0), u16f!(4), ElementArrayTail { from: 8, kind: U32 }],
        // 16 InternAtom: nbytes(u16 at offset 0), pad(u16 at 2), name(opaque from 4)
        // The byte `data` of header is `only_if_exists` — already u8, no swap.
        16 => &[u16f!(0), OpaqueTail { from: 4 }],
        // 17 GetAtomName: atom(u32 at 0)
        17 => &[u32f!(0)],
        // 18 ChangeProperty: window(u32) property(u32) type(u32) format(u8)
        //                   pad(u8 × 3) data_len(u32) data(opaque, format-
        //                   dependent — swapping per format would belong in
        //                   a Custom handler; for now treat as opaque so we
        //                   don't corrupt e.g. format=32 data).
        // The format byte at offset 12 is u8 — no swap needed. data_len
        // at offset 16 is u32 — must be swapped.
        18 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            u32f!(16),
            OpaqueTail { from: 20 },
        ],
        // 19 DeleteProperty: window(u32) property(u32)
        19 => &[u32f!(0), u32f!(4)],
        // 20 GetProperty: window(u32) property(u32) type(u32) long_off(u32) long_len(u32)
        20 => &[u32f!(0), u32f!(4), u32f!(8), u32f!(12), u32f!(16)],
        // 22 SetSelectionOwner: owner(u32) selection(u32) time(u32)
        22 => &[u32f!(0), u32f!(4), u32f!(8)],
        // 24 ConvertSelection: requestor(u32) selection(u32) target(u32) property(u32) time(u32)
        24 => &[u32f!(0), u32f!(4), u32f!(8), u32f!(12), u32f!(16)],
        // 25 SendEvent: destination(u32) event_mask(u32) event(32 bytes opaque)
        // The event template is in the *sender's* byte order; the SendEvent
        // handler re-encodes per recipient. Swap only the destination/mask.
        25 => &[u32f!(0), u32f!(4), OpaqueTail { from: 8 }],
        // 26 GrabPointer: window(u32) event_mask(u16) [u8 u8] confine_to(u32) cursor(u32) time(u32)
        26 => &[u32f!(0), u16f!(4), u32f!(8), u32f!(12), u32f!(16)],
        // 27 UngrabPointer: time(u32)
        27 => &[u32f!(0)],
        // 28 GrabButton: window(u32) event_mask(u16) [u8 u8] confine_to(u32) cursor(u32) [u8 button]
        //               [u8 pad] modifiers(u16)
        28 => &[u32f!(0), u16f!(4), u32f!(8), u32f!(12), u16f!(20)],
        // 29 UngrabButton: window(u32) modifiers(u16)
        29 => &[u32f!(0), u16f!(4)],
        // 30 ChangeActivePointerGrab: cursor(u32) time(u32) event_mask(u16)
        30 => &[u32f!(0), u32f!(4), u16f!(8)],
        // 31 GrabKeyboard: window(u32) time(u32) [u8 u8 u8 u8]
        31 => &[u32f!(0), u32f!(4)],
        // 32 UngrabKeyboard: time(u32)
        32 => &[u32f!(0)],
        // 33 GrabKey: window(u32) modifiers(u16) [u8 u8 u8 u8]
        33 => &[u32f!(0), u16f!(4)],
        // 34 UngrabKey: window(u32) modifiers(u16)
        34 => &[u32f!(0), u16f!(4)],
        // 35 AllowEvents: time(u32). Mode is in header.data.
        35 => &[u32f!(0)],
        // 39 GetMotionEvents: window(u32) start(u32) stop(u32)
        39 => &[u32f!(0), u32f!(4), u32f!(8)],
        // 40 TranslateCoordinates: src(u32) dst(u32) src_x(i16) src_y(i16)
        40 => &[u32f!(0), u32f!(4), i16f!(8), i16f!(10)],
        // 41 WarpPointer: src(u32) dst(u32) src_x(i16) src_y(i16) src_w(u16) src_h(u16)
        //                dst_x(i16) dst_y(i16)
        41 => &[
            u32f!(0),
            u32f!(4),
            i16f!(8),
            i16f!(10),
            u16f!(12),
            u16f!(14),
            i16f!(16),
            i16f!(18),
        ],
        // 42 SetInputFocus: focus(u32) time(u32)
        42 => &[u32f!(0), u32f!(4)],
        // 45 OpenFont: fid(u32) name_len(u16) [pad u16] name(opaque)
        45 => &[u32f!(0), u16f!(4), OpaqueTail { from: 8 }],
        // 46 CloseFont, 47 QueryFont: font(u32)
        46 | 47 => &[u32f!(0)],
        // 48 QueryTextExtents: font_or_gc(u32) string(u16[])
        48 => &[u32f!(0), ElementArrayTail { from: 4, kind: U16 }],
        // 49/50 ListFonts/ListFontsWithInfo: max(u16) nlen(u16) name(opaque)
        49 | 50 => &[u16f!(0), u16f!(2), OpaqueTail { from: 4 }],
        // 51 SetFontPath: nstr(u16) [u16 pad] strings(opaque)
        51 => &[u16f!(0), OpaqueTail { from: 4 }],
        // 53 CreatePixmap: pid(u32) drawable(u32) w(u16) h(u16). depth in header.data.
        53 => &[u32f!(0), u32f!(4), u16f!(8), u16f!(10)],
        // 54 FreePixmap: pixmap(u32)
        54 => &[u32f!(0)],
        // 55 CreateGC: cid(u32) drawable(u32) mask(u32) values(u32[])
        55 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            ElementArrayTail {
                from: 12,
                kind: U32,
            },
        ],
        // 56 ChangeGC: gc(u32) mask(u32) values(u32[])
        56 => &[u32f!(0), u32f!(4), ElementArrayTail { from: 8, kind: U32 }],
        // 57 CopyGC: src(u32) dst(u32) mask(u32)
        57 => &[u32f!(0), u32f!(4), u32f!(8)],
        // 58 SetDashes: gc(u32) dash_offset(u16) ndashes(u16) dashes(opaque)
        58 => &[u32f!(0), u16f!(4), u16f!(6), OpaqueTail { from: 8 }],
        // 59 SetClipRectangles: gc(u32) clip_x(i16) clip_y(i16) rects(i16[]/u16[])
        59 => &[
            u32f!(0),
            i16f!(4),
            i16f!(6),
            ElementArrayTail { from: 8, kind: U16 },
        ],
        // 60 FreeGC: gc(u32)
        60 => &[u32f!(0)],
        // 61 ClearArea: window(u32) x(i16) y(i16) w(u16) h(u16)
        61 => &[u32f!(0), i16f!(4), i16f!(6), u16f!(8), u16f!(10)],
        // 62 CopyArea: src(u32) dst(u32) gc(u32) src_x(i16) src_y(i16)
        //             dst_x(i16) dst_y(i16) w(u16) h(u16)
        62 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            i16f!(12),
            i16f!(14),
            i16f!(16),
            i16f!(18),
            u16f!(20),
            u16f!(22),
        ],
        // 63 CopyPlane: src(u32) dst(u32) gc(u32) src_x(i16) src_y(i16)
        //              dst_x(i16) dst_y(i16) w(u16) h(u16) bit_plane(u32)
        63 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            i16f!(12),
            i16f!(14),
            i16f!(16),
            i16f!(18),
            u16f!(20),
            u16f!(22),
            u32f!(24),
        ],
        // 64-71 Poly* drawing: drawable(u32) gc(u32) coords(i16/u16 array)
        64 | 65 | 66 | 67 | 70 => &[u32f!(0), u32f!(4), ElementArrayTail { from: 8, kind: U16 }],
        // 68/71 PolyArc/PolyFillArc: drawable(u32) gc(u32) arcs(i16+u16+i16+i16 each)
        68 | 71 => &[u32f!(0), u32f!(4), ElementArrayTail { from: 8, kind: U16 }],
        // 69 FillPoly: drawable(u32) gc(u32) shape/coords flags(u8 u8 u16) points(i16[])
        69 => &[
            u32f!(0),
            u32f!(4),
            u16f!(10),
            ElementArrayTail {
                from: 12,
                kind: I16,
            },
        ],
        // 72 PutImage: drawable(u32) gc(u32) w(u16) h(u16) dst_x(i16) dst_y(i16)
        //             [u8 left_pad u8 depth u16 pad] data(opaque)
        72 => &[
            u32f!(0),
            u32f!(4),
            u16f!(8),
            u16f!(10),
            i16f!(12),
            i16f!(14),
            OpaqueTail { from: 16 },
        ],
        // 73 GetImage: drawable(u32) x(i16) y(i16) w(u16) h(u16) plane_mask(u32)
        73 => &[u32f!(0), i16f!(4), i16f!(6), u16f!(8), u16f!(10), u32f!(12)],
        // 74 PolyText8 / 75 PolyText16 / 76 ImageText8 / 77 ImageText16:
        //   drawable(u32) gc(u32) x(i16) y(i16) text(opaque)
        74..=77 => &[
            u32f!(0),
            u32f!(4),
            i16f!(8),
            i16f!(10),
            OpaqueTail { from: 12 },
        ],
        // 78 CreateColormap: mid(u32) window(u32) visual(u32). alloc in header.data.
        78 => &[u32f!(0), u32f!(4), u32f!(8)],
        // 80 CopyColormapAndFree: mid(u32) src(u32)
        80 => &[u32f!(0), u32f!(4)],
        // 84 AllocColor: cmap(u32) red(u16) green(u16) blue(u16)
        84 => &[u32f!(0), u16f!(4), u16f!(6), u16f!(8)],
        // 85 AllocNamedColor: cmap(u32) name_len(u16) [u16 pad] name(opaque)
        85 => &[u32f!(0), u16f!(4), OpaqueTail { from: 8 }],
        // 86 AllocColorCells: cmap(u32) colors(u16) planes(u16). contiguous in header.data.
        86 => &[u32f!(0), u16f!(4), u16f!(6)],
        // 87 AllocColorPlanes: cmap(u32) colors(u16) reds(u16) greens(u16) blues(u16)
        87 => &[u32f!(0), u16f!(4), u16f!(6), u16f!(8), u16f!(10)],
        // 88 FreeColors: cmap(u32) plane_mask(u32) pixels(u32[])
        88 => &[u32f!(0), u32f!(4), ElementArrayTail { from: 8, kind: U32 }],
        // 89 StoreColors: cmap(u32) items(u32+u16+u16+u16+u8+pad each = 12 bytes)
        // Approximate: typed prefix u32, then a u16 array tail (under-swaps the u8s
        // but they're not sensitive).
        89 => &[u32f!(0), ElementArrayTail { from: 4, kind: U16 }],
        // 90 StoreNamedColor: cmap(u32) pixel(u32) name_len(u16) [u16 pad] name(opaque)
        // do_red/do_green/do_blue are bits in header.data.
        90 => &[u32f!(0), u32f!(4), u16f!(8), OpaqueTail { from: 12 }],
        // 91 QueryColors: cmap(u32) pixels(u32[])
        91 => &[u32f!(0), ElementArrayTail { from: 4, kind: U32 }],
        // 92 LookupColor: cmap(u32) name_len(u16) [u16 pad] name(opaque)
        92 => &[u32f!(0), u16f!(4), OpaqueTail { from: 8 }],
        // 93 CreateCursor: cid(u32) source(u32) mask(u32) fg/bg colors u16 x6
        //                 hot x(u16) hot y(u16)
        93 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            u16f!(12),
            u16f!(14),
            u16f!(16),
            u16f!(18),
            u16f!(20),
            u16f!(22),
            u16f!(24),
            u16f!(26),
        ],
        // 94 CreateGlyphCursor: cid(u32) source_font(u32) mask_font(u32)
        //                      source_char(u16) mask_char(u16) fg/bg u16x6
        94 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            u16f!(12),
            u16f!(14),
            u16f!(16),
            u16f!(18),
            u16f!(20),
            u16f!(22),
            u16f!(24),
            u16f!(26),
        ],
        // 96 RecolorCursor: cursor(u32) fg/bg u16 x6
        96 => &[
            u32f!(0),
            u16f!(4),
            u16f!(6),
            u16f!(8),
            u16f!(10),
            u16f!(12),
            u16f!(14),
        ],
        // 97 QueryBestSize: drawable(u32) w(u16) h(u16). class in header.data.
        97 => &[u32f!(0), u16f!(4), u16f!(6)],
        // 98 QueryExtension: name_len(u16) [u16 pad] name(opaque)
        98 => &[u16f!(0), OpaqueTail { from: 4 }],
        // 100 ChangeKeyboardMapping: keysym_per_keycode(u8 in header.data),
        //   first_keycode(u8 at body[0])? Actually:
        //   header.data = keycode_count, body[0] = first_keycode,
        //   body[1] = keysyms_per_keycode, body[2..4] = pad, body[4..] = u32[] keysyms
        100 => &[ElementArrayTail { from: 4, kind: U32 }],
        // 101 GetKeyboardMapping: first_keycode(u8) count(u8) [u16 pad]
        101 => &[],
        // 102 ChangeKeyboardControl: mask(u32) values(u32[])
        102 => &[u32f!(0), ElementArrayTail { from: 4, kind: U32 }],
        // 105 ChangePointerControl: accel_num(i16) accel_denom(i16) threshold(i16)
        //                           [u8 do_accel u8 do_threshold]
        105 => &[i16f!(0), i16f!(2), i16f!(4)],
        // 107 SetScreenSaver: timeout(i16) interval(i16) [u8 prefer_blanking u8 allow_exposures]
        107 => &[i16f!(0), i16f!(2)],
        // 109 ChangeHosts: family(u8 header.data), nbytes(u16 body[2..4])? Actually:
        //   mode(u8 header.data) family(u8 body[0]) [u8 pad] address_len(u16 body[2..4])
        //   address(opaque)
        109 => &[u16f!(2), OpaqueTail { from: 4 }],
        // 112 SetCloseDownMode: in header.data — body empty
        // 113 KillClient: resource(u32) — covered above
        // 114 RotateProperties: window(u32) num_props(u16) delta(i16) properties(u32[])
        114 => &[
            u32f!(0),
            u16f!(4),
            i16f!(6),
            ElementArrayTail { from: 8, kind: U32 },
        ],
        // 116 SetPointerMapping / 118 SetModifierMapping — body is u8[] (no swap)
        // 127 NoOperation — body opaque
        // Default: no entry.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_4_destroy_window_swaps_window_id() {
        let mut body = vec![0xaa, 0xbb, 0xcc, 0xdd];
        swap_request_body(4, ClientByteOrder::BigEndian, &mut body);
        // u32 0xaabbccdd in BE → bytes [0xaa, 0xbb, 0xcc, 0xdd]; native LE
        // representation after swap → [0xdd, 0xcc, 0xbb, 0xaa].
        assert_eq!(body, vec![0xdd, 0xcc, 0xbb, 0xaa]);
    }

    #[test]
    fn opcode_84_alloc_color_swaps_cmap_and_color_components() {
        // body: cmap(u32) red(u16) green(u16) blue(u16) [u16 pad]
        let mut body = vec![
            0x00, 0x00, 0x00, 0x10, // cmap = 0x10 in BE
            0x00, 0xff, // red = 0xff in BE
            0x00, 0xaa, // green = 0xaa
            0x00, 0x55, // blue = 0x55
            0x00, 0x00, // pad
        ];
        swap_request_body(84, ClientByteOrder::BigEndian, &mut body);
        // After swap the body is in LE, decoded values stay the same.
        assert_eq!(
            u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
            0x10
        );
        assert_eq!(u16::from_le_bytes([body[4], body[5]]), 0xff);
        assert_eq!(u16::from_le_bytes([body[6], body[7]]), 0xaa);
        assert_eq!(u16::from_le_bytes([body[8], body[9]]), 0x55);
    }

    #[test]
    fn unknown_opcode_passes_through() {
        let mut body = vec![1, 2, 3, 4];
        let original = body.clone();
        // 200 is well beyond core + extension space.
        swap_request_body(200, ClientByteOrder::BigEndian, &mut body);
        assert_eq!(body, original);
    }

    #[test]
    fn le_client_no_op() {
        let mut body = vec![0xaa, 0xbb, 0xcc, 0xdd];
        let original = body.clone();
        swap_request_body(4, ClientByteOrder::LittleEndian, &mut body);
        assert_eq!(body, original);
    }
}
