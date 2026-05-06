//! Per-opcode length specifications for X11 core protocol requests
//! (opcodes 1–127).
//!
//! Each request has a wire length expressed in 4-byte units, declared in
//! the request header. The X11 protocol defines, per opcode, either a
//! fixed length or a minimum length plus a variable tail. xts5 probes
//! both under-length and (for fixed-length opcodes) over-length headers
//! and expects the server to reply with `BadLength`.

/// Length contract for a single core opcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LenSpec {
    /// The request is exactly this many 4-byte units.
    Fixed(u32),
    /// The request is at least this many 4-byte units; the tail is
    /// variable (list, string, value-list, image data, ...).
    AtLeast(u32),
}

/// Return the length contract for a core X11 opcode (1–127), or `None`
/// for opcodes outside that range or that we don't enforce.
#[must_use]
pub fn core_request_length(opcode: u8) -> Option<LenSpec> {
    use LenSpec::{AtLeast, Fixed};
    Some(match opcode {
        1 => AtLeast(8),   // CreateWindow         8 + n
        2 => AtLeast(3),   // ChangeWindowAttrs    3 + n
        3 => Fixed(2),     // GetWindowAttributes
        4 => Fixed(2),     // DestroyWindow
        5 => Fixed(2),     // DestroySubwindows
        6 => Fixed(2),     // ChangeSaveSet
        7 => Fixed(4),     // ReparentWindow
        8 => Fixed(2),     // MapWindow
        9 => Fixed(2),     // MapSubwindows
        10 => Fixed(2),    // UnmapWindow
        11 => Fixed(2),    // UnmapSubwindows
        12 => AtLeast(3),  // ConfigureWindow      3 + n
        13 => Fixed(2),    // CirculateWindow
        14 => Fixed(2),    // GetGeometry
        15 => Fixed(2),    // QueryTree
        16 => AtLeast(2),  // InternAtom           2 + (n+p)/4
        17 => Fixed(2),    // GetAtomName
        18 => AtLeast(6),  // ChangeProperty       6 + (n+p)/4
        19 => Fixed(3),    // DeleteProperty
        20 => Fixed(6),    // GetProperty
        21 => Fixed(2),    // ListProperties
        22 => Fixed(4),    // SetSelectionOwner
        23 => Fixed(2),    // GetSelectionOwner
        24 => Fixed(6),    // ConvertSelection
        25 => Fixed(11),   // SendEvent
        26 => Fixed(6),    // GrabPointer
        27 => Fixed(2),    // UngrabPointer
        28 => Fixed(6),    // GrabButton
        29 => Fixed(3),    // UngrabButton
        30 => Fixed(4),    // ChangeActivePointerGrab
        31 => Fixed(4),    // GrabKeyboard
        32 => Fixed(2),    // UngrabKeyboard
        33 => Fixed(4),    // GrabKey
        34 => Fixed(3),    // UngrabKey
        35 => Fixed(2),    // AllowEvents
        36 => Fixed(1),    // GrabServer
        37 => Fixed(1),    // UngrabServer
        38 => Fixed(2),    // QueryPointer
        39 => Fixed(4),    // GetMotionEvents
        40 => Fixed(4),    // TranslateCoordinates
        41 => Fixed(6),    // WarpPointer
        42 => Fixed(3),    // SetInputFocus
        43 => Fixed(1),    // GetInputFocus
        44 => Fixed(1),    // QueryKeymap
        45 => AtLeast(3),  // OpenFont             3 + (n+p)/4
        46 => Fixed(2),    // CloseFont
        47 => Fixed(2),    // QueryFont
        48 => AtLeast(2),  // QueryTextExtents     2 + (2n+p)/4
        49 => AtLeast(2),  // ListFonts            2 + (n+p)/4
        50 => AtLeast(2),  // ListFontsWithInfo    2 + (n+p)/4
        51 => AtLeast(2),  // SetFontPath          2 + (n+p)/4
        52 => Fixed(1),    // GetFontPath
        53 => Fixed(4),    // CreatePixmap
        54 => Fixed(2),    // FreePixmap
        55 => AtLeast(4),  // CreateGC             4 + n
        56 => AtLeast(3),  // ChangeGC             3 + n
        57 => Fixed(4),    // CopyGC
        58 => AtLeast(3),  // SetDashes            3 + (n+p)/4
        59 => AtLeast(3),  // SetClipRectangles    3 + 2n
        60 => Fixed(2),    // FreeGC
        61 => Fixed(4),    // ClearArea
        62 => Fixed(7),    // CopyArea
        63 => Fixed(8),    // CopyPlane
        64 => AtLeast(3),  // PolyPoint            3 + n
        65 => AtLeast(3),  // PolyLine             3 + n
        66 => AtLeast(3),  // PolySegment          3 + 2n
        67 => AtLeast(3),  // PolyRectangle        3 + 2n
        68 => AtLeast(3),  // PolyArc              3 + 3n
        69 => AtLeast(4),  // FillPoly             4 + n
        70 => AtLeast(3),  // PolyFillRectangle    3 + 2n
        71 => AtLeast(3),  // PolyFillArc          3 + 3n
        72 => AtLeast(6),  // PutImage             6 + (n+p)/4
        73 => Fixed(5),    // GetImage
        74 => AtLeast(4),  // PolyText8            4 + (n+p)/4
        75 => AtLeast(4),  // PolyText16           4 + (n+p)/4
        76 => AtLeast(4),  // ImageText8           4 + (n+p)/4
        77 => AtLeast(4),  // ImageText16          4 + (n+p)/4
        78 => Fixed(4),    // CreateColormap
        79 => Fixed(2),    // FreeColormap
        80 => Fixed(3),    // CopyColormapAndFree
        81 => Fixed(2),    // InstallColormap
        82 => Fixed(2),    // UninstallColormap
        83 => Fixed(2),    // ListInstalledColormaps
        84 => Fixed(4),    // AllocColor
        85 => AtLeast(3),  // AllocNamedColor      3 + (n+p)/4
        86 => Fixed(3),    // AllocColorCells
        87 => Fixed(4),    // AllocColorPlanes
        88 => AtLeast(3),  // FreeColors           3 + n
        89 => AtLeast(1),  // StoreColors          1 + 3n
        90 => AtLeast(4),  // StoreNamedColor      4 + (n+p)/4
        91 => AtLeast(2),  // QueryColors          2 + n
        92 => AtLeast(3),  // LookupColor          3 + (n+p)/4
        93 => Fixed(8),    // CreateCursor
        94 => Fixed(8),    // CreateGlyphCursor
        95 => Fixed(2),    // FreeCursor
        96 => Fixed(5),    // RecolorCursor
        97 => Fixed(3),    // QueryBestSize
        98 => AtLeast(2),  // QueryExtension       2 + (n+p)/4
        99 => Fixed(1),    // ListExtensions
        100 => AtLeast(2), // ChangeKeyboardMapping 2 + nm
        101 => Fixed(2),   // GetKeyboardMapping
        102 => AtLeast(2), // ChangeKeyboardControl 2 + n
        103 => Fixed(1),   // GetKeyboardControl
        104 => Fixed(1),   // Bell
        105 => Fixed(3),   // ChangePointerControl
        106 => Fixed(1),   // GetPointerControl
        107 => Fixed(3),   // SetScreenSaver
        108 => Fixed(1),   // GetScreenSaver
        109 => AtLeast(2), // ChangeHosts          2 + (n+p)/4
        110 => Fixed(1),   // ListHosts
        111 => Fixed(1),   // SetAccessControl
        112 => Fixed(1),   // SetCloseDownMode
        113 => Fixed(2),   // KillClient
        114 => AtLeast(3), // RotateProperties     3 + n
        115 => Fixed(1),   // ForceScreenSaver
        116 => AtLeast(1), // SetPointerMapping    1 + (n+p)/4
        117 => Fixed(1),   // GetPointerMapping
        118 => AtLeast(1), // SetModifierMapping   1 + 2n
        119 => Fixed(1),   // GetModifierMapping
        127 => AtLeast(1), // NoOperation          1 + n
        _ => return None,
    })
}

