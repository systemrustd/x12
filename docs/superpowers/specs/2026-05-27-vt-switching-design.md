# VT switching design

**Status:** Draft, 2026-05-27 (revised after codex review).
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
- XI2 raw-event listener (`XI_RawKeyPress` / `XI_RawButtonPress`) coherence across suspend. See "Input" → "XI2 raw events" for the explicit behaviour.

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

Use **libseat** as the session/seat manager. When libseat opens a session successfully, all DRM and input device opens go through it and VT switching is available. When no seat manager is available, fall back to the current direct-open behaviour and VT switching is disabled. The TTY takeover in `kms/console.rs` continues to run unchanged in both modes — libseat does not manage TTY mode for compositors.

This keeps the daily-driver path (yoga, bee, silence under logind) unprivileged and modern, while preserving the bring-up / minimal-system path (iMac 19,2, raw-tty experimentation) that today's code already supports.

### When the Direct fallback is taken

`libseat::Seat::open` can fail for distinct reasons that must be handled differently. Silently falling back to Direct on every failure would let a denied-by-policy session look identical to "no seat manager installed", which is both a debugging hazard and a security-behaviour change (it bypasses logind/seatd device-access policy).

| libseat error class                                  | Action                                                              |
|------------------------------------------------------|---------------------------------------------------------------------|
| No backend available (no logind on D-Bus, no `/run/seatd.sock`, no `LIBSEAT_BACKEND` env) | Fall back to `Direct`, log `INFO yserver: no seat manager; VT switching disabled`. |
| Backend present, session activation denied (permission, missing seat, malformed env) | **Hard fail.** Log `ERROR yserver: seat open denied: <detail>; refusing to bypass policy`; exit non-zero. |
| Backend present, transient I/O / D-Bus error         | Hard fail (same as above). The user can retry; we won't silently downgrade. |

Distinguishing these uses `libseat::Error` discriminants (the `libseat-rs` crate exposes `enum Error { Unavailable, IoError(_), Other(_) }` — exact mapping to be confirmed in the impl plan against the dependency version chosen). The `LIBSEAT_BACKEND=direct` escape hatch documented by libseat is also disabled — we don't want the server demoting itself to the direct backend silently.

## Architecture

### Threading model

`libseat::Seat` is `!Send` + `!Sync` — the C client and its dispatch state are bound to the thread that created them. We give it its own thread.

```
┌──────────────┐         ┌────────────────────────────────────────┐
│ Main thread  │ msg ch  │ Seat thread                            │
│ (core_loop)  │◄────────┤   - owns libseat::Seat                 │
│              │         │   - epoll on seat.get_fd() + req ch    │
│              │  req ch │   - converts seatd callbacks → Messages│
│              ├────────►│   - serializes open_device, switch,    │
│              │         │     disable_seat                       │
└──────────────┘         └────────────────────────────────────────┘
        ▲
        │ msg ch
        │
┌───────┴──────┐
│ Input thread │
│              │  Stop / Resume commands via dedicated control ch
│              │  Drained events flushed before suspend snapshot
└──────────────┘
```

**Seat thread** (new, `crates/yserver/src/seat/thread.rs`):

- Owns the `libseat::Seat` and the `Vec<ManagedDevice>`.
- Pumps the libseat event source: epolls `seat.get_fd()` and a request `Receiver<SeatRequest>`. When the seat fd fires, calls `seat.dispatch()` which invokes the C callbacks; we translate each callback into a `Message` sent into the main `core_loop` channel.
- Serves requests over the channel: `OpenDevice { path, kind, reply }`, `CloseDevice { id }`, `SwitchSession { vt, reply }`, `DisableSeat { reply }`. Replies carry `Result`s back over oneshot channels (Send-able by virtue of the inner fds being `OwnedFd`).
- On startup synchronously: opens the seat, blocks on the initial `Enable` callback, then enters its event loop. The initial `Enable` becomes `Message::SeatEnable` (treated as "we are active for the first time").

**Main thread** holds a `SeatHandle { request_tx: Sender<SeatRequest> }`. Every DRM and input fd is acquired by sending `OpenDevice` and awaiting the reply. The Seat enum collapses into:

```rust
enum Seat {
    Libseat(SeatHandle),
    Direct,
}

impl Seat {
    fn open_device(&self, path: &Path, kind: DeviceKind) -> io::Result<OwnedFd> { ... }
    fn switch_session(&self, vt: u32) -> io::Result<()> { ... }
    // request_disable is internal — the main loop triggers it when suspend cleanup
    // is done; tests can drive it via the same channel.
}
```

`SeatHandle` is `Clone` and `Send`. Under `Direct` the methods do the obvious local thing.

### Callback semantics

libseat exposes only two callbacks on `struct libseat_seat_listener`:

- **`enable_seat`** — seat-wide. Fires on initial activation and on every return from another VT.
- **`disable_seat`** — seat-wide. Fires when the seat is about to be deactivated. We must call `libseat_disable_seat()` to ack once we've quiesced; after that, the underlying device fds are no longer usable for privileged operations (DRM master is gone, input fds are revoked at the kernel level).

There is **no per-device pause callback** surfaced to client code. Per-device revocation is handled inside libseat / seatd / logind and reflected back to us as "this fd now errors on read or ioctl." The spec's earlier per-device callback story was wrong; correcting it.

| libseat event   | Where it goes                            | Triggers                                           |
|-----------------|------------------------------------------|----------------------------------------------------|
| `enable_seat`   | `Message::SeatEnable` to main loop       | Resume sequence (reopen devices, modeset, repaint) |
| `disable_seat`  | `Message::SeatDisable` to main loop      | Suspend sequence                                   |

Practically: on `disable_seat`, we close (or stop using) every managed device fd ourselves before ack'ing. On `enable_seat`, we reopen each device we still want via `seat.open_device(path)`, which may return the same numerical fd or a new one — we treat the returned fd as opaque and replace the old one unconditionally.

### Console / TTY state stays in `kms/console.rs`

libseat does not call `KDSETMODE` / `KDSKBMODE` for us. Those remain `ConsoleGuard`'s responsibility. Both Seat modes (Libseat and Direct) keep using it unchanged.

### Vulkan validity across suspend

Since libseat doesn't surface a per-device pause type to us, the question of "is the underlying device still the same hardware on resume?" is implicit. The MVP assumption:

- **For DRM render-node fds**: under normal VT switching (no hot-unplug between switch-away and switch-back), the same kernel device backs the fd we get from `seat.open_device(path)` on resume. The kernel revokes only the master capability and / or pauses access; the device, its memory, and our `VkDevice` / queues / pinned objects remain valid. We resume `vkQueueSubmit2` without buffer reupload.
- **For DRM render-node fds across a real hot-unplug** (USB GPU removed, eGPU detached): the path may resolve to a different device or fail to open. We detect this on resume by re-querying the device topology before submitting; if the previously-known render card is no longer present, we log `ERROR yserver: render device <path> gone after resume; cannot continue` and exit. Recovering Vulkan from a real device-loss event (full `VkDevice` teardown + recreate, scene rebuild, client-visible drawable invalidation) is its own large project and is explicitly out of scope.
- **For input fds across hot-unplug**: a missing input device is non-fatal — we drop that `ManagedDevice` and continue.

## State machine

Two `Message` variants drive the main loop:

- `Message::SeatEnable` — seat-wide `enable_seat` callback fired.
- `Message::SeatDisable` — seat-wide `disable_seat` callback fired.

A single `enum SeatState { Active, Suspending, Suspended, Resuming }` field on `Kms` drives the logic. Resuming is a brief transient — between receiving `SeatEnable` and finishing modeset — but is its own state so we can reject events that arrive during the window.

```
       ┌──────── SeatEnable (initial) ────────┐
       ▼                                      │
   ┌────────┐  SeatDisable  ┌────────────┐    │
   │ Active │ ────────────► │ Suspending │    │
   └────────┘               └─────┬──────┘    │
       ▲                          │ ack       │
       │                          ▼           │
   ┌──────────┐  modeset OK ┌───────────┐     │
   │ Resuming │ ◄──────SeatEnable────── │ Suspended │
   └────┬─────┘                         └───────────┘
        │
        ▼
   (back to Active)
```

