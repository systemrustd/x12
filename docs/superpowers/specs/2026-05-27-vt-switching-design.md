# VT switching design

**Status:** Draft, 2026-05-27.
**Branch (planned):** `feature/vt-switch`.
**Related:** `crates/yserver/src/kms/console.rs` (existing TTY takeover, explicitly defers VT switching), `xserver/hw/xfree86/os-support/linux/lnx_init.c` + `systemd-logind.c` (upstream reference).

## Goal

Allow the user to Ctrl-Alt-F<N> away from a running yserver to a text console (or another graphical session) and back, without wedging the screen, losing client connections, or leaving stuck keys / buttons. Match the behaviour users get from Xorg.

## Non-goals

The first cut is deliberately lean. Out of scope:

- XKB `XF86Switch_VT_N` action — clients cannot request a switch via keysym remap or `XTest`.
- DPMS state preservation across switch. Scanout is fully re-enabled on resume regardless of the pre-suspend DPMS state.
- `Ctrl-Alt-KP_Plus` / `Ctrl-Alt-KP_Minus` Xorg-style mode hotkeys. RandR clients can re-mode.
- Returning to the boot VT on yserver exit if we weren't on it at startup. Xorg does this; we can add later.
- `Session.SetType("x11")` D-Bus call to logind. Worth doing for completeness but doesn't affect VT switch correctness.
- Spawning `seatd` ourselves when no seat manager is present.
- Multi-seat (running multiple yservers on `seat1`, `seat2`, ...).

## Background

`crates/yserver/src/kms/console.rs` already takes over `/dev/tty` on startup: saves termios, switches keyboard mode to `K_OFF` (fallback `K_RAW`) and screen mode to `KD_GRAPHICS`, restores on drop. The module's header comment says VT switching (`VT_PROCESS`/`VT_SETMODE`) is intentionally out of scope. This design fills that gap.

Today:

- DRM card fds are opened directly by `crates/yserver/src/drm/device.rs::Device::open` (centralised).
- Input device fds are opened directly in `crates/yserver/src/input_thread.rs`.
- The signalfd thread in `crates/yserver/src/lib.rs` handles SIGINT/SIGTERM/SIGUSR1/SIGUSR2. No VT-release signal handler.
- No `drmSetMaster` / `drmDropMaster` ioctls anywhere.
- No D-Bus, no logind integration, no libseat dependency.

Upstream Xorg has two paths for the same problem: a classic `VT_SETMODE { VT_PROCESS, relsig=SIGUSR1, acqsig=SIGUSR1 }` path (lnx_init.c + xf86Events.c) and a modern logind path (systemd-logind.c). Modern Wayland compositors use logind, usually via `libseat` which abstracts logind / seatd / direct-VT behind one API.

## Approach

Use **libseat** as the session/seat manager. When libseat opens a session successfully, all DRM and input device opens go through it and VT switching is available. When libseat is unavailable (no logind, no seatd), fall back to the current direct-open behaviour and VT switching is disabled (switch keys do nothing). The TTY takeover in `kms/console.rs` continues to run unchanged in both modes — libseat does not manage TTY mode for compositors.

This keeps the daily-driver path (yoga, bee under logind) unprivileged and modern, while preserving the bring-up / minimal-system path (iMac 19,2, raw-tty experimentation) that today's code already supports.

## Architecture

### A single `Seat` module owns device fds

New module `crates/yserver/src/seat/`:

```rust
enum Seat {
    Libseat(LibseatBackend),
    Direct,
}

impl Seat {
    fn open(events_tx: Sender<Message>) -> Self { ... }
    fn open_device(&mut self, path: &Path, kind: DeviceKind) -> io::Result<OwnedFd> { ... }
    fn switch_session(&mut self, vt: u32) -> io::Result<()> { ... }
    fn disable_seat(&mut self) -> io::Result<()> { ... }
}

enum DeviceKind {
    Drm { is_kms: bool },
    Input,
}

struct ManagedDevice {
    id: libseat::DeviceId,
    path: PathBuf,
    fd: OwnedFd,
    kind: DeviceKind,
}
```

