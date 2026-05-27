# VT switching design

**Status:** Draft, 2026-05-27 (revision 3 — simplified per wlroots + codex re-review).
**Branch (planned):** `feature/vt-switch`.
**Related:** `crates/yserver/src/kms/console.rs` (existing TTY takeover, explicitly defers VT switching), `xserver/hw/xfree86/os-support/linux/lnx_init.c` + `systemd-logind.c` (upstream reference), `wlroots/backend/session/session.c` (the authoritative production reference for libseat-driven session management).

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
- **Hot-plug / hot-unplug while suspended.** Outputs / DRM cards / input devices that appear or disappear between switch-away and switch-back are not handled as a graceful change. A disappeared essential card → exit; a disappeared output → dropped on resume with a RandR change event; a newly-appeared output → ignored until the next full resume cycle (or until a separate udev integration is added in a follow-up).
- **Vulkan device-loss recovery.** If a `VkDevice` ever transitions to `VK_ERROR_DEVICE_LOST` we exit. Full `VkDevice` teardown + recreate + scene rebuild + client-visible drawable invalidation is its own large project.

## Background

`crates/yserver/src/kms/console.rs` already takes over `/dev/tty` on startup: saves termios, switches keyboard mode to `K_OFF` (fallback `K_RAW`) and screen mode to `KD_GRAPHICS`, restores on drop. The module's header comment says VT switching (`VT_PROCESS`/`VT_SETMODE`) is intentionally out of scope. This design fills that gap.

Today:

- DRM card fds are opened directly by `crates/yserver/src/drm/device.rs::Device::open` (centralised).
- Input device fds are opened directly in `crates/yserver/src/input_thread.rs`.
- The signalfd thread in `crates/yserver/src/lib.rs` handles SIGINT/SIGTERM/SIGUSR1/SIGUSR2. No VT-release signal handler.
- No `drmSetMaster` / `drmDropMaster` ioctls anywhere.
- No D-Bus, no logind integration, no libseat dependency.

Upstream references:

- **Xorg classic** (`xserver/hw/xfree86/os-support/linux/lnx_init.c` + `common/xf86Events.c`): direct `VT_SETMODE { VT_PROCESS, relsig=SIGUSR1, acqsig=SIGUSR1 }`. Required root/suid historically.
- **Xorg logind** (`xserver/hw/xfree86/os-support/linux/systemd-logind.c`): D-Bus `TakeControl` + per-device `PauseDevice` / `ResumeDevice` signals. Unprivileged.
- **wlroots** (`wlroots/backend/session/session.c`): libseat as the abstraction; single thread; `enable_seat`/`disable_seat` are the only callbacks; the session backend integrates into the compositor's main event loop. This is the model we follow.

## Approach

Use **libseat** as the session/seat manager. When libseat opens a session successfully, all DRM and input device opens go through it and VT switching is available. When no seat manager is available, fall back to the current direct-open behaviour and VT switching is disabled. The TTY takeover in `kms/console.rs` continues to run unchanged in both modes — libseat does not manage TTY mode for compositors.

This keeps the daily-driver path (yoga, bee, silence under logind) unprivileged and modern, while preserving the bring-up / minimal-system path (iMac 19,2, raw-tty experimentation) that today's code already supports.

### When the Direct fallback is taken

Simple rule, matching wlroots: if `libseat_open_seat` (via `libseat-rs`) returns success, use the `Libseat` backend; if it returns an error (any error), log it and fall back to `Direct`. The `libseat-rs` API returns `Errno`-flavoured errors and does not expose a discriminated enum that would let us safely distinguish "no backend available" from "backend present, denied" — so we don't try.

We do not gate which libseat backend gets selected. Whichever backend libseat picks — logind, seatd, or direct (auto-detected or `LIBSEAT_BACKEND=direct` forced) — we use it, log which one we got, and proceed. Whether the chosen backend actually succeeds depends on syscall-level permissions (file permissions on `/dev/dri/*` and `/dev/input/event*`, `CAP_SYS_TTY_CONFIG` for VT control, etc.) which the kernel enforces independently of yserver. There's no honest correctness or security reason to special-case `direct`; we'd just be enforcing a support-policy preference, and the spec doesn't take a position on which backend is "supported" — the hardware matrix is what's tested, and logind-on-Linux is what every host in it runs.