### Event × state reentrancy

VT switch bugs cluster around events arriving out of order. Explicit behaviour for every (state, event) cell:

| State \ Event       | `SeatEnable`                  | `SeatDisable`                | Hotkey switch_session         |
|---------------------|-------------------------------|------------------------------|-------------------------------|
| `Active`            | Log warn, ignore (no-op)      | Begin suspend sequence       | Forward to seat thread        |
| `Suspending`        | Set `pending_enable = true`, continue suspend; act on it once suspend finishes | Log warn, ignore (already suspending) | Reject (the user is already mid-switch); log debug |
| `Suspended`         | Begin resume sequence         | Log warn, ignore             | Reject (no point switching while inactive); log debug |
| `Resuming`          | Set `pending_enable = true`   | Set `pending_disable = true`, abort resume after current step, then run suspend | Reject |

The two `pending_*` flags collapse the rapid-double-switch case: if a Disable arrives during Resuming, we finish the in-flight modeset step (so the kernel sees consistent DRM state), then immediately transition into a fresh Suspending. If an Enable arrives during Suspending, we run the suspend to completion (we owe libseat the `disable_seat()` ack regardless), then transition straight into Resuming. We never queue more than one pending event of each kind — coalesce rather than queue.

### Suspend (Active → Suspending → Suspended)

On `SeatDisable`:

1. Mark `seat_state = Suspending`. Frame builder / scene compose / scanout work flushes are no-ops, modesets queued but not committed, `vkQueueSubmit2` gated.
2. **Quiesce the input thread.** Send a `Stop` command on the input control channel. The input thread:
   - Stops reading new events from `/dev/input/event*`.
   - Drains any events already read from kernel buffers into the main loop channel.
   - Sends back an `Ack` on the control channel.
   The main loop blocks on the `Ack` (bounded by a short timeout; on timeout we proceed and accept the race). This is the explicit barrier that closes the snapshot race between "current `xkb_state`" and "an in-flight key event we haven't dispatched yet."
3. Drain any input events still in the main channel up to the `Ack` watermark, dispatching them normally. Now `xkb_state` and `pointer_state` reflect every input event the kernel delivered before suspend.
4. **Synthesize held-key release.** Walk `XkbState` for the master keyboard and per-pointer button masks; dispatch synthetic `KeyRelease` / `ButtonRelease` through the normal fanout (Core delivery, XI2 *focused* event delivery, focus rules, passive-grab release semantics — same machinery exercised by the Cinnamon keyring fix `0c117e7`). XI2 raw listeners are **not** updated — see "XI2 raw events" below.
5. Wait for any in-flight Vulkan submits to finish (`vkQueueWaitIdle` per render queue, bounded by the same timeout we use on shutdown).
6. Drop DRM master on every KMS card (`drmDropMaster`). Best-effort; an error here is logged but doesn't abort the sequence (we still need to ack libseat to release the VT).
7. Close (drop) every managed input fd. The input thread is already stopped from step 2; we now drop the `OwnedFd`s so the kernel can reclaim them. DRM fds stay open numerically — Vulkan still references them — but are no longer master.
8. Call `seat.disable_seat()` (via the Seat thread). This is the ack to libseat. After this point our DRM fds may have additional ioctls revoked. Set `seat_state = Suspended`.
9. If `pending_enable` was set during the sequence, immediately begin the resume sequence.

Crossing events (Enter / Leave) are **not** synthesised at suspend.

### Resume (Suspended → Resuming → Active)

On `SeatEnable`:

1. Set `seat_state = Resuming`.
2. For every previously known DRM card, send `OpenDevice` to the seat thread; receive a fresh fd. Treat as a new fd; release the old one. (Under `pause`-type suspend this often returns the same numeric fd; we assume nothing.)
3. Reacquire DRM master on every KMS card.
4. Re-query connector state via `drmModeGetResources`; drop outputs that disappeared (hot-unplug while suspended); fire RandR change-event for any that did.
5. Redo modeset on every surviving output using the saved `OutputLayout`.
6. Re-arm cursor plane.
7. Repaint everything: post full-screen damage to every output and request an immediate composite.
8. Send `Resume` to the input thread; it reopens each input device via the seat thread and goes back to polling.
9. Set `seat_state = Active`.
10. If `pending_disable` was set, immediately begin the suspend sequence.

