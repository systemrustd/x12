# Touchpad Tier 2b Implementation Plan — writable properties + property-change events

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make discovered touchpad libinput properties take effect (XIChangeProperty/ChangeDeviceProperty → live libinput config) and notify subscribers (XI2 `XI_PropertyEvent` + XI1 `DevicePropertyNotify`), with Xorg-faithful error codes and the `XIGetProperty` `BadAtom` gap closed.

**Architecture:** A data-driven descriptor table in `crates/yserver-core/src/xinput/libinput_props.rs` is the single source of truth for the touchpad/pointer property set (X type/format/value-kind/access + libinput binding). Seeding reads it; the writable router validates per value-kind, decodes to a typed `DeviceConfigChange`, applies via a new `Backend::apply_device_config` hook (libseat backend owns live `input::Device` handles on the core thread), and only commits the registry + emits events on success.

**Tech stack:** Rust; `yserver-core` (protocol/dispatch), `yserver` (KMS/libseat backend); the `input` 0.10 crate (libinput); reference headers `/usr/include/X11/extensions/{XI2,XI2proto,XIproto,XInput}.h`, `/usr/include/xorg/libinput-properties.h`; reference server `../xserver/Xi/`.

**Spec:** `docs/superpowers/specs/2026-06-02-touchpad-tier2b-design.md` (Rev 3 final; codex-converged).

**Conventions (read before starting):**
- All multi-byte wire fields are little/big-endian per the client's `ClientByteOrder`; reuse `xinput.rs` helpers `write_u16`/`write_u32`/`fixed_header`/`pad_to_4`.
- Property values are stored as raw LE bytes in `XiProperty { type_atom, format, data }` (xinput.rs:71). `format` ∈ {8,16,32}.
- Existing property machinery lives in `crates/yserver-core/src/xinput.rs`: `apply_change_property` (541), `apply_delete_property` (605), `compute_get_property` (398), `encode_get_property_reply` (481), `encode_xi1_get_property_reply` (694), `seed_touchpad` (152), `clear_touchpad` (213), `XiPropError` (269).
- Dispatch arms in `crates/yserver-core/src/core_loop/process_request.rs`: XI2 XIChangeProperty ~8675, XIDeleteProperty ~8790, XIGetProperty ~8821; XI1 ChangeDeviceProperty(minor 37) ~9656, XI1 GetDeviceProperty(minor 39) ~9789.
- Run a single test: `cargo test -p yserver-core --lib <test_name>`. Workspace build: `cargo build`. Before any commit: `cargo +nightly fmt`, `cargo clippy -- -W clippy::pedantic`, `cargo test`.
- Commit per task. Branch: `feat/touch-input`.

**Verified wire facts (from headers — do NOT re-derive/guess):**
- XI2 `XI_PropertyEvent`: `evtype = 12`; mask bit `(1 << 12)` (`XI_PropertyEventMask`). `xXIPropertyEvent` (32 bytes): `type`=GenericEvent(35), `extension`=XI major(byte 1), `sequenceNumber`(u16), `length`=0(u32), `evtype`=12(u16), `deviceid`(u16), `time`(u32), `property`(Atom u32), `what`(u8: Deleted=0/Created=1/Modified=2), `pad0`(u8), `pad1`(u16), `pad2`(u32), `pad3`(u32).
- XI1 `DevicePropertyNotify`: type = `first_event + 16` (`XI_DevicePropertyNotify=16`). `devicePropertyNotify` (32 bytes, XIproto.h): `type`(u8), `state`(u8: PropertyNewValue=0/PropertyDelete=1), `sequenceNumber`(u16), `time`(u32), `atom`(u32), `pad0`(u32), `pad1`(u32), `pad2`(u32), `pad3`(u32), `pad5`(u16), `pad4`(u8), `deviceid`(u8). **No window field.**
- XI1 event class = `(deviceid << 8) | (first_event + 16)`; selection request = `SelectExtensionEvent` (XInput minor `X_SelectExtensionEvent = 6`).
- Predefined atoms: `INTEGER=19`, `CARDINAL=6`, `STRING=31`. The `FLOAT` atom is NOT predefined — intern the literal name `"FLOAT"`.
- `input` crate confirmed setters: `config_tap_set_enabled(bool)`, `config_tap_set_drag_enabled(bool)`, `config_tap_set_drag_lock_enabled(DragLockState)`, `config_tap_set_button_map(TapButtonMap)`, `config_scroll_set_natural_scroll_enabled(bool)`, `config_scroll_set_method(ScrollMethod)`, `config_scroll_set_button(u32)`, `config_scroll_set_button_lock(ScrollButtonLockState)`, `config_dwt_set_enabled(bool)`, `config_left_handed_set(bool)`, `config_middle_emulation_set_enabled(bool)`, `config_click_set_method(ClickMethod)`, `config_send_events_set_mode(SendEventsMode)`, `config_accel_set_speed(f64)`, `config_accel_set_profile(AccelProfile)`. Each has matching `config_*_is_available`/`*_default*`/getter. `AccelProfile::Custom` is `#[cfg(feature="libinput_1_23")]` — NOT available (workspace uses default features); custom is rejected, not honored.

---

## File structure

- **Create** `crates/yserver-core/src/xinput/libinput_props.rs` — descriptor table, `ValueKind`, value encode/decode + validation, `DeviceConfigChange`, `DeviceConfigError`, `prop_change → DeviceConfigChange` mapping. Pure, fully unit-tested.
- **Modify** `crates/yserver-core/src/xinput.rs` — becomes `xinput/mod.rs` re-exporting `libinput_props`; add `XiDevice.device_node`, `PropChange`, BadAtom-aware validation hooks, event-encoder fns (`encode_xi2_property_event`, `encode_xi1_device_property_notify`); rework `seed_touchpad` to drive off the descriptor table.
- **Modify** `crates/yserver-core/src/core_loop/message.rs` — replace the 6 ad-hoc `DeviceInfo` config fields with `LibinputConfigSnapshot`.
- **Modify** `crates/yserver/src/input/context.rs` — fill `LibinputConfigSnapshot` from the live device.
- **Modify** `crates/yserver-core/src/backend/trait_def.rs` — add `apply_device_config` hook (default no-op `Ok`).
- **Modify** `crates/yserver/src/kms/v2/backend.rs` — `device_node → input::Device` map; implement `apply_device_config`.
- **Modify** `crates/yserver-core/src/server.rs` — add `xi1_event_selections` store (T6) + `xi1_first_event` constant; `ValidAtom` helper if absent.
- **Modify** `crates/yserver-core/src/core_loop/process_request.rs` — wire validate-before-commit + BadAtom into the 5 dispatch arms; emit events; handle `SelectExtensionEvent`.

