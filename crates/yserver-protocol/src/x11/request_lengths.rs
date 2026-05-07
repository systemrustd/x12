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
    bytes.div_ceil(4)
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
        // header.data = mode (Replace/Prepend/Append). format is at body[12]
        // (u8), value_len at body[16..20] (u32).
        18 if body.len() >= 20 => {
            let format = u32::from(body[12]);
            if format != 8 && format != 16 && format != 32 {
                return None;
            }
            let value_len = read_u32_le(&body[16..20]);
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
        // 59 SetClipRectangles: body = gc(4) + clip_x(2) + clip_y(2)
        //                              + rects(8 × N). Body must be
        //                              8 + 8k bytes; otherwise BadLength.
        // (length_units already matches body.len() by construction in
        // read_request; the constraint is on the body's *shape*.)
        59 => {
            if body.len() >= 8 && (body.len() - 8).is_multiple_of(8) {
                None // shape is valid, length_units already passes
            } else {
                Some(u32::MAX) // sentinel: never matches, fires BadLength
            }
        }
        // 89 StoreColors: body = cmap(4) + items(12 × N).
        89 => {
            if body.len() >= 4 && (body.len() - 4).is_multiple_of(12) {
                None
            } else {
                Some(u32::MAX)
            }
        }
        // 88 FreeColors: body = cmap(4) + plane_mask(4) + pixels(u32 × N).
        88 => {
            if body.len() >= 8 && (body.len() - 8).is_multiple_of(4) {
                None
            } else {
                Some(u32::MAX)
            }
        }
        // 64 PolyPoint: body = drawable(4) + gc(4) + points(4 × N)
        // 65 PolyLine: same shape (points are 4 bytes each: i16 x + i16 y).
        64 | 65 => {
            if body.len() >= 8 && (body.len() - 8).is_multiple_of(4) {
                None
            } else {
                Some(u32::MAX)
            }
        }
        // 66 PolySegment: drawable(4) + gc(4) + segments(8 × N)
        // 67 PolyRectangle: drawable(4) + gc(4) + rects(8 × N)
        // 70 PolyFillRectangle: same shape.
        66 | 67 | 70 => {
            if body.len() >= 8 && (body.len() - 8).is_multiple_of(8) {
                None
            } else {
                Some(u32::MAX)
            }
        }
        // 68 PolyArc, 71 PolyFillArc: drawable(4) + gc(4) + arcs(12 × N)
        68 | 71 => {
            if body.len() >= 8 && (body.len() - 8).is_multiple_of(12) {
                None
            } else {
                Some(u32::MAX)
            }
        }
        // 76 ImageText8 / 77 ImageText16 / 74 PolyText8 / 75 PolyText16:
        //   drawable(4) + gc(4) + x(2) + y(2) + text(opaque). header.data
        //   carries the string length for ImageText*. Total body must be
        //   at least 12 (header) + nbytes (rounded to 4).
        76 if body.len() >= 12 => {
            // ImageText8: nbytes = header.data, total body = 12 + ceil(nbytes/4)*4
            let n = u32::from(header_data);
            #[allow(clippy::cast_possible_truncation)]
            let body_units = (body.len() / 4) as u32;
            let expected_units = 4 + n.div_ceil(4); // total request length_units
            if body_units == expected_units - 1 {
                None
            } else {
                Some(u32::MAX)
            }
        }
        77 if body.len() >= 12 => {
            // ImageText16: nbytes = 2 * header.data, total body = 12 + ceil(2n/4)*4
            let n = 2 * u32::from(header_data);
            #[allow(clippy::cast_possible_truncation)]
            let body_units = (body.len() / 4) as u32;
            let expected_units = 4 + n.div_ceil(4);
            if body_units == expected_units - 1 {
                None
            } else {
                Some(u32::MAX)
            }
        }
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
    exact_required_length(opcode, header_data, body).is_none_or(|req| length_units == req)
}