### Subtleties

**Outstanding pageflips at disable time.** Once we drop master we are no longer the foreground VT. A flip queued just before drop-master either (a) completes — the PageFlip event arrives but the buffer never reaches the screen because the new VT owns the connector — or (b) errors at ack time. We tolerate both by treating "missed pageflip during suspend" as expected, not a telemetry alert.

**Frame-builder retire pins.** The v2 frame builder pins resources until the GPU retires them via fence. `vkQueueWaitIdle` in step 5 retires everything. Under `pause`-type suspend (the assumed case) the render fd is still valid; pinned `VkBuffer`/`VkImage` objects stay live and submissions resume without buffer reupload. Under `gone`-type we exit (see "Device pause types").

## Multi-card / split-driver

silence has the split-driver layout (KMS-only card + separate render card). The current code already handles this — `kms/v2/platform.rs` picks a KMS-capable card explicitly (commit `30a22e0`).

Under libseat:

- The seat-wide `enable_seat` / `disable_seat` callbacks drive the *server-wide* `SeatState`. There is one `SeatState`, not one per card.
- On `disable_seat`, we iterate every `ManagedDevice` that is a KMS card and `drmDropMaster` it (suspend step 6). Render-only DRM cards have no master to drop; we just stop submitting to them.
- Submitting to a post-disable render-only DRM fd will start returning `ENODEV` from `drm` ioctls inside libvulkan once libseat / seatd has revoked the fd, which Mesa propagates as `VK_ERROR_DEVICE_LOST`. The `vkQueueSubmit2` gate on `seat_state != Active` (set in suspend step 1) prevents this from ever happening — we stop submitting before the first revoke.
- On `enable_seat`, we reopen every `ManagedDevice` via `seat.open_device(path)`. Each may return a fresh fd; we replace the held fd in the `ManagedDevice` and re-acquire master on KMS cards.

## Input

### Held-key release on suspend

Without intervention, a client that owned the keyboard at switch time sees a stuck-down Ctrl, Alt, mouse button, etc. on resume — the user physically releases the keys while we hold no input fd.

Suspend step 4 walks the current `XkbState` for the master keyboard and the per-pointer button mask in `PointerState`, then dispatches synthetic `KeyRelease` / `ButtonRelease` through the normal `core_loop` keyboard/pointer fanout. The input-quiesce barrier (suspend step 2) ensures the snapshot we walk reflects every event the kernel actually delivered.

### XI2 raw events

XI2 raw event listeners (`XI_RawKeyPress` / `XI_RawKeyRelease` / `XI_RawButtonPress` / `XI_RawButtonRelease`) receive device-level events independent of focus and grabs. They are intentionally **not** updated by the synthetic-release path:

- The synthetic releases go through focus-routed fanout, which doesn't touch raw listeners.
- We don't track which keys are "raw-press-without-raw-release" outstanding per listener.
- The X server typically delivers raw events as they arrive from the device; from a raw-listener client's perspective, the input device went silent during the switch (same as if the user truly stopped typing). On resume, real events resume.

This is a deliberate behavioural choice for MVP. The cost is small: most raw-event consumers (xinput-style logging tools, libinput debug, some game-recording tools) reset internal state on long silences. If a real-world client breaks on this, we revisit.

### Triggering the switch from inside yserver

`console.rs` sets the TTY to `K_OFF` (or `K_RAW`), so the kernel keyboard layer does **not** translate Ctrl-Alt-F<N> into VT switches. We detect it in the input thread, **before** evdev → XKB translation:

- Inspect the raw evdev keycode (not the XKB keysym) so the binding survives any keymap remap.
- Match `KEY_F1..KEY_F12` (codes 59..68, 87, 88) with Ctrl + Alt held in the kernel-style modifier mask.
- On match: do not dispatch to clients (mirrors Xorg's `XF86Switch_VT_N` action handling). Send `SeatRequest::SwitchSession { vt: N }` to the seat thread.

**`switch_session` is fire-and-forget.** libseat documents that the call does not guarantee a VT switch will occur (it's a request to seatd/logind, which may reject it or simply choose not to switch). The hotkey path does **not** transition `SeatState`. State transitions happen only via the `disable_seat` callback that arrives some time later (or never, if the switch is rejected). If the request returns an error synchronously, we log and stay put.

This separates "user requested a switch" from "switch is happening". The first does nothing visible; the second drives suspend.

Pre-translation rather than post-XKB so that a paranoid client grabbing the full keyboard cannot race or swallow the switch. Same dispatch layer as the existing Ctrl-Alt-Backspace zap (`input_thread.rs::Hotkey`); the new VT switch fits naturally as another `Hotkey` variant.

## Testing

### Unit (yserver-core, no DRM, no libseat)

- `seat_state_machine`: exhaustively cover the (state, event) matrix from "Event × state reentrancy" — including `SeatEnable`-during-Suspending, `SeatDisable`-during-Resuming, double `SeatDisable`. Assert `pending_*` flags are set/cleared correctly and submit gates flip at the right edges.
- `held_key_release_on_suspend`: build an `XkbState` with keys depressed and a held pointer button; run the synthesise-release path; assert the correct `KeyRelease` / `ButtonRelease` were emitted (reuse the harness from the Cinnamon grab tests). Assert no XI2 raw event is generated.
- `ctrl_alt_fn_detector`: feed evdev events with Ctrl+Alt+F2 held into the input pre-translation layer; assert `SeatRequest::SwitchSession { vt: 2 }` was sent and the key event was not forwarded to XKB.
- `input_quiesce_barrier`: drive the input thread with an in-flight event burst, send `Stop`, assert all events drained before `Ack`, then verify the snapshot reflects the final post-drain state.

### Integration

Two distinct integration tracks:

**Stub-backend integration (yserver crate, no libseat dep):**

- `TestSeat` impl behind the same trait as `LibseatBackend`. Drive enable/disable + pause/resume events programmatically. Assert end-to-end: input-quiesce → synthesise-release → vkQueueWaitIdle → drop-master → ack disable → re-open → master → modeset → repaint. Assert full damage posts on every output on resume.
- `ynest --simulate-vt-switch` debug knob: fires fake `SeatDisable` / `SeatEnable` into the loop after N seconds. Confirms the resume path doesn't deadlock and the next composite goes through.

**Real-libseat integration (in CI under a seatd container):**

- A test binary that links the real libseat-rs crate against a seatd running as a child process inside the test container. Drives a real switch (`switch_session`) between two pseudo-VTs and asserts the `disable_seat` callback fires, our ack returns, the next `enable_seat` fires, and reopened device fds work for ioctls again.
- Catches: thread-affinity violations on the Seat thread, dispatch-loop ordering bugs (e.g. forgetting to call `seat.dispatch()` after the fd fires), real fd revoke behaviour, and that our reopen path actually gets working fds back.
- Without this, the stub-backend tests miss the most likely real failure mode: resume wedging because events aren't pumped or the device-reopen order is wrong.

### Manual hardware

Each gets one telemetry capture covering: start yserver from a tty under MATE / Cinnamon → Ctrl-Alt-F2 → confirm getty visible → Ctrl-Alt-F1 → confirm cursor + desktop restored, full-screen damage repaint in telemetry, zero `vk_device_lost`, zero `missed_pageflips` once `seat_state == Active` again.

- bee (Ryzen 6900HX / RADV)
- yoga (Snapdragon X1 / Turnip)
- silence (rx580 + split-driver iGPU — multi-output added)
- iMac 19,2 (i5-8500 + Polaris / RADV — different amdgpu master-handoff path from yoga's msm)

A rapid-double-switch capture is added to bee's run: Ctrl-Alt-F2 then immediately Ctrl-Alt-F1 (within ~100 ms) to exercise the `pending_enable`-during-Suspending path.

### Known false-positive sources

- Held-key release on suspend can dispatch to a client that's been disconnected mid-suspend; `core_loop::process_disconnect` already handles "send to dead client = drop", so we just need to not panic on the release write itself.
- The "VT switch unavailable" test must exercise the `Direct` path on a system without a seat manager, and the "VT switch denied" test must exercise the hard-fail path on a system where seatd/logind exists but denies access — these are two different fallback behaviours per the error-class table.

## Risks

Ordered by likelihood × blast radius.

1. **DRM master not dropped before VT switch fires.** Kernel switches foreground VT while we're still master → new owner can't take master → screen wedges. Mitigation: `seat.disable_seat()` is the last call in suspend; libseat ensures VT_RELDISP doesn't fire until after that. If we crash / panic during suspend before the ack, wrap suspend steps in error-tolerant code and ack disable in all paths. Final escape: Ctrl-Alt-Backspace zap.
2. **Input-quiesce barrier deadlocks.** Input thread doesn't ack `Stop` (it's blocked in a kernel read, or panicked). Mitigation: bounded timeout on the ack; on timeout, log `WARN yserver: input quiesce timeout; suspend proceeding with stale snapshot` and continue. Worst case is one or two stuck keys on resume — clients tolerate this far better than a wedged screen.
3. **`vkQueueWaitIdle` hang on suspend.** Hung GPU submit blocks indefinitely. Mitigation: bound the wait with a timeout (already used on shutdown); proceed to drop-master + ack on timeout. Worst case is GPU work continues invisibly on the other VT.
4. **Reopen on resume returns a different device or fails for an essential card.** Hardware actually disappeared mid-suspend (eGPU detached, USB DRM device removed). We detect by re-querying device topology on resume; if a previously-known render or KMS card is gone, we can't recover Vulkan from this at MVP. Mitigation: log loudly and exit; the user sees yserver die rather than wedge. Followed up by the Vulkan-device-loss recovery project (out of scope here).
5. **Modeset on resume fails on a hot-unplugged output.** User switches VT, unplugs a monitor, switches back. Mitigation: re-query connector state via `drmModeGetResources` before applying the saved layout (suspend step 4); drop missing outputs; fire RandR change-event so clients are notified.
6. **Rapid double-switch races the state machine.** User mashes Ctrl-Alt-F2-F1-F2 in 100 ms. Mitigation: `pending_enable` / `pending_disable` coalescing flags described in "Event × state reentrancy"; we always run sequences to completion before applying a pending counter-event. Tested explicitly in the bee hardware capture.
7. **libseat D-Bus connection dies mid-suspend.** Logind crashes are rare but possible — seat is "suspended forever". Mitigation: on libseat dispatch errors during suspend, log loudly and exit. Reboot. Not worth elaborate recovery for an MVP.
8. **Direct fallback silently bypasses policy.** Already mitigated by the error-class table in "When the Direct fallback is taken" — fallback only on the explicit `Unavailable` (or equivalent) error, hard-fail otherwise.
9. **Held-key release dispatching while focus is mid-transition.** Suspend can race a `SetInputFocus`. Worst case: a release is delivered to the client that was focused at suspend, not the resume target. Acceptable — matches Xorg behaviour.

## Rollout

- Feature branch `feature/vt-switch`.
- Build behind runtime detection (`libseat::Seat::open` returns `Unavailable`); no env knob. libseat absence is the off-switch.
- Land in one PR with spec + plan + impl together. No staged sub-phases.
- After merge, daily-driver hosts (yoga, bee, silence) get logind-managed sessions, so VT switching becomes available transparently the next time yserver starts.

## Follow-ups unblocked by this work

- Host suspend-to-RAM (laptop lid close on yoga). Kernel sends device-removed-and-re-added events that look architecturally similar to a VT switch; the resume path is reusable.
- Full Vulkan device-loss recovery (for the missing-device-on-resume case and for `VK_ERROR_DEVICE_LOST` from any other source).
- Multi-seat (one yserver per seat). libseat already takes a seat name parameter; defaults to `seat0`.
- XKB `XF86Switch_VT_N` action wiring, for clients that want to remap switch keys.
- `Session.SetType("x11")` D-Bus call to logind for full session-type reporting.
- XI2 raw-listener coherence across suspend, if a real client surfaces a complaint.