---

## Task 1: `DeviceInfo` config snapshot + Backend hook + intern FLOAT

**Files:**
- Modify: `crates/yserver-core/src/core_loop/message.rs:28-35` (DeviceInfo fields), tests ~183-219
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (add hook)
- Modify: `crates/yserver-core/src/xinput.rs` (add `XA_CARDINAL`, `FLOAT` intern helper)

- [ ] **Step 1: Write failing test for the snapshot type**

In `message.rs` tests module add:

```rust
#[test]
fn libinput_config_snapshot_is_copy_and_default() {
    let s = LibinputConfigSnapshot::default();
    assert!(!s.tap.available);
    let _copy = s; // must be Copy
    let _again = s; // still usable → Copy, not Move
}
```

- [ ] **Step 2: Run, expect fail** — `cargo test -p yserver-core --lib libinput_config_snapshot_is_copy_and_default` → FAIL (`LibinputConfigSnapshot` undefined).

- [ ] **Step 3: Define the snapshot** in `message.rs`. One `BoolSetting` per available/current/default boolean group; richer fields for the irregular ones.

```rust
/// One libinput boolean config item: whether it's available on the device,
/// its current value, and its default. `Copy`/`Send` — no libinput handles.
#[derive(Debug, Clone, Copy, Default)]
pub struct BoolSetting { pub available: bool, pub current: bool, pub default: bool }

/// Full touchpad/pointer libinput config snapshot, gathered at DeviceAdded.
#[derive(Debug, Clone, Copy, Default)]
pub struct LibinputConfigSnapshot {
    pub tap: BoolSetting,
    pub tap_drag: BoolSetting,
    pub tap_drag_lock: BoolSetting,
    pub natural_scroll: BoolSetting,
    pub dwt: BoolSetting,
    pub left_handed: BoolSetting,
    pub middle_emulation: BoolSetting,
    pub scroll_button_lock: BoolSetting,
    /// Accel speed (FLOAT). available + current + default.
    pub accel: FloatSetting,
    /// Button-scrolling button number (CARD32).
    pub scroll_button: U32Setting,
    /// Tapping button map: available + which of {LRM=0,LMR=1} is current/default.
    pub tap_button_map: OneHot2,
    /// Scroll methods: bit0=2fg, bit1=edge, bit2=button. available_mask/current/default.
    pub scroll_method: OneHot3,
    /// Click methods: bit0=buttonareas, bit1=clickfinger.
    pub click_method: OneHot2,
    /// Accel profiles: bit0=adaptive, bit1=flat (custom excluded — feature-gated).
    pub accel_profile: OneHot2,
    /// Send-events: bitflags bit0=disabled, bit1=disabled-on-external-mouse.
    pub send_events: BitFlags2,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FloatSetting { pub available: bool, pub current: f32, pub default: f32 }
#[derive(Debug, Clone, Copy, Default)]
pub struct U32Setting { pub available: bool, pub current: u32, pub default: u32 }
/// One-hot over 2 slots: `current`/`default` are the active index (0 or 1) or None.
#[derive(Debug, Clone, Copy, Default)]
pub struct OneHot2 { pub available: bool, pub current: Option<u8>, pub default: Option<u8> }
#[derive(Debug, Clone, Copy, Default)]
pub struct OneHot3 { pub available_mask: u8, pub current: Option<u8>, pub default: Option<u8> }
/// Bitflags over 2 slots.
#[derive(Debug, Clone, Copy, Default)]
pub struct BitFlags2 { pub available_mask: u8, pub current_mask: u8, pub default_mask: u8 }
```

Then in `DeviceInfo` (message.rs:28-35) **replace** the six fields `tap_enabled..dwt_default` with:

```rust
    /// Full libinput config snapshot (meaningful only when is_touchpad).
    pub config: LibinputConfigSnapshot,
```

- [ ] **Step 4: Sweep ALL `DeviceInfo` constructors/field-users (codex plan-review: more sites than first listed).** Removing the 6 fields breaks every literal that set them. Fix each to use `config: LibinputConfigSnapshot { <group>: BoolSetting { available: true, current: …, default: … }, ..Default::default() }` (or `::default()` where the old values were all false). Known sites — update all, then `cargo build -p yserver-core` and fix any remaining compiler hits:
  - `crates/yserver-core/src/core_loop/message.rs` tests ~191 and ~213
  - `crates/yserver-core/src/core_loop/run.rs:1531-1544`
  - `crates/yserver-core/src/core_loop/process_request.rs:17229-17242`, `17357-17370`, `17783-17796`
  - `crates/yserver-core/src/xinput.rs:772-802` (test helper builders)
  - `crates/yserver/src/input/context.rs` (the live construction — fully reworked in T4 Step 1; for now just make it compile with `config: LibinputConfigSnapshot::default()`)
  The old `.tap_enabled`/`.tap_default`/`.natural_scroll_*`/`.dwt_*` field reads in `seed_touchpad` (xinput.rs:177-182) are replaced in T2 Step 7 — for this task, leave `seed_touchpad` reading `info.config.<group>.current` minimally so it builds.

- [ ] **Step 5: Run snapshot test, expect pass** — `cargo test -p yserver-core --lib libinput_config_snapshot_is_copy_and_default` → PASS.

- [ ] **Step 6: Add the Backend hook.** In `crates/yserver-core/src/backend/trait_def.rs`, near `on_host_input`/`probe_input_devices`, add the change type and hook. Put `DeviceConfigChange`/`DeviceConfigError` in `libinput_props` (T2) — for now forward-declare by adding the hook referencing `crate::xinput::libinput_props::{DeviceConfigChange, DeviceConfigError}` and create a minimal stub module so it compiles:

Create `crates/yserver-core/src/xinput/` dir: move `xinput.rs` → `xinput/mod.rs` (keep all contents), add at top `pub mod libinput_props;` and create `crates/yserver-core/src/xinput/libinput_props.rs` with just:

```rust
//! Data-driven libinput property descriptor table (Tier 2b). Filled in Task 2.
use crate::core_loop::message::LibinputConfigSnapshot; // (used in T2)

/// A typed device-config write target (one variant per libinput setter).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceConfigChange { Placeholder } // replaced in T2

/// Why a config apply failed → mapped to an X error by the dispatch layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceConfigError { Unsupported, Invalid }
```