/// Returns `true` if `length_units` (the value carried in the request
/// header, possibly extended via BIG-REQUESTS) satisfies the spec for
/// `opcode`. Opcodes outside the core range or unknown to us return
/// `true` (the dispatcher decides).
#[must_use]
pub fn validate_core_request_length(opcode: u8, length_units: u32) -> bool {
    match core_request_length(opcode) {
        Some(LenSpec::Fixed(n)) => length_units == n,
        Some(LenSpec::AtLeast(n)) => length_units >= n,
        None => true,
    }
}

/// Helper: round `bytes` up to the next 4-byte multiple, expressed
/// in 4-byte units.
const fn pad_units(bytes: u32) -> u32 {
    (bytes + 3) / 4
}

const fn read_u16_le(b: &[u8]) -> u32 {
    (b[0] as u32) | ((b[1] as u32) << 8)
}

const fn read_u32_le(b: &[u8]) -> u32 {
    (b[0] as u32) | ((b[1] as u32) << 8) | ((b[2] as u32) << 16) | ((b[3] as u32) << 24)
}

/// Compute the *exact* required `length_units` for a variable-length
/// core opcode given its header.data byte and (LE-decoded) body.
/// Returns `None` for opcodes whose length is fully determined by
/// `core_request_length` (i.e., fixed) or for those we don't model
/// content-aware.
///
/// Body must be at least the spec minimum length; callers should
/// have already passed `validate_core_request_length`.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn exact_required_length(opcode: u8, header_data: u8, body: &[u8]) -> Option<u32> {
    match opcode {
        // 1 CreateWindow: 8 + popcount(value_mask u32 at body[24..28])
        1 if body.len() >= 28 => {
            let mask = read_u32_le(&body[24..28]);
            Some(8 + mask.count_ones())
        }
        // 2 ChangeWindowAttributes: 3 + popcount(mask u32 at body[4..8])
        2 if body.len() >= 8 => {
            let mask = read_u32_le(&body[4..8]);
            Some(3 + mask.count_ones())
        }
        // 12 ConfigureWindow: 3 + popcount(mask u16 at body[4..6])
        12 if body.len() >= 6 => {
            let mask = read_u16_le(&body[4..6]);
            Some(3 + mask.count_ones())
        }
        // 16 InternAtom: 2 + pad_units(name_len)
        16 if body.len() >= 2 => {
            let nlen = read_u16_le(&body[0..2]);
            Some(2 + pad_units(nlen))
        }
        // 18 ChangeProperty: 6 + pad_units(value_len * format / 8)
        // header.data = format ∈ {8, 16, 32}. body[12..16] = value_len (u32, units).
        18 if body.len() >= 16 => {
            let format = u32::from(header_data);
            if format != 8 && format != 16 && format != 32 {
                return None;
            }
            let value_len = read_u32_le(&body[12..16]);
            let bytes = value_len.checked_mul(format / 8)?;
            Some(6 + pad_units(bytes))
        }
        // 45 OpenFont: 3 + pad_units(name_len u16 at body[4..6])
        45 if body.len() >= 6 => {
            let nlen = read_u16_le(&body[4..6]);
            Some(3 + pad_units(nlen))
        }
        // 49 ListFonts / 50 ListFontsWithInfo: 2 + pad_units(name_len u16 at body[2..4])
        49 | 50 if body.len() >= 4 => {
            let nlen = read_u16_le(&body[2..4]);
            Some(2 + pad_units(nlen))
        }
        // 55 CreateGC: 4 + popcount(mask u32 at body[8..12])
        55 if body.len() >= 12 => {
            let mask = read_u32_le(&body[8..12]);
            Some(4 + mask.count_ones())
        }
        // 56 ChangeGC: 3 + popcount(mask u32 at body[4..8])
        56 if body.len() >= 8 => {
            let mask = read_u32_le(&body[4..8]);
            Some(3 + mask.count_ones())
        }
        // 58 SetDashes: 3 + pad_units(ndashes u16 at body[6..8])
        58 if body.len() >= 8 => {
            let ndash = read_u16_le(&body[6..8]);
            Some(3 + pad_units(ndash))
        }
        // 85 AllocNamedColor: 3 + pad_units(name_len u16 at body[4..6])
        // 92 LookupColor: same shape
        85 | 92 if body.len() >= 6 => {
            let nlen = read_u16_le(&body[4..6]);
            Some(3 + pad_units(nlen))
        }
        // 90 StoreNamedColor: 4 + pad_units(name_len u16 at body[8..10])
        90 if body.len() >= 10 => {
            let nlen = read_u16_le(&body[8..10]);
            Some(4 + pad_units(nlen))
        }
        // 98 QueryExtension: 2 + pad_units(name_len u16 at body[0..2])
        98 if body.len() >= 2 => {
            let nlen = read_u16_le(&body[0..2]);
            Some(2 + pad_units(nlen))
        }
        // 100 ChangeKeyboardMapping: 2 + keycode_count * keysyms_per_keycode
        // header.data = keycode_count, body[1] = keysyms_per_keycode
        100 if body.len() >= 2 => {
            let kpk = u32::from(body[1]);
            let count = u32::from(header_data);
            Some(2 + count.checked_mul(kpk)?)
        }
        // 102 ChangeKeyboardControl: 2 + popcount(mask u32 at body[0..4])
        102 if body.len() >= 4 => {
            let mask = read_u32_le(&body[0..4]);
            Some(2 + mask.count_ones())
        }
        // 109 ChangeHosts: 2 + pad_units(nbytes u16 at body[2..4])
        109 if body.len() >= 4 => {
            let n = read_u16_le(&body[2..4]);
            Some(2 + pad_units(n))
        }
        // 114 RotateProperties: 3 + nprops (u16 at body[4..6])
        114 if body.len() >= 6 => {
            let n = read_u16_le(&body[4..6]);
            Some(3 + n)
        }
        // 116 SetPointerMapping: 1 + pad_units(map_len = header.data)
        116 => Some(1 + pad_units(u32::from(header_data))),
        // 118 SetModifierMapping: 1 + 2 * keycodes_per_modifier (header.data)
        118 => Some(1 + 2 * u32::from(header_data)),
        _ => None,
    }
}