/// For opcodes that carry a value-mask, returns `Some(bad_value)` if
/// the mask has bits set beyond the spec-defined range. Returns
/// `None` if the mask is valid or the opcode doesn't carry a mask.
///
/// The X11 spec requires the server to reply `BadValue` (not
/// `BadMatch`) when an unused mask bit is set.
#[must_use]
pub fn invalid_value_mask(opcode: u8, body: &[u8]) -> Option<u32> {
    fn check_u32(body: &[u8], offset: usize, valid_bits: u32) -> Option<u32> {
        if body.len() < offset + 4 {
            return None;
        }
        let mask = read_u32_le(&body[offset..offset + 4]);
        if mask & !valid_bits != 0 {
            Some(mask)
        } else {
            None
        }
    }
    fn check_u16(body: &[u8], offset: usize, valid_bits: u16) -> Option<u32> {
        if body.len() < offset + 2 {
            return None;
        }
        let mask = read_u16_le(&body[offset..offset + 2]) as u16;
        if mask & !valid_bits != 0 {
            Some(u32::from(mask))
        } else {
            None
        }
    }
    // Bit ranges from the X11 protocol value-mask definitions.
    const CW_VALID: u32 = 0x7FFF; // CreateWindow / ChangeWindowAttributes: 15 bits
    const CFG_VALID: u16 = 0x7F; // ConfigureWindow: 7 bits
    const GC_VALID: u32 = 0x3F_FFFF; // CreateGC / ChangeGC / CopyGC: 22 bits

    const KB_VALID: u32 = 0xFF; // ChangeKeyboardControl: 8 bits

    match opcode {
        1 => check_u32(body, 24, CW_VALID),  // CreateWindow
        2 => check_u32(body, 4, CW_VALID),   // ChangeWindowAttributes
        12 => check_u16(body, 4, CFG_VALID), // ConfigureWindow
        55 => check_u32(body, 8, GC_VALID),  // CreateGC
        56 => check_u32(body, 4, GC_VALID),  // ChangeGC
        57 => check_u32(body, 8, GC_VALID),  // CopyGC
        102 => check_u32(body, 0, KB_VALID), // ChangeKeyboardControl
        _ => None,
    }
}

/// Walk a value-list for a request whose header carries a value-mask:
/// for each bit set in `mask`, locate the corresponding 4-byte entry
/// at `values_start + n*4` (n = ordinal of this bit among set bits)
/// and call `validate_bit`. Returns the first `Some(bad_value)` from
/// the validator, or `None` if every set bit's value passes.
///
/// `validate_bit` receives `(bit_index, value_u32)`; bit indexes refer
/// to the spec-defined CW/GC/KB constants and are 0-based.
fn validate_value_list<F>(
    body: &[u8],
    values_start: usize,
    mask: u32,
    validate_bit: F,
) -> Option<u32>
where
    F: Fn(u32, u32) -> Option<u32>,
{
    let mut idx = 0usize;
    for bit in 0..32 {
        if mask & (1 << bit) == 0 {
            continue;
        }
        let off = values_start + idx * 4;
        if body.len() < off + 4 {
            return None;
        }
        let v = read_u32_le(&body[off..off + 4]);
        if let Some(bad) = validate_bit(bit, v) {
            return Some(bad);
        }
        idx += 1;
    }
    None
}

/// CW (CreateWindow / ChangeWindowAttributes) per-bit value validator.
/// Reference: X11 protocol §11 (Window Attributes).
fn cw_validate_bit(bit: u32, v: u32) -> Option<u32> {
    let bad = |x: u32| Some(x);
    match bit {
        // bit_gravity ∈ Forget(0)..Static(10)
        4 if v > 10 => bad(v),
        // win_gravity ∈ Unmap(0)..Static(10)
        5 if v > 10 => bad(v),
        // backing_store ∈ {NotUseful=0, WhenMapped=1, Always=2}
        6 if v > 2 => bad(v),
        // override_redirect: BOOL
        9 if v > 1 => bad(v),
        // save_under: BOOL
        10 if v > 1 => bad(v),
        // event_mask: 25 bits (KeyPress..OwnerGrabButton)
        11 if v & !0x01ff_ffff != 0 => bad(v),
        // do_not_propagate_mask: subset of pointer/keyboard events.
        // Bits: KeyPress(0), KeyRelease(1), ButtonPress(2),
        // ButtonRelease(3), PointerMotion(6), Button1..5Motion(8..12),
        // ButtonMotion(13). Mask = 0x0000_3F4F.
        12 if v & !0x0000_3F4F != 0 => bad(v),
        _ => None,
    }
}