Construction order in `lib.rs::run`:

1. Build `Seat` first, before `Kms::init` and `input_thread::spawn`.
2. `Seat::open` attempts `libseat::Seat::open(...)`. On success → `Seat::Libseat`; on failure → log `INFO yserver: no libseat session; VT switching disabled` and use `Seat::Direct`.
3. Pass `&mut Seat` to KMS init and input thread spawn so all subsequent device opens route through it.

Under `Libseat`, `open_device` calls `seat.open_device(path)`, stores the returned `device_id` in `ManagedDevice`, and returns the fd. Under `Direct`, it's today's `OpenOptions::new().read(true).write(true).open(path)` verbatim.

### Why a single Seat owning both DRM and input fds

libseat pause / resume events arrive per `device_id`. We must be able to route a pause for "DRM card 0" to the KMS backend and a pause for "/dev/input/event7" to the input thread, from one place. Splitting ownership across modules would duplicate the libseat client.

### Console / TTY state stays in `kms/console.rs`

libseat does not call `KDSETMODE` / `KDSKBMODE` for us. Those remain the responsibility of `ConsoleGuard`. Both modes (libseat and direct) keep using it unchanged.

## State machine

Two new `Message` variants are pushed into the main `core_loop` channel from the libseat dispatch path:

- `Message::SeatEnable` — libseat says our session is active (initial activation, plus every return from another VT).
- `Message::SeatDisable` — libseat says our session is about to be deactivated.

libseat's contract: after we receive `SeatDisable` we must call `seat.disable_seat()` once we've quiesced; the kernel then completes the VT switch. After that, our DRM device fds are revoked (still open numerically, ioctls return `ENODEV`); input device fds are similarly revoked.

A single `enum SeatState { Active, Suspending, Suspended }` field on `Kms` drives the logic:

```
       ┌────────── SeatEnable (initial) ───────────┐
       ▼                                           │
   ┌────────┐  SeatDisable   ┌────────────┐  ack   ┴
   │ Active │ ─────────────► │ Suspending │ ─────► Suspended
   └────────┘                └────────────┘            │
       ▲                                               │
       └───────────── SeatEnable ──────────────────────┘
                  (reopen devices, modeset, repaint)
```

### Suspend (Active → Suspending → Suspended)

On `SeatDisable`:

1. Mark `seat_state = Suspending`. From this point the main loop refuses to run any frame builder / scene compose / scanout work — flushes are no-ops, modesets are queued but not committed, `vkQueueSubmit2` is gated.
2. Wait for any in-flight Vulkan submits to finish (`vkQueueWaitIdle` per render queue, bounded by a timeout we already use on shutdown).
3. Drop DRM master on every KMS card (`drmDropMaster`).
4. Synthesize key-release for every key currently held in `xkb_state`, and `ButtonRelease` for any held pointer buttons. Dispatch through the normal fanout paths so XI2 / Core delivery, focus rules, and passive-grab release semantics all run (same machinery exercised by the Cinnamon keyring fix `0c117e7`).
5. Call `seat.disable_seat()`. Set `seat_state = Suspended`. The input thread keeps polling its fds; reads now error `ENODEV` and are silently dropped until resume.

Crossing events (Enter / Leave) are **not** synthesised at suspend — pointer position is conceptually still where it was. On resume the cursor reappears at the same coordinates and no Leave / Enter is needed.

### Resume (Suspended → Active)

On `SeatEnable`:

1. For every previously known DRM card, call `seat.open_device(path)` to get a fresh fd. Treat as a new fd; release the old one.
2. Reacquire DRM master, redo modeset on every output using the saved `OutputLayout` (modes, CRTC routing, scanout fb). Outputs that disappeared (hot-unplug while suspended) are dropped; the RandR change-event path notifies clients.
3. Re-arm cursor plane.
4. Repaint everything: post full-screen damage to every output and request an immediate composite.
5. Re-open every input device the same way.
6. Set `seat_state = Active`. Main loop resumes normal scheduling.

### Subtleties