/// Returns `true` iff the request's `length_units` matches the exact
/// content-derived required length, when applicable. Returns `true`
/// for opcodes we don't model (the simple AtLeast/Fixed check from
/// `validate_core_request_length` is the only gate for those).
#[must_use]
pub fn validate_exact_request_length(
    opcode: u8,
    header_data: u8,
    length_units: u32,
    body: &[u8],
) -> bool {
    exact_required_length(opcode, header_data, body).map_or(true, |req| length_units == req)
}

#[cfg(test)]
mod exact_tests {
    use super::*;

    #[test]
    fn alloc_color_is_not_modeled() {
        // Fixed-length AllocColor (84) is fully covered by Fixed(4); no
        // content-aware check needed.
        assert!(exact_required_length(84, 0, &[0; 12]).is_none());
    }

    #[test]
    fn change_gc_zero_mask_is_3_units() {
        // body: gc(4) + mask(4) = 8 bytes, mask = 0, no values.
        let body = [0u8; 8];
        assert_eq!(exact_required_length(56, 0, &body), Some(3));
    }

    #[test]
    fn change_gc_two_bits_set_is_5_units() {
        let mut body = [0u8; 8];
        body[4] = 0b0000_0011; // mask = 3
        assert_eq!(exact_required_length(56, 0, &body), Some(5));
    }