/// GC (CreateGC / ChangeGC) per-bit value validator.
/// Reference: X11 protocol §7 (Graphics Context).
fn gc_validate_bit(bit: u32, v: u32) -> Option<u32> {
    let bad = |x: u32| Some(x);
    match bit {
        // function ∈ Clear(0)..Set(15)
        0 if v > 15 => bad(v),
        // line_style ∈ {Solid=0, OnOffDash=1, DoubleDash=2}
        5 if v > 2 => bad(v),
        // cap_style ∈ {NotLast=0, Butt=1, Round=2, Projecting=3}
        6 if v > 3 => bad(v),
        // join_style ∈ {Miter=0, Round=1, Bevel=2}
        7 if v > 2 => bad(v),
        // fill_style ∈ {Solid=0, Tiled=1, Stippled=2, OpaqueStippled=3}
        8 if v > 3 => bad(v),
        // fill_rule ∈ {EvenOdd=0, Winding=1}
        9 if v > 1 => bad(v),
        // subwindow_mode ∈ {ClipByChildren=0, IncludeInferiors=1}
        15 if v > 1 => bad(v),
        // graphics_exposures: BOOL
        16 if v > 1 => bad(v),
        // arc_mode ∈ {Chord=0, PieSlice=1}
        22 if v > 1 => bad(v),
        _ => None,
    }
}

/// ConfigureWindow per-bit value validator. Only stack_mode (bit 6)
/// has a fixed range; the geometry/sibling values are validated in
/// the dedicated handler.
fn cfg_validate_bit(bit: u32, v: u32) -> Option<u32> {
    match bit {
        // stack_mode ∈ {Above=0, Below=1, TopIf=2, BottomIf=3, Opposite=4}
        6 if v > 4 => Some(v),
        _ => None,
    }
}

/// ChangeKeyboardControl per-bit value validator. Percent fields use
/// INT8 wire encoding with the actual value in the low byte; we treat
/// the entry as i32 and require either -1 or [0,100].
fn kb_validate_bit(bit: u32, v: u32) -> Option<u32> {
    let bad = |x: u32| Some(x);
    // INT8/INT16 fields ride in the low byte/word of the 4-byte entry,
    // sign-extended. Cast through the smaller type to recover the sign.
    let as_i8 = (v & 0xff) as i8;
    let as_i16 = (v & 0xffff) as i16;
    match bit {
        // key_click_percent (INT8): -1 (default) or [0,100]
        0 if !(as_i8 == -1 || (0..=100).contains(&as_i8)) => bad(v),
        // bell_percent: same range as above
        1 if !(as_i8 == -1 || (0..=100).contains(&as_i8)) => bad(v),
        // bell_pitch (INT16, Hz): -1 or non-negative
        2 if as_i16 < -1 => bad(v),
        // bell_duration (INT16, ms): -1 or non-negative
        3 if as_i16 < -1 => bad(v),
        // led_mode ∈ {Off=0, On=1}
        5 if v > 1 => bad(v),
        // auto_repeat_mode ∈ {Off=0, On=1, Default=2}
        7 if v > 2 => bad(v),
        _ => None,
    }
}

