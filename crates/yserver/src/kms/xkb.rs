use xkbcommon::xkb::{Keycode, Keymap};

/// Per-key data extracted from `xkbcommon::Keymap`, ready to lay
/// out into the `KeySymMap` wire structure xkb.xml defines.
struct KeyData {
    /// Width per group — derived from `num_levels_for_key(kc, 0)`,
    /// capped at the wire `width` field's u8 range. With
    /// `num_groups == 0` this is 0 and `nSyms == 0`, yielding the
    /// 8-byte fixed `KeySymMap` header.
    width: u8,
    /// 1 when at least one keysym is defined for layout 0; xkb.xml
    /// stores it in `groupInfo`'s low nibble. We only ever publish
    /// layout 0 to keep the reply shape predictable.
    num_groups: u8,
    /// Index into the `KeyTypes` table — 0 (`ONE_LEVEL`) when
    /// `width <= 1`, 1 (`TWO_LEVEL`) otherwise.
    type_index: u8,
    /// `nSyms = width * num_groups` keysyms, level-0 first then
    /// level-1, etc. Empty levels are filled with `NoSymbol` (0).
    syms: Vec<u32>,
}

/// Modifier-map entry for the `ModifierMap` section: keycode plus
/// the bitset of standard X11 modifier bits the key triggers
/// (Shift=0x01, Lock=0x02, Control=0x04, Mod1..Mod5=0x08..0x80).
struct ModMapEntry {
    keycode: u8,
    mods: u8,
}

/// Map a level-0 keysym to its standard X11 modifier-map bit.
/// Returns 0 when the keysym is not a recognised modifier so the
/// keycode is excluded from the modifier-map list. Mirrors the
/// default xkb modifier conventions used by `evdev_aliases(qwerty)`
/// and friends — enough for xkbcommon to drive Shift/Ctrl/Alt/Lock
/// in any client that re-derives modifier state from the keymap
/// (xcb-rs / xkbcommon-rs in particular).
fn modifier_bit_for_keysym(sym: u32) -> u8 {
    match sym {
        0xFFE1 | 0xFFE2 => 0x01,                            // Shift_L, Shift_R
        0xFFE5 | 0xFFE6 => 0x02,                            // Caps_Lock, Shift_Lock
        0xFFE3 | 0xFFE4 => 0x04,                            // Control_L, Control_R
        0xFFE7 | 0xFFE8 | 0xFFE9 | 0xFFEA | 0xFE03 => 0x08, // Meta_*, Alt_*, ISO_Level3_Shift→Mod1 (xkb default)
        0xFF7F => 0x10,                                     // Num_Lock
        0xFFEB | 0xFFEC => 0x40,                            // Super_L, Super_R
        _ => 0,
    }
}

/// X11 modifier-map bitmask that picks `Shift` — used by the
/// `TWO_LEVEL` `KeyType`'s map entry to say "Shift selects level 1".
const SHIFT_MASK: u8 = 0x01;

/// XKB UseExtension reply (minor=0). Fixed 32 bytes.
/// Reports success and server protocol version 1.0.
pub(super) fn reply_use_extension() -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // success
    // [2..4] sequence: rewritten by caller
    // [4..8] extra length in 4-byte units = 0
    r[8] = 1; // server-major
    r[9] = 0; // server-minor
    r
}

/// XKB GetControls reply (minor=6). Fixed 92 bytes
/// (`sz_xkbGetControlsReply`). Field offsets follow `xkbGetControlsReply`
/// in `/usr/include/X11/extensions/XKBproto.h`:
///   [0] type, [1] deviceID, [2..4] seq, [4..8] length,
///   [8] mkDfltBtn, [9] numGroups, [10] groupsWrap, [11] internalMods,
///   [12] ignoreLockMods, [13] internalRealMods,
///   [14] ignoreLockRealMods, [15] pad1,
///   [16..18] internalVMods, [18..20] ignoreLockVMods,
///   [20..22] repeatDelay, [22..24] repeatInterval,
///   [24..26] slowKeysDelay, [26..28] debounceDelay,
///   [28..30] mkDelay, [30..32] mkInterval,
///   [32..34] mkTimeToMax, [34..36] mkMaxSpeed,
///   [36..38] mkCurve, [38..40] axOptions,
///   [40..42] axTimeout, [42..44] axtOptsMask,
///   [44..46] axtOptsValues, [46..48] pad2,
///   [48..52] axtCtrlsMask, [52..56] axtCtrlsValues,
///   [56..60] enabledCtrls, [60..92] perKeyRepeat[32].
///
/// xkbcommon's `get_controls` checks
/// `reply->numGroups > 0 && reply->numGroups <= 4` (verified
/// against `objdump` of libxkbcommon-x11.so.0.13.1) — so the
/// previous reply with `numGroups=0` and `repeatDelay/interval/
/// enabledCtrls` written at the wrong offsets failed the keymap
/// build for any xkbcommon-using client.
pub(super) fn reply_get_controls(keymap: &Keymap) -> Vec<u8> {
    let mut r = vec![0u8; 92];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [4..8] extra length = (92-32)/4 = 15
    r[4..8].copy_from_slice(&15u32.to_le_bytes());
    // numGroups: xkbcommon requires 1..=4; clamp from keymap.num_layouts()
    // (may be 0 for an empty keymap).
    let num_groups: u8 = u8::try_from(keymap.num_layouts()).unwrap_or(1).clamp(1, 4);
    r[9] = num_groups;
    // Repeat delay = 500ms, interval = 33ms (≈30 Hz)
    r[20..22].copy_from_slice(&500_u16.to_le_bytes());
    r[22..24].copy_from_slice(&33_u16.to_le_bytes());
    // EnabledControls: RepeatKeys (bit 0) | PerKeyRepeat — pick
    // RepeatKeys so xkbcommon enables auto-repeat by default.
    r[56..60].copy_from_slice(&0x0000_0001_u32.to_le_bytes());
    r
}

