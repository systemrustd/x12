//! Data-driven libinput property descriptor table.
//!
//! Single source of truth for the libinput XInput property surface.
//! Each [`PropDescriptor`] row pairs a libinput property name (as listed
//! in `/usr/include/xorg/libinput-properties.h`) with its X11 type
//! (`Bool`/`Card32`/`Float`), wire format (8 or 32), value cardinality
//! ([`ValueKind`]), access mode ([`Access`]), and — for writable rows —
//! the [`Binding`] that maps to a libinput setter.
//!
//! Used by:
//!   * `xinput::seed_touchpad` — iterates the table to populate the XI2
//!     property registry from a [`LibinputConfigSnapshot`] (T2).
//!   * `core_loop::process_request` XI2 / XI1 property dispatch arms —
//!     validates incoming writes ([`validate_value`]) and decodes them
//!     to [`DeviceConfigChange`] ([`decode_change`]) for the backend's
//!     `apply_device_config` hook (T3).
//!
//! [`LibinputConfigSnapshot`]: crate::core_loop::message::LibinputConfigSnapshot

/// A typed device-config write target (one variant per libinput setter).
///
/// Produced by [`decode_change`] after a successful [`validate_value`]
/// pass; consumed by `Backend::apply_device_config` to write through to
/// the live libinput device. `PartialEq` only (no `Eq`) because
/// `AccelSpeed` carries an `f32`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceConfigChange {
    /// `libinput Tapping Enabled` (Scalar Bool).
    Tap(bool),
    /// `libinput Tapping Drag Enabled` (Scalar Bool).
    TapDrag(bool),
    /// `libinput Tapping Drag Lock Enabled` (Scalar Bool).
    TapDragLock(bool),
    /// `libinput Tapping Button Mapping Enabled` (OneHot, 2 slots).
    /// `0 = LRM` (default), `1 = LMR` (left-handed swap).
    TapButtonMap(u8),
    /// `libinput Natural Scrolling Enabled` (Scalar Bool).
    NaturalScroll(bool),
    /// `libinput Disable While Typing Enabled` (Scalar Bool).
    Dwt(bool),
    /// `libinput Left Handed Enabled` (Scalar Bool).
    LeftHanded(bool),
    /// `libinput Middle Emulation Enabled` (Scalar Bool).
    MiddleEmulation(bool),
    /// `libinput Scroll Method Enabled` (OneHotOrNone, 3 slots).
    /// `Some(0) = two-finger`, `Some(1) = edge`, `Some(2) = button`,
    /// `None = scrolling disabled`.
    ScrollMethod(Option<u8>),
    /// `libinput Click Method Enabled` (OneHotOrNone, 2 slots).
    /// `Some(0) = button-areas`, `Some(1) = clickfinger`,
    /// `None = clicks disabled`.
    ClickMethod(Option<u8>),
    /// `libinput Send Events Mode Enabled` (BitFlags, 2 bits).
    /// `bit0 = disabled`, `bit1 = disabled-on-external-mouse`.
    SendEvents(u8),
    /// `libinput Accel Speed` (Scalar Float).
    AccelSpeed(f32),
    /// `libinput Accel Profile Enabled` (OneHotOrNone, 3 slots).
    /// `Some(0) = adaptive`, `Some(1) = flat`, `None = profile disabled`.
    /// Index 2 (custom) is rejected at decode-time with
    /// [`DeviceConfigError::Invalid`] — the workspace doesn't yet
    /// enable the `libinput_1_23` feature that exposes the custom
    /// curve setter.
    AccelProfile(Option<u8>),
    /// `libinput Button Scrolling Button` (Scalar Card32).
    ScrollButton(u32),
    /// `libinput Button Scrolling Button Lock Enabled` (Scalar Bool).
    ScrollButtonLock(bool),
}

/// Why a config apply failed → mapped to an X error by the dispatch layer.
/// `Unsupported` → BadMatch, `Invalid` → BadValue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceConfigError {
    /// Setting not supported on this device.
    Unsupported,
    /// Value out of range / not a legal one-hot / wrong byte count.
    Invalid,
}