/// Per-opcode value-range validation for fixed-position scalar fields
/// (Group A) and value-list walking (Group B). Returns `Some(bad_value)`
/// if any field is out of range; the caller must respond `BadValue`.
///
/// Run this *after* `invalid_value_mask` so that mask bits are clean
/// before we trust value-list payloads.
#[must_use]
pub fn invalid_value(opcode: u8, header_data: u8, body: &[u8]) -> Option<u32> {
    fn bad_bool(b: u8) -> Option<u32> {
        if b > 1 { Some(u32::from(b)) } else { None }
    }
    fn check_byte_range(body: &[u8], off: usize, max_inclusive: u8) -> Option<u32> {
        if body.len() <= off {
            return None;
        }
        let v = body[off];
        if v > max_inclusive {
            Some(u32::from(v))
        } else {
            None
        }
    }
    match opcode {
        // GrabPointer: header.data=owner_events; body[6]=pointer_mode,
        // body[7]=keyboard_mode (Sync=0, Async=1).
        26 => bad_bool(header_data)
            .or_else(|| check_byte_range(body, 6, 1))
            .or_else(|| check_byte_range(body, 7, 1)),
        // GrabButton: GrabPointer fields + body[16]=button (0=AnyButton, 1..=5).
        28 => bad_bool(header_data)
            .or_else(|| check_byte_range(body, 6, 1))
            .or_else(|| check_byte_range(body, 7, 1))
            .or_else(|| check_byte_range(body, 16, 5)),
        // GrabKeyboard: header.data=owner_events;
        // body[8]=pointer_mode, body[9]=keyboard_mode.
        31 => bad_bool(header_data)
            .or_else(|| check_byte_range(body, 8, 1))
            .or_else(|| check_byte_range(body, 9, 1)),
        // GrabKey: header.data=owner_events;
        // body[7]=pointer_mode, body[8]=keyboard_mode.
        33 => bad_bool(header_data)
            .or_else(|| check_byte_range(body, 7, 1))
            .or_else(|| check_byte_range(body, 8, 1)),
        // CopyPlane: body[24..28]=bit_plane, must have exactly one bit set.
        63 => {
            if body.len() < 28 {
                return None;
            }
            let plane = read_u32_le(&body[24..28]);
            if plane.count_ones() == 1 {
                None
            } else {
                Some(plane)
            }
        }
        // CreateWindow: mask u32 @ body[24..28], values @ body[28].
        1 if body.len() >= 28 => {
            let mask = read_u32_le(&body[24..28]);
            validate_value_list(body, 28, mask, cw_validate_bit)
        }
        // ChangeWindowAttributes: mask u32 @ body[4..8], values @ body[8].
        2 if body.len() >= 8 => {
            let mask = read_u32_le(&body[4..8]);
            validate_value_list(body, 8, mask, cw_validate_bit)
        }
        // CreateGC: mask u32 @ body[8..12], values @ body[12].
        55 if body.len() >= 12 => {
            let mask = read_u32_le(&body[8..12]);
            validate_value_list(body, 12, mask, gc_validate_bit)
        }
        // ChangeGC: mask u32 @ body[4..8], values @ body[8].
        56 if body.len() >= 8 => {
            let mask = read_u32_le(&body[4..8]);
            validate_value_list(body, 8, mask, gc_validate_bit)
        }
        // ChangeKeyboardControl: mask u32 @ body[0..4], values @ body[4].
        102 if body.len() >= 4 => {
            let mask = read_u32_le(&body[0..4]);
            validate_value_list(body, 4, mask, kb_validate_bit)
        }
        // ChangePointerControl: fixed body. No mask. Conditional checks:
        // do_accel/do_threshold are bool; if do_accel is set, accel
        // numerator/denominator must satisfy ≥-1 and denominator ≠ 0;
        // if do_threshold, threshold must be ≥-1.
        105 if body.len() >= 8 => {
            let num = i16::from_le_bytes([body[0], body[1]]);
            let den = i16::from_le_bytes([body[2], body[3]]);
            let thr = i16::from_le_bytes([body[4], body[5]]);
            let do_accel = body[6];
            let do_thr = body[7];
            if do_accel > 1 {
                return Some(u32::from(do_accel));
            }
            if do_thr > 1 {
                return Some(u32::from(do_thr));
            }
            if do_accel == 1 {
                if num < -1 {
                    return Some(num as u32);
                }
                if den < -1 {
                    return Some(den as u32);
                }
                if den == 0 {
                    return Some(0);
                }
            }
            if do_thr == 1 && thr < -1 {
                return Some(thr as u32);
            }
            None
        }
        // ChangeSaveSet: header.data = mode ∈ {Insert=0, Delete=1}.
        6 if header_data > 1 => Some(u32::from(header_data)),
        // ConfigureWindow: mask u16 @ body[4..6], values @ body[8].
        // Only stack_mode (bit 6) has a fixed range we enforce here.
        12 if body.len() >= 6 => {
            let mask = u32::from(read_u16_le(&body[4..6]));
            validate_value_list(body, 8, mask, cfg_validate_bit)
        }
        // CirculateWindow: header.data = direction ∈ {RaiseLowest=0, LowerHighest=1}.
        13 if header_data > 1 => Some(u32::from(header_data)),
        // SendEvent: header.data = propagate (BOOL).
        25 if header_data > 1 => Some(u32::from(header_data)),
        // AllowEvents: header.data = mode (CARD8) ∈ {0..=7}
        // (AsyncPointer..ReplayKeyboard, AsyncBoth, SyncBoth).
        35 if header_data > 7 => Some(u32::from(header_data)),
        // CreateColormap: header.data = alloc ∈ {None=0, All=1}.
        78 if header_data > 1 => Some(u32::from(header_data)),
        // Bell: header.data = percent (INT8) ∈ [-100, 100].
        104 => {
            let p = header_data as i8;
            if (-100..=100).contains(&p) {
                None
            } else {
                Some(u32::from(header_data))
            }
        }
        // SetScreenSaver: body[4] = prefer_blanking, body[5] = allow_exposures
        // both ∈ {No=0, Yes=1, Default=2}.
        107 if body.len() >= 6 => {
            if body[4] > 2 {
                return Some(u32::from(body[4]));
            }
            if body[5] > 2 {
                return Some(u32::from(body[5]));
            }
            None
        }
        // ForceScreenSaver: header.data = mode ∈ {Reset=0, Activate=1}.
        115 if header_data > 1 => Some(u32::from(header_data)),
        _ => None,
    }
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
    fn grab_server_empty_body_is_not_misclassified_as_freecolors() {
        // Opcode 36 is GrabServer (Fixed 1 unit, no body). Regression
        // guard: previously the FreeColors content-aware shape check
        // was keyed on opcode 36 (the correct opcode is 88), which
        // made every GrabServer fire spurious BadLength.
        assert!(validate_exact_request_length(36, 0, 1, &[]));
    }

    #[test]
    fn free_colors_shape_check_is_keyed_on_opcode_88() {
        // body = cmap(4) + plane_mask(4) + 1 pixel(4) = 12 bytes,
        // length_units = 1 + 12/4 = 4 (header) — but caller passes
        // length_units, so just verify the shape gate accepts it.
        assert!(validate_exact_request_length(88, 0, 4, &[0u8; 12]));
        // 11-byte body is malformed (not 8 + multiple of 4).
        assert!(!validate_exact_request_length(88, 0, 4, &[0u8; 11]));
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

#[cfg(test)]
mod value_range_tests {
    use super::invalid_value;

    fn body_with(byte_at: &[(usize, u8)]) -> Vec<u8> {
        let max_off = byte_at.iter().map(|(o, _)| *o).max().unwrap_or(0);
        let mut v = vec![0u8; max_off + 1];
        for (o, b) in byte_at {
            v[*o] = *b;
        }
        v
    }

    #[test]
    fn grab_pointer_owner_events_bool() {
        // header.data=2 → bad bool.
        let body = vec![0u8; 20];
        assert_eq!(invalid_value(26, 2, &body), Some(2));
        assert_eq!(invalid_value(26, 1, &body), None);
        assert_eq!(invalid_value(26, 0, &body), None);
    }

    #[test]
    fn grab_pointer_modes() {
        // body[6]=pointer_mode, body[7]=keyboard_mode (Sync=0, Async=1).
        let body = body_with(&[(6, 2)]);
        assert_eq!(invalid_value(26, 0, &body), Some(2));
        let body = body_with(&[(7, 5)]);
        assert_eq!(invalid_value(26, 0, &body), Some(5));
        let body = body_with(&[(6, 1), (7, 0)]);
        assert_eq!(invalid_value(26, 0, &body), None);
    }

    #[test]
    fn grab_button_button_range() {
        // body[16]=button: 0=AnyButton, 1..=5 valid; 6 invalid.
        let body = body_with(&[(16, 6)]);
        assert_eq!(invalid_value(28, 0, &body), Some(6));
        let body = body_with(&[(16, 5)]);
        assert_eq!(invalid_value(28, 0, &body), None);
        let body = body_with(&[(16, 0)]);
        assert_eq!(invalid_value(28, 0, &body), None);
    }

    #[test]
    fn grab_keyboard_modes() {
        // body[8]=pointer_mode, body[9]=keyboard_mode.
        let body = body_with(&[(8, 3)]);
        assert_eq!(invalid_value(31, 0, &body), Some(3));
        let body = body_with(&[(9, 7)]);
        assert_eq!(invalid_value(31, 0, &body), Some(7));
    }

    #[test]
    fn grab_key_modes() {
        // body[7]=pointer_mode, body[8]=keyboard_mode.
        let body = body_with(&[(7, 4)]);
        assert_eq!(invalid_value(33, 0, &body), Some(4));
        let body = body_with(&[(8, 9)]);
        assert_eq!(invalid_value(33, 0, &body), Some(9));
        let body = vec![0u8; 12];
        assert_eq!(invalid_value(33, 0, &body), None);
    }

    #[test]
    fn copy_plane_bit_plane_must_be_single_bit() {
        // body[24..28] = bit_plane (LE u32). 0x10 = single bit → ok.
        let mut body = vec![0u8; 28];
        body[24] = 0x10;
        assert_eq!(invalid_value(63, 0, &body), None);
        // Zero: zero bits → bad.
        let body = vec![0u8; 28];
        assert_eq!(invalid_value(63, 0, &body), Some(0));
        // Two bits set → bad.
        let mut body = vec![0u8; 28];
        body[24] = 0b0000_0011;
        assert_eq!(invalid_value(63, 0, &body), Some(3));
    }

    #[test]
    fn unmodelled_opcode_returns_none() {
        // GetGeometry (14) — not a value-bearing op.
        assert_eq!(invalid_value(14, 0, &[0u8; 4]), None);
    }

    // ── Group B: value-list walking ───────────────────────────────

    fn cw_body(mask: u32, values: &[u32]) -> Vec<u8> {
        let mut b = vec![0u8; 8 + values.len() * 4];
        b[4..8].copy_from_slice(&mask.to_le_bytes());
        for (i, v) in values.iter().enumerate() {
            b[8 + i * 4..12 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn cwa_bit_gravity_out_of_range() {
        // CWBitGravity = bit 4. Static = 10 (max). 11 → BadValue.
        let body = cw_body(1 << 4, &[11]);
        assert_eq!(invalid_value(2, 0, &body), Some(11));
        let body = cw_body(1 << 4, &[10]);
        assert_eq!(invalid_value(2, 0, &body), None);
    }

    #[test]
    fn cwa_backing_store_out_of_range() {
        // CWBackingStore = bit 6. Allowed: 0,1,2.
        let body = cw_body(1 << 6, &[3]);
        assert_eq!(invalid_value(2, 0, &body), Some(3));
        let body = cw_body(1 << 6, &[2]);
        assert_eq!(invalid_value(2, 0, &body), None);
    }

    #[test]
    fn cwa_override_redirect_bool() {
        // CWOverrideRedirect = bit 9.
        let body = cw_body(1 << 9, &[2]);
        assert_eq!(invalid_value(2, 0, &body), Some(2));
        let body = cw_body(1 << 9, &[1]);
        assert_eq!(invalid_value(2, 0, &body), None);
    }

    #[test]
    fn cwa_event_mask_unused_bits() {
        // CWEventMask = bit 11. Bit 25+ is reserved.
        let body = cw_body(1 << 11, &[1u32 << 25]);
        assert_eq!(invalid_value(2, 0, &body), Some(1 << 25));
        let body = cw_body(1 << 11, &[0x01ff_ffff]);
        assert_eq!(invalid_value(2, 0, &body), None);
    }

    #[test]
    fn cwa_multiple_values_first_bad_wins() {
        // Set bits 4 (BitGravity=ok) and 6 (BackingStore=bad). Walking
        // in bit order, the bad value at bit 6 should be reported.
        let body = cw_body((1 << 4) | (1 << 6), &[5, 7]);
        assert_eq!(invalid_value(2, 0, &body), Some(7));
    }

    #[test]
    fn create_window_value_list_validated() {
        // CreateWindow: mask at body[24..28], values at body[28].
        let mut body = vec![0u8; 28 + 4];
        let mask = 1u32 << 4; // BitGravity
        body[24..28].copy_from_slice(&mask.to_le_bytes());
        body[28..32].copy_from_slice(&15u32.to_le_bytes());
        assert_eq!(invalid_value(1, 0, &body), Some(15));
    }

    fn gc_body(mask: u32, values: &[u32]) -> Vec<u8> {
        let mut b = vec![0u8; 8 + values.len() * 4];
        b[4..8].copy_from_slice(&mask.to_le_bytes());
        for (i, v) in values.iter().enumerate() {
            b[8 + i * 4..12 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn gc_function_out_of_range() {
        // GCFunction = bit 0. Set(=15) is max.
        let body = gc_body(1 << 0, &[16]);
        assert_eq!(invalid_value(56, 0, &body), Some(16));
        let body = gc_body(1 << 0, &[15]);
        assert_eq!(invalid_value(56, 0, &body), None);
    }

    #[test]
    fn gc_line_cap_join_styles() {
        // line_style (bit 5) ∈ {0,1,2}
        let body = gc_body(1 << 5, &[3]);
        assert_eq!(invalid_value(56, 0, &body), Some(3));
        // cap_style (bit 6) ∈ {0,1,2,3}
        let body = gc_body(1 << 6, &[4]);
        assert_eq!(invalid_value(56, 0, &body), Some(4));
        // join_style (bit 7) ∈ {0,1,2}
        let body = gc_body(1 << 7, &[3]);
        assert_eq!(invalid_value(56, 0, &body), Some(3));
        // valid combos pass.
        let body = gc_body((1 << 5) | (1 << 6) | (1 << 7), &[2, 3, 2]);
        assert_eq!(invalid_value(56, 0, &body), None);
    }

    #[test]
    fn gc_subwindow_mode_and_arc_mode_bool() {
        let body = gc_body(1 << 15, &[2]);
        assert_eq!(invalid_value(56, 0, &body), Some(2));
        let body = gc_body(1 << 22, &[2]);
        assert_eq!(invalid_value(56, 0, &body), Some(2));
    }

    #[test]
    fn create_gc_value_list_validated() {
        // CreateGC: mask at body[8..12], values at body[12].
        let mut body = vec![0u8; 12 + 4];
        let mask = 1u32 << 5; // LineStyle
        body[8..12].copy_from_slice(&mask.to_le_bytes());
        body[12..16].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(invalid_value(55, 0, &body), Some(5));
    }

    fn kb_body(mask: u32, values: &[u32]) -> Vec<u8> {
        let mut b = vec![0u8; 4 + values.len() * 4];
        b[0..4].copy_from_slice(&mask.to_le_bytes());
        for (i, v) in values.iter().enumerate() {
            b[4 + i * 4..8 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn kb_percent_range() {
        // key_click_percent (bit 0) ∈ {-1, 0..=100}
        let body = kb_body(1 << 0, &[101u32]);
        assert_eq!(invalid_value(102, 0, &body), Some(101));
        // -1 (sign-extended) acceptable; encode as u32::from((-1i8) as u8).
        let body = kb_body(1 << 0, &[u32::from((-1i8) as u8)]);
        assert_eq!(invalid_value(102, 0, &body), None);
        // 100 (max) acceptable.
        let body = kb_body(1 << 0, &[100]);
        assert_eq!(invalid_value(102, 0, &body), None);
        // -2 invalid.
        let body = kb_body(1 << 0, &[u32::from((-2i8) as u8)]);
        assert!(invalid_value(102, 0, &body).is_some());
    }

    #[test]
    fn kb_auto_repeat_mode() {
        // bit 7: ∈ {0,1,2}
        let body = kb_body(1 << 7, &[3]);
        assert_eq!(invalid_value(102, 0, &body), Some(3));
        let body = kb_body(1 << 7, &[2]);
        assert_eq!(invalid_value(102, 0, &body), None);
    }

    #[test]
    fn pointer_control_denominator_zero() {
        // do_accel=1, denom=0 → BadValue 0.
        let mut body = vec![0u8; 8];
        body[2..4].copy_from_slice(&0i16.to_le_bytes()); // denom
        body[6] = 1; // do_accel
        assert_eq!(invalid_value(105, 0, &body), Some(0));
    }

    #[test]
    fn pointer_control_bool_validation() {
        let mut body = vec![0u8; 8];
        body[6] = 2; // do_accel out of range
        assert_eq!(invalid_value(105, 0, &body), Some(2));
        let mut body = vec![0u8; 8];
        body[7] = 5; // do_threshold out of range
        assert_eq!(invalid_value(105, 0, &body), Some(5));
    }

    #[test]
    fn pointer_control_threshold_minus_two_bad() {
        let mut body = vec![0u8; 8];
        body[4..6].copy_from_slice(&(-2i16).to_le_bytes());
        body[7] = 1; // do_threshold
        // -2 cast to u32 via `as u32` sign-extends → 0xFFFFFFFE.
        assert_eq!(invalid_value(105, 0, &body), Some((-2i32) as u32));
    }

    #[test]
    fn pointer_control_default_bytes_pass() {
        // All zeros: do_accel=0, do_threshold=0 → no checks.
        let body = vec![0u8; 8];
        assert_eq!(invalid_value(105, 0, &body), None);
    }

    // ── Group C: header.data scalars + ConfigureWindow stack_mode ──

    #[test]
    fn change_save_set_mode() {
        // ChangeSaveSet (6): header.data ∈ {0,1}.
        assert_eq!(invalid_value(6, 2, &[]), Some(2));
        assert_eq!(invalid_value(6, 0, &[]), None);
        assert_eq!(invalid_value(6, 1, &[]), None);
    }

    #[test]
    fn configure_window_stack_mode() {
        // ConfigureWindow (12): u16 mask at body[4..6], values at body[8].
        // bit 6 = stack_mode ∈ {0..=4}.
        let mut body = vec![0u8; 8 + 4];
        body[4..6].copy_from_slice(&(1u16 << 6).to_le_bytes());
        body[8..12].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(invalid_value(12, 0, &body), Some(5));
        body[8..12].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(invalid_value(12, 0, &body), None);
    }

    #[test]
    fn circulate_window_direction() {
        // CirculateWindow (13): header.data ∈ {0,1}.
        assert_eq!(invalid_value(13, 2, &[]), Some(2));
        assert_eq!(invalid_value(13, 1, &[]), None);
    }

    #[test]
    fn send_event_propagate() {
        // SendEvent (25): header.data is BOOL.
        assert_eq!(invalid_value(25, 2, &[0u8; 40]), Some(2));
        assert_eq!(invalid_value(25, 1, &[0u8; 40]), None);
    }

    #[test]
    fn allow_events_mode() {
        // AllowEvents (35): header.data ∈ {0..=7}.
        assert_eq!(invalid_value(35, 8, &[]), Some(8));
        assert_eq!(invalid_value(35, 7, &[]), None);
    }

    #[test]
    fn create_colormap_alloc() {
        // CreateColormap (78): header.data ∈ {0,1}.
        assert_eq!(invalid_value(78, 2, &[]), Some(2));
        assert_eq!(invalid_value(78, 0, &[]), None);
    }

    #[test]
    fn bell_percent_range() {
        // Bell (104): header.data is INT8 ∈ [-100, 100].
        assert_eq!(invalid_value(104, 50, &[]), None);
        assert_eq!(invalid_value(104, 100, &[]), None);
        assert_eq!(invalid_value(104, 101, &[]), Some(101));
        // -100 (signed) encoded as 156 unsigned.
        assert_eq!(invalid_value(104, (-100i8) as u8, &[]), None);
        // -101 invalid.
        assert_eq!(
            invalid_value(104, (-101i8) as u8, &[]),
            Some(u32::from((-101i8) as u8))
        );
    }

    #[test]
    fn set_screen_saver_blanking_and_exposures() {
        // SetScreenSaver (107): body[4]=prefer_blanking, body[5]=allow_exposures.
        let mut body = vec![0u8; 8];
        body[4] = 3;
        assert_eq!(invalid_value(107, 0, &body), Some(3));
        let mut body = vec![0u8; 8];
        body[5] = 5;
        assert_eq!(invalid_value(107, 0, &body), Some(5));
        let mut body = vec![0u8; 8];
        body[4] = 2;
        body[5] = 2;
        assert_eq!(invalid_value(107, 0, &body), None);
    }

    #[test]
    fn force_screen_saver_mode() {
        // ForceScreenSaver (115): header.data ∈ {0,1}.
        assert_eq!(invalid_value(115, 2, &[]), Some(2));
        assert_eq!(invalid_value(115, 0, &[]), None);
    }
}