Then in `trait_def.rs`:

```rust
    /// Apply a decoded touchpad config change to the live input device
    /// identified by `device_node`. `Ok` = applied (or nothing to apply on
    /// this backend). `Err(Unsupported)` → BadMatch, `Err(Invalid)` → BadValue.
    /// Default: no-op (host-X11 / nested / Direct mode — see spec §E).
    fn apply_device_config(
        &mut self,
        _device_node: &str,
        _change: crate::xinput::libinput_props::DeviceConfigChange,
    ) -> Result<(), crate::xinput::libinput_props::DeviceConfigError> {
        Ok(())
    }
```

- [ ] **Step 7: Intern FLOAT + add CARDINAL atom.** In `xinput/mod.rs` add `pub const XA_CARDINAL: AtomId = AtomId(6);` next to `XA_INTEGER` (xinput.rs:37). Find the server-init atom interning that already interns MOUSE/KEYBOARD/TOUCHPAD (commit 57e659b — grep `intern("TOUCHPAD"`); add `atoms.intern("FLOAT", false)` there and expose the resulting `AtomId` on `ServerState` as `pub float_atom: AtomId` (set at init). Write a test asserting `state.float_atom` is non-zero after construction.

- [ ] **Step 8: fmt + clippy + full build** — `cargo +nightly fmt && cargo clippy -- -W clippy::pedantic && cargo build`. Fix warnings.

- [ ] **Step 9: Commit** — `git add -A && git commit -m "feat(xinput): Tier 2b T1 — DeviceInfo config snapshot, apply_device_config hook, FLOAT atom"`

---

## Task 2: Descriptor table + value encode/decode/validate + seeding

**Files:**
- Modify: `crates/yserver-core/src/xinput/libinput_props.rs` (the real table + logic)
- Modify: `crates/yserver-core/src/xinput/mod.rs` (`XiDevice.device_node`; rework `seed_touchpad`)

- [ ] **Step 1: Define the descriptor model + table.** Replace the T1 placeholder in `libinput_props.rs`:

```rust
use crate::core_loop::message::LibinputConfigSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XiValType { Bool, Card32, Float }

/// Value-cardinality + validation rule for a property's data block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Scalar,                 // n=1
    OneHot { n: u8 },       // exactly one of n bits set
    OneHotOrNone { n: u8 }, // at most one of n bits set (all-zero allowed)
    BitFlags { n: u8 },     // any subset of n bits
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access { ReadWrite, ReadOnly }

/// Which libinput config a writable descriptor maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    Tap, TapDrag, TapDragLock, TapButtonMap,
    NaturalScroll, Dwt, LeftHanded, MiddleEmulation,
    ScrollMethod, ClickMethod, SendEvents, AccelSpeed, AccelProfile,
    ScrollButton, ScrollButtonLock,
}

pub struct PropDescriptor {
    pub name: &'static str,
    pub val: XiValType,
    pub format: u8,        // 8 (Bool) | 32 (Card32/Float)
    pub kind: ValueKind,
    pub access: Access,
    pub binding: Option<Binding>, // None = read-only companion
}
```

Then the table (names verbatim from `/usr/include/xorg/libinput-properties.h`). Include each writable prop, its `…Default` companion (ReadOnly, same val/format/kind, binding None), and `…Available` for the method/profile/send-events groups. Example rows (write the full set):

```rust
pub static DESCRIPTORS: &[PropDescriptor] = &[
    PropDescriptor { name: "libinput Tapping Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadWrite, binding: Some(Binding::Tap) },
    PropDescriptor { name: "libinput Tapping Enabled Default", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Tapping Drag Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadWrite, binding: Some(Binding::TapDrag) },
    PropDescriptor { name: "libinput Tapping Drag Enabled Default", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Tapping Drag Lock Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadWrite, binding: Some(Binding::TapDragLock) },
    PropDescriptor { name: "libinput Tapping Drag Lock Enabled Default", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Tapping Button Mapping Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::OneHot { n: 2 }, access: Access::ReadWrite, binding: Some(Binding::TapButtonMap) },
    PropDescriptor { name: "libinput Tapping Button Mapping Default", val: XiValType::Bool, format: 8, kind: ValueKind::OneHot { n: 2 }, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Natural Scrolling Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadWrite, binding: Some(Binding::NaturalScroll) },
    PropDescriptor { name: "libinput Natural Scrolling Enabled Default", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Disable While Typing Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadWrite, binding: Some(Binding::Dwt) },
    PropDescriptor { name: "libinput Disable While Typing Enabled Default", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Left Handed Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadWrite, binding: Some(Binding::LeftHanded) },
    PropDescriptor { name: "libinput Left Handed Enabled Default", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Middle Emulation Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadWrite, binding: Some(Binding::MiddleEmulation) },
    PropDescriptor { name: "libinput Middle Emulation Enabled Default", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Scroll Methods Available", val: XiValType::Bool, format: 8, kind: ValueKind::BitFlags { n: 3 }, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Scroll Method Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::OneHotOrNone { n: 3 }, access: Access::ReadWrite, binding: Some(Binding::ScrollMethod) },
    PropDescriptor { name: "libinput Scroll Method Enabled Default", val: XiValType::Bool, format: 8, kind: ValueKind::OneHotOrNone { n: 3 }, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Click Methods Available", val: XiValType::Bool, format: 8, kind: ValueKind::BitFlags { n: 2 }, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Click Method Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::OneHotOrNone { n: 2 }, access: Access::ReadWrite, binding: Some(Binding::ClickMethod) },
    PropDescriptor { name: "libinput Click Method Enabled Default", val: XiValType::Bool, format: 8, kind: ValueKind::OneHotOrNone { n: 2 }, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Send Events Modes Available", val: XiValType::Bool, format: 8, kind: ValueKind::BitFlags { n: 2 }, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Send Events Mode Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::BitFlags { n: 2 }, access: Access::ReadWrite, binding: Some(Binding::SendEvents) },
    PropDescriptor { name: "libinput Send Events Mode Enabled Default", val: XiValType::Bool, format: 8, kind: ValueKind::BitFlags { n: 2 }, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Accel Speed", val: XiValType::Float, format: 32, kind: ValueKind::Scalar, access: Access::ReadWrite, binding: Some(Binding::AccelSpeed) },
    PropDescriptor { name: "libinput Accel Speed Default", val: XiValType::Float, format: 32, kind: ValueKind::Scalar, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Accel Profiles Available", val: XiValType::Bool, format: 8, kind: ValueKind::BitFlags { n: 3 }, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Accel Profile Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::OneHotOrNone { n: 3 }, access: Access::ReadWrite, binding: Some(Binding::AccelProfile) },
    PropDescriptor { name: "libinput Accel Profile Enabled Default", val: XiValType::Bool, format: 8, kind: ValueKind::OneHotOrNone { n: 3 }, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Button Scrolling Button", val: XiValType::Card32, format: 32, kind: ValueKind::Scalar, access: Access::ReadWrite, binding: Some(Binding::ScrollButton) },
    PropDescriptor { name: "libinput Button Scrolling Button Default", val: XiValType::Card32, format: 32, kind: ValueKind::Scalar, access: Access::ReadOnly, binding: None },
    PropDescriptor { name: "libinput Button Scrolling Button Lock Enabled", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadWrite, binding: Some(Binding::ScrollButtonLock) },
    PropDescriptor { name: "libinput Button Scrolling Button Lock Enabled Default", val: XiValType::Bool, format: 8, kind: ValueKind::Scalar, access: Access::ReadOnly, binding: None },
];

pub fn descriptor_by_name(name: &str) -> Option<&'static PropDescriptor> {
    DESCRIPTORS.iter().find(|d| d.name == name)
}
```
Note: `Accel Profile Enabled` is wire-width 3 (adaptive/flat/custom) to match the driver, but the custom slot is rejected on write (T3) and seeded 0.