When `libseat_open_seat` itself fails (no working backend at all), fall back to the legacy direct-open path and log `INFO yserver: libseat unavailable (<errno>); VT switching disabled, opening devices directly`. The legacy path is what runs today — the existing bring-up scenarios (iMac 19,2, raw-tty experimentation) keep working.

## Architecture

### Threading model

`libseat::Seat` is `!Send` + `!Sync` — the C client and its dispatch state are bound to the thread that created them. We satisfy this by keeping libseat on the existing main `core_loop` thread, as one more event source alongside client sockets, the input-thread channel, and the signalfd-thread channel.

No new dedicated thread. No `SeatHandle` / `SeatRequest` channel.

```
┌─────────────────────────────────────────────┐
│ Main thread (core_loop)                     │
│   - polls: client sockets, input-thread ch, │
│     signalfd-thread ch, libseat fd          │
│   - owns: libseat::Seat, Vec<ManagedDevice> │
│   - runs: open_device, switch_session,      │
│     disable_seat directly (no IPC)          │
└─────────────────────────────────────────────┘
        ▲
        │ msg ch (existing) — input events,
        │ stop/resume control
┌───────┴──────┐
│ Input thread │
└──────────────┘
```

**libseat as a poll source.** The main loop's existing poll set gains `libseat::Seat::get_fd()` as a readable source. When it fires, the main loop calls `seat.dispatch()`. libseat invokes our `enable_seat` / `disable_seat` callbacks synchronously inside that call — on the main thread — so the suspend/resume sequences run inline without channel hops or thread crossings. This is exactly the wlroots pattern (`backend/session/session.c:103-104`).

**Module layout.** `crates/yserver/src/seat/mod.rs`:

```rust
enum Seat {
    Libseat(LibseatBackend),
    Direct,
}

struct LibseatBackend {
    seat: libseat::Seat,
    devices: Vec<ManagedDevice>,
    // Set by the callbacks during seat.dispatch(); the main loop reads
    // after each dispatch and acts on transitions.
    pending: SeatPending,
}

#[derive(Default)]
struct SeatPending {
    enable_fired: bool,
    disable_fired: bool,
}

enum DeviceKind { Drm { is_kms: bool }, Input }

struct ManagedDevice {
    id: libseat::DeviceId,
    path: PathBuf,
    fd: OwnedFd,
    kind: DeviceKind,
}

impl Seat {
    fn open_device(&mut self, path: &Path, kind: DeviceKind) -> io::Result<OwnedFd> { ... }
    fn switch_session(&mut self, vt: u32) -> io::Result<()> { ... }
    fn dispatch_pending(&mut self, kms: &mut Kms) { ... } // main loop drives suspend/resume
}
```

The main loop owns the `Seat` (mutably). KMS init and input-thread init are passed `&mut Seat` at startup to acquire their initial device fds, then release the borrow. Subsequent device reopens during resume happen from the suspend/resume code in the main loop, which has the `&mut Seat` again.

The input thread does not see the `Seat` directly. It receives an evdev fd at spawn time. On suspend the main loop sends it a `Stop` control message; on resume the main loop reopens the fd via `seat.open_device` and sends a `Resume { fd }` control message back.

### Callback semantics

libseat exposes only two callbacks on `struct libseat_seat_listener`:

- **`enable_seat`** — seat-wide. Fires on initial activation and on every return from another VT.
- **`disable_seat`** — seat-wide. Fires when the seat is about to be deactivated. We must call `libseat_disable_seat()` to ack once we've quiesced; only after that does the kernel allow the VT to switch away.

There is **no per-device pause callback** surfaced to client code. Per-device revocation is handled inside libseat / seatd / logind and reflected back to us as "this fd now errors on ioctl/read" — but we never see a callback per device.

