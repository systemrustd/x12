# Touchpad Tier 2b — writable properties + property-change events (design)

**Status:** Revision 3 (final), 2026-06-02. Brainstormed interactively; codex review **converged over 3 rounds — verdict "ready to implement."** R1: 4 blockers (XI event numbers/structs, apply-before-commit ordering, multi-value semantics, AccelProfile::Custom feature gap). R2: those fixed, +2 residual blockers (stale 26/82 summary; `Result<(),()>` couldn't carry BadMatch/BadValue). R3: blockers cleared; only 2 cosmetic nits (stale §F hedge, runtime-assigned major opcodes) — both cleaned up. Awaiting user sign-off before `writing-plans`.
**Branch (planned):** `feat/touch-input` (continues from Tier 2a, commits `74f9010`..`42e3afa`).
**Related:**
- `docs/superpowers/specs/2026-06-02-touchpad-xi2-properties-design.md` — **Tier 2a** (read-only discovery, shipped + HW-verified on M2/Asahi under MATE & XFCE). This is **Tier 2b**: make the discovered properties *take effect* and *notify*.
- `docs/superpowers/findings/2026-06-01-touch-input-gap.md` — the three-tier framing.
- `/usr/include/xorg/libinput-properties.h` — **authoritative** property names + value layouts (the driver the Linux desktop stack is written against). Source of truth for the descriptor table; do not invent names/formats.
- `crates/yserver-core/src/xinput.rs` — `XiDevice` registry, `apply_change_property`/`apply_delete_property`/`compute_get_property` (the 3 `TODO(Tier 2b)` mutation seams at ~519/594/607).
- `crates/yserver-core/src/core_loop/message.rs` — `DeviceInfo` (input→core snapshot), `HostInputEvent`.
- `crates/yserver/src/input/context.rs` — libinput device lifecycle; `configure_touchpad` already calls `config_tap_set_enabled` etc. (the live-device config surface).
- `crates/yserver-core/src/backend/trait_def.rs` — `on_host_input`, `on_libinput_ready` (libseat mode), `probe_input_devices`.
- `crates/yserver/src/kms/v2/backend.rs` — libseat-mode libinput owner (the live `Device` handles).
- `crates/yserver-core/src/core_loop/fanout.rs` — XI2 per-`(window,device)` mask store + `fanout_event_to_clients` (reused for `XI_PropertyEvent`).
- Reference: `xf86-input-libinput` (`src/libinput.c`: `LibinputSetProperty`, the per-property `LibinputSetPropertyXXX` handlers, availability gating) and `../xserver/Xi/xiproperty.c` (`ProcXIGetProperty` `ValidAtom`/`BadAtom`, property-event emission) / `XIproto.h` + `XI2proto.h` (event wire layout).

## Goal

A touchpad setting toggled in the desktop UI (tap-to-click, natural scrolling, click method, accel speed, …) **actually changes device behavior** on yserver, and clients that subscribe to property changes are **notified** — matching what `xf86-input-libinput` + Xorg deliver. Tier 2a made the touchpad *discoverable*; Tier 2b makes it *configurable* end-to-end.

## Scope

**In:**
- **Writable touchpad/pointer libinput properties** routed to live libinput config. The set, taken from `libinput-properties.h`, gated per-device on libinput availability:
  - `Tapping Enabled`, `Tapping Drag Enabled`, `Tapping Drag Lock Enabled`, `Tapping Button Mapping Enabled` (2-val LRM/LMR)
  - `Natural Scrolling Enabled`, `Disable While Typing Enabled`, `Left Handed Enabled`, `Middle Emulation Enabled`
  - `Scroll Method Enabled` (3-val 2fg/edge/button), `Click Method Enabled` (2-val areas/clickfinger), `Send Events Mode Enabled` (2-val)
  - `Accel Speed` (FLOAT, 1), `Accel Profile Enabled` (BOOL, **2-val adaptive/flat**)
  - `Button Scrolling Button` (CARD32), `Scroll Button Lock Enabled` (BOOL)
  - Plus the read-only `…Default` / `…Available` companions (seeded, write-rejected).
  - Each writable descriptor maps to a **known libinput config setter**, all confirmed present in the `input` 0.10 crate (codex round 1): `config_tap_set_enabled`/`set_drag_enabled`/`set_drag_lock_enabled`/`set_button_map`, `config_scroll_set_natural_scroll_enabled`/`set_method`/`set_button`/`set_button_lock`, `config_dwt_set_enabled`, `config_left_handed_set`, `config_middle_emulation_set_enabled`, `config_click_set_method`, `config_send_events_set_mode`, `config_accel_set_speed`/`set_profile`.
  - **`Accel Profile Enabled` is wire-exposed as 3 values (adaptive/flat/custom)** to match xf86-input-libinput, but **only adaptive/flat are honored on write** — the `custom` profile (`AccelProfile::Custom`) requires the `input` crate's `libinput_1_23` feature and the workspace is pinned to default features (`libinput_1_21`). A write attempting to set the `custom` bit → **BadValue** (we cannot honor it); seeding leaves the custom slot read as 0. (Alternative if `custom` is later wanted: bump the `input` feature in `Cargo.toml` — out of scope here.)
  - **`config_tap_set_drag_lock_enabled` takes a `DragLockState` enum in the crate, not a `bool`** — the `Tapping Drag Lock Enabled` BOOL maps 0→`Disabled`, 1→`EnabledTimeout` (the libinput default lock mode). Made explicit so T4 doesn't assume a bool setter.
- **Property-change events:** XI2 `XI_PropertyEvent` (`evtype = 12`) **and** XI1 `DevicePropertyNotify` (type `first_event + 16`), emitted at create/modify/delete. (Exact layouts in §G.)
- **`XIGetProperty` `BadAtom` validation** (closes the documented Tier 2a gap).
- **Scroll-flags FINDING** verify-then-fix (see §8).

**Out (YAGNI / other tiers):**
- Tablet-tool / tablet-pad properties (pressure curve, pad mode groups, tool serial/ID) and touchscreen `Calibration Matrix` — belong to a tablet/Tier 3 effort.
- The custom accel-profile point arrays (`Accel Custom *`) — exotic, variable-length; defer.
- **Driver-level-only props with no libinput config setter:** `Horizontal Scroll Enabled`, `High Resolution Wheel Scroll Enabled`, `Scrolling Pixel Distance`. In xf86-input-libinput these are driver-internal event-discard/scaling flags, not libinput `config_*` settings, and yserver has no equivalent behavior yet. Excluded to avoid implying a libinput API that doesn't exist; revisit if/when yserver grows the corresponding scroll-handling.
- `Rotation Angle` — pointer/tablet-oriented, not a touchpad setting; defer.
- **Direct/input-thread mode** config application. Direct mode does not own its libinput on the core thread; it is also slated for removal (yserver targets a modern, libseat-only input path). The apply path is implemented for **libseat mode** only; Direct mode gets a documented no-op behind the same Backend hook.

## Architecture

### A. Data-driven descriptor table (single source of truth)

New module `crates/yserver-core/src/xinput/libinput_props.rs`. One static table; every other concern (seeding, read-only enforcement, encode/decode, the writable router) reads it:

```rust
enum XiValType { Bool, Int8, Card32, Float }   // X atom: Bool/Int8/Int32→INTEGER, Card32→CARDINAL, Float→"FLOAT"
enum Access { ReadWrite, ReadOnly }            // …Default / …Available are ReadOnly

// Per-property value semantics — NOT a single generic "decode bitmap → index"
// rule (codex round 1). Each kind has its own decode + validation:
enum ValueKind {
    Scalar,                 // n=1: Bool / Card32 / Float (Accel Speed, Tapping Enabled, Scroll Button…)
    OneHot { n: u8 },       // exactly one of n bits set (Tapping/Clickfinger Button Mapping: LRM|LMR)
    OneHotOrNone { n: u8 }, // at most one of n bits set; all-zero = "none/disabled"
                            //   (Scroll Method 2fg/edge/button, Click Method areas/clickfinger,
                            //    Accel Profile adaptive/flat[/custom-rejected])
    BitFlags { n: u8 },     // any combination of n bits (Send Events Mode: disabled |
                            //   disabled-on-external-mouse — libinput SendEventsMode is a true bitflag)
}

struct PropDescriptor {
    name: &'static str,        // exactly the libinput-properties.h string
    val: XiValType,
    format: u8,                // 8 (Bool/Int8) | 32 (Card32/Float)
    kind: ValueKind,           // drives decode + write validation (rejects malformed → BadValue)
    access: Access,
    binding: Option<Binding>,  // None for read-only companions; Some(setter+getter+available) for live config
}
```

Write validation is per-kind: `OneHot` rejects (BadValue) anything but exactly-one-bit; `OneHotOrNone` rejects >1 bit; `BitFlags` accepts any subset; `Scalar` Float/Card32 range-checks where libinput defines a range. This is the "checkonly" equivalent and must run **before** the libinput apply (see §D/§E).
```

- **`val`/`format`/`n`** come verbatim from `libinput-properties.h` comments (e.g. `Accel Speed` = FLOAT/32/1; `Scroll Method Enabled` = BOOL/8/3).
- **`Float`** requires interning an atom literally named `"FLOAT"` at server init (xf86-input-libinput does this; 32-bit IEEE-754 stored in a format-32 property). Added alongside the existing MOUSE/KEYBOARD/TOUCHPAD interns.
- **`binding`** identifies the libinput config group so seeding can read current+default+available and the router can apply. Bindings are an enum, not function pointers (keeps the table `const` and `Send`-free; the backend matches on it).

### B. `DeviceInfo` config snapshot (input → core)

`DeviceInfo` (message.rs) today carries only tap/natural-scroll/dwt. Replace the 6 ad-hoc fields with a structured `LibinputConfigSnapshot` carrying, per in-scope config group: **available?**, **current**, **default** (plain `Copy` data, `Send`-safe). Filled in `context.rs` from the live device via the `config_*_is_available` / `config_*_get_*` / `config_*_get_default_*` libinput calls. This is the only place that touches a live libinput handle for *reading*.

### C. Seeding (availability-gated)

On `HostInputEvent::DeviceAdded`, `xinput.rs` iterates the descriptor table: for each descriptor whose **availability predicate** holds in the snapshot, insert the property at its current value and insert its read-only `…Default`/`…Available` companion. Mirrors xf86-input-libinput, which only creates a property when libinput reports the config available. `XiDevice` gains `device_node: Option<String>` (recorded at seed time) so a later write knows which physical device to apply to.

### D. Writable routing → libinput

The XI1 `ChangeDeviceProperty` (minor 37) and XI2 `XIChangeProperty` (minor 57) dispatch paths call into the property machinery. (Major opcodes are runtime-assigned at extension-query time — yserver advertises XInput major 131 and XI2 major 137; only the minor opcodes are fixed by the protocol.) **Ordering is load-bearing (codex round 1): validate + apply to libinput BEFORE committing to the registry**, mirroring Xorg's `XIChangeDeviceProperty` two-pass (`checkonly` validate, then commit) where a handler error fails the request and nothing is committed. A panel must never see "success" while the device is unchanged. New flow for a known descriptor:

1. Look up the descriptor by atom. If **ReadOnly** → **BadAccess**, no mutation (see §F). Unknown-but-interned atom → generic registry write as today (no libinput effect). Uninterned atom → **BadAtom**.
2. **Validate** the incoming bytes against the descriptor `kind` (`OneHot`/`OneHotOrNone`/`BitFlags`/`Scalar` range). Malformed → **BadValue**, no mutation. For `Accel Profile`, a set of the `custom` bit → **BadValue**.
3. **Decode** to a typed `DeviceConfigChange` (e.g. `TapEnabled(bool)`, `AccelSpeed(f64)`, `ScrollMethod(Option<Method>)`, `SendEventsMode(SendEventsModeBits)`, `TapDragLock(DragLockState)`).
4. Call `backend.apply_device_config(device_node, change)`. **If it returns `Err` → fail the X request with the mapped code (`Unsupported`→BadMatch, `Invalid`→BadValue, §E), do NOT commit the registry.**
5. Only on `Ok` → commit the value into `XiDevice.properties` and emit the property-change events (§G).

`apply_change_property` is therefore split: a pure validate/decode step (in `libinput_props.rs`, unit-tested) and the commit, with the backend apply gating the commit. `DeviceConfigChange` is a typed enum (one variant per libinput setter). For a writable property with **no live device** (e.g. nested/host-X11 backend whose hook is the no-op `Ok`), the commit proceeds — the registry still reflects the client's write even where there's nothing to apply to.

### E. Backend hook + libseat apply

Backend trait gains a hook returning a **typed error** so the dispatch layer maps to the correct X error (codex round 2 — `Result<(),()>` couldn't carry BadMatch vs BadValue):
```rust
enum DeviceConfigError {
    Unsupported,  // device/capability mismatch (libinput STATUS_UNSUPPORTED) → BadMatch
    Invalid,      // bad value for this device   (libinput STATUS_INVALID)     → BadValue
}
// Ok  => applied (or no live device to apply to — nested/host-X11 no-op).
// Err => fail the X request with the mapped error; do NOT commit the registry (§D step 4).
fn apply_device_config(&mut self, _device_node: &str, _change: DeviceConfigChange)
    -> Result<(), DeviceConfigError> { Ok(()) /* no-op default */ }
```
The dispatch layer maps `Unsupported → BadMatch`, `Invalid → BadValue`. (Read-only writes never reach the hook — §D step 1 returns BadAccess first.)
- **KMS v2 (libseat)**: retains `HashMap<String /*device_node*/, libinput::Device>` populated in the libinput dispatch (`DeviceAdded`), removed on `DeviceRemoved`. The hook looks up the live `Device`, calls the matching `config_*_set_*`, and maps the `input` crate's `DeviceConfigError`/`ConfigStatus` (`Unsupported`/`Invalid`) onto the enum above. Same thread — no channel. If the device_node isn't in the map (already removed) → `Ok` (nothing to apply; the registry write stands).
- **Direct mode / host-X11 / nested**: default no-op `Ok`. Documented TODO; Direct mode is slated for removal.

Threading is fully isolated inside the backend impl — the core/dispatch layer is mode-agnostic.

### F. Read-only enforcement + `BadAtom`

Canonical error codes (applied in §D's validate-before-commit order):
- ReadOnly descriptor write → **BadAccess**, no mutation. (Settled: BadAccess is the X convention for read-only device properties and matches `xf86-input-libinput` `LibinputSetProperty`.)
- Malformed value for the descriptor `kind` (wrong bit-count, out-of-range, `Accel Profile` custom bit) → **BadValue**, no mutation.
- libinput rejects the applied value (`apply_device_config` → `Err`) → **BadMatch** for `Unsupported` (device/capability mismatch), **BadValue** for `Invalid` (bad value), no mutation. (§E typed-error mapping.)
- Uninterned property atom on `XIGetProperty`/`XIChangeProperty`/`XIDeleteProperty` → **BadAtom** with `errorValue = property`. Add the missing `ValidAtom(property)` check (matches `ProcXIGetProperty`; today an unknown atom is silently treated as absent).

### G. Property-change events

Emitted from the 3 mutation seams. The validate/commit path returns a `PropChange { property, what }` (`what` ∈ {Created, Modified, Deleted}); the dispatch layer turns that into events. **Event numbers/structs verified against the installed headers (codex round 1 corrected my earlier draft).**

- **XI2 `XI_PropertyEvent`** — **`evtype = 12`** (`XI2.h`; **not** 26, which is `XI_BarrierLeave`). GenericEvent. Exact layout from `XI2proto.h` `xXIPropertyEvent` (32 bytes): `type`=GenericEvent(35), `extension`=XI major, `sequenceNumber`, `length`=0, `evtype`=12, `deviceid` (u16), `time`, `property` (Atom u32), `what` (u8: `XIPropertyDeleted=0`/`Created=1`/`Modified=2`), `pad0` u8, `pad1` u16, `pad2` u32, `pad3` u32. Delivered via the existing `fanout_event_to_clients` to clients that selected the **`(1 << 12)`** mask bit (`XI_PropertyEventMask`) for that device on a window. Infra exists (Tier 2a uses it for `XI_DeviceChanged`).
- **XI1 `DevicePropertyNotify`** — type = `first_event + XI_DevicePropertyNotify`, where **`XI_DevicePropertyNotify = 16`** (`XIproto.h`; **not** 82). Exact 32-byte layout from `XIproto.h` `devicePropertyNotify`: `type` (BYTE, base+16), `state` (BYTE: `PropertyNewValue=0`/`PropertyDelete=1` from `X.h`), `sequenceNumber` (CARD16), `time` (CARD32), `atom` (Atom), `pad0..pad2` (CARD32×3), `pad3` (CARD32), `pad5` (CARD16), `pad4` (CARD8), `deviceid` (CARD8). **There is NO `window` field** on the wire (the Xlib `XDevicePropertyNotifyEvent` has an *unused* `window` member, but it is not in the protocol struct). Note `state` has only NewValue/Deleted — a Created and a Modified both map to `NewValue=0`; a Deleted maps to `PropertyDelete=1`.
- **XI1 delivery / selection (discovery item).** XI1 device-event delivery requires client class-selection via `SelectExtensionEvent` (XInput minor `X_SelectExtensionEvent = 6`) registering the `DevicePropertyNotify` event class for the device. Grep found no XI1 `SelectExtensionEvent` infra. **Per user decision, b.2 includes building the minimal XI1 event-class selection plumbing** (only the `DevicePropertyNotify` class; broader XI1 classes stay out). T6 first confirms what exists, then builds the gap; the event class value is `(deviceid << 8) | (first_event + XI_DevicePropertyNotify)` — confirmed correct by codex r2 against `../xserver` `Xi/selectev.c` + `grabdev.c` (Xorg defines `XEventClass = (deviceid << 8 | eventtype)`).

## Non-goals (restate)

Custom accel point arrays; tablet/pad/tablet-tool props; touchscreen calibration; Direct-mode config apply; multi-touchpad distinct-device topology (one touchpad assumed, collapsed onto slave pointer id 4 as in Tier 2a).

## Testing

Pure, byte-level unit tests in the existing `xinput.rs`/`libinput_props.rs` style (no live libinput):
- Descriptor encode/decode roundtrips per `XiValType` incl. FLOAT (IEEE-754 format-32) and the 2-/3-value bitmaps ("only one set" invariant).
- `change → DeviceConfigChange` mapping for every ReadWrite descriptor.
- Read-only write → BadAccess; unknown-but-valid atom → generic write; `XIGetProperty` uninterned atom → BadAtom.
- XI2 `XI_PropertyEvent` and XI1 `DevicePropertyNotify` **byte-for-byte** vs XI2proto.h/XIproto.h, both byte orders, for Created/Modified/Deleted.
- Seeding: availability predicate gates presence; companions are ReadOnly.

Live-device apply (`apply_device_config`) and event delivery to real clients are **HW-smoke only** (no fixture) — verify on M2/Asahi + yoga: toggle each setting in MATE/GNOME/XFCE and confirm behavior changes; `xinput set-prop` round-trips; a property watcher sees the events.

## Task breakdown (subagent-driven, each spec+code reviewed like Tier 2a)

- **T1** — `DeviceInfo` → `LibinputConfigSnapshot` expansion; `context.rs` gathers it from the live device; Backend `apply_device_config` hook signature + no-op default; `DeviceConfigChange` enum skeleton. Intern the `"FLOAT"` atom at init.
- **T2** — `libinput_props.rs` descriptor table + value encode/decode (incl. FLOAT, bitmaps); seeding from the snapshot (availability-gated, read-only companions); `XiDevice.device_node`.
- **T3** — Writable routing in the validate-before-commit order (§D): descriptor lookup, read-only **BadAccess**, per-`kind` validate → **BadValue**, decode → `DeviceConfigChange`, call the hook, fail the X request on `Err` (no commit), commit + emit on `Ok`; plus `XIGetProperty`/change/delete **BadAtom** (`ValidAtom`) validation.
- **T4** — KMS v2 libseat apply impl: `device_node → libinput::Device` map (add/remove lifecycle) + `config_*_set_*` per `DeviceConfigChange`.
- **T5** — XI2 `XI_PropertyEvent` encoder + emission at the 3 seams + delivery via the existing mask/fanout.
- **T6** — XI1 `DevicePropertyNotify`: discover XI1 event-class selection state; build the minimal `SelectExtensionEvent` + `DevicePropertyNotifyClass` plumbing if absent; encoder + delivery.
- **T7** — Scroll-flags FINDING: verify `emit_xi2_device_changed_bootstrap` scroll `flags=1` vs `XIQueryDevice` `flags=0` against xserver; fix to consistent value. HW-confirm before changing (possible Chrome XI crash-class per memory).

## Open risks

1. **XI1 selection plumbing size (T6)** — if `SelectExtensionEvent` is wholly absent, T6 grows. Mitigated by scoping to only the `DevicePropertyNotify` class.
2. **Read-only error code (§F)** — BadAccess vs BadMatch; resolve from `xf86-input-libinput` source at impl time, not by guessing.
3. **Multi-device apply target** — all pointers collapse onto slave id 4; a write applies to the single recorded touchpad `device_node`. Multiple touchpads → only the seeded one is configured (documented limitation).
4. **`DeviceInfo` snapshot completeness** — every in-scope config group must be read in `context.rs`; a missing `is_available` read silently drops a property. T1 review must check the snapshot against the descriptor table.
5. **`input` crate binding coverage** — RESOLVED (codex round 1): all in-scope writable setters exist in `input` 0.10 default features. Two binding quirks captured in scope above: `Accel Profile` `custom` needs `libinput_1_23` (dropped from honored writes), and `set_drag_lock_enabled` takes `DragLockState` not `bool`.
6. **Error codes** — RESOLVED (codex r2): read-only → BadAccess, capability/device mismatch → BadMatch (`Unsupported`), bad value → BadValue (`Invalid`), uninterned atom → BadAtom. Matches Xorg/xf86-input-libinput.
7. **XI1 event-class encoding** — RESOLVED (codex r2): `(deviceid<<8)|type` confirmed against `../xserver` `Xi/selectev.c`. The remaining risk is only the *amount* of `SelectExtensionEvent` plumbing to build (T6), not its correctness — T6 is the riskiest/largest task.
8. **Event ordering vs validate-before-commit** — T5/T6 emit events only on the `Ok`/commit path (§D step 5); never for a write that later fails. T5/T6 depend on T3's boundary.