- [ ] **Step 2: Write failing tests for per-kind validation.**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validate_onehot_requires_exactly_one() {
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[1, 0]).is_ok());
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[0, 1]).is_ok());
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[0, 0]).is_err()); // none
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[1, 1]).is_err()); // two
        assert!(validate_value(ValueKind::OneHot { n: 2 }, 8, &[1]).is_err());    // wrong count
    }
    #[test]
    fn validate_onehotornone_allows_zero() {
        assert!(validate_value(ValueKind::OneHotOrNone { n: 3 }, 8, &[0, 0, 0]).is_ok());
        assert!(validate_value(ValueKind::OneHotOrNone { n: 3 }, 8, &[0, 1, 0]).is_ok());
        assert!(validate_value(ValueKind::OneHotOrNone { n: 3 }, 8, &[1, 1, 0]).is_err());
    }
    #[test]
    fn validate_bitflags_allows_any_subset() {
        for v in [&[0u8,0], &[1,0], &[0,1], &[1,1]] {
            assert!(validate_value(ValueKind::BitFlags { n: 2 }, 8, v).is_ok());
        }
        assert!(validate_value(ValueKind::BitFlags { n: 2 }, 8, &[1]).is_err()); // wrong count
    }
    #[test]
    fn validate_scalar_float_is_four_bytes() {
        assert!(validate_value(ValueKind::Scalar, 32, &0.5f32.to_le_bytes()).is_ok());
        assert!(validate_value(ValueKind::Scalar, 32, &[0,0,0]).is_err());
    }
}
```

- [ ] **Step 3: Run, expect fail** — `cargo test -p yserver-core --lib libinput_props` → FAIL (`validate_value` undefined).

- [ ] **Step 4: Implement `validate_value`.** Returns `Result<(), DeviceConfigError>` (`Invalid` on any violation). For format-8 kinds, element count = `data.len()` (each value is 1 byte); for `Scalar` format-32, require exactly 4 bytes. `OneHot{n}` → len==n && exactly one nonzero; `OneHotOrNone{n}` → len==n && ≤1 nonzero; `BitFlags{n}` → len==n; `Scalar` → len == format/8.

- [ ] **Step 5: Run, expect pass.**

- [ ] **Step 6: Add `XiDevice.device_node`.** In `xinput/mod.rs` `XiDevice` (xinput.rs:79) add `pub device_node: Option<String>,`; init `None` in `XiDevice::new` (94) and in `clear_touchpad` reset to `None`.

- [ ] **Step 7: Rework `seed_touchpad` to drive off the table + snapshot.** Replace the hand-written `insert_bool` block (xinput.rs:164-204) with: keep Device Node + Device Product ID inserts (and set `dev.device_node = Some(info.device_node.clone())`), then for each `PropDescriptor` whose availability predicate holds in `info.config`, insert `XiProperty { type_atom, format, data }` where `type_atom` = INTEGER for Bool, CARDINAL for Card32, the FLOAT atom for Float (pass `float_atom: AtomId` into `seed_touchpad` from the caller), and `data` = current value encoded per kind (helpers `encode_bool`, `encode_onehot(idx,n)`, `encode_bitflags(mask,n)`, `encode_card32`, `encode_float`). Availability predicates map descriptor `binding`→`info.config.<group>.available`/`available_mask != 0`. Write a `seed_then_table_matches` unit test: build a `DeviceInfo` with `config.tap.available=true, current=true`, seed, assert the registry contains `"libinput Tapping Enabled"` = `[1]` INTEGER/8 and `"libinput Tapping Enabled Default"`, and does NOT contain accel props (accel.available=false).

- [ ] **Step 8: Update callers of `seed_touchpad`** for the new `float_atom` param (grep `seed_touchpad(` / `xi_seed_touchpad`; the `ServerState::xi_seed_touchpad` wrapper passes `self.float_atom`). Fix the existing seed/clear tests in `process_request.rs` (~17243, 17371, 17783) that build `DeviceInfo` with old fields → use `config: LibinputConfigSnapshot { tap: BoolSetting{available:true,current:true,default:false}, ..Default::default() }`.

- [ ] **Step 9: fmt + clippy + test** — `cargo +nightly fmt && cargo clippy -- -W clippy::pedantic && cargo test -p yserver-core`. All green.

- [ ] **Step 10: Commit** — `git commit -am "feat(xinput): Tier 2b T2 — descriptor table, value validation, table-driven seeding"`

---

## Task 3: Writable routing (validate→apply→commit) + BadAtom

**Files:**
- Modify: `crates/yserver-core/src/xinput/libinput_props.rs` (`DeviceConfigChange` real variants + `decode_change`)
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (5 dispatch arms)
- Modify: `crates/yserver-core/src/server.rs` (ValidAtom helper if absent)

- [ ] **Step 1: Replace the `DeviceConfigChange` placeholder** with real variants and write a failing decode test:

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceConfigChange {
    Tap(bool), TapDrag(bool), TapDragLock(bool), TapButtonMap(u8 /*0=LRM,1=LMR*/),
    NaturalScroll(bool), Dwt(bool), LeftHanded(bool), MiddleEmulation(bool),
    ScrollMethod(Option<u8> /*0=2fg,1=edge,2=button*/),
    ClickMethod(Option<u8> /*0=areas,1=clickfinger*/),
    SendEvents(u8 /*bitmask: bit0 disabled, bit1 disabled-on-ext*/),
    AccelSpeed(f32), AccelProfile(Option<u8> /*0=adaptive,1=flat*/),
    ScrollButton(u32), ScrollButtonLock(bool),
}
```
Test:
```rust
#[test]
fn decode_change_maps_each_binding() {
    use Binding::*;
    assert_eq!(decode_change(Tap, 8, &[1]).unwrap(), Some(DeviceConfigChange::Tap(true)));
    assert_eq!(decode_change(ScrollMethod, 8, &[0,1,0]).unwrap(), Some(DeviceConfigChange::ScrollMethod(Some(1))));
    assert_eq!(decode_change(ScrollMethod, 8, &[0,0,0]).unwrap(), Some(DeviceConfigChange::ScrollMethod(None)));
    assert_eq!(decode_change(AccelSpeed, 32, &0.25f32.to_le_bytes()).unwrap(), Some(DeviceConfigChange::AccelSpeed(0.25)));
    // Accel profile custom (index 2) → Invalid, we can't honor it
    assert!(decode_change(AccelProfile, 8, &[0,0,1]).is_err());
}
```

- [ ] **Step 2: Run, expect fail.**

- [ ] **Step 3: Implement `decode_change(binding, format, data) -> Result<Option<DeviceConfigChange>, DeviceConfigError>`.** Assumes value already `validate_value`-passed. One-hot decodes to active index; `OneHotOrNone` → `Option`; `AccelProfile` index 2 (custom) → `Err(Invalid)`; bitflags pack to a mask; Float reads 4 LE bytes → f32; Card32 reads 4 LE bytes.

- [ ] **Step 4: Run, expect pass.**

- [ ] **Step 5: Atom validity.** `AtomTable` already exposes `exists(AtomId) -> bool` and `name(AtomId) -> Option<…>` (`server.rs:67-97`, confirmed by codex). Use `state.atoms.exists(property)` directly for the BadAtom guard (no new helper needed); use `state.atoms.name(property)` for the descriptor reverse-lookup. No separate task step beyond using these in Steps 6-8.

- [ ] **Step 6: Wire the XI2 XIChangeProperty arm** (`process_request.rs` ~8675-8790). Currently it looks up the device, then calls `apply_change_property` (8755) and commits unconditionally. Restructure to the spec §D order. The handler has `state`, `client_id`, `deviceid`, `mode`, `format`, `property` (AtomId), `type_atom`, `data`. Replace the body from device-lookup onward with this logic (keep the existing BadDevice/lookup + error-encoding helpers the arm already uses):

```rust
// 1. Atom validity (BadAtom) — applies to all property requests.
if !state.atoms.exists(property) { /* send BadAtom, errorValue=property.0; return */ }
let dev_node = crate::xinput::find_device(&state.xi_devices, deviceid)
    .and_then(|d| d.device_node.clone());
let prop_name = state.atoms.name(property); // Option<&str> (AtomTable::name); borrow ends before find_device_mut
// 2. Known descriptor?
if let Some(name) = prop_name.as_deref()
    && let Some(desc) = crate::xinput::libinput_props::descriptor_by_name(name)
{
    if desc.access == crate::xinput::libinput_props::Access::ReadOnly {
        /* send BadAccess */ return Ok(());
    }
    // validate against kind (BadValue on failure)
    if crate::xinput::libinput_props::validate_value(desc.kind, format, &data).is_err() {
        /* send BadValue */ return Ok(());
    }
    if let Some(binding) = desc.binding {
        match crate::xinput::libinput_props::decode_change(binding, format, &data) {
            Err(_) => { /* send BadValue */ return Ok(()); }
            Ok(Some(change)) => {
                if let Some(node) = dev_node.as_deref() {
                    match backend.apply_device_config(node, change) {
                        Ok(()) => {}
                        Err(crate::xinput::libinput_props::DeviceConfigError::Unsupported) => { /* BadMatch */ return Ok(()); }
                        Err(crate::xinput::libinput_props::DeviceConfigError::Invalid) => { /* BadValue */ return Ok(()); }
                    }
                }
            }
            Ok(None) => {}
        }
    }
}
// 3. Commit to registry (now safe).
let device = crate::xinput::find_device_mut(&mut state.xi_devices, deviceid).expect("looked up above");
match crate::xinput::apply_change_property(device, mode, format, property, type_atom, &data) {
    Ok(()) => { /* T5: emit XI_PropertyEvent Created/Modified + XI1 DevicePropertyNotify */ }
    Err(crate::xinput::XiPropError::BadValue) => { /* BadValue */ }
    Err(crate::xinput::XiPropError::BadMatch) => { /* BadMatch */ }
    _ => unreachable!("apply_change_property never returns BadDevice; lookup handled above"),
}
```
Use the arm's existing error-emit pattern (grep how it currently sends BadValue/BadMatch — reuse it verbatim for BadAccess/BadAtom by swapping the error code constant; X error codes: BadValue=2, BadMatch=8, BadAccess=10, BadAtom=5). Note `backend` is in scope in `process_request` — confirm the surrounding fn signature carries `backend: &mut dyn Backend` (it does for input paths); if this specific dispatch fn doesn't, thread it (grep the fn signature; XI requests are dispatched from a fn that has backend access — the `probe`/`on_host_input` callers prove the dispatch loop holds `backend`).

- [ ] **Step 7: Mirror the same logic into the XI1 ChangeDeviceProperty arm** (~9656-9786). Same validate→apply→commit, same error codes, same event emission hook (T5/T6). The XI1 arm already does device-lookup + calls `apply_change_property` at 9729.

- [ ] **Step 8: Add BadAtom to XIGetProperty (8821), XIDeleteProperty (8790), XI1 GetDeviceProperty (9789).** At the top of each arm, after parsing `property`, `if !state.atoms.exists(property) { send BadAtom; return }`. (XIDeleteProperty also gets read-only check: deleting a libinput descriptor property is a client error — match xserver; if `descriptor_by_name(name).is_some()` treat as BadAccess only if the driver forbids it — keep simple: allow delete as today, but BadAtom-guard. Note this in the commit message.)

- [ ] **Step 9: Add byte-level dispatch tests** following the existing pattern at `process_request.rs:17606+` (`xi_seed_touchpad` then build an `xXIChangePropertyReq` body). New tests: (a) writing `"libinput Tapping Enabled"`=`[0]` returns no error and the registry shows `[0]`; (b) writing `"libinput Tapping Enabled Default"` (read-only) returns **BadAccess**; (c) `XIGetProperty` on an uninterned atom returns **BadAtom**; (d) writing `"libinput Scroll Method Enabled"`=`[1,1,0]` (two bits) returns **BadValue** and does NOT mutate. Use a stub backend whose `apply_device_config` returns `Ok` (the default trait impl via the test's recording backend).

- [ ] **Step 10: fmt + clippy + test; Commit** — `git commit -am "feat(xinput): Tier 2b T3 — writable routing validate→apply→commit + BadAtom/BadAccess/BadValue"`

---

## Task 4: KMS v2 libseat apply implementation

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (device map + `apply_device_config`)
- Modify: `crates/yserver/src/input/context.rs` (fill `LibinputConfigSnapshot`)

- [ ] **Step 1: Fill the snapshot in context.rs.** Find where `DeviceInfo` is constructed (grep `DeviceInfo {` in `context.rs` — the device-add path that currently sets `tap_enabled: …`). Build `LibinputConfigSnapshot` from the live `&input::Device` using the verified getters: `config_tap_is_available`/`config_tap_enabled`/`config_tap_default_enabled` → `tap`; `config_scroll_natural_scroll_enabled` (+`config_scroll_has_natural_scroll` for available) → `natural_scroll`; `config_dwt_*`; `config_left_handed_*`; `config_middle_emulation_*`; `config_accel_is_available`/`config_accel_speed`/`config_accel_default_speed` → `accel`; `config_scroll_methods()` (Vec→available_mask), `config_scroll_method()` (Option→current idx); `config_click_methods()`/`config_click_method()`; `config_send_events_modes()`/`config_send_events_mode()` (bitflags→masks); `config_accel_profiles()`/`config_accel_profile()` (map Adaptive→bit0, Flat→bit1, ignore Custom); `config_scroll_button*`; `config_scroll_button_lock` (ScrollButtonLockState→bool). Map libinput enums→bit indices with small helper fns. No new test here (live device; covered by HW smoke) — just `cargo build -p yserver`.

- [ ] **Step 2: Add the device map field** to the KMS v2 backend struct (grep the struct in `backend.rs`): `touchpad_devices: std::collections::HashMap<String, input::Device>,` init empty. In the libinput `DeviceAdded` dispatch (grep where `InputEvent::DeviceAdded`/`DeviceInfo` is produced in `on_libinput_ready`), insert the device handle keyed by node. **Codex note:** `input::Device` is an `ffi_ref_struct!` handle — verify whether it implements `Clone` (libinput refcount). If it does, `map.insert(node.clone(), device.clone())`. If not, obtain a fresh handle (the libinput event exposes `.device()`) and move it in, or store the handle the add-event already owns. On `DeviceRemoved`, `map.remove(&node)`.

- [ ] **Step 3: Implement `apply_device_config`** on the KMS v2 backend:

```rust
fn apply_device_config(&mut self, device_node: &str, change: DeviceConfigChange)
    -> Result<(), DeviceConfigError> {
    let Some(dev) = self.touchpad_devices.get_mut(device_node) else { return Ok(()) };
    use DeviceConfigChange as C;
    let status = match change {
        C::Tap(b) => dev.config_tap_set_enabled(b),
        C::TapDrag(b) => dev.config_tap_set_drag_enabled(b),
        C::TapDragLock(b) => dev.config_tap_set_drag_lock_enabled(
            if b { input::DragLockState::EnabledTimeout } else { input::DragLockState::Disabled }),
        C::TapButtonMap(i) => dev.config_tap_set_button_map(
            if i == 0 { input::TapButtonMap::LeftRightMiddle } else { input::TapButtonMap::LeftMiddleRight }),
        C::NaturalScroll(b) => dev.config_scroll_set_natural_scroll_enabled(b),
        C::Dwt(b) => dev.config_dwt_set_enabled(b),
        C::LeftHanded(b) => dev.config_left_handed_set(b),
        C::MiddleEmulation(b) => dev.config_middle_emulation_set_enabled(b),
        C::ScrollMethod(opt) => dev.config_scroll_set_method(match opt {
            None => input::ScrollMethod::NoScroll,
            Some(0) => input::ScrollMethod::TwoFinger,
            Some(1) => input::ScrollMethod::Edge,
            _ => input::ScrollMethod::OnButtonDown,
        }),
        C::ClickMethod(opt) => dev.config_click_set_method(match opt {
            None => input::ClickMethod::None_,           // confirm exact variant name in crate
            Some(0) => input::ClickMethod::ButtonAreas,
            _ => input::ClickMethod::Clickfinger,
        }),
        C::SendEvents(mask) => dev.config_send_events_set_mode(send_events_from_mask(mask)),
        C::AccelSpeed(s) => dev.config_accel_set_speed(f64::from(s)),
        C::AccelProfile(opt) => dev.config_accel_set_profile(match opt {
            Some(1) => input::AccelProfile::Flat,
            _ => input::AccelProfile::Adaptive,
        }),
        C::ScrollButton(b) => dev.config_scroll_set_button(b),
        C::ScrollButtonLock(b) => dev.config_scroll_set_button_lock(
            if b { input::ScrollButtonLockState::Enabled } else { input::ScrollButtonLockState::Disabled }),
    };
    match status {
        Ok(()) => Ok(()),
        // input crate maps STATUS_UNSUPPORTED→Unsupported, STATUS_INVALID→Invalid
        Err(input::DeviceConfigError::Unsupported) => Err(DeviceConfigError::Unsupported),
        Err(input::DeviceConfigError::Invalid) => Err(DeviceConfigError::Invalid),
    }
}
```
Confirm exact crate enum variant spellings (`ScrollMethod`, `ClickMethod`, `SendEventsMode`, `AccelProfile`, `DragLockState`, `TapButtonMap`, `ScrollButtonLockState`, `DeviceConfigError`) by reading `~/.cargo/registry/src/*/input-0.10.0/src/device.rs` while implementing — adjust names to match. `send_events_from_mask` ORs `SendEventsMode::DISABLED`/`DISABLED_ON_EXTERNAL_MOUSE` bitflags from the mask.

- [ ] **Step 4: Build** — `cargo build -p yserver`. Fix any variant-name mismatches against the crate.

- [ ] **Step 5: fmt + clippy + test (workspace)** — `cargo +nightly fmt && cargo clippy -- -W clippy::pedantic && cargo test`.

- [ ] **Step 6: Commit** — `git commit -am "feat(input): Tier 2b T4 — libseat apply_device_config + config snapshot gather"`

---

## Task 5: XI2 `XI_PropertyEvent` emission

**Files:**
- Modify: `crates/yserver-core/src/xinput/mod.rs` (add `PropChange` + `encode_xi2_property_event`)
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (emit at the change/delete/get-delete sites)

- [ ] **Step 1: Add `PropChange` return + write a failing encoder test.**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropWhat { Deleted = 0, Created = 1, Modified = 2 }

#[test]
fn xi2_property_event_bytes() {
    let ev = encode_xi2_property_event(
        ClientByteOrder::LSBFirst, SequenceNumber(7), /*xi_major*/131,
        /*deviceid*/4, /*time*/1000, AtomId(0x123), PropWhat::Modified);
    assert_eq!(ev.len(), 32);
    assert_eq!(ev[0], 35);          // GenericEvent
    assert_eq!(ev[1], 131);         // extension (xi major)
    assert_eq!(u16::from_le_bytes([ev[8], ev[9]]), 12); // evtype
    assert_eq!(u16::from_le_bytes([ev[10], ev[11]]), 4); // deviceid
    assert_eq!(u32::from_le_bytes([ev[16],ev[17],ev[18],ev[19]]), 0x123); // property
    assert_eq!(ev[20], 2);          // what=Modified
}
```

- [ ] **Step 2: Run, expect fail.**

- [ ] **Step 3: Implement `encode_xi2_property_event`** producing exactly the verified `xXIPropertyEvent` 32-byte layout (length field = 0; bytes 21-31 padding). Make `apply_change_property` return `Result<PropWhat, XiPropError>` (Created when the property was absent before the write, else Modified) and `apply_delete_property` return `Option<PropWhat>` (`Some(Deleted)` when it removed something) — update their existing call sites/tests accordingly.

- [ ] **Step 4: Run encoder test + the updated apply tests, expect pass.**

- [ ] **Step 5: Emit at the dispatch sites — CORRECT delivery model (codex plan-review).** Xorg's `send_property_event` (`../xserver/Xi/xiproperty.c:189-208`) delivers via `SendEventToAllWindows(dev, filter, …)` — i.e. to **every client that selected `XI_PropertyEvent` for this device on ANY window**, NOT through the root-only `XI_DeviceChanged` helper (`fanout.rs:251` `emit_xi2_device_changed_slave_pointer` is device-changed-specific — **do not reuse it**). Because `xXIPropertyEvent` has **no event-window field**, per-window distinction only affects *which clients* receive it; deliver each matching client once (dedup by client id). Write a fresh collector:

```rust
// Collect distinct clients with an XI_PropertyEvent selection for `deviceid`
// on any window. xi2_masks: HashMap<(ResourceId /*window*/, u16 /*dev*/), u32 /*mask*/>.
let targets: Vec<ClientId> = state.clients.iter()
    .filter(|(_, c)| c.xi2_masks.iter()
        .any(|(&(_, dev), &mask)| dev == deviceid && (mask & (1 << 12)) != 0))
    .map(|(id, _)| ClientId(*id))
    .collect();
fanout_event_to_clients(state, &targets, |buf, seq, order| {
    buf.extend_from_slice(&encode_xi2_property_event(order, seq, xi_major, deviceid, time, property, what));
});
```
Call this after a committed change/delete AND at the `XIGetProperty(delete=1)` removal (xinput.rs:518 — thread the emit out to the dispatch arm, since `compute_get_property` is pure). `xi_major` = advertised XI major (grep `XI2_MAJOR_OPCODE`, =131/137 per the constant). `time` = the server time helper other events use (grep `server_time`/`current_time`). Factor the collector + both encoders into one `emit_property_change(state, deviceid, property, what)` helper so T6 can add the XI1 emit beside it.

- [ ] **Step 6: Add a dispatch test** that selects `(1<<12)` for device 4 on a window (set `xi2_masks`), writes `"libinput Tapping Enabled"`, and asserts the client received a 32-byte event with evtype 12 and what=Modified (follow the event-assertion pattern used by existing XI2 tests).

- [ ] **Step 7: fmt + clippy + test; Commit** — `git commit -am "feat(xinput): Tier 2b T5 — emit XI2 XI_PropertyEvent on property change/delete"`

---

## Task 6: XI1 `DevicePropertyNotify` + `SelectExtensionEvent` (riskiest)

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (XI1 selection store + `xi1_first_event`)
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (`SelectExtensionEvent` handler + emit)
- Modify: `crates/yserver-core/src/xinput/mod.rs` (`encode_xi1_device_property_notify`)

- [ ] **Step 1: DISCOVERY — confirm current XI1 state (codex plan-review already partially answered).** `SelectExtensionEvent` (XInput minor 6) does NOT exist: the XI1 dispatch jumps from minor 2 straight to 36 and only handles 36-39 (`process_request.rs:9656-9799`) — **this plumbing must be ADDED, not "follow an existing helper."** The `first_event` base is in `crates/yserver-core/src/nested.rs` (the `EXTENSIONS` table / `XI2_FIRST_EVENT` constant, ~line 41/165-168; `process_request.rs:10913-10933` already derives query replies from it — codex r2). Use that constant for the event `type` byte (`first_event + 16`) and the class encoding; confirm its concrete value while implementing.

- [ ] **Step 2: Add the selection store.** On the per-client struct in `server.rs` (where `xi2_masks` lives, ~1128) add `pub xi1_event_classes: HashSet<u32>,` (set of selected `XEventClass` values) and init `HashSet::new()` at every client-construction site (the same ~30 sites that init `xi2_masks` — use an editor multi-replace; the compiler lists any missed). Add a constant `pub const XI_DEVICE_PROPERTY_NOTIFY_OFFSET: u8 = 16;`.

- [ ] **Step 3: Write a failing XI1 event-encoder test.**

```rust
#[test]
fn xi1_device_property_notify_bytes() {
    let ev = encode_xi1_device_property_notify(
        ClientByteOrder::LSBFirst, SequenceNumber(3),
        /*first_event*/ 66, /*deviceid*/ 4, /*time*/ 50, AtomId(0x77), /*deleted*/ false);
    assert_eq!(ev.len(), 32);
    assert_eq!(ev[0], 66 + 16);  // type = first_event + XI_DevicePropertyNotify
    assert_eq!(ev[1], 0);        // state = PropertyNewValue
    assert_eq!(u32::from_le_bytes([ev[8],ev[9],ev[10],ev[11]]), 0x77); // atom
    assert_eq!(ev[31], 4);       // deviceid (last byte)
}
```
(Confirm `first_event` real value in Step 1; the 66 here is illustrative for the test's own input.)

- [ ] **Step 4: Run, expect fail; implement `encode_xi1_device_property_notify`** per the verified `devicePropertyNotify` 32-byte layout (no window field; `state` 0=NewValue/1=Deleted; `deviceid` is the LAST byte). Run, expect pass.

- [ ] **Step 5: Implement the `SelectExtensionEvent` handler** (XInput major, minor 6). Body: `count` event classes follow; for each `XEventClass` (u32), if its low byte == `first_event + 16` (a DevicePropertyNotify class), insert into the client's `xi1_event_classes` (store the class verbatim). Ignore other classes (out of scope) — but do NOT error on them (a client may select several; we just don't deliver the ones we don't implement). Parse per `xSelectExtensionEventReq` (grep XIproto.h for the request layout: window + count + class list).

- [ ] **Step 6: Emit XI1 DevicePropertyNotify** inside the T5 `emit_property_change` helper, beside the XI2 emit. Collect distinct clients whose `xi1_event_classes` contains `(deviceid << 8) | (first_event + 16)`:

```rust
let class = (u32::from(deviceid) << 8) | u32::from(first_event + 16);
let xi1_targets: Vec<ClientId> = state.clients.iter()
    .filter(|(_, c)| c.xi1_event_classes.contains(&class))
    .map(|(id, _)| ClientId(*id))
    .collect();
fanout_event_to_clients(state, &xi1_targets, |buf, seq, order| {
    buf.extend_from_slice(&encode_xi1_device_property_notify(
        order, seq, first_event, deviceid, time, property, /*deleted=*/ what == PropWhat::Deleted));
});
```
`state` field = NewValue (0) for Created/Modified, Deleted (1) for delete. The two emits (XI2 + XI1) are independent — a client may have selected via either path.

- [ ] **Step 7: Add tests** — (a) `SelectExtensionEvent` with a DevicePropertyNotify class for device 4 populates `xi1_event_classes`; (b) after selecting, a property write delivers a 32-byte XI1 event with the right `type` byte and `deviceid` in the last byte.

- [ ] **Step 8: fmt + clippy + test; Commit** — `git commit -am "feat(xinput): Tier 2b T6 — XI1 SelectExtensionEvent + DevicePropertyNotify"`

---

## Task 7: scroll-flags FINDING (verify-then-fix)

**Files:**
- Modify (maybe): `crates/yserver-core/src/core_loop/process_request.rs` (`emit_xi2_device_changed_bootstrap` ~8319) and/or `crates/yserver-protocol/src/x11/mod.rs` (`encode_xi2_device_changed_event`)

- [ ] **Step 1: Verify the discrepancy.** Read `emit_xi2_device_changed_bootstrap` and the `XIQueryDevice` scroll-class encoder. Compare the scroll-class `flags` field each emits. Cross-check against `../xserver/Xi/xichangehierarchy.c` / `xiquerydevice.c` for what `flags` (XIScrollFlagNoEmulation=1) should be in each context. Document the finding.

- [ ] **Step 2: If they disagree without justification,** make them consistent (match `XIQueryDevice`'s value, which clients read first). Add/adjust a byte-level test asserting both encoders emit the same scroll `flags`.

- [ ] **Step 3: If they legitimately differ,** add a code comment citing the xserver source explaining why, and a test pinning the intended values. (No behavior change.)

- [ ] **Step 4: fmt + clippy + test; Commit** — `git commit -am "fix(xinput): Tier 2b T7 — reconcile XI_DeviceChanged scroll flags with XIQueryDevice"`

> **HW smoke before any merge** (per `feedback_no_commit_before_smoke` is about render/KMS; here the gate is functional): on M2/Asahi + yoga, toggle tap-to-click / natural-scroll / click-method in MATE, GNOME, and XFCE settings and confirm behavior changes; `xinput set-prop` round-trips; an `xinput --watch-props`-style client sees the property events. T7's flags change in particular wants HW confirmation (possible Chrome XI crash-class per memory).

---

## Self-review notes (author)

- **Spec coverage:** writable set → T2/T3/T4; events XI2 → T5; events XI1 + SelectExtensionEvent → T6; BadAtom → T3; read-only BadAccess → T3; typed error mapping → T3/T4; FLOAT atom → T1; descriptor table → T2; scroll-flags → T7. All spec sections mapped.
- **Type consistency:** `DeviceConfigChange`/`DeviceConfigError` defined T1 (placeholder) → finalized T2/T3, used T3/T4. `validate_value`/`decode_change` defined T2/T3, used T3. `PropWhat` defined T5, returned by reworked `apply_change_property`/`apply_delete_property` (T5) and consumed at emit sites T5/T6. `xi2_masks` bit is `(1<<12)`; `XiDevice.device_node` added T2 used T3/T5.
- **Known discovery risks (flagged, not placeholders):** XI1 `first_event` base (T6 Step 1) and `input::Device: Clone?` (T4 Step 2) — read from real source while implementing.

### Codex plan review (round 1)
Verdict "not ready as written" → 2 blockers fixed:
1. **Incomplete `DeviceInfo` caller sweep** → T1 Step 4 now enumerates every site (run.rs:1531, process_request.rs:17229/17357/17783, xinput.rs:772, message.rs, context.rs).
2. **Wrong XI2 event-delivery model** (had reused the root-only `XI_DeviceChanged` helper) → T5 Step 5 rewritten to the Xorg `send_property_event` model (deliver to every client with an `XI_PropertyEvent` selection for the device on any window; `xXIPropertyEvent` has no window field so dedup by client). T6 emit aligned.
Non-blocking confirmations folded in: `AtomTable::exists()`/`name()` used for BadAtom (no new helper); `input` 0.10 enum/setter names verified correct; `SelectExtensionEvent` (minor 6) confirmed absent → T6 builds it. Re-review pending.