    #[test]
    fn intern_atom_eight_byte_name_is_4_units() {
        let mut body = [0u8; 12];
        body[0] = 8; // nlen LO
        body[1] = 0;
        // Required = 2 + ceil(8/4) = 2 + 2 = 4
        assert_eq!(exact_required_length(16, 0, &body), Some(4));
    }

    #[test]
    fn change_keyboard_mapping_3_keycodes_2_keysyms_each_is_8_units() {
        let mut body = [0u8; 4];
        body[1] = 2; // keysyms_per_keycode
        // header.data = 3 keycodes
        // Required = 2 + 3 * 2 = 8
        assert_eq!(exact_required_length(100, 3, &body), Some(8));
    }
}

#[cfg(test)]
mod tests {
    use super::{LenSpec, core_request_length, validate_core_request_length};

    #[test]
    fn fixed_opcode_rejects_under_and_over() {
        // GetGeometry is fixed at 2 units.
        assert!(!validate_core_request_length(14, 1));
        assert!(validate_core_request_length(14, 2));
        assert!(!validate_core_request_length(14, 3));
    }

    #[test]
    fn variable_opcode_rejects_under_only() {
        // CreateWindow is at least 8 units.
        assert!(!validate_core_request_length(1, 7));
        assert!(validate_core_request_length(1, 8));
        assert!(validate_core_request_length(1, 100));
    }

    #[test]
    fn extension_opcodes_pass_through() {
        // 128+ are extensions; we don't enforce here.
        assert!(core_request_length(128).is_none());
        assert!(core_request_length(146).is_none());
        assert!(validate_core_request_length(128, 1));
    }

    #[test]
    fn alloc_color_is_fixed_4() {
        assert_eq!(core_request_length(84), Some(LenSpec::Fixed(4)));
    }

    #[test]
    fn send_event_is_fixed_11() {
        assert_eq!(core_request_length(25), Some(LenSpec::Fixed(11)));
    }
}