| libseat event   | Where it goes                            | Triggers                                           |
|-----------------|------------------------------------------|----------------------------------------------------|
| `enable_seat`   | Sets `pending.enable_fired = true`       | Resume sequence (reopen devices, modeset, repaint) |
| `disable_seat`  | Sets `pending.disable_fired = true`      | Suspend sequence                                   |

Practically: on `disable_seat`, we **close every input fd** and **drop DRM master while leaving DRM fds open** before calling `libseat_disable_seat()` to ack. The asymmetry matters; see "Device fd lifetime contract" below. On `enable_seat`, we reopen each device via `seat.open_device(path)`, which returns a fresh `(device_id, fd)` pair — we treat the fd as opaque and replace the old one unconditionally.

### Console / TTY state stays in `kms/console.rs`

libseat does not call `KDSETMODE` / `KDSKBMODE` for us. Those remain `ConsoleGuard`'s responsibility. Both Seat modes (Libseat and Direct) keep using it unchanged.

### Device fd lifetime contract

wlroots's `wlr_session_open_file` header documents what libseat guarantees:

> When the session becomes inactive:
> - DRM files lose their DRM master status
> - evdev files become invalid and should be closed

That contract covers only the fds *we* hold. Vulkan's behaviour across the suspend depends on a separate, simpler mechanism in the kernel DRM design — Mesa renders through a **render node**, and render nodes don't participate in master at all:

- **DRM primary-node fds (`/dev/dri/cardN`) — the ones we open via libseat for KMS:** after `disable_seat`, the fd remains open and usable for non-master operations. Master capability is gone; modeset and pageflip ioctls will fail. We don't `close` it ourselves; the kernel revokes master via `drm_master_release`, not via fd closure. On `enable_seat` we call `seat.open_device(path)` again to get a fresh primary-node fd and `drmSetMaster` on it.
- **DRM render-node fds (`/dev/dri/renderD12N`) — opened by Mesa internally when we create `VkInstance` / `VkDevice`:** kernel UAPI documentation is explicit that render nodes "drop the DRM-Master concept" — rendering ioctls are allowed unconditionally on a render node, master never applies. So when libseat revokes master on our primary-node fd, Mesa's render-node fd is completely unaffected. They're independent open files in the kernel; nothing about our master-revoke touches Mesa's render context. `VkDevice`, `VkQueue`, `VkBuffer`, `VkImage`, command pools, and the frame builder's pinned resources all stay valid. The only reason to gate `vkQueueSubmit2` during suspend (step 1) is to avoid in-flight submits racing with our modeset/scanout teardown — not because of any Vulkan-side invalidation.
- **Input (evdev) fds:** invalid after `disable_seat`. We close them ourselves before ack'ing (the kernel will revoke if we don't, but explicit close gives us cleaner ownership accounting). On `enable_seat` we reopen via `seat.open_device(path)` and hand the new fd to the input thread.

Every wlroots+Vulkan compositor (sway, Hyprland, smithay derivatives) in production demonstrates this pattern working across logind / seatd / direct backends — because none of them need to actively keep Vulkan alive; the kernel's render-node design makes it automatic. wlroots's own DRM backend has zero driver-specific code (`backend/drm/`); it treats every KMS driver identically through standard DRM uapi.

**Covers nvidia-drm too.** The proprietary NVIDIA Linux driver, when loaded with `nvidia-drm.modeset=1` (which is required for yserver to run on it at all — KMS is non-optional), exposes a standard DRM primary node + render node like any other KMS driver. The render-node argument applies unchanged: nvidia's render node drops master per kernel uapi, Mesa-equivalent Vulkan ICD operations are master-immune. wlroots confirms this in production — sway etc. run on nvidia-drm via the same code path as amdgpu, with no nvidia-specific handling at the session/DRM layer. yserver already runs on this host class today; VT switching adds no new requirements.