**Outstanding pageflips at disable time.** Once we drop master we are no longer the foreground VT. A flip queued just before drop-master either (a) completes — the PageFlip event arrives but the buffer never reaches the screen because the new VT owns the connector — or (b) errors at ack time. We tolerate both by treating "missed pageflip during suspend" as expected, not a telemetry alert.

**Frame-builder retire pins.** The v2 frame builder pins resources until the GPU retires them via fence. The `vkQueueWaitIdle` in step 2 retires everything; on resume, the pinned resources are still valid Vulkan objects (Vulkan is decoupled from DRM master), so we resume submissions without buffer reupload.

## Multi-card / split-driver

silence has the split-driver layout (KMS-only card + separate render card). The current code already handles this — `kms/v2/platform.rs` picks a KMS-capable card explicitly (commit `30a22e0`).

Under libseat, each `/dev/dri/cardN` we open gets its own `device_id` and its own pause / resume callbacks. The state machine is naturally per-device but `seat_state` is a single global gate: the whole server is Active or Suspended.

Rules:

- libseat only pauses devices on our active seat. A render-only DRM node on the same seat is also paused. We must drop master on the KMS card and stop submitting to the render card.
- Submitting to a paused render-only DRM fd returns `ENODEV` from `drm` ioctls inside libvulkan, which Mesa propagates as `VK_ERROR_DEVICE_LOST`. We avoid this by gating all `vkQueueSubmit2` / `vkCmdCopyBufferToImage` on `seat_state == Active`.
- The transition is atomic from the server's perspective: we don't enter `Suspended` until every device has been quiesced and ack'd, and don't return to `Active` until every device has been re-opened and re-mastered.
- Render-card-only seats (GPU with no displays attached): still tracked. No master to drop, but `seat.open_device` is still how we acquire them.

## Input

### Held-key release on suspend

Without intervention, a client that owned the keyboard at switch time sees a stuck-down Ctrl, Alt, mouse button, etc. on resume — the user physically releases the keys while we hold no input fd.

Before calling `seat.disable_seat()`:

1. Walk the current `XkbState` for the master keyboard and the per-pointer button mask in `PointerState`.
2. For each pressed key, dispatch a synthetic `KeyRelease` through the normal `core_loop` keyboard fanout.
3. For each pressed pointer button, dispatch a synthetic `ButtonRelease` through `pointer_fanout`. Active pointer grabs are released by the same logic that handles a real release.
4. Modifier state (`xkb_state` mod_depressed / mod_latched) collapses to whatever the released keys cleared.

### Triggering the switch from inside yserver

`console.rs` sets the TTY to `K_OFF` (or `K_RAW`), so the kernel keyboard layer does **not** translate Ctrl-Alt-F<N> into VT switches. We detect it ourselves in the input thread, **before** evdev → XKB translation:

- Inspect the raw evdev keycode (not the XKB keysym) so the binding survives any keymap remap.
- Match `KEY_F1..KEY_F12` (codes 59..68, 87, 88) with Ctrl + Alt held in the kernel-style modifier mask.
- On match: do not dispatch to clients (mirrors Xorg's `XF86Switch_VT_N` action handling). Call `seat.switch_session(N)`. libseat → logind/seatd → `VT_ACTIVATE` ioctl. The kernel fires the disable callback on its own schedule.

Pre-translation rather than post-XKB so that a paranoid client grabbing the full keyboard cannot race or swallow the switch. Same dispatch layer as the existing Ctrl-Alt-Backspace zap (`input_thread.rs::Hotkey`); the new VT switch fits naturally as another `Hotkey` variant.

## Testing

### Unit (yserver-core, no DRM, no libseat)

- `seat_state_machine`: feed `SeatEnable` / `SeatDisable` events into a pure state struct; assert transitions and assert submit gates flip at the right edges.
- `held_key_release_on_suspend`: build an `XkbState` with keys depressed and a held pointer button; run the synthesise-release path; assert the correct `KeyRelease` / `ButtonRelease` were emitted (reuse the harness from the Cinnamon grab tests).
- `ctrl_alt_fn_detector`: feed evdev events with Ctrl+Alt+F2 held into the input pre-translation layer; assert `switch_session(2)` was called on a mock `Seat` and the key event was not forwarded to XKB.

### Integration (yserver crate, stub libseat backend)

- `TestSeat` impl behind the same trait as `LibseatBackend`. Drive pause / resume events programmatically. Assert: drop-master is called, frame-builder gating engages, `disable_seat()` is acked, repaint posts full damage on every output on resume.
- `ynest --simulate-vt-switch` debug knob: fires fake `SeatDisable` / `SeatEnable` into the loop after N seconds. Confirms the resume path doesn't deadlock and the next composite goes through.

### Manual hardware

Each gets one telemetry capture covering: start yserver from a tty under MATE / Cinnamon → Ctrl-Alt-F2 → confirm getty visible → Ctrl-Alt-F1 → confirm cursor + desktop restored, full-screen damage repaint in telemetry, zero `vk_device_lost`, zero `missed_pageflips` once `seat_state == Active` again.

- bee (Ryzen 6900HX / RADV)
- yoga (Snapdragon X1 / Turnip)
- silence (rx580 + split-driver iGPU — multi-output added)
- iMac 19,2 (i5-8500 + Polaris / RADV — different amdgpu master-handoff path from yoga's msm)

### Known false-positive sources

- libseat's seatd backend on a system without the right group / without seatd running falls back to Direct; the test for "VT switch unavailable" must assert that path works, not error.
- Held-key release on suspend can dispatch to a client that's been disconnected mid-suspend; `core_loop::process_disconnect` already handles "send to dead client = drop", so we just need to not panic on the release write itself.

## Risks

Ordered by likelihood × blast radius.

1. **DRM master not actually dropped before VT switch fires.** The kernel switches the foreground VT while we're still master → new owner can't take master → screen wedges. Mitigation: `seat.disable_seat()` is the last call in the suspend path; libseat ensures VT_RELDISP doesn't fire until after that. If we crash / panic during suspend before the ack, wrap suspend steps in error-tolerant code and ack disable in all paths (success, partial error). Final escape: Ctrl-Alt-Backspace zap.
2. **`vkQueueWaitIdle` hang on suspend.** A hung GPU submit could block indefinitely. Mitigation: bound the wait with a timeout (already used on shutdown); proceed to drop-master + ack on timeout. Worst case is GPU work continues invisibly on the other VT.
3. **Modeset on resume fails on a hot-unplugged output.** User switches VT, unplugs a monitor, switches back. Mitigation: re-query connector state via `drmModeGetResources` before applying the saved layout; drop missing outputs; trigger RandR change-event so clients are notified.
4. **libseat D-Bus connection dies mid-suspend.** Logind crashes are rare but possible — seat is "suspended forever". Mitigation: on libseat dispatch errors during suspend, log loudly and tear down. Reboot. Not worth elaborate recovery for an MVP.
5. **Held-key release dispatching while focus is mid-transition.** Suspend can race a `SetInputFocus`. Worst case: a release is delivered to the client that was focused at suspend, not the resume target. Acceptable — matches Xorg behaviour; clients tolerate spurious releases far better than missed ones.

## Rollout

- Feature branch `feature/vt-switch`.
- Build behind runtime detection (`libseat::Seat::open` succeeds); no env knob. libseat absence is the off-switch.
- Land in one PR with spec + plan + impl together. No staged sub-phases.
- After merge, daily-driver hosts (yoga, bee, silence) get logind-managed sessions, so VT switching becomes available transparently the next time yserver starts.

## Follow-ups unblocked by this work

- Host suspend-to-RAM (laptop lid close on yoga). Kernel sends device-removed-and-re-added events that look architecturally similar to a VT switch; the resume path is reusable.
- Multi-seat (one yserver per seat). libseat already takes a seat name parameter; defaults to `seat0`.
- XKB `XF86Switch_VT_N` action wiring, for clients that want to remap switch keys.
- `Session.SetType("x11")` D-Bus call to logind for full session-type reporting.