/// XKB GetMap reply (minor=8). Builds a wire-correct reply from
/// `xkbcommon::Keymap` — real key types, per-key syms, and a
/// modifier map — so xkbcommon-x11 clients (wezterm via xkbcommon-rs,
/// every modern toolkit using libxkbcommon) get a usable keymap
/// rather than `NULL`.
///
/// What's published:
/// * Two `KeyTypes`: `ONE_LEVEL` (no modifiers, single level) and
///   `TWO_LEVEL` (Shift→level 1). Wider keys still slot into
///   `TWO_LEVEL`; xkbcommon then derives higher levels from the
///   `kt_index` + level data without us having to encode
///   `ALPHABETIC` / `KEYPAD`.
/// * Per-key `KeySymMap` entries with `num_groups=1` (we publish
///   only layout 0 to keep replies bounded), `width =
///   num_levels_for_key`, and the level-0..width-1 keysyms pulled
///   straight from xkbcommon. Keys with no syms get the 8-byte
///   header only (width=0, num_groups=0).
/// * `ModifierMap` populated from a keysym→mod-bit lookup
///   (`modifier_bit_for_keysym`) so Shift/Ctrl/Alt/Lock translate
///   correctly on the client.
/// * `KeyActions` advertises the full `[min, max]` range with
///   per-key counts of zero — xkbcommon's `get_actions` requires
///   exact range coverage, but accepts no actions per key.
/// * Other sections (`KeyBehaviors`, `VirtualMods`,
///   `ExplicitComponents`, `VirtualModMap`) stay empty —
///   xkbcommon's per-section validators tolerate that here.
///
/// Reply layout follows xkb.xml's `GetMap` switch order, which is
/// XML order, *not* bit-position order:
///   KeyTypes → KeySyms → KeyActions → KeyBehaviors → VirtualMods
///   → ExplicitComponents → ModifierMap → VirtualModMap.
pub(super) fn reply_get_map(keymap: &Keymap) -> Vec<u8> {
    // X11 keycodes are CARD8 — xkbcommon's keymap can carry a wider
    // range (min=9, max=709 for the default us layout). Clamp into
    // [8, 255] so wire counts fit in u8 and ensure min <= max.
    let raw_min = keymap.min_keycode().raw();
    let raw_max = keymap.max_keycode().raw();
    let min_kc: u8 = u8::try_from(raw_min).unwrap_or(8).max(8);
    let max_kc: u8 = u8::try_from(raw_max.min(255)).unwrap_or(255).max(min_kc);
    let n_keys: u8 = max_kc - min_kc + 1;

    // Walk xkbcommon's layout 0 for each keycode in the published
    // range and snapshot the data we'll serialise. Layout 0 covers
    // the user's primary group; secondary groups (AltGr, etc.)
    // would require multi-group KeySymMap entries — followup.
    let mut keys: Vec<KeyData> = Vec::with_capacity(usize::from(n_keys));
    let mut modmap: Vec<ModMapEntry> = Vec::new();
    let mut total_syms: u32 = 0;
    for kc_raw in min_kc..=max_kc {
        let kc = Keycode::new(u32::from(kc_raw));
        let layouts = keymap.num_layouts_for_key(kc);
        let levels = if layouts == 0 {
            0
        } else {
            keymap.num_levels_for_key(kc, 0)
        };
        // u8::MAX is more than enough — XKB caps each key at 8
        // levels in practice; clamp defensively.
        let width: u8 = u8::try_from(levels).unwrap_or(u8::MAX);
        let num_groups: u8 = if width > 0 && layouts > 0 { 1 } else { 0 };
        let mut syms: Vec<u32> = Vec::with_capacity(usize::from(width) * usize::from(num_groups));
        if num_groups == 1 {
            for level in 0..u32::from(width) {
                let level_syms = keymap.key_get_syms_by_level(kc, 0, level);
                let sym = level_syms.first().map(|s| s.raw()).unwrap_or(0);
                syms.push(sym);
            }
        }
        let nsyms_this = u32::from(width) * u32::from(num_groups);
        total_syms = total_syms.saturating_add(nsyms_this);
        let type_index: u8 = if width >= 2 { 1 } else { 0 };
        // Capture modmap entry from level-0 keysym (matches the
        // physical-modifier convention every client expects).
        if num_groups == 1 && !syms.is_empty() {
            let bit = modifier_bit_for_keysym(syms[0]);
            if bit != 0 {
                modmap.push(ModMapEntry {
                    keycode: kc_raw,
                    mods: bit,
                });
            }
        }
        keys.push(KeyData {
            width,
            num_groups,
            type_index,
            syms,
        });
    }

    let total_modmap = u8::try_from(modmap.len()).unwrap_or(u8::MAX);

    // -- Section sizes --------------------------------------------
    // KeyTypes: 2 types. ONE_LEVEL = 8 B (header, no map entries).
    // TWO_LEVEL = 8 B header + 1 KTSetMapEntry (8 B) = 16 B.
    let key_types_bytes: usize = 8 + 16;

    // KeySyms: 8-byte header + nSyms * 4 per key.
    let key_syms_bytes: usize = keys
        .iter()
        .map(|k| 8 + 4 * usize::from(k.width) * usize::from(k.num_groups))
        .sum();

    // KeyActions: nKeyActions CARD8s + pad to 4-byte align + 0
    // Action structs (per-key count is zero everywhere).
    let nk = usize::from(n_keys);
    let actions_count_pad = (4 - nk % 4) % 4;
    let key_actions_bytes: usize = nk + actions_count_pad;

    // ModifierMap: 2 bytes per entry + pad to 4-byte align.
    let modmap_raw_bytes: usize = usize::from(total_modmap) * 2;
    let modmap_pad = (4 - modmap_raw_bytes % 4) % 4;
    let modmap_bytes: usize = modmap_raw_bytes + modmap_pad;

    // Other sections empty: VirtualMods (popcount(virtualMods)=0
    // CARD8 + pad), ExplicitComponents (0 + pad), VirtualModMap
    // (0 entries, no pad). All contribute zero bytes here.

    let extra = key_types_bytes + key_syms_bytes + key_actions_bytes + modmap_bytes;
    let total = 40 + extra;
    let length_words = u32::try_from((total - 32) / 4).unwrap_or(u32::MAX);

    // -- Fixed 40-byte reply header ------------------------------
    let mut r = vec![0u8; total];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    r[4..8].copy_from_slice(&length_words.to_le_bytes());
    r[10] = min_kc;
    r[11] = max_kc;
    // present: KeyTypes|KeySyms|ModifierMap|ExplicitComponents
    //         |KeyActions|VirtualMods|VirtualModMap = 0xDF.
    r[12..14].copy_from_slice(&0x00DF_u16.to_le_bytes());
    // [14] firstType=0
    r[15] = 2; // nTypes
    r[16] = 2; // totalTypes
    r[17] = min_kc; // firstKeySym
    r[18..20].copy_from_slice(&u16::try_from(total_syms).unwrap_or(u16::MAX).to_le_bytes());
    r[20] = n_keys; // nKeySyms — covers full range
    r[21] = min_kc; // firstKeyAction
    // [22..24] totalActions = 0
    r[24] = n_keys; // nKeyActions — covers full range
    r[25] = min_kc; // firstKeyBehavior (bit 5 unset → empty)
    // [26..28] nKeyBehaviors=0, totalKeyBehaviors=0
    r[28] = min_kc; // firstKeyExplicit
    // [29..31] nKeyExplicit=0, totalKeyExplicit=0
    r[31] = min_kc; // firstModMapKey
    r[32] = n_keys; // nModMapKeys (range; totalModMapKeys is the
    //                              actual list length, set next)
    r[33] = total_modmap;
    r[34] = min_kc; // firstVModMapKey
    // [35..37] nVModMapKeys=0, totalVModMapKeys=0, [37] pad
    // [38..40] virtualMods = 0

    // -- Section bodies ------------------------------------------
    let mut off = 40;

    // KeyTypes: ONE_LEVEL then TWO_LEVEL.
    // ONE_LEVEL: mods_mask=0, mods_mods=0, mods_vmods=0,
    //            numLevels=1, nMapEntries=0, hasPreserve=0, pad=0.
    r[off + 4] = 1; // numLevels
    off += 8;
    // TWO_LEVEL: mods_mask=Shift, mods_mods=Shift, mods_vmods=0,
    //            numLevels=2, nMapEntries=1, hasPreserve=0, pad=0,
    //            then KTSetMapEntry { active=1, mask=Shift,
    //            level=1, real_mods=Shift, vmods=0, pad=0 }.
    r[off] = SHIFT_MASK; // mods_mask
    r[off + 1] = SHIFT_MASK; // mods_mods (real_mods)
    // [off+2..off+4] mods_vmods = 0
    r[off + 4] = 2; // numLevels
    r[off + 5] = 1; // nMapEntries
    // [off+6] hasPreserve=0, [off+7] pad
    let map_entry = off + 8;
    r[map_entry] = 1; // active = true
    r[map_entry + 1] = SHIFT_MASK; // mask
    r[map_entry + 2] = 1; // level
    r[map_entry + 3] = SHIFT_MASK; // realMods
    // [map_entry+4..+6] virtualMods=0, [map_entry+6..+8] pad
    off += 16;

    // KeySyms: per-key KeySymMap.
    for k in &keys {
        // [off..off+4] kt_index[4] — group 0 uses k.type_index;
        // unused groups stay 0 (ONE_LEVEL), well within nTypes.
        r[off] = k.type_index;
        // [off+4] groupInfo: low 4 bits = num_groups
        r[off + 4] = k.num_groups & 0x0F;
        r[off + 5] = k.width;
        let nsyms = u16::try_from(k.syms.len()).unwrap_or(u16::MAX);
        r[off + 6..off + 8].copy_from_slice(&nsyms.to_le_bytes());
        let mut sym_off = off + 8;
        for sym in &k.syms {
            r[sym_off..sym_off + 4].copy_from_slice(&sym.to_le_bytes());
            sym_off += 4;
        }
        off = sym_off;
    }

    // KeyActions: n_keys per-key counts (all zero by default-init)
    // + pad to 4-byte boundary + 0 Action structs.
    off += nk + actions_count_pad;

    // VirtualMods empty (virtualMods=0 ⇒ popcount=0 ⇒ 0 bytes,
    // no pad needed when already aligned).
    // ExplicitComponents empty (0 entries, 0 pad).

    // ModifierMap: 2 bytes per entry, then pad.
    for entry in &modmap {
        r[off] = entry.keycode;
        r[off + 1] = entry.mods;
        off += 2;
    }
    off += modmap_pad;

    // VirtualModMap empty.

    debug_assert_eq!(off, total, "GetMap reply body length matches total");
    r
}

