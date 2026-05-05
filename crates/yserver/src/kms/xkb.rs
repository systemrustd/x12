use xkbcommon::xkb::Keymap;

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

/// XKB GetControls reply (minor=24). Fixed 92 bytes.
/// Reports repeat delay/interval and enable flags.
pub(super) fn reply_get_controls(_keymap: &Keymap) -> Vec<u8> {
    let mut r = vec![0u8; 92];
    r[0] = 1; // reply type
    // [4..8] extra length = (92-32)/4 = 15
    r[4..8].copy_from_slice(&15u32.to_le_bytes());
    // Repeat delay = 500ms, interval = 33ms (≈30 Hz)
    let delay: u16 = 500;
    let interval: u16 = 33;
    r[12..14].copy_from_slice(&delay.to_le_bytes());
    r[14..16].copy_from_slice(&interval.to_le_bytes());
    // EnabledControls: RepeatKeys (bit 28) | PerKeyRepeat (bit 0)
    let flags: u32 = 0x1000_0001;
    r[32..36].copy_from_slice(&flags.to_le_bytes());
    r
}

/// XKB GetMap reply (minor=8). Reports min/max keycode; present=0 (no tables).
/// Clients that only need keycode range (e.g. for event validation) are unblocked.
pub(super) fn reply_get_map(keymap: &Keymap) -> Vec<u8> {
    let min_kc = keymap.min_keycode().raw() as u8;
    let max_kc = keymap.max_keycode().raw() as u8;
    // 32-byte header; present=0 means no type/sym/mod tables follow.
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    // [4..8] extra length = 0
    r[4] = 1; // deviceID=1
    r[8] = min_kc;
    r[9] = max_kc;
    // [10..12] present bitmask = 0 (no tables)
    r
}

/// XKB GetNames reply (minor=17). Empty name lists.
pub(super) fn reply_get_names(_keymap: &Keymap) -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1;
    // which=0 → no name arrays follow; extra length = 0
    r
}

/// XKB GetCompatMap reply (minor=20). Empty compat map.
pub(super) fn reply_get_compat_map() -> Vec<u8> {
    // Header + 4 bytes for n_si_rtrn=0, groups_rtrn=0xff (all groups unchanged)
    let mut r = vec![0u8; 36];
    r[0] = 1;
    // [4..8] extra length = (36-32)/4 = 1
    r[4] = 1;
    // [8] deviceID=1, [9] groupsRtrn=0
    r[8] = 1;
    // [10..12] firstSIRtrn=0, [12..14] nSIRtrn=0 (no SI entries follow)
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
    fn get_map_min_max_keycode() {
        let km = test_keymap();
        let r = reply_get_map(&km);
        assert!(r.len() >= 10);
        let min = r[8];
        let max = r[9];
        assert!(min <= max, "min_keycode must be <= max_keycode");
    }
}
