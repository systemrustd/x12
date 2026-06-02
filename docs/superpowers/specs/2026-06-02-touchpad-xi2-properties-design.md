# Touchpad XI2 property discovery design (Tier 2)

**Status:** Ready to implement, 2026-06-02 (revision 3 — codex review converged over 2 rounds; verdict "implementable as written"). Round 1 resolved the classification signal (GDK keys on `libinput Tapping Enabled` *presence*, not a name heuristic; gsd = udev `ID_INPUT_TOUCHPAD` + `Device Node` pairing), demoted device name to secondary, made XI1/XI2 enumeration consistency a hard requirement, added property-change events + exact XIGetProperty semantics + FLOAT encoding, and tiered the seed property set R/U/O. Round 2 closed the last nit: `XIGetProperty(delete=1)` must also emit the property-deletion notification on the delete path (parity with `ProcXIGetProperty`).
**Branch (planned):** `feat/touch-input` (continues from the Tier 1 commit `4771d5f`).
**Related:**
- `docs/superpowers/findings/2026-06-01-touch-input-gap.md` — the three-tier framing. This is **Tier 2**.
- Tier 1 (shipped, `4771d5f`): `configure_touchpad` enables tap-to-click + DWT at device-add. That makes the touchpad *usable* but still invisible to the desktop's settings UI. Tier 2 makes it *discoverable/configurable*.
- `crates/yserver/src/input/context.rs` — libinput device lifecycle (`DeviceEvent::Added/Removed`, `dispatch()`), where classification currently dies.
- `crates/yserver/src/input/event.rs` — the `InputEvent` enum (input-thread → backend).
- `crates/yserver/src/kms/v2/backend.rs` — `on_libinput_ready` (translates `InputEvent` → `HostInputEvent`, sends via `CoreSender`).
- `crates/yserver-core/src/core_loop/process_request.rs` — XIQueryDevice (opcode 48, ~8356), XIGetProperty stub (59, ~8662); XIListProperties/XIChangeProperty/XIDeleteProperty (56/57/58) **not handled**.
- `crates/yserver-protocol/src/x11/mod.rs` — `encode_xi2_device_changed_event` (~2291).
- Reference: `xf86-input-libinput` (the driver the Linux desktop stack is written against) for the canonical property names/formats; `xserver` Xi/ (`xiproperty.c`, `xiquerydevice.c`) at `../xserver` for protocol layout.

## Goal

Make the laptop touchpad **discoverable and configurable as a touchpad** by an X11 desktop (Cinnamon/GNOME settings, GDK gesture support) running on yserver: the device shows up with a touchpad identity and the `libinput *` properties those tools read, and toggling them in the settings UI actually changes behavior.

## Classification signal — RESOLVED (codex review round 1, source-grounded)

**How does the desktop decide "this X device is a touchpad"?** This was the load-bearing unknown; it is now answered from source, and the answer reshapes the design:

- **GDK (GTK3, `gdkdevicemanager-xi2.c`, `is_touchpad_device()`)** classifies `GDK_SOURCE_TOUCHPAD` by the **presence of an XI device property** — `libinput Tapping Enabled` (or `Synaptics Off`, or `Raw Touch Passthrough`). **Not** a name substring, **not** valuator/scroll classes. → The recognition signal is **"the `libinput Tapping Enabled` property exists on the device."** That is the single most important thing this feature must deliver. GDK gestures (two-finger scroll/swipe) hang off this source.
- **gnome-/cinnamon-settings-daemon (X11)** is **udev-driven**: it keys on `ID_INPUT_TOUCHPAD=1` and pairs the udev device to the X device by **device node** (`gsd-device-manager-x11.c:add_device` hash key = device node; cf. GNOME bug 747956, RH bug 1528356). → The settings panel needs the X device to expose a correct **`Device Node`** (and `Device Product ID`) so it pairs to the udev touchpad. udev tagging itself already happens on yoga; we just must not break the pairing inputs.

Consequence: **the device NAME is secondary** (a UI/display/pairing aid), not the classification signal — so the "rename the slave pointer" idea below is de-emphasized accordingly. The recognition-critical deliverables are (1) the `libinput Tapping Enabled` property *present* on the slave pointer, and (2) a correct `Device Node`/`Device Product ID`.