/// XKB GetNames reply (minor=17). The `which` mask advertises
/// `KeyTypeNames|KTLevelNames|KeyNames|VirtualModNames`
/// (bits 6,7,9,11 = 0x40|0x80|0x200|0x800 = `0xAC0`), the bitset
/// xkbcommon's `get_names_required` validates. (Verified against
/// `objdump` of `libxkbcommon-x11.so.0.13.1`'s `get_names`: the
/// AND-mask in the `FAIL_UNLESS` is `0xac0`.)
///
/// The reply must agree with `reply_get_map` on `[min_kc, max_kc]`
/// and the type count: xkbcommon's `get_key_names` asserts
/// `firstKey == min_key_code`, `firstKey + nKeys - 1 == max_key_code`,
/// and `reply->{min,max}KeyCode == keymap->{min,max}_key_code`;
/// `get_type_names` asserts `reply->nTypes == keymap->num_types`.
///
/// Names themselves go on the wire as the empty atom (id 0) — the
/// keymap stays usable; xkbcommon records anonymous names. The
/// `nLevelsPerType` list mirrors what `reply_get_map` published:
/// `[1, 2]` for `ONE_LEVEL` and `TWO_LEVEL`. `sumof(nLevelsPerType)`
/// = 3 atoms follow.
pub(super) fn reply_get_names(keymap: &Keymap) -> Vec<u8> {
    let raw_min = keymap.min_keycode().raw();
    let raw_max = keymap.max_keycode().raw();
    let min_kc: u8 = u8::try_from(raw_min).unwrap_or(8).max(8);
    let max_kc: u8 = u8::try_from(raw_max.min(255)).unwrap_or(255).max(min_kc);
    let n_keys: u8 = max_kc - min_kc + 1;

    // -- which mask -----------------------------------------------
    // KeyTypeNames|KTLevelNames|VirtualModNames|KeyNames is what
    // xkbcommon's `get_names_required` enforces (0xAC0). But
    // `get_names()` in libxkbcommon also unconditionally reads
    // `list.keycodesName`, `list.symbolsName`, `list.typesName`,
    // and `list.compatName` from a *stack-uninitialized*
    // `xcb_xkb_get_names_value_list_t list;` (keymap.c:1139-1146).
    // xcb-generated `value_list_unpack` only writes fields whose
    // bit is set in `which`; an absent bit leaves stack garbage
    // there, which xkbcommon then dispatches as GetAtomName
    // requests. We saw the resulting bogus atoms (0xAE4BAA70,
    // 22057, …) in the wire log. Set Keycodes|Symbols|Types|Compat
    // (0x35) on top of 0xAC0 so xcb actually writes zeros into
    // those fields.
    const REQUIRED: u32 = 0x0000_0AC0; // KeyTypeNames|KTLevelNames|VirtualModNames|KeyNames
    const UNCONDITIONALLY_READ: u32 = 0x0000_0035; // Keycodes|Symbols|Types|Compat
    let which: u32 = REQUIRED | UNCONDITIONALLY_READ;

    // -- Section sizes (in xkb.xml switch order, which is bit
    // order — Keycodes(0)→Geometry(1)→Symbols(2)→PhysSymbols(3)→
    // Types(4)→Compat(5)→KeyTypeNames(6)→KTLevelNames(7)→
    // IndicatorNames(8)→VirtualModNames(9)→GroupNames(10)→
    // KeyNames(11)→KeyAliases(12)→RGNames(13)). For us:
    //
    // * keycodesName   ATOM = 4 bytes  (Keycodes)
    // * symbolsName    ATOM = 4 bytes  (Symbols)
    // * typesName      ATOM = 4 bytes  (Types)
    // * compatName     ATOM = 4 bytes  (Compat)
    // * typeNames[2]   ATOM = 8 bytes  (KeyTypeNames)
    // * nLevelsPerType[2] + pad + ktLevelNames[3] = 16 bytes
    //                                  (KTLevelNames)
    // * virtualModNames: 0 bytes       (VirtualModNames, popcount=0)
    // * keyNames[nKeys] KeyName(4) = nKeys * 4 bytes (KeyNames)
    let unconditional_names_bytes = 4 * 4;
    let key_type_names_bytes = 2 * 4;
    let kt_levels_count = 2;
    let kt_levels_count_pad = (4 - kt_levels_count % 4) % 4;
    let kt_level_names_bytes = kt_levels_count + kt_levels_count_pad + 3 * 4;
    let nk = usize::from(n_keys);
    let key_names_bytes = nk * 4;
    let extra =
        unconditional_names_bytes + key_type_names_bytes + kt_level_names_bytes + key_names_bytes;
    let total = 32 + extra;

    let mut r = vec![0u8; total];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID
    let length_words = u32::try_from(extra / 4).unwrap_or(u32::MAX);
    r[4..8].copy_from_slice(&length_words.to_le_bytes());
    r[8..12].copy_from_slice(&which.to_le_bytes());
    r[12] = min_kc;
    r[13] = max_kc;
    r[14] = 2; // nTypes — must match GetMap's nTypes
    // [15] groupNames = 0
    // [16..18] virtualMods = 0
    r[18] = min_kc; // firstKey
    r[19] = n_keys; // nKeys — full range
    // [20..24] indicators = 0
    // [24] nRadioGroups = 0
    // [25] nKeyAliases = 0
    // [26..28] nKTLevels (sum of nLevelsPerType) — Xlib uses nTypes
    // here per xkb.xml's note; xkbcommon honours nTypes for the
    // ATOM list length, so the value is informational.
    r[26..28].copy_from_slice(&3_u16.to_le_bytes());
    // [28..32] pad

    // -- Body ----------------------------------------------------
    // All atom values go out as 0 (None / "no name") — xkbcommon's
    // `get_escaped_atom_name` returns early on `atom == 0` and
    // records anonymous names, so this keeps the keymap usable.
    let mut off = 32;
    // keycodesName + symbolsName + typesName + compatName: 4 zero
    // ATOMs (already zero from the vec! init).
    off += unconditional_names_bytes;
    // typeNames: 2 ATOMs (zeroed).
    off += key_type_names_bytes;
    // KTLevelNames: nLevelsPerType[] = [1, 2], pad to 4-byte
    // boundary (2 bytes), then 3 ATOMs (zeroed).
    r[off] = 1; // ONE_LEVEL has 1 level
    r[off + 1] = 2; // TWO_LEVEL has 2 levels
    off += kt_levels_count + kt_levels_count_pad + 3 * 4;
    // VirtualModNames is empty (popcount(virtualMods=0)=0).
    // KeyNames: n_keys × 4 zero bytes (anonymous names).
    off += key_names_bytes;
    debug_assert_eq!(off, total, "GetNames reply body length matches total");
    r
}