/// X11 property value type for a descriptor row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XiValType {
    /// `INTEGER` atom, format 8 (one byte per element).
    Bool,
    /// `CARDINAL` atom, format 32 (one u32 per element).
    Card32,
    /// `FLOAT` (runtime-interned) atom, format 32 (one f32 per element).
    Float,
}

/// Value-cardinality + validation rule for a property's data block.
///
/// Element count is implied by the format:
///   * format 8 → `data.len()` elements.
///   * format 32 → `data.len() / 4` elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// Exactly one element (1 byte for Bool, 4 bytes for Card32/Float).
    Scalar,
    /// Exactly one of `n` Bool slots set, rest zero.
    OneHot { n: u8 },
    /// At most one of `n` Bool slots set (all-zero allowed).
    OneHotOrNone { n: u8 },
    /// Any subset of `n` Bool slots set (each byte non-zero ⇔ bit set).
    BitFlags { n: u8 },
}

/// Whether clients may modify a property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Writable via XIChangeProperty / XChangeDeviceProperty.
    ReadWrite,
    /// Read-only companion (`…Default`, `…Available`).
    ReadOnly,
}

/// Which libinput config a writable descriptor maps to.
///
/// One variant per libinput setter; T3 will switch on this in the
/// dispatch layer to call the matching `libinput_device_config_*_set_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    Tap,
    TapDrag,
    TapDragLock,
    TapButtonMap,
    NaturalScroll,
    Dwt,
    LeftHanded,
    MiddleEmulation,
    ScrollMethod,
    ClickMethod,
    SendEvents,
    AccelSpeed,
    AccelProfile,
    ScrollButton,
    ScrollButtonLock,
}

/// One row of the libinput property descriptor table.
pub struct PropDescriptor {
    /// Property name as listed in `libinput-properties.h`. Verbatim.
    pub name: &'static str,
    /// X11 property type atom kind.
    pub val: XiValType,
    /// Wire format: 8 for Bool, 32 for Card32/Float.
    pub format: u8,
    /// Per-row validation rule.
    pub kind: ValueKind,
    /// Read-only or read-write.
    pub access: Access,
    /// `Some(_)` for writable rows; `None` for read-only companions.
    pub binding: Option<Binding>,
}