**UPDATE (2026-06-02, from HW testing on M2/Asahi under MATE): recognition is PER-DESKTOP — there is no single signal.** A third path was found that the GDK-centric analysis above missed:
- **MATE (`mate-settings-daemon`) and classic XInput clients** detect a touchpad via the **XInput-1 device TYPE atom**: they `InternAtom("TOUCHPAD", only_if_exists=true)` (the `XI_TOUCHPAD` device-type atom) and check each `XListInputDevices` device's `type` Atom field. This is INDEPENDENT of the XI2 `libinput Tapping Enabled` property. yserver originally interned no device-type atoms and set every `XListInputDevices` device `type` to 0/None, so MATE saw no touchpad (observed: repeated `InternAtom "TOUCHPAD" -> 0`). Fixed (commit `57e659b`): intern `MOUSE`/`KEYBOARD`/`TOUCHPAD` at server init so `InternAtom(only_if_exists=true)` finds them, add `XiDevice.is_touchpad`, and set the slave-pointer's `XListInputDevices` `type` field to the TOUCHPAD atom when it's a touchpad.

So the implementation now provides ALL THREE X11 recognition signals (no desktop has to use the same one): (a) XI2 `libinput Tapping Enabled` property (GDK/GNOME-Shell/Cinnamon), (b) XI1 `TOUCHPAD` device-type atom (MATE/older XInput), (c) `Device Node` for udev-`ID_INPUT_TOUCHPAD` pairing (settings daemons). Future desktops may key on yet another signal — re-check per-desktop when one fails to recognize the touchpad.

Remaining grounding (cheap sanity check, not a blocker): once implemented, confirm with `xinput list-props` on yoga that the property set matches a real Xorg+libinput session, and that the settings panel lists the touchpad. Sources: GTK `gdk/x11/gdkdevicemanager-xi2.c`; GNOME bug 747956 (comment 13 has a real libinput property dump); `../xserver/Xi/xiproperty.c`, `../xserver/include/xserver-properties.h`.

## Non-goals