/// XKB GetCompatMap reply (minor=10). Per xkbproto, fixed 32 bytes
/// — `sz_xkbGetCompatMapReply`. Empty compat map (no SI entries
/// follow).
pub(super) fn reply_get_compat_map() -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [4..8] extra length = 0
    // [8] groupsRtrn = 0
    // [9] pad1
    // [10..12] firstSIRtrn = 0
    // [12..14] nSIRtrn = 0
    // [14..16] nTotalSI = 0
    // [16..32] pad2[16] = 0
    r
}

/// XKB GetDeviceInfo reply (minor=24). The wire-correct *empty*
/// reply is 36 bytes, not 32 — `sizeof(xcb_xkb_get_device_info_reply_t)`
/// (verified via gcc on `xcb/xkb.h`) is 36 because the C struct
/// places `nameLen: CARD16` at offset 32 with 2 bytes of trailing
/// pad. xcb-based clients (xkbcommon-x11 inside `vkgears`,
/// `wezterm`, …) cast the libxcb reply pointer straight to that
/// struct and access `reply->nameLen` plus
/// `xcb_xkb_get_device_info_name(reply) = (char*)(reply + 1)`
/// — both **read past a 32-byte allocation**, producing garbage
/// atoms that the client then fans out as GetAtomName requests
/// (we saw 0xAE4BAA70, 0xAE4B5808, 22057 in the log). xkbcommon-x11
/// then errors out and returns NULL, so `vkgears` segfaults on the
/// resulting `xkb_keymap_ref(NULL)`.
///
/// We publish an empty keyboard: no LED feedbacks, no buttons, no
/// name string, no actions. That's still a 36-byte body — fixed
/// header + `nameLen=0` (2B) + pad-to-4 (2B) — with `length = 1`.
pub(super) fn reply_get_device_info() -> Vec<u8> {
    let mut r = vec![0u8; 36];
    r[0] = 1; // reply
    r[1] = 1; // deviceID = 1
    // [4..8] extra length = (36 - 32) / 4 = 1
    r[4..8].copy_from_slice(&1u32.to_le_bytes());
    // [8..10] present, [10..12] supported, [12..14] unsupported = 0
    // [14..16] nDeviceLedFBs = 0
    // [16] firstBtnWanted, [17] nBtnsWanted
    // [18] firstBtnRtrn, [19] nBtnsRtrn
    // [20..22] totalBtns = 0
    // [22] hasOwnState
    // [23] (padding/alignment)
    // [24..26] dfltKbdFB, [26..28] dfltLedFB
    // [28..32] devType atom = 0
    // [32..34] nameLen = 0
    // [34..36] pad align(4)
    r
}