For the render-node-less / primary-node-only case (rare — pre-render-node-era drivers, or `nvidia-drm.modeset=0` where there is no DRM device at all and yserver wouldn't run anyway), Risks #9 covers the failure mode and mitigation.

### What we do NOT rely on

- We do not assume a specific pause/revoke type (`pause` vs `force` vs `gone` from the underlying logind protocol). libseat hides that.
- We do not assume the post-disable DRM fd is fully revoked. It may be — we don't care, because we already stopped issuing master ioctls in suspend step 1.
- We do not assume the resume `open_device` returns the same numeric fd. We replace.

## State machine

A single `enum SeatState { Active, Suspending, Suspended, Resuming }` field on `Kms` drives the logic. `Suspending` and `Resuming` are transient states bracketing the actual sequences (which may be long because of `vkQueueWaitIdle` and modeset).

```
       ┌──────── enable_seat (initial) ────────┐
       ▼                                       │
   ┌────────┐  disable_seat  ┌────────────┐    │
   │ Active │ ─────────────► │ Suspending │    │
   └────────┘                └─────┬──────┘    │
       ▲                           │ ack       │
       │                           ▼           │
   ┌──────────┐  resume done  ┌───────────┐    │
   │ Resuming │ ◄─enable_seat │ Suspended │    │
   └────┬─────┘               └───────────┘
        │
        ▼
   (back to Active)
```

### Event × state reentrancy

Two pending flags collapse rapid-double-switch behaviour: `pending_enable` and `pending_disable`. Sequences always run to completion before applying a pending counter-event. We never queue more than one of each — coalesce.

| State \ Event       | `enable_seat`                                     | `disable_seat`                                                                  | Hotkey `switch_session`         |
|---------------------|---------------------------------------------------|---------------------------------------------------------------------------------|---------------------------------|
| `Active`            | Log warn, ignore                                  | Begin suspend sequence                                                          | Forward to libseat              |
| `Suspending`        | Set `pending_enable = true`; act after suspend completes | Log warn, ignore (already suspending)                                          | Reject (user is mid-switch); log debug |
| `Suspended`         | Begin resume sequence                             | Log warn, ignore                                                                | Reject (no point); log debug    |
| `Resuming`          | Set `pending_enable = true`; ignore (we're already resuming) | Set `pending_disable = true`; resume completes first, then suspend immediately follows | Reject |

**Resume completion boundary.** After the last step of the resume sequence (modeset OK, input thread resumed) but **before** the `seat_state = Active` commit, we re-check `pending_disable`. If set, we go straight into Suspending without ever touching Active. This avoids a "blink" where the screen briefly appears active to the rest of the system before going dark again. The commit to Active is the single linearisation point of the resume; nothing else observes "we are Active" until then.

**Symmetrically on suspend completion**: after `libseat_disable_seat()` returns but before `seat_state = Suspended` commits, re-check `pending_enable`. If set, transition through Suspended → Resuming in the next iteration (we cannot skip Suspended because libseat needs to actually deliver `enable_seat` again first; the pending flag just primes us to act on it the moment it arrives).

### Suspend (Active → Suspending → Suspended)

Triggered by `disable_seat` callback firing on the main thread during `seat.dispatch()`.

1. Mark `seat_state = Suspending`. Frame builder / scene compose / scanout flushes are no-ops, modesets queued but not committed, `vkQueueSubmit2` gated. (We are already on the main thread; this is a plain field write.)
2. **Quiesce the input thread.** Send a `Stop` command on the input control channel. The input thread:
   - Stops reading new events from `/dev/input/event*`.
   - Drains any events already read from kernel buffers into the main loop channel.
   - Sends back an `Ack` on the control channel.
   The main loop blocks on the `Ack`, bounded by a short timeout. **On timeout we proceed and accept the race** — see Risks #2. This is the explicit barrier that closes the snapshot race between "current `xkb_state`" and "an in-flight key event we haven't dispatched yet".
3. Drain any input events still in the main channel up to the `Ack` watermark, dispatching them normally. Now `xkb_state` and `pointer_state` reflect every input event the kernel delivered before suspend.
4. **Synthesize held-key release.** Walk `XkbState` for the master keyboard and per-pointer button masks; dispatch synthetic `KeyRelease` / `ButtonRelease` through the normal fanout (Core delivery, XI2 *focused* event delivery, focus rules, passive-grab release semantics — same machinery exercised by the Cinnamon keyring fix `0c117e7`). XI2 raw listeners are **not** updated — see "XI2 raw events" below.
5. Wait for in-flight Vulkan submits to finish (`vkQueueWaitIdle` per render queue, bounded by the same timeout we use on shutdown).
6. Drop DRM master on every KMS card (`drmDropMaster`). Best-effort; an error here is logged but doesn't abort the sequence (we still need to ack libseat to release the VT).
7. Close every managed input fd. The input thread is already stopped from step 2; we drop the `OwnedFd`s so the kernel can reclaim them before libseat / seatd needs to forcibly revoke. DRM fds are **not** closed — see "Device fd lifetime contract" — Mesa retains them and they remain valid for non-master operations.
8. Call `libseat_disable_seat()` to ack. Set `seat_state = Suspended`.
9. Re-check `pending_enable`; if set, primes the next iteration to begin resume the moment `enable_seat` arrives.

Crossing events (Enter / Leave) are **not** synthesised at suspend.

### Resume (Suspended → Resuming → Active)

Triggered by `enable_seat` callback firing on the main thread during `seat.dispatch()`.

1. Set `seat_state = Resuming`.
2. For every previously known DRM card, call `seat.open_device(path)` to get a fresh fd. Drop the old `ManagedDevice.fd`, install the new one. (We keep the Mesa-internal references via `VkDevice`; only our user-space fd handle is replaced.)
3. `drmSetMaster` on every KMS card.
4. Re-query connector state via `drmModeGetResources`. Drop outputs that disappeared (hot-unplug while suspended) and fire RandR change-events. **Newly-appeared outputs are ignored** at MVP — they will not be incorporated into the layout. (Hot-plug while suspended is a follow-up; see Non-goals.)
5. Redo modeset on every surviving output using the saved `OutputLayout`.
6. Re-arm cursor plane.
7. Repaint: post full-screen damage on every output; request an immediate composite.
8. Send `Resume { fd: new_fd }` to the input thread for each input device (fd obtained via `seat.open_device`); the thread goes back to polling.
9. **Re-check `pending_disable`** before committing. If set, transition straight into the Suspending sequence — do not pass through Active.
10. Otherwise: set `seat_state = Active`. Main loop resumes normal scheduling.

### Subtleties

**Main-loop stall during suspend / resume.** The `disable_seat` and `enable_seat` callbacks run on the main thread inside `seat.dispatch()`. Suspend blocks on the input-quiesce ack, `vkQueueWaitIdle`, and a series of `drmDropMaster` calls. Resume blocks on `seat.open_device`, `drmSetMaster`, modeset, and the input thread's `Resume` ack. During these blocks the main loop is **not polling** client sockets, the signalfd channel, or any other event source. Crossbeam's unbounded channel queues incoming messages — no events are lost, just delayed.

**Each blocking step must have an explicit timeout enforced by the implementation.** In-tree today, only `FenceTicket::wait` (5s on the GPU fence) has a real bound; everything else listed above is new code introduced by this work. The implementation plan must define and enforce:

- Input-quiesce ack timeout (initial value ~100 ms — tuning target, not a proven bound).
- `vkQueueWaitIdle` timeout (reuse the existing 5s shutdown bound).
- Modeset timeout per CRTC (initial value ~1 s; kernel atomic commit typically completes in tens of ms).
- Resume input-thread `Resume` ack timeout (initial value ~100 ms — same caveat).

The above are starting points; the implementation should treat them as tuning knobs to be validated under load on the hardware matrix (a loaded laptop kernel may need higher ack timeouts), not as guarantees the spec proves. With those bounds in place, the worst-case stall sums to ~6 s. The screen is going dark during the stall anyway; clients perceive a brief input lag at the VT-switch boundary, identical to what Xorg shows. If telemetry shows a typical switch exceeding ~250 ms we revisit (likely by moving the GPU wait to a worker thread and pumping the main loop), but the lean MVP path doesn't pay that cost upfront.

**Outstanding pageflips at disable time.** Once we drop master we are no longer the foreground VT. A flip queued just before drop-master either (a) completes — the PageFlip event arrives but the buffer never reaches the screen because the new VT owns the connector — or (b) errors at ack time. We tolerate both by treating "missed pageflip during suspend" as expected, not a telemetry alert.

**Frame-builder retire pins.** The v2 frame builder pins resources until the GPU retires them via fence. `vkQueueWaitIdle` in step 5 retires everything. The render fd remains valid (wlroots contract); pinned `VkBuffer`/`VkImage` objects stay live; submissions resume without buffer reupload.

**`VK_ERROR_DEVICE_LOST`.** If we ever see this — from the post-resume submit path or any other source — we exit. Recovery is explicitly out of scope (see Non-goals).

## Multi-card / split-driver

silence has the split-driver layout (KMS-only card + separate render card). The current code already handles this — `kms/v2/platform.rs` picks a KMS-capable card explicitly (commit `30a22e0`).

Under libseat:

- The seat-wide `enable_seat` / `disable_seat` callbacks drive the *server-wide* `SeatState`. There is one `SeatState`, not one per card.
- On `disable_seat`, we iterate every `ManagedDevice` that is a KMS card and `drmDropMaster` it. Render-only DRM cards have no master to drop.
- Submitting to a render-only DRM fd after the seat is disabled may eventually return errors from libvulkan once libseat / seatd has applied per-device revocation, but the `vkQueueSubmit2` gate on `seat_state != Active` (set in suspend step 1) prevents us from ever reaching that point.
- On `enable_seat`, we reopen every `ManagedDevice` via `seat.open_device(path)`. Each may return a fresh fd; we replace the held fd and re-acquire master on KMS cards.

## Input

### Held-key release on suspend

Without intervention, a client that owned the keyboard at switch time sees a stuck-down Ctrl, Alt, mouse button, etc. on resume — the user physically releases the keys while we hold no input fd.

Suspend step 4 walks the current `XkbState` for the master keyboard and the per-pointer button mask in `PointerState`, then dispatches synthetic `KeyRelease` / `ButtonRelease` through the normal `core_loop` keyboard/pointer fanout. The input-quiesce barrier (suspend step 2) ensures the snapshot we walk reflects every event the kernel actually delivered — modulo the timeout-race documented in Risks #2.

### XI2 raw events

XI2 raw event listeners (`XI_RawKeyPress` / `XI_RawKeyRelease` / `XI_RawButtonPress` / `XI_RawButtonRelease`) receive device-level events independent of focus and grabs. They are intentionally **not** updated by the synthetic-release path:

- The synthetic releases go through focus-routed fanout, which doesn't touch raw listeners.
- We don't track which keys are "raw-press-without-raw-release" outstanding per listener.
- From a raw-listener client's perspective, the input device went silent during the switch (same as if the user truly stopped typing). On resume, real events resume.

This is a deliberate behavioural choice for MVP. If a real-world client breaks on this, we revisit.

### Triggering the switch from inside yserver

`console.rs` sets the TTY to `K_OFF` (or `K_RAW`), so the kernel keyboard layer does **not** translate Ctrl-Alt-F<N> into VT switches. We detect it in the input thread, **before** evdev → XKB translation:

- Inspect the raw evdev keycode (not the XKB keysym) so the binding survives any keymap remap.
- Match `KEY_F1..KEY_F12` (codes 59..68, 87, 88) with Ctrl + Alt held in the kernel-style modifier mask.
- On match: do not dispatch to clients. Send a new `Hotkey::SwitchVt(N)` from the input thread on the existing input → main channel. The main loop reads it and calls `seat.switch_session(N)`.

**`switch_session` is fire-and-forget.** libseat documents that the call does not guarantee a VT switch will occur. The hotkey path does **not** transition `SeatState`. State transitions happen only via the `disable_seat` callback that arrives some time later (or never, if the switch is rejected by logind/seatd). If `switch_session` returns an error synchronously, we log and stay put.

Pre-translation rather than post-XKB so a paranoid client grabbing the full keyboard cannot race or swallow the switch. Same dispatch layer as the existing Ctrl-Alt-Backspace zap (`input_thread.rs::Hotkey`); the new switch fits naturally as another `Hotkey` variant.

## Testing

### Unit (yserver-core, no DRM, no libseat)

- `seat_state_machine`: exhaustively cover the (state, event) matrix — including `enable_seat`-during-Suspending, `disable_seat`-during-Resuming, double `disable_seat`, and the resume-completion → pending_disable → Suspending edge that bypasses Active. Assert `pending_*` flags set/cleared correctly and submit gates flip at the right edges.
- `held_key_release_on_suspend`: build an `XkbState` with keys depressed and a held pointer button; run the synthesise-release path; assert the correct `KeyRelease` / `ButtonRelease` were emitted (reuse the harness from the Cinnamon grab tests). Assert no XI2 raw event is generated.
- `ctrl_alt_fn_detector`: feed evdev events with Ctrl+Alt+F2 held into the input pre-translation layer; assert `Hotkey::SwitchVt(2)` was emitted and the key event was not forwarded to XKB.
- `input_quiesce_barrier`: drive the input thread with an in-flight event burst, send `Stop`, assert all events drained before `Ack`, then verify the snapshot reflects the final post-drain state. Also assert behaviour on `Stop`-timeout (the snapshot may be stale; we log a WARN and proceed).

### Integration

Two distinct tracks:

**Stub-backend integration (yserver crate, no libseat dep):**

- `TestSeat` impl behind the same trait as `LibseatBackend`. Drive enable/disable callbacks programmatically. Assert end-to-end: input-quiesce → synthesise-release → vkQueueWaitIdle → drop-master → close input fds → ack disable → re-open → master → modeset → repaint. Assert full damage posts on every output on resume.
- `ynest --simulate-vt-switch` debug knob: fires fake `enable_seat` / `disable_seat` into the loop after N seconds. Confirms the resume path doesn't deadlock and the next composite goes through.

**Real-libseat integration (CI with seatd in a container):**

- A test binary links the real libseat-rs crate against a seatd running as a child process inside the test container. Drives a real `switch_session` between two pseudo-VTs and asserts: `disable_seat` callback fires, our ack returns, the next `enable_seat` fires, reopened device fds work for ioctls. Asserts dispatch ordering (call `seat.dispatch()` after fd fires; don't drop callbacks).
- This catches what the stub backend can't: real fd revoke behaviour, real callback ordering, real-error returns from `seat.switch_session`.

### Manual hardware

Each gets one telemetry capture: start yserver from a tty under MATE / Cinnamon → Ctrl-Alt-F2 → confirm getty visible → Ctrl-Alt-F1 → confirm cursor + desktop restored, full-screen damage repaint, zero `vk_device_lost`, zero `missed_pageflips` once `seat_state == Active` again.

- bee (Ryzen 6900HX / RADV)
- yoga (Snapdragon X1 / Turnip)
- silence (rx580 + split-driver iGPU — multi-output added)
- iMac 19,2 (i5-8500 + Polaris / RADV — different amdgpu master-handoff path from yoga's msm)

A rapid-double-switch capture is added to bee's run: Ctrl-Alt-F2 then immediately Ctrl-Alt-F1 (within ~100 ms) to exercise the resume-completion → pending_disable boundary.

### Known false-positive sources

- Held-key release can dispatch to a client that disconnected mid-suspend; `core_loop::process_disconnect` already handles "send to dead client = drop", so we just need to not panic on the release write itself.
- The "no seat manager" test must exercise the Direct path on a system without logind/seatd. The "policy bypass refused" test must exercise the hard-fail path with `LIBSEAT_BACKEND=direct` set.

## Risks

Ordered by likelihood × blast radius.

1. **DRM master not dropped before VT switch fires.** Kernel switches foreground VT while we're still master → new owner can't take master → screen wedges. Mitigation: `libseat_disable_seat()` is the last call in suspend; libseat ensures the underlying VT_RELDISP doesn't fire until after that. If we crash / panic during suspend before the ack, wrap suspend steps in error-tolerant code and ack disable in all paths. Final escape: Ctrl-Alt-Backspace zap.
2. **Input-quiesce barrier times out.** Input thread doesn't ack `Stop` (blocked in a kernel read, or panicked). Mitigation: bounded timeout, then proceed with a best-effort snapshot. On timeout, log `WARN yserver: input quiesce timeout; suspend proceeding with stale snapshot`. Worst case is one or two stuck keys on resume — clients tolerate this far better than a wedged screen.
3. **`vkQueueWaitIdle` hang on suspend.** Hung GPU submit blocks indefinitely. Mitigation: bound the wait with a timeout (already used on shutdown); proceed to drop-master + ack on timeout. Worst case is GPU work continues invisibly on the other VT.
4. **Reopen on resume returns a different device or fails for an essential card.** Hardware actually disappeared mid-suspend (eGPU detached, USB DRM device removed). Mitigation: log loudly and exit; the user sees yserver die rather than wedge. Full recovery is the Vulkan-device-loss follow-up.
5. **Modeset on resume fails on a hot-unplugged output.** User switches VT, unplugs a monitor, switches back. Mitigation: re-query connector state via `drmModeGetResources` (resume step 4); drop missing outputs; fire RandR change-event.
6. **Rapid double-switch races the state machine.** Mitigation: `pending_enable` / `pending_disable` flags; explicit resume-completion / suspend-completion re-checks before committing the next stable state. Tested in the bee hardware capture.
7. **libseat D-Bus connection dies mid-suspend.** Logind crashes are rare but possible — seat would be "suspended forever". Mitigation: on libseat dispatch errors during suspend, log loudly and exit. Reboot.
8. **Held-key release dispatching while focus is mid-transition.** Suspend can race a `SetInputFocus`. Worst case: a release is delivered to the client that was focused at suspend, not the resume target. Acceptable — matches Xorg behaviour.
9. **Hardware / driver without a DRM render node forces Vulkan onto the primary node.** Every KMS driver in yserver's tested matrix (amdgpu, msm, apple_drm/asahi, nouveau, i915 where present, nvidia-drm) exposes a render node via `/dev/dri/renderD12N`, which doesn't participate in master — see "Device fd lifetime contract". Some old or niche drivers don't expose a render node; the Vulkan ICD falls back to the primary node and *does* depend on master. On those, our master drop (whether explicit or via libseat's revoke) invalidates the Vulkan context and we'd see `VK_ERROR_DEVICE_LOST` on the first post-resume submit. Mitigation: if telemetry shows this on a tested host (none expected in the matrix), we either exit-and-restart on resume or implement full Vulkan device-loss recovery as the follow-up.

## Rollout

- Feature branch `feature/vt-switch`.
- Build behind runtime detection (`libseat_open_seat` succeeds); no env knob to disable. libseat absence is the off-switch.
- Land in one PR with spec + plan + impl together. No staged sub-phases.
- After merge, daily-driver hosts (yoga, bee, silence) get logind-managed sessions, so VT switching becomes available transparently the next time yserver starts.

## Follow-ups unblocked by this work

- **Hot-plug / hot-unplug while suspended.** Subscribe to udev (as wlroots does in `handle_udev_event`) to detect new DRM cards / outputs / input devices during the suspended period; integrate them on resume.
- **Host suspend-to-RAM** (laptop lid close on yoga). Kernel sends device-removed-and-re-added events that look architecturally similar to a VT switch; the resume path is reusable.
- **Full Vulkan device-loss recovery** (for the missing-device-on-resume case, real backend-driven device-loss, or `VK_ERROR_DEVICE_LOST` from any source).
- **Multi-seat** (one yserver per seat). libseat already takes a seat name parameter; defaults to `seat0`.
- **XKB `XF86Switch_VT_N` action wiring**, for clients that want to remap switch keys.
- **`Session.SetType("x11")` D-Bus call to logind** for full session-type reporting.
- **XI2 raw-listener coherence across suspend**, if a real client surfaces a complaint.