/// The full libinput descriptor table.
///
/// Names verbatim from `/usr/include/xorg/libinput-properties.h`. Includes
/// every writable property, its `…Default` companion (ReadOnly with the
/// same `val`/`format`/`kind`, binding `None`), and the `…Available`
/// companion for method/profile/send-events groups (ReadOnly, kind
/// `BitFlags`).
///
/// `Accel Profile Enabled` is wire-width 3 (adaptive/flat/custom) to
/// match the Xorg driver; the custom slot is seeded zero and T3 rejects
/// writes that set it.
pub static DESCRIPTORS: &[PropDescriptor] = &[
    PropDescriptor {
        name: "libinput Tapping Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::Tap),
    },
    PropDescriptor {
        name: "libinput Tapping Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Tapping Drag Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::TapDrag),
    },
    PropDescriptor {
        name: "libinput Tapping Drag Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Tapping Drag Lock Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::TapDragLock),
    },
    PropDescriptor {
        name: "libinput Tapping Drag Lock Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Tapping Button Mapping Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHot { n: 2 },
        access: Access::ReadWrite,
        binding: Some(Binding::TapButtonMap),
    },
    PropDescriptor {
        name: "libinput Tapping Button Mapping Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHot { n: 2 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Natural Scrolling Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::NaturalScroll),
    },
    PropDescriptor {
        name: "libinput Natural Scrolling Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Disable While Typing Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::Dwt),
    },
    PropDescriptor {
        name: "libinput Disable While Typing Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Left Handed Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::LeftHanded),
    },
    PropDescriptor {
        name: "libinput Left Handed Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Middle Emulation Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::MiddleEmulation),
    },
    PropDescriptor {
        name: "libinput Middle Emulation Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Scroll Methods Available",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::BitFlags { n: 3 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Scroll Method Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHotOrNone { n: 3 },
        access: Access::ReadWrite,
        binding: Some(Binding::ScrollMethod),
    },
    PropDescriptor {
        name: "libinput Scroll Method Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHotOrNone { n: 3 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Click Methods Available",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::BitFlags { n: 2 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Click Method Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHotOrNone { n: 2 },
        access: Access::ReadWrite,
        binding: Some(Binding::ClickMethod),
    },
    PropDescriptor {
        name: "libinput Click Method Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHotOrNone { n: 2 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Send Events Modes Available",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::BitFlags { n: 2 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Send Events Mode Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::BitFlags { n: 2 },
        access: Access::ReadWrite,
        binding: Some(Binding::SendEvents),
    },
    PropDescriptor {
        name: "libinput Send Events Mode Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::BitFlags { n: 2 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Accel Speed",
        val: XiValType::Float,
        format: 32,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::AccelSpeed),
    },
    PropDescriptor {
        name: "libinput Accel Speed Default",
        val: XiValType::Float,
        format: 32,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Accel Profiles Available",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::BitFlags { n: 3 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Accel Profile Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHotOrNone { n: 3 },
        access: Access::ReadWrite,
        binding: Some(Binding::AccelProfile),
    },
    PropDescriptor {
        name: "libinput Accel Profile Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::OneHotOrNone { n: 3 },
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Button Scrolling Button",
        val: XiValType::Card32,
        format: 32,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::ScrollButton),
    },
    PropDescriptor {
        name: "libinput Button Scrolling Button Default",
        val: XiValType::Card32,
        format: 32,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
    PropDescriptor {
        name: "libinput Button Scrolling Button Lock Enabled",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadWrite,
        binding: Some(Binding::ScrollButtonLock),
    },
    PropDescriptor {
        name: "libinput Button Scrolling Button Lock Enabled Default",
        val: XiValType::Bool,
        format: 8,
        kind: ValueKind::Scalar,
        access: Access::ReadOnly,
        binding: None,
    },
];

/// Look up a descriptor by exact property name.
#[must_use]
pub fn descriptor_by_name(name: &str) -> Option<&'static PropDescriptor> {
    DESCRIPTORS.iter().find(|d| d.name == name)
}

/// Validate that `data` is a legal encoding for a property of `kind` at
/// the given wire `format` (8 or 32).
///
/// Returns [`DeviceConfigError::Invalid`] on any byte-count or
/// cardinality violation.
///
/// Rules:
///   * `Scalar` — `data.len() == format / 8` (1 byte for Bool, 4 for
///     Card32/Float).
///   * `OneHot { n }` — `format` MUST be 8; `data.len() == n` and
///     exactly one byte non-zero.
///   * `OneHotOrNone { n }` — `format` MUST be 8; `data.len() == n` and
///     at most one byte non-zero (all-zero allowed).
///   * `BitFlags { n }` — `format` MUST be 8; `data.len() == n` (any
///     pattern of zero/non-zero bytes is legal).
///
/// # Errors
/// Returns [`DeviceConfigError::Invalid`] for any byte-count or
/// cardinality violation.
pub fn validate_value(kind: ValueKind, format: u8, data: &[u8]) -> Result<(), DeviceConfigError> {
    match kind {
        ValueKind::Scalar => {
            let expected = usize::from(format / 8);
            if data.len() != expected {
                return Err(DeviceConfigError::Invalid);
            }
            Ok(())
        }
        ValueKind::OneHot { n } => {
            if format != 8 || data.len() != usize::from(n) {
                return Err(DeviceConfigError::Invalid);
            }
            let nonzero = data.iter().filter(|b| **b != 0).count();
            if nonzero == 1 {
                Ok(())
            } else {
                Err(DeviceConfigError::Invalid)
            }
        }
        ValueKind::OneHotOrNone { n } => {
            if format != 8 || data.len() != usize::from(n) {
                return Err(DeviceConfigError::Invalid);
            }
            let nonzero = data.iter().filter(|b| **b != 0).count();
            if nonzero <= 1 {
                Ok(())
            } else {
                Err(DeviceConfigError::Invalid)
            }
        }
        ValueKind::BitFlags { n } => {
            if format != 8 || data.len() != usize::from(n) {
                return Err(DeviceConfigError::Invalid);
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Decode — wire bytes to typed `DeviceConfigChange`.
// ---------------------------------------------------------------------------

/// Decode an already-validated on-wire value into a typed
/// [`DeviceConfigChange`] for the given [`Binding`].
///
/// Caller MUST have called [`validate_value`] first; this function
/// assumes the byte count and cardinality constraints already hold and
/// only extracts the active index / mask / scalar value. The single
/// case it rejects beyond pure validation is
/// [`Binding::AccelProfile`] with the *custom* slot set (index 2),
/// since the workspace's libinput build doesn't expose the custom
/// profile setter — that returns [`DeviceConfigError::Invalid`] and
/// the dispatcher maps it to `BadValue`.
///
/// The "method/profile disabled" cases (all-zero `OneHotOrNone`) map
/// to the *inner* `None` on `ScrollMethod` / `ClickMethod` /
/// `AccelProfile`, NOT to a surrounding `Option`; every legal input
/// produces exactly one [`DeviceConfigChange`].
///
/// # Errors
///
/// Returns [`DeviceConfigError::Invalid`] only for the
/// `AccelProfile::custom` rejection above.
pub fn decode_change(
    binding: Binding,
    data: &[u8],
) -> Result<DeviceConfigChange, DeviceConfigError> {
    // Decode helpers — caller has already validated byte counts.
    let bool_scalar = |b: &[u8]| b.first().is_some_and(|v| *v != 0);
    let onehot_index = |b: &[u8]| b.iter().position(|v| *v != 0).map(|i| i as u8);
    let bitmask = |b: &[u8]| {
        b.iter().enumerate().fold(
            0u8,
            |acc, (i, v)| {
                if *v != 0 { acc | (1 << i) } else { acc }
            },
        )
    };
    let card32 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let float32 = |b: &[u8]| f32::from_le_bytes([b[0], b[1], b[2], b[3]]);

    Ok(match binding {
        Binding::Tap => DeviceConfigChange::Tap(bool_scalar(data)),
        Binding::TapDrag => DeviceConfigChange::TapDrag(bool_scalar(data)),
        Binding::TapDragLock => DeviceConfigChange::TapDragLock(bool_scalar(data)),
        Binding::TapButtonMap => {
            // OneHot { n: 2 } — validation already guaranteed exactly one slot set.
            let idx = onehot_index(data).unwrap_or(0);
            DeviceConfigChange::TapButtonMap(idx)
        }
        Binding::NaturalScroll => DeviceConfigChange::NaturalScroll(bool_scalar(data)),
        Binding::Dwt => DeviceConfigChange::Dwt(bool_scalar(data)),
        Binding::LeftHanded => DeviceConfigChange::LeftHanded(bool_scalar(data)),
        Binding::MiddleEmulation => DeviceConfigChange::MiddleEmulation(bool_scalar(data)),
        Binding::ScrollMethod => DeviceConfigChange::ScrollMethod(onehot_index(data)),
        Binding::ClickMethod => DeviceConfigChange::ClickMethod(onehot_index(data)),
        Binding::SendEvents => DeviceConfigChange::SendEvents(bitmask(data)),
        Binding::AccelSpeed => DeviceConfigChange::AccelSpeed(float32(data)),
        Binding::AccelProfile => {
            // OneHotOrNone { n: 3 } — slot 2 is the libinput-1.23 "custom"
            // profile, which we can't honor on the current workspace build.
            // Reject at decode-time so the dispatcher emits BadValue.
            if data.get(2).is_some_and(|b| *b != 0) {
                return Err(DeviceConfigError::Invalid);
            }
            DeviceConfigChange::AccelProfile(onehot_index(data))
        }
        Binding::ScrollButton => DeviceConfigChange::ScrollButton(card32(data)),
        Binding::ScrollButtonLock => DeviceConfigChange::ScrollButtonLock(bool_scalar(data)),
    })
}

// ---------------------------------------------------------------------------
// Value encoders — emit the on-wire byte layout for a descriptor row.
// ---------------------------------------------------------------------------

/// Encode a Bool scalar as a one-byte vector (`[0]` or `[1]`).
#[must_use]
pub fn encode_bool(value: bool) -> Vec<u8> {
    vec![u8::from(value)]
}

/// Encode a one-hot Bool group with `n` slots.
///
/// `idx = Some(i)` sets byte `i` to 1, rest 0. `idx = None` yields all
/// zeros (only legal for `OneHotOrNone` consumers).
///
/// # Panics
/// Panics in debug if `idx` is out of range; release truncates to `n`.
#[must_use]
pub fn encode_onehot(idx: Option<u8>, n: u8) -> Vec<u8> {
    let len = usize::from(n);
    let mut buf = vec![0u8; len];
    if let Some(i) = idx {
        debug_assert!(usize::from(i) < len, "encode_onehot: idx {i} out of {len}");
        if let Some(slot) = buf.get_mut(usize::from(i)) {
            *slot = 1;
        }
    }
    buf
}

/// Encode a bitflags Bool group: one byte per bit, byte = 1 if bit set.
#[must_use]
pub fn encode_bitflags(mask: u8, n: u8) -> Vec<u8> {
    (0..n).map(|i| u8::from(mask & (1 << i) != 0)).collect()
}

/// Encode a CARDINAL/32 scalar as 4 little-endian bytes.
#[must_use]
pub fn encode_card32(value: u32) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

/// Encode a FLOAT/32 scalar as 4 little-endian IEEE-754 bytes.
#[must_use]
pub fn encode_float(value: f32) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_onehot_requires_exactly_one() {
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[1, 0]).is_ok());
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[0, 1]).is_ok());
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[0, 0]).is_err()); // none
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[1, 1]).is_err()); // two
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[1]).is_err()); // wrong count
    }

    #[test]
    fn validate_onehotornone_allows_zero() {
        assert!(validate_value(ValueKind::OneHotOrNone { n: 3 }, 8, &[0, 0, 0]).is_ok());
        assert!(validate_value(ValueKind::OneHotOrNone { n: 3 }, 8, &[0, 1, 0]).is_ok());
        assert!(validate_value(ValueKind::OneHotOrNone { n: 3 }, 8, &[1, 1, 0]).is_err());
    }

    #[test]
    fn validate_bitflags_allows_any_subset() {
        for v in [&[0u8, 0][..], &[1, 0][..], &[0, 1][..], &[1, 1][..]] {
            assert!(validate_value(ValueKind::BitFlags { n: 2 }, 8, v).is_ok());
        }
        assert!(validate_value(ValueKind::BitFlags { n: 2 }, 8, &[1]).is_err()); // wrong count
    }

    #[test]
    fn validate_scalar_float_is_four_bytes() {
        assert!(validate_value(ValueKind::Scalar, 32, &0.5f32.to_le_bytes()).is_ok());
        assert!(validate_value(ValueKind::Scalar, 32, &[0, 0, 0]).is_err());
    }

    #[test]
    fn validate_scalar_bool_is_one_byte() {
        assert!(validate_value(ValueKind::Scalar, 8, &[1]).is_ok());
        assert!(validate_value(ValueKind::Scalar, 8, &[0]).is_ok());
        assert!(validate_value(ValueKind::Scalar, 8, &[0, 0]).is_err());
    }

    #[test]
    fn descriptor_by_name_finds_known() {
        let d = descriptor_by_name("libinput Tapping Enabled").unwrap();
        assert_eq!(d.val, XiValType::Bool);
        assert_eq!(d.format, 8);
        assert_eq!(d.kind, ValueKind::Scalar);
        assert_eq!(d.access, Access::ReadWrite);
        assert_eq!(d.binding, Some(Binding::Tap));
    }

    #[test]
    fn descriptor_by_name_returns_none_for_unknown() {
        assert!(descriptor_by_name("not a libinput prop").is_none());
    }

    #[test]
    fn accel_profile_enabled_is_three_wide() {
        let d = descriptor_by_name("libinput Accel Profile Enabled").unwrap();
        assert_eq!(d.kind, ValueKind::OneHotOrNone { n: 3 });
    }

    #[test]
    fn every_writable_has_default_companion() {
        for d in DESCRIPTORS.iter().filter(|d| d.access == Access::ReadWrite) {
            let default_name = format!("{} Default", d.name);
            // Some props use a slightly different default suffix; check
            // for at least one ReadOnly companion that mirrors the kind.
            let companion = DESCRIPTORS.iter().find(|c| {
                c.access == Access::ReadOnly
                    && c.binding.is_none()
                    && c.kind == d.kind
                    && c.format == d.format
                    && (c.name == default_name
                        || c.name == "libinput Tapping Button Mapping Default"
                            && d.name == "libinput Tapping Button Mapping Enabled")
            });
            assert!(
                companion.is_some(),
                "no Default companion for writable `{}`",
                d.name
            );
        }
    }

    #[test]
    fn encode_onehot_sets_correct_byte() {
        assert_eq!(encode_onehot(Some(0), 2), vec![1, 0]);
        assert_eq!(encode_onehot(Some(1), 2), vec![0, 1]);
        assert_eq!(encode_onehot(None, 3), vec![0, 0, 0]);
        assert_eq!(encode_onehot(Some(2), 3), vec![0, 0, 1]);
    }

    #[test]
    fn encode_bitflags_one_byte_per_bit() {
        assert_eq!(encode_bitflags(0b00, 2), vec![0, 0]);
        assert_eq!(encode_bitflags(0b01, 2), vec![1, 0]);
        assert_eq!(encode_bitflags(0b10, 2), vec![0, 1]);
        assert_eq!(encode_bitflags(0b11, 2), vec![1, 1]);
        assert_eq!(encode_bitflags(0b101, 3), vec![1, 0, 1]);
    }

    #[test]
    fn encode_float_is_little_endian_ieee754() {
        assert_eq!(encode_float(0.5), 0.5f32.to_le_bytes().to_vec());
        assert_eq!(encode_float(-1.0), (-1.0f32).to_le_bytes().to_vec());
    }

    #[test]
    fn decode_change_maps_each_binding() {
        use Binding::*;
        assert_eq!(
            decode_change(Tap, &[1]).unwrap(),
            DeviceConfigChange::Tap(true)
        );
        assert_eq!(
            decode_change(Tap, &[0]).unwrap(),
            DeviceConfigChange::Tap(false)
        );
        assert_eq!(
            decode_change(TapButtonMap, &[1, 0]).unwrap(),
            DeviceConfigChange::TapButtonMap(0)
        );
        assert_eq!(
            decode_change(TapButtonMap, &[0, 1]).unwrap(),
            DeviceConfigChange::TapButtonMap(1)
        );
        assert_eq!(
            decode_change(ScrollMethod, &[0, 1, 0]).unwrap(),
            DeviceConfigChange::ScrollMethod(Some(1))
        );
        assert_eq!(
            decode_change(ScrollMethod, &[0, 0, 0]).unwrap(),
            DeviceConfigChange::ScrollMethod(None)
        );
        assert_eq!(
            decode_change(ClickMethod, &[1, 0]).unwrap(),
            DeviceConfigChange::ClickMethod(Some(0))
        );
        assert_eq!(
            decode_change(SendEvents, &[1, 0]).unwrap(),
            DeviceConfigChange::SendEvents(0b01)
        );
        assert_eq!(
            decode_change(SendEvents, &[1, 1]).unwrap(),
            DeviceConfigChange::SendEvents(0b11)
        );
        assert_eq!(
            decode_change(AccelSpeed, &0.25f32.to_le_bytes()).unwrap(),
            DeviceConfigChange::AccelSpeed(0.25)
        );
        assert_eq!(
            decode_change(AccelProfile, &[1, 0, 0]).unwrap(),
            DeviceConfigChange::AccelProfile(Some(0))
        );
        assert_eq!(
            decode_change(AccelProfile, &[0, 0, 0]).unwrap(),
            DeviceConfigChange::AccelProfile(None)
        );
        // Accel profile custom (index 2) → Invalid, we can't honor it.
        assert!(decode_change(AccelProfile, &[0, 0, 1]).is_err());
        assert_eq!(
            decode_change(ScrollButton, &274u32.to_le_bytes()).unwrap(),
            DeviceConfigChange::ScrollButton(274)
        );
        assert_eq!(
            decode_change(ScrollButtonLock, &[1]).unwrap(),
            DeviceConfigChange::ScrollButtonLock(true)
        );
    }
}