/// Minimal all-zero 32-byte reply for XKB minors that clients tolerate silently.
/// Only use for minors with no required reply content (e.g. SetControls has none).
pub(super) fn reply_minimal(minor: u8) -> Vec<u8> {
    log::debug!("xkb: unimplemented minor {minor}, returning minimal reply");
    let mut r = vec![0u8; 32];
    r[0] = 1;
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_keymap() -> xkbcommon::xkb::Keymap {
        let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        xkbcommon::xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .or_else(|| {
            xkbcommon::xkb::Keymap::new_from_names(
                &ctx,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .expect("test xkb keymap")
    }

    #[test]
    fn use_extension_reply_length() {
        assert_eq!(reply_use_extension().len(), 32);
    }

    #[test]
    fn use_extension_success_flag() {
        let r = reply_use_extension();
        assert_eq!(r[1], 1, "success must be 1");
    }

    #[test]
    fn get_controls_reply_length() {
        let km = test_keymap();
        assert_eq!(reply_get_controls(&km).len(), 92);
    }

    #[test]
    fn get_controls_field_offsets_match_xkbproto() {
        // Field offsets ground-truthed against
        // /usr/include/X11/extensions/XKBproto.h's
        // xkbGetControlsReply struct. xkbcommon's get_controls
        // asserts `numGroups > 0 && numGroups <= 4` (objdump on
        // libxkbcommon-x11.so.0.13.1 shows the test mask 0x05 →
        // unsigned-greater-than-4 reject after the >0 reject).
        let km = test_keymap();
        let r = reply_get_controls(&km);
        assert_eq!(r[0], 1, "reply type");
        assert_eq!(r[1], 1, "deviceID");
        assert_eq!(
            u32::from_le_bytes([r[4], r[5], r[6], r[7]]),
            15,
            "length = (92 - 32) / 4 = 15"
        );
        assert!(r[9] >= 1 && r[9] <= 4, "numGroups in 1..=4");
        // repeatDelay at offset 20..22, repeatInterval at 22..24.
        assert_eq!(u16::from_le_bytes([r[20], r[21]]), 500);
        assert_eq!(u16::from_le_bytes([r[22], r[23]]), 33);
        // enabledCtrls at offset 56..60, with at least RepeatKeys.
        let enabled = u32::from_le_bytes([r[56], r[57], r[58], r[59]]);
        assert_eq!(enabled & 0x01, 0x01, "RepeatKeys bit set");
    }

    #[test]
    fn get_map_reply_invariants() {
        let km = test_keymap();
        let r = reply_get_map(&km);
        // 40-byte fixed reply + body, 4-byte aligned, length matches.
        assert!(r.len() >= 40);
        assert!(r.len().is_multiple_of(4));
        let length_words = u32::from_le_bytes([r[4], r[5], r[6], r[7]]) as usize;
        assert_eq!(length_words * 4 + 32, r.len());

        let min_kc = r[10];
        let max_kc = r[11];
        assert!(min_kc <= max_kc);
        assert!(min_kc >= 8);
        let n_keys = max_kc - min_kc + 1;

        // present advertises every required map part — xkbcommon's
        // get_map_required_components is a subset of 0xDF.
        let present = u16::from_le_bytes([r[12], r[13]]);
        assert_eq!(present & 0xDF, 0xDF);

        // KeyTypes: 2 types (ONE_LEVEL, TWO_LEVEL).
        assert_eq!(r[15], 2, "nTypes = 2");
        assert_eq!(r[16], 2, "totalTypes = 2");

        // KeySyms covers full range, firstKeySym >= minKeyCode and
        // firstKeySym + nKeySyms <= maxKeyCode + 1.
        assert_eq!(r[17], min_kc, "firstKeySym = min_kc");
        assert_eq!(r[20], n_keys, "nKeySyms = full range");

        // KeyActions covers full range, firstKeyAction == min_key_code
        // and firstKeyAction + nKeyActions == max_key_code + 1.
        assert_eq!(r[21], min_kc, "firstKeyAction = min_kc");
        assert_eq!(r[24], n_keys, "nKeyActions = full range");

        // ModifierMap covers full range; totalModMapKeys must be ≤ nModMapKeys.
        assert_eq!(r[31], min_kc, "firstModMapKey = min_kc");
        assert_eq!(r[32], n_keys, "nModMapKeys = full range");
        assert!(r[33] <= r[32], "totalModMapKeys ≤ nModMapKeys");
    }

    #[test]
    fn get_map_publishes_real_keysyms_for_letter_keys() {
        // The level-0 keysym for a letter key under the default us
        // layout should be the lowercase ASCII codepoint. Walk the
        // KeySyms section looking for the 'a' keysym (0x61) — at
        // least one key should publish it. Pre-fix the section was
        // empty, so this is the regression guard.
        let km = test_keymap();
        let r = reply_get_map(&km);
        let n_keys = r[11] - r[10] + 1;
        // Skip past KeyTypes (8 + 16 = 24 B) to the start of KeySyms.
        let mut off = 40 + 24;
        let mut found_a = false;
        for _ in 0..n_keys {
            // KeySymMap: kt_index[4], groupInfo, width, nSyms, syms[nSyms]
            let width = r[off + 5] as usize;
            let num_groups = (r[off + 4] & 0x0F) as usize;
            let nsyms = u16::from_le_bytes([r[off + 6], r[off + 7]]) as usize;
            assert_eq!(nsyms, width * num_groups, "nSyms = width * num_groups");
            for s in 0..nsyms {
                let sym_off = off + 8 + s * 4;
                let sym = u32::from_le_bytes([
                    r[sym_off],
                    r[sym_off + 1],
                    r[sym_off + 2],
                    r[sym_off + 3],
                ]);
                if sym == b'a' as u32 {
                    found_a = true;
                }
            }
            off += 8 + nsyms * 4;
        }
        assert!(
            found_a,
            "expected level-0 'a' keysym somewhere in the KeySyms section"
        );
    }

    #[test]
    fn get_map_modifier_map_includes_shift() {
        // Shift_L / Shift_R are mod_map'd to bit 0 — find at least
        // one entry with mods == 0x01 to prove the modmap walk
        // worked.
        let km = test_keymap();
        let r = reply_get_map(&km);
        let n_keys = r[11] - r[10] + 1;
        let total_modmap = r[33] as usize;

        // Compute the offset to the ModifierMap section.
        let mut off = 40 + 24; // past KeyTypes
        // Walk KeySyms to advance.
        for _ in 0..n_keys {
            let nsyms = u16::from_le_bytes([r[off + 6], r[off + 7]]) as usize;
            off += 8 + nsyms * 4;
        }
        // KeyActions: nk + pad.
        let nk = usize::from(n_keys);
        off += nk + ((4 - nk % 4) % 4);
        // VirtualMods/ExplicitComponents are empty (0 bytes).

        let mut found_shift = false;
        for i in 0..total_modmap {
            let mods = r[off + 2 * i + 1];
            if mods == 0x01 {
                found_shift = true;
            }
        }
        assert!(
            found_shift,
            "expected at least one Shift modifier-map entry"
        );
    }

    #[test]
    fn get_names_advertises_required_bits_with_real_data() {
        let km = test_keymap();
        let r = reply_get_names(&km);
        let which = u32::from_le_bytes([r[8], r[9], r[10], r[11]]);
        // Bits 6 (KeyTypeNames=0x40) | 7 (KTLevelNames=0x80) |
        // 9 (VirtualModNames=0x200) | 11 (KeyNames=0x800) = 0xAC0
        // is what xkbcommon-x11's `get_names_required` enforces
        // (verified via objdump on libxkbcommon-x11.so.0.13.1).
        let required = 0x0000_0AC0_u32;
        assert_eq!(
            which & required,
            required,
            "GetNames which (0x{which:08x}) must contain all required name detail bits"
        );
        // Reply must agree with reply_get_map on the keycode range.
        let map = reply_get_map(&km);
        assert_eq!(r[12], map[10], "GetNames minKeyCode == GetMap minKeyCode");
        assert_eq!(r[13], map[11], "GetNames maxKeyCode == GetMap maxKeyCode");
        let n_keys = map[11] - map[10] + 1;
        assert_eq!(r[18], map[10], "firstKey == min_kc");
        assert_eq!(r[19], n_keys, "nKeys covers full range");
        assert_eq!(r[14], 2, "nTypes == GetMap's nTypes (2)");
    }

    #[test]
    fn get_names_advertises_xkbcommon_unconditional_read_bits() {
        // xkbcommon-x11's `get_names()` (keymap.c:1139-1146) reads
        // `list.{keycodesName,symbolsName,typesName,compatName}`
        // unconditionally from a stack-uninitialized struct. The
        // xcb-generated `value_list_unpack` only writes those
        // fields when their bit is set in `which`; absent bits
        // leave stack garbage there, which the client then
        // dispatches as `GetAtomName(garbage)` requests. Advertise
        // bits 0|2|4|5 (= 0x35 = Keycodes|Symbols|Types|Compat)
        // so xcb writes our zero atoms and xkbcommon skips the
        // GetAtomName calls (atom == 0 short-circuits).
        let km = test_keymap();
        let r = reply_get_names(&km);
        let which = u32::from_le_bytes([r[8], r[9], r[10], r[11]]);
        let unconditionally_read = 0x0000_0035_u32;
        assert_eq!(
            which & unconditionally_read,
            unconditionally_read,
            "GetNames which (0x{which:08x}) must include Keycodes|Symbols|Types|Compat so \
             xcb unpacks zeros into the struct fields xkbcommon-x11 reads unconditionally"
        );
        // The four unconditional ATOMs sit at offsets 32..48 in
        // bit order (Keycodes, Symbols, Types, Compat — Geometry
        // and PhysSymbols bits are not advertised, so xcb skips
        // those slots). All values are 0.
        for i in 0..4 {
            let off = 32 + i * 4;
            let atom = u32::from_le_bytes([r[off], r[off + 1], r[off + 2], r[off + 3]]);
            assert_eq!(atom, 0, "unconditional name atom at offset {off} is 0");
        }
    }

    #[test]
    fn get_compat_map_reply_size_32() {
        assert_eq!(reply_get_compat_map().len(), 32);
    }

    #[test]
    fn get_device_info_reply_matches_xcb_struct_size() {
        // `sizeof(xcb_xkb_get_device_info_reply_t)` is 36 — verified
        // via gcc on `xcb/xkb.h`. A 32-byte reply makes xcb-based
        // clients (xkbcommon-x11) read past the allocation and pick
        // up uninit heap as `nameLen`/atoms; the fix is to publish
        // the full 36 bytes with `length = 1` and `nameLen = 0`.
        let r = reply_get_device_info();
        assert_eq!(
            r.len(),
            36,
            "matches sizeof(xcb_xkb_get_device_info_reply_t)"
        );
        assert_eq!(r[0], 1, "reply type");
        assert_eq!(r[1], 1, "deviceID");
        assert_eq!(
            u32::from_le_bytes([r[4], r[5], r[6], r[7]]),
            1,
            "length = (36 - 32) / 4 = 1"
        );
        assert_eq!(
            u16::from_le_bytes([r[32], r[33]]),
            0,
            "nameLen = 0 (no name follows)"
        );
    }
}