- Touchscreen / touch events (Tier 3 — separate, larger).
- A faithful per-physical-device model. We keep the master/slave virtual-device topology and attach touchpad identity to the **slave pointer**; we do not enumerate every mouse/keyboard as its own XI device.
- Pointer acceleration curves, gesture (swipe/pinch) XI2 delivery, tablet/pad properties.
- Hotplug of multiple simultaneous touchpads with distinct property sets (one touchpad assumed; design leaves room but doesn't implement N).

## Background — what exists

The XI2 device model is **hardcoded** in the XIQueryDevice handler (`process_request.rs:8356+`): `write_device_info` emits literal `"Virtual core pointer"` (id 2), `"Virtual core keyboard"` (id 3), and a slave pointer/keyboard pair, each with fixed Button/Valuator/Scroll/Key classes. There is **no device registry** in `ServerState` (device ids are bare constants VCP=2/VCK=3 used by the fanouts) and **no property store**. `XIGetProperty` (59) returns not-found unconditionally; `XIListProperties` (56), `XIChangeProperty` (57), `XIDeleteProperty` (58) are absent. Device metadata from libinput is logged and discarded at `context.rs` `DeviceEvent::Added`.

So Tier 2 is three pieces: (A) carry device metadata across the input→core boundary, (B) a device + property store in core, (C) the XI property protocol + naming + DeviceChanged, reading from that store.

## Design

### Part A — convey device metadata input-thread → core

Today `Context::dispatch()` returns `Vec<InputEvent>` (translated motion/button/key only); device add/remove is dropped. Add device lifecycle to that channel:

- New `InputEvent` variants (input-thread-local):
  ```rust
  InputEvent::DeviceAdded(DeviceInfo)
  InputEvent::DeviceRemoved { device_node: String }   // canonical identity (syspath/eventN), per codex
  ```
  where `DeviceInfo` is a plain-data snapshot captured at `DeviceEvent::Added` (no libinput handle crosses the thread):
  ```rust
  struct DeviceInfo {
      name: String,
      sysname: String,            // udev sysname, stable key
      is_touchpad: bool,          // tap_finger_count > 0 (Tier 1 classifier)
      vendor_id: u32, product_id: u32,
      device_node: Option<String>,// /dev/input/eventN
      // current + default config snapshot for seeding properties:
      tap_enabled: bool, tap_default: bool,
      natural_scroll: bool, natural_scroll_default: bool,
      dwt_enabled: bool, dwt_default: bool,
      // scroll/click methods available+enabled, send-events modes, accel — as needed by the confirmed property set
  }
  ```
- `configure_touchpad` (Tier 1) already mutates config at add-time; capture the post-config snapshot into `DeviceInfo`.
- Backend `on_libinput_ready` forwards these as new `HostInputEvent::DeviceAdded(DeviceInfo)` / `DeviceRemoved` over `CoreSender`, alongside the existing pointer/key events. `DeviceInfo` (or a core mirror) must live in `yserver-core` since `HostInputEvent` does.

This is the architecturally significant change: a one-way metadata path that didn't exist. Keep `DeviceInfo` `Clone + Send` plain data.

### Part B — device + property registry in `ServerState`

Add a minimal registry (not a full device-tree rework):

```rust
struct XiDevice {
    id: u16,                     // 4 = slave pointer (touchpad attaches here)
    name: String,               // settable from DeviceInfo (was a literal)
    properties: HashMap<AtomId, XiProperty>,
}
struct XiProperty { type_atom: AtomId, format: u8 /*8|16|32*/, data: Vec<u8> }
```

- `ServerState` gains `xi_devices: Vec<XiDevice>` (or a map). XIQueryDevice stops using literals for the slave-pointer **name** and reads it from the registry; classes stay as today (no touch class).
- On `HostInputEvent::DeviceAdded(info)` with `info.is_touchpad`: rename the slave pointer to `info.name`, and **seed its property map** from `info` (Part C list). On `DeviceRemoved`: revert to the generic name, drop touchpad props.
- Property atoms are interned via the existing `state.atoms.intern(...)`.

### Part C — XI property protocol + DeviceChanged + property events

Implement, all reading/writing the registry, with layouts taken from `../xserver/Xi/xiproperty.c` (verify against it + x11trace, per the protocol-bool-pads lesson — do not invent byte offsets):

- **XIListProperties (56):** reply the device's property atoms.
- **XIGetProperty (59):** real, matching Xorg semantics exactly (`xiproperty.c:237-318`, `ProcXIGetProperty`): **absent property → `type=None(0), format=0, num_items=0, bytes_after=0, no data`**; **type mismatch → return the property's type/format/bytes_after but NO data**; offsets/lengths are in **4-byte units**; honor `delete` (delete iff fully read and `bytes_after==0` and type matched) — and **when that delete actually removes the property, emit the same deletion notifications as XIDeleteProperty** (xserver calls `send_property_event(..., XIPropertyDeleted)` on this path). Replaces the current unconditional not-found.
- **XIChangeProperty (57):** validate (format ∈ {8,16,32}, length consistent) and store. For writable `libinput *` props this is the settings-panel toggle path — Part D routes it to libinput.
- **XIDeleteProperty (58):** remove from the store.
- **Property-change notifications (REQUIRED, not optional):** on ChangeProperty, DeleteProperty, **and the delete path of XIGetProperty(delete=1)**, emit **both** legacy XI1 `DevicePropertyNotify` **and** XI2 `XI_PropertyEvent` (with the right `XIProperty{Created,Modified,Deleted}` state) to selecting clients (`xiproperty.c:185-209`, `:1218-1219`). Without these a running settings daemon won't refresh after a write. (New encoders needed in `yserver-protocol`.)
- **XI_DeviceChanged** (`encode_xi2_device_changed_event`, exists) emitted on touchpad add/remove so a running desktop re-reads the device.
- **Device NAME** sourced from the registry is a *secondary* nice-to-have (display/pairing), **NOT** the recognition signal (see Classification section). If we change the XI2 slave-pointer name we MUST also change it in XI1 — see enumeration consistency below. Keep all XInput **classes identical** to today (the declared scroll valuators that fixed the Gdk-CRITICAL at `process_request.rs:8519-8528` must stay).

**XI1/XI2 enumeration consistency (REQUIRED):** the actual invariant is that `XListInputDevices` (XI1) and `XIQueryDevice` (XI2) describe the **same device set** — real clients (Chromium/Electron) cross-check them and fatal-CHECK on mismatch (the documented `ListInputDevices` crash class, `x11/mod.rs:2121-2129`, `encode_list_input_devices_reply`). Mirroring the slave-pointer name across both is the simple, conservative way to satisfy it (and avoids confusing UIs that read either); a unit test asserts XI1 and XI2 agree on the device set + names.

**FLOAT properties:** atom `FLOAT` (intern it), `format=32`, IEEE-754 32-bit in client byte order — matches `../xserver/include/xserver-properties.h:28-30` and `xiproperty.c:495-519`.

**Seed property set — tiered (from GNOME bug 747956 comment 13's real libinput dump):**

*Tier R — required for recognition* (without these the desktop won't treat it as a touchpad):
- `libinput Tapping Enabled` (INTEGER/8, 0/1) — **the GDK touchpad signal; its mere presence is what GDK keys on.**
- `Device Node` (STRING/8) = `info.device_node` — gsd pairs the udev touchpad to the X device by this.
- `Device Product ID` (INTEGER/32 ×2) = `[vendor_id, product_id]`.

*Tier U — required for correct visible UI state* (panel shows real toggle states):
- `libinput Natural Scrolling Enabled`, `libinput Disable While Typing Enabled`
- `libinput Scroll Methods Available` / `Scroll Method Enabled`
- `libinput Send Events Modes Available` / `Send Events Mode Enabled`
- `libinput Left Handed Enabled`
- the `* Default` companion for each settable bool.

*Tier O — optional parity / follow-up* (not recognition- or basic-UI-critical):
- `libinput Accel Speed` (FLOAT), `libinput Accel Profiles Available/Enabled`, `libinput Click Methods Available/Enabled`, middle-emulation, etc.

### Part D — writable properties → libinput (phase 2b)

Read-only properties (Parts A–C) make the touchpad *visible* and show current state. To make the settings UI's toggles *take effect*, an `XIChangeProperty` on a `libinput *` property must call the corresponding libinput config setter on the device — a **core → input-thread** message (the reverse direction, which doesn't exist today). Phase this:

- **2a (this spec's core):** read-only discovery + correct current values. Settings UI shows the touchpad and its state; toggles update the X property but not (yet) the hardware. Verifiable: `xinput list-props` shows the libinput props; settings panel recognizes the touchpad.
- **2b (follow-up):** add a `CoreSender → input-thread` config channel; on `XIChangeProperty` of a known `libinput *` prop, send `{sysname, ConfigChange}`; the input thread calls `config_tap_set_enabled` / etc. on the live device. Echo back a `DeviceChanged`/property update.

Splitting keeps the reverse-channel complexity out of the first landing.

## Testing

### Unit (yserver-core, no DRM/libinput)
- Property store round-trip: ChangeProperty → GetProperty returns same (type, format, data); ListProperties includes it; DeleteProperty removes it; GetProperty 4-byte-unit windowing (offset/length, bytes_after) matches spec.
- **XIGetProperty edge semantics:** absent property → `None/0/0/0` no data; type mismatch → type/format/bytes_after but no data; `delete=1` only when fully read + type matched.
- **Property events:** ChangeProperty and DeleteProperty each emit XI1 `DevicePropertyNotify` + XI2 `XI_PropertyEvent` to selecting clients only.
- **FLOAT round-trip:** a FLOAT property (atom `FLOAT`, format 32) survives Change→Get bit-exact in client byte order.
- `DeviceAdded(info)` with `is_touchpad` seeds Tier R + Tier U atoms (esp. `libinput Tapping Enabled` present); `DeviceRemoved` clears them.
- XIQueryDevice emits the registry-sourced slave-pointer name; **XI1 `XListInputDevices` emits the SAME name** (enumeration consistency — no XI1/XI2 mismatch).
- XI_DeviceChanged emitted on add/remove to selecting clients only.
- Wire-layout tests for XIGetProperty/XIListProperties/property-event bytes vs `../xserver/Xi/xiproperty.c` field order (verify with `just yserver-*-trace`, per the protocol-bool-pads lesson).

### Integration / manual hardware (yoga)
- `xinput list-props "<device>"` shows the Tier R+U `libinput *` properties with correct current values, matching a real Xorg+libinput session.
- GTK3 app sees the device as `GDK_SOURCE_TOUCHPAD` (presence of `libinput Tapping Enabled` is the GDK signal) — two-finger scroll / gestures route as touchpad.
- Cinnamon/GNOME mouse-&-touchpad settings panel **lists the touchpad** and shows tap/scroll state (validates `Device Node` pairing + property exposure).
- (2b) Toggling tap in the settings panel changes behavior live.
- Regression: keyboard + mouse + existing pointer/scroll still work; **GTK/Chromium/Electron apps still start** (XIQueryDevice classes unchanged — guard the GDK scroll-valuator assertion at `process_request.rs:8519-8528`; XI1/XI2 enumeration kept in sync — guard the `ListInputDevices` fatal-CHECK crash class).

## Risks

- **Classification signal (was top risk — now resolved + verifiable).** GDK keys on presence of `libinput Tapping Enabled`; gsd on udev `ID_INPUT_TOUCHPAD` + `Device Node` pairing. So the recognition deliverables are concrete (Tier R). Residual risk is only getting the property bytes/identity right — gated by the `xinput list-props` + settings-panel HW checks.
- **XI1/XI2 enumeration mismatch (new, real).** Renaming/identifying the slave pointer in XI2 without mirroring XI1 `XListInputDevices` resurfaces the Chromium/Electron `ListInputDevices` fatal-CHECK (`x11/mod.rs:2121-2129`). Mitigation: change both, with a unit test asserting they agree.
- **Missing property-change events.** If ChangeProperty/DeleteProperty don't emit `DevicePropertyNotify` + `XI_PropertyEvent`, a running settings daemon won't refresh after writes (and 2b would look broken). Treated as required, not optional.
- **Property wire layout / FLOAT.** XIGetProperty reply windowing and FLOAT (atom `FLOAT`, format 32, IEEE-754 client-order) are fiddly; verify against `../xserver/Xi/xiproperty.c` + `xserver-properties.h` + x11trace, not from memory.
- **GDK scroll-valuator regression.** Keep XInput classes identical (declared scroll valuators stay). Covered by the GTK-starts HW check.
- **Device metadata threading.** Use the **device node** (stable `/dev/input/eventN` / syspath) as the canonical registry identity, not just `sysname`; ensure add/remove ordering can't strand a renamed slave pointer.
- **Scope creep into 2b.** Keep writable→libinput out of the first landing; read-only is sufficient for recognition + visible state (confirmed by codex).

## Rollout

1. (Classification signal already resolved — see that section.) Implement Part A (metadata channel) → B (registry/store) → C (XI property protocol + property events + XI1/XI2 enumeration sync + DeviceChanged), TDD per layer; seed Tier R then Tier U.
2. `cargo +nightly fmt` / `cargo clippy` / `cargo test` green.
3. HW-smoke on yoga: `xinput list-props` matches a real session; a GTK3 app sees `GDK_SOURCE_TOUCHPAD`; settings panel lists the touchpad; GTK/Electron apps still start (XI1/XI2 sync + classes unchanged).
4. Commit to `feat/touch-input`; decide whether 2b (writable→libinput) lands now or as a follow-up.

## Follow-ups

- Tier 2b: writable `libinput *` properties routed back to the device (reverse config channel).
- Tier 3: touchscreen (touch InputEvent variants, libinput touch begin/update/end/cancel, XI2 touch classes + TouchBegin/Update/End delivery + XIAllowEvents touch modes + passive touch grabs).
- Pointer acceleration / gesture properties if the settings UI surfaces them.
