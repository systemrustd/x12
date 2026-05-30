# DPMS Design

**Goal:** implement the X11 DPMS extension on `yserver-hw` (KMS backend) end
to end — protocol, idle timer, input-driven wake, and atomic
CRTC/connector/plane disable on the modern KMS path — so `xset dpms`,
MATE/Cinnamon idle handlers, and `xscreensaver` all behave the way they
do on Xorg.

**Non-goals:**

- ynest hardware integration. ynest is not an end-goal; the *backend
  hook* (`set_dpms_power`) on host_x11 is a no-op (we don't power-manage
  the host display through a nested server). The X11 protocol state
  machine still runs normally so clients don't error — `DPMSCapable`
  returns `false` (matching Xorg's `screen->DPMS == NULL` reporting),
  `enabled` starts `false` (mirrors Xorg's `DPMSEnabled =
  DPMSSupported()` at init, `Xext/dpms.c:587`), and `DPMSForceLevel`
  returns `BadMatch` while disabled (Xorg `:428`). A client that calls
  `DPMSEnable` *can* flip `enabled = true` (matches Xorg, which does
  not gate Enable on hardware support), after which protocol-level
  transitions and notify firings still happen — but the host display
  is untouched and the idle timer doesn't fire (see "Idle timer" below,
  which gates the deadline on the frozen `kms_capable` field so ynest doesn't spin
  through notify cascades for no visible reason).
- SCREEN-SAVER extension. yserver only has core opcodes 107/108/115 stubbed
  (`process_request.rs:205, 206, 213`); the bitmap saver doesn't exist, and
  Xorg's `dixSaveScreens` coordination has nothing to coordinate. The idle
  timer added here is reusable when SCREEN-SAVER lands.
- Per-screen DPMS state. Xorg's protocol is global (one
  `DPMSEnabled`/`DPMSPowerLevel` for the server) — mirror that. Per-output
  power-down (e.g. dim only the laptop panel while keeping HDMI on) is a
  future feature; not what `xset dpms` drives.
- Cursor-on-wake gymnastics. Xorg modesetting does nothing special; the
  kernel preserves cursor state across DPMS transitions and yserver's
  legacy-ioctl cursor path (`feedback_cursor_legacy_ioctl_lesson`) is
  independent of the atomic path used for DPMS. No-op.

**Spec reference:** DPMS Extension Specification 1.2 (`/usr/include/X11/extensions/dpmsproto.h`)
and `/home/jos/Projects/xserver/Xext/dpms.c` as the de-facto behavior reference
(see `feedback_xorg_is_the_de_facto_spec`).

---

## Architecture

### Extension shape (matches Xorg)

Nine minor opcodes, one GenericEvent:

| Minor | Request | Semantics |
|------:|---------|-----------|
| 0 | `DPMSGetVersion` | Returns (1, 2). |
| 1 | `DPMSCapable` | Always `true` on yserver-hw, `false` on ynest. |
| 2 | `DPMSGetTimeouts` | Returns `(standby, suspend, off)` in seconds (CARD16). |
| 3 | `DPMSSetTimeouts` | Validates `off >= suspend >= standby` when non-zero, else `BadValue`. Stores ms internally. |
| 4 | `DPMSEnable` | If `was_enabled` was `false`, set `enabled = true` and emit `DPMSInfoNotify` (Xorg `Xext/dpms.c:395-398`). Does not call the backend; level stays where it was. |
| 5 | `DPMSDisable` | Call the transition helper to On (which emits notify iff level changed), then set `enabled = false`; if `was_enabled`, emit a second notify for the enabled-change. Two notifies are possible when called from Off (Xorg `Xext/dpms.c:404-418`). |
| 6 | `DPMSForceLevel` | `BadMatch` if `!enabled`; `BadValue` if level ∉ {0,1,2,3}; calls transition helper. |
| 7 | `DPMSInfo` | Returns `(power_level, state)`. |
| 8 | `DPMSSelectInput` | `event_mask` must be 0 or `DPMSInfoNotifyMask` (bit 0), else `BadValue`. |

Internal yserver major opcode: **134**. The static registry in
`crates/yserver-core/src/nested.rs:21-77` has several free slots in
128-149 (129, 131, 132, 134); 134 is what we pick — the important
constraint is no collision in yserver, not "first free" (extension
opcodes are server-assigned, not spec-required).

DPMS registers itself as a GenericEvent extension keyed by its major
opcode (134) — same plumbing yserver already uses for Present via
`GE_MAJOR_OPCODE = 138` (`crates/yserver-core/src/nested.rs:29`). One
event type: `DPMSInfoNotify` (XGE `evtype = 0`). Wire layout per
`/usr/include/X11/extensions/dpmsproto.h` `xDPMSInfoNotifyEvent`:
12-byte GenericEvent header (`type=GenericEvent` 1B, `extension=134`
1B, `sequence` 2B, `length=0` 4B, `evtype=0` 2B, `pad0` 2B), then
`timestamp:u32` at byte 12, `power_level:u16` at byte 16, `state:u8`
at byte 18, then 13 bytes of pad — total 32 bytes.

Notify is emitted whenever **either** `power_level` **or** `enabled`
changes, to clients that called `DPMSSelectInput(DPMSInfoNotifyMask)`.
A single client request can emit two notifies (e.g. `DPMSDisable` from
Off: one for Off→On, one for enabled true→false — see row 5 above).

### State model — global on ServerState

```rust
// crates/yserver-core/src/server.rs (add near other ext state)
pub struct DpmsState {
    pub kms_capable: bool,     // snapshot of backend.dpms_capable() at init; frozen.
    pub enabled: bool,         // initialized to kms_capable; clients can toggle via Enable/Disable.
    pub power_level: u8,       // 0=On, 1=Standby, 2=Suspend, 3=Off
    pub standby_ms: u32,       // 0 = disabled (no transition to this level)
    pub suspend_ms: u32,
    pub off_ms: u32,
    pub last_activity: Instant,
    pub selected_by: HashSet<ClientId>,  // clients with DPMSSelectInput(DPMSInfoNotifyMask)
}
```

Default timeouts at init mirror Xorg's defaults: `standby=600s`,
`suspend=600s`, `off=600s`. (Xorg uses `ScreenSaverTime` for all three when
no explicit values were set; 600s is its default.)

`enabled` starts `true` on yserver-hw (the canonical case), `false` on
ynest. The backend's capabilities flag drives this at extension init.

### Idle timer

A new `last_input_event_time: Instant` field on `ServerState` (placed in
`DpmsState.last_activity` above) is touched in:

- `key_event_fanout_to_state` (`core_loop/key_fanout.rs:39`)
- `pointer_event_fanout_to_state` (companion function in
  `core_loop/pointer_fanout.rs`)
- The XTest synthetic-input path (so `xdotool key` wakes the screen — matches
  Xorg).

The core loop's poll-deadline computation in `core_loop/run.rs` already
computes a minimum of several deadlines (key-repeat, present-completion,
etc.). Add one more: the next DPMS transition deadline. When poll wakes,
evaluate transitions.

All timeouts are **absolute from last input** (Xorg `os/WaitFor.c:434-451`),
not per-state dwell time. With `idle_ms = (now - last_activity).as_millis()`:

```rust
fn next_level(power_level: u8, idle_ms: u32, dpms: &DpmsState) -> u8 {
    // Highest-first so we can leapfrog: if standby=suspend=off=600s,
    // a single 600s idle goes directly to Off, not Standby.
    // Matches Xorg's DPMS_CHECK_MODE evaluation order at
    // /home/jos/Projects/xserver/os/WaitFor.c:446-448.
    if power_level < DPMS_MODE_OFF     && dpms.off_ms     > 0 && idle_ms >= dpms.off_ms     { return DPMS_MODE_OFF; }
    if power_level < DPMS_MODE_SUSPEND && dpms.suspend_ms > 0 && idle_ms >= dpms.suspend_ms { return DPMS_MODE_SUSPEND; }
    if power_level < DPMS_MODE_STANDBY && dpms.standby_ms > 0 && idle_ms >= dpms.standby_ms { return DPMS_MODE_STANDBY; }
    power_level
}
```

**Zero-skipping:** a zero timeout disables *that* level but does not halt
the cascade. Example: `standby=0, suspend=900, off=0` from On → after 900s
idle, transition to Suspend (skipping Standby). From Suspend → nothing more
(off=0 disabled). Matches Xorg `os/WaitFor.c:403-410` where
`DPMS_CHECK_MODE` short-circuits on `time > 0`.

The deadline for poll is the soonest moment a future level could fire:

```rust
fn dpms_transition_deadline(state: &ServerState) -> Option<Instant> {
    // Gated on both `enabled` AND `kms_capable` so that a ynest client
    // who calls DPMSEnable doesn't spin up an idle timer that has no
    // backend to drive. On yserver-hw, kms_capable is true so this
    // collapses to the usual `enabled` check.
    if !state.dpms.enabled || !state.dpms.kms_capable { return None; }
    let mut next: Option<u32> = None;
    let lvl = state.dpms.power_level;
    let push = |next: &mut Option<u32>, ms: u32| {
        if ms > 0 { *next = Some(next.map_or(ms, |n| n.min(ms))); }
    };
    if lvl < DPMS_MODE_STANDBY { push(&mut next, state.dpms.standby_ms); }
    if lvl < DPMS_MODE_SUSPEND { push(&mut next, state.dpms.suspend_ms); }
    if lvl < DPMS_MODE_OFF     { push(&mut next, state.dpms.off_ms); }
    Some(state.dpms.last_activity + Duration::from_millis(next? as u64))
}
```

(Picking the **smallest** non-zero timeout among levels above the current
gives the earliest possible deadline. The cascade evaluator then leapfrogs
to the highest level whose absolute deadline has expired.)

When the resulting `target != power_level`, transition: call
`apply_dpms_transition(state, backend, target)`.

### Input-driven wake

In the same fanout functions where `last_activity` is updated, check
`if dpms.power_level != On { apply_dpms_transition(state, backend, On) }`
*before* fanning out the event. Policy reasoning: we want the connector
re-lit before the client sees its first event of the resumed session
so a focused window's repaint lands on a visible scanout.

This means input fanout is **synchronous on the wake atomic commit**: a
key/pointer event that arrives in Off will block until `commit_modeset`
returns. Acceptable — the commit is microseconds in the common path and
the alternative (deliver event, lag the screen) is worse UX. The
transition helper logs and continues on backend `Err`; the input event
is still fanned out so the user isn't locked out if the kernel is
unhappy. (See "Backend hook" below for the exact error contract.)

Xorg's input-wake path is split across input drivers and the core
event-processing loop; the spec for yserver is "any input event resets
DPMS to On before fanout" — simpler than tracing the exact Xorg call
graph, and behaviourally equivalent.

### Backend hook

Add to the `Backend` trait (`crates/yserver-core/src/backend/trait_def.rs`):

```rust
fn dpms_capable(&self) -> bool { false }
fn set_dpms_power(&mut self, _level: u8) -> io::Result<()> { Ok(()) }
```

Defaults are the ynest behaviour. yserver-hw overrides both:

- `dpms_capable() = true`
- `set_dpms_power(level)`: collapse `level` to a binary KMS state — `On`
  → outputs active; `Standby`/`Suspend`/`Off` → outputs inactive — and
  only act when the binary state changes. Track a cached
  `kms_outputs_active: bool` on the backend.

  **Critical: must mirror `KmsBackendV2::run_suspend` and
  `run_resume`'s drain/rearm sequence around the actual
  disable/commit. The minimal "just call disable_output / commit_modeset"
  shape originally specified is INSUFFICIENT — it leaves the cursor
  plane bound to a now-disabled CRTC and orphans in-flight page-flips,
  producing an EINVAL storm on the first page-flip after wake
  ([[project_einval_atomic_commit_storm_wedge]]). The DPMS off/on
  sequence is effectively a "soft suspend/resume" that doesn't touch
  libseat but otherwise needs the same care.**

  - **inactive transition** (any non-On from active), in order:
    1. `self.platform.wait_idle_bounded()` — let in-flight GPU work
       finish before the kernel disables the CRTC.
    2. `self.scene.drain_all(&mut self.platform)` — drain pending
       page-flip acks, release pool slots, **reset per-output cursor
       state** (this is the cursor-plane-hide step that prevents EINVAL
       on the next wake). Mirrors `run_suspend:3551`.
    3. `self.platform.reset_scanout_bos_for_suspend()` — force every BO
       back to Free; otherwise the orphaned Pending/OnScreen entries
       leak each cycle. Mirrors `run_suspend:3563`.
    4. `drm::modeset::disable_output(device, output)` for each output
       (`crates/yserver/src/drm/modeset.rs:516-535`).
  - **active transition** (On from inactive), in order:
    1. `drm::modeset::commit_modeset(device, output, fb_id)` for each
       output (`crates/yserver/src/drm/modeset.rs:537-602`),
       re-establishing `MODE_ID`, plane `FB_ID`/`CRTC_ID`, `ACTIVE=1`,
       plane SRC/CRTC rects.
    2. `self.platform.rearm_cursor(hot_x, hot_y, cx, cy)` — re-bind the
       cursor plane to each visible CRTC via the legacy ioctl. Mirrors
       `run_resume:3661-3667`. Use the same hotspot+position fetch as
       `run_resume`: `self.effective_cursor_xid →
       self.cursor_records.get(&xid).map(|r| (r.hot_x, r.hot_y))` plus
       `self.core.cursor_x/y as i32`.
    3. `self.scene.wake_for_damage()` so the next composite tick paints
       a fresh full frame.
  - **same binary state** (e.g. Standby → Suspend, both inactive): no
    KMS work; only the in-memory `power_level` and notify advance.

  **Borrow-check consequence:** the orchestration accesses `platform`,
  `scene`, AND `core` (for cursor pos) on the same `&mut self`. The
  cleanest restructure is to keep this in `KmsBackendV2::set_dpms_power`
  itself (not buried in `PlatformBackend`). The PlatformBackend can
  still own the per-output disable_output / commit_modeset loops; the
  backend method orchestrates around them.

**Why not the connector `"DPMS"` property?** Xorg modesetting's atomic
path explicitly does *not* set the connector DPMS property — it
toggles connector `CRTC_ID`, CRTC `ACTIVE`/`MODE_ID`, and plane state
(`hw/xfree86/drivers/modesetting/drmmode_display.c:686-736`). wlroots'
atomic path agrees (`backend/drm/atomic.c:556-628`); both reserve the
connector DPMS property for their *legacy* fallback. The atomic
disable/enable model is the modern, kernel-blessed approach, and it
maps cleanly onto yserver's existing `disable_output`/`commit_modeset`
helpers — no new DRM plumbing.

**Why collapse Standby/Suspend/Off to one KMS state?** DRM-KMS has no
intermediate active state below "CRTC fully off". The protocol-level
`power_level` is preserved exactly (clients querying `DPMSInfo` see
what they last requested), but the kernel doesn't need to know — it
just knows "on" or "off". This matches what Xorg modesetting effectively
does when atomic.

**Error contract** (matches Xorg `Xext/dpms.c:262-293` semantics):
`apply_dpms_transition` updates the in-memory `power_level`, fans out
notify, and resumes/pauses the flip loop **regardless** of whether
`set_dpms_power` returned `Ok` or `Err`. The backend logs at `error!`
on failure; clients see the level they requested. This avoids bricking
`xset dpms` on a transient EBUSY and matches Xorg's "set it anyway"
posture. Concretely: the helper does **not** use `?` between the
backend call and the state mutation.

### Page-flip gating

When `dpms.power_level != On`, the v2 compositor must not submit
page-flips. With `ACTIVE=0` on the CRTC, a flip submitted via
`crates/yserver/src/drm/page_flip.rs:25` would be rejected by the
kernel (EBUSY or EINVAL) and never deliver a `DRM_EVENT_FLIP_COMPLETE`,
which would wedge the compositor's flip-pending bookkeeping.

The KMS backend gates submission at `composite_and_flip` in
`crates/yserver/src/kms/v2/backend.rs`: if `dpms.power_level != On`,
short-circuit before submission. The composite state stays buffered;
on wake, `set_dpms_power(On)` marks the scene dirty (so the next
composite tick produces a full frame) and the compositor naturally
resumes.

No cursor-path changes: the cursor uses legacy ioctls
(`feedback_cursor_legacy_ioctl_lesson`); kernel preserves cursor
visibility across CRTC disable/enable.

### VT switching interaction

Xorg forces DPMS to `On` on VT-leave
(`hw/xfree86/common/xf86Events.c:358-360`) so the session that takes
over the VT (or the user when switching back) lands on a sane state.

yserver's VT-leave path (the in-flight libseat work tracked under
[`docs/superpowers/plans/2026-05-28-vt-switching.md`](../plans/2026-05-28-vt-switching.md))
hands DRM master off, so any KMS commit during/after VT-leave is moot.
The right shape here is **in-memory only**: on VT-leave, reset
`dpms.power_level = On` and `last_activity = Instant::now()` so the
post-resume modeset path comes up On and the idle timer restarts
fresh. No backend call (we don't have master anyway). No notify either
— Xorg sends one because its `DPMSSet` always notifies on transitions,
but during VT-leave no clients are getting events anyway and the
post-resume state is "On from the user's perspective."

Wire this into the libseat suspend handler whichever it lands as —
spec-only for now (this hook is a single line of state mutation; the
actual call site lives with VT-switching).

---

## Components

### `crates/yserver-protocol/src/x11/dpms.rs` (new)

Wire codecs:

- `parse_set_timeouts_request(body) -> Option<(u16, u16, u16)>`
- `parse_force_level_request(body) -> Option<u16>`
- `parse_select_input_request(body) -> Option<u32>`
- `encode_get_version_reply(out, seq, order, major, minor)`
- `encode_capable_reply(out, seq, order, capable: bool)`
- `encode_get_timeouts_reply(out, seq, order, standby, suspend, off)`
- `encode_info_reply(out, seq, order, power_level: u16, state: bool)`
- `encode_dpms_info_notify_event(out, seq, order, ge_opcode: u8, timestamp: u32, power_level: u16, state: bool)`

All replies are 32 bytes (8-byte header + payload + pad). The GenericEvent
is 32 bytes total: 12-byte GenericEvent header (`type` 1B, `extension` 1B,
`sequence` 2B, `length=0` 4B, `evtype` 2B, `pad` 2B) + 20 bytes payload/pad
(`timestamp` 4B at offset 12, `power_level` 2B at offset 16, `state` 1B at
offset 18, then 13 bytes of pad). See the protocol section above for the
full wire layout.

Constants:

```rust
pub const DPMS_INFO_NOTIFY_MASK: u32 = 0x0000_0001;
pub const DPMS_MODE_ON: u16 = 0;
pub const DPMS_MODE_STANDBY: u16 = 1;
pub const DPMS_MODE_SUSPEND: u16 = 2;
pub const DPMS_MODE_OFF: u16 = 3;
```

### `crates/yserver-core/src/nested.rs`

Add to the const block at `:21-77`:

```rust
const DPMS_MAJOR_OPCODE: u8 = 134;
```

Add to `EXTENSIONS` at `:110`:

```rust
ExtensionMetadata {
    name: "DPMS",
    major_opcode: DPMS_MAJOR_OPCODE,
    first_event: 0,
    event_count: 0,            // uses XGE
    first_error: 0,            // no errors of its own; uses core BadValue/BadMatch
    availability: ExtensionAvailability::Always,
    unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
},
```

### `crates/yserver-core/src/server.rs`

Add `DpmsState` (struct above) and embed in `ServerState` at `:198+`:

```rust
pub dpms: DpmsState,
```

Initialize in `ServerState::new(...)` with `kms_capable =
backend.dpms_capable()` snapshotted once, `enabled = kms_capable`
(matching Xorg's `DPMSEnabled = DPMSSupported()` at init), default
timeouts `600s` × 3, `power_level = On`, `last_activity = Instant::now()`,
empty `selected_by`. `kms_capable` is never updated after init.

Add helper `dpms_transition_deadline` (body shown in the
"Idle timer" section above — picks the smallest non-zero timeout above
the current level, returns `None` if every higher level is disabled or
DPMS is off).

Add helper `next_dpms_level` (body in same section — highest-first
evaluation with zero-skip).

### `crates/yserver-core/src/core_loop/process_request.rs`

Add a case for opcode 134 next to the existing extension dispatchers
(`:373, 375`):

```rust
134 => handle_dpms_request(state, backend, client_id, sequence, header, body),
```

Implement `handle_dpms_request(...)` modeled on `handle_shape_request`
(`:2627`). Minor-opcode switch covers all 9 requests.

Two helpers — one for level transitions, one for the enable/disable axis,
to match Xorg's two-notify case from `DPMSDisable`-while-Off:

```rust
// Returns Ok regardless of backend result; logs backend errors.
fn apply_dpms_transition(state, backend, new_level: u8) {
    let old = state.dpms.power_level;
    state.dpms.power_level = new_level;
    if let Err(e) = backend.set_dpms_power(new_level) {
        log::error!("set_dpms_power({new_level}) failed: {e}");
        // state still advances — see "Backend hook / Error contract".
    }
    if new_level != old {
        emit_dpms_notify(state);
    }
}

fn emit_dpms_notify(state) {
    let ts = state.timestamp_now();
    let lvl = state.dpms.power_level;
    let st = state.dpms.enabled;
    for client_id in state.dpms.selected_by.clone() {
        write_dpms_info_notify_to_client(state, client_id, ts, lvl, st);
    }
}
```

Request dispatchers (cribbed from Xorg `Xext/dpms.c:388-440`):

- `DPMSEnable`: if `!was_enabled` { `enabled = true; emit_dpms_notify`; }
- `DPMSDisable`: `was_enabled = dpms.enabled; apply_dpms_transition(On); enabled = false; if was_enabled { emit_dpms_notify; }` — possibly two notifies in one request.
- `DPMSForceLevel`: validate, `apply_dpms_transition(level)`.

The notify fanout reuses the GenericEvent shape — yserver already has
`GE_MAJOR_OPCODE = 138` plumbed for Present, so the encoder just takes the
DPMS major opcode (134) as `extension_opcode`.

### `crates/yserver-core/src/core_loop/{key,pointer}_fanout.rs`

At the top of `key_event_fanout_to_state` (`:39`) and the equivalent in
`pointer_fanout.rs`, before any other work:

```rust
state.dpms.last_activity = Instant::now();
if state.dpms.enabled && state.dpms.power_level != DPMS_MODE_ON {
    apply_dpms_transition(state, backend, DPMS_MODE_ON);
}
```

The `backend: &mut dyn Backend` parameter is already in scope at the fanout
callsite (both fanouts run inside the core-loop tick that owns the backend).
The fanout function itself currently takes only `state` — push `backend`
through the signature; it's already available at every caller. Note this
makes the fanouts synchronous on the KMS commit when waking from Off; see
"Input-driven wake" above for why that's the right tradeoff.

XTest synthetic-event path (`process_request.rs:` search for `XTest`): same
treatment — DPMS treats synthetic input as activity (Xorg does).

### `crates/yserver-core/src/core_loop/run.rs`

In the poll-deadline computation (search for `next_wakeup`/`next_fire`
around `:380+`), include `state.dpms_transition_deadline()` in the
minimum. After poll returns, between the existing key-repeat and
present-completion blocks, add:

```rust
if let Some(deadline) = state.dpms_transition_deadline()
    && Instant::now() >= deadline
{
    let idle_ms = state.dpms.last_activity.elapsed().as_millis() as u32;
    let target = next_dpms_level(state.dpms.power_level, idle_ms, &state.dpms);
    if target != state.dpms.power_level {
        apply_dpms_transition(&mut state, &mut *backend, target);
    }
}
```

(`apply_dpms_transition` returns unit — backend errors are logged, not
propagated. See "Backend hook / Error contract".)

### `crates/yserver-core/src/backend/trait_def.rs`

Add at `:260+` (alongside other capability-style methods):

```rust
fn dpms_capable(&self) -> bool { false }
fn set_dpms_power(&mut self, _level: u8) -> io::Result<()> { Ok(()) }
```

### `crates/yserver/src/kms/backend.rs` (yserver-hw)

`dpms_capable` returns `true`.

`set_dpms_power(level)`:

1. Compute `want_active = level == DPMS_MODE_ON`.
2. If `want_active == self.kms_outputs_active` (already in the right
   binary state — same-binary-state case from "Backend hook"), return
   `Ok(())` — nothing to do.
3. If becoming inactive: for each output, call
   `drm::modeset::disable_output(&self.device, output)`
   (`crates/yserver/src/drm/modeset.rs:516-535`). Collect the first
   error; continue to disable the rest so we don't leave half the
   system lit. Set `kms_outputs_active = false`.
4. If becoming active: for each output, call
   `drm::modeset::commit_modeset(&self.device, output, current_fb_id)`
   (`crates/yserver/src/drm/modeset.rs:537-602`) with the same `fb_id`
   the compositor will paint into next (the existing scanout pool's
   current frame). Set `kms_outputs_active = true`, then call the v2
   backend's "mark scene fully dirty" hook so the next composite tick
   produces a complete frame.
5. Return the first collected error (caller logs and advances state
   anyway).

Add `kms_outputs_active: bool` to the KMS backend struct (initial
`true` — outputs come up active at startup).

No interaction with the cursor plane (cursor is on the legacy ioctl path —
see `feedback_cursor_legacy_ioctl_lesson`).

### `crates/yserver/src/kms/v2/backend.rs` — page-flip gating

In the v2 backend's `composite_and_flip` (or whatever the current
function name is for the submit path that calls into
`crates/yserver/src/drm/page_flip.rs`), short-circuit before submission
when `state.dpms.power_level != DPMS_MODE_ON`. Specifically:

```rust
fn composite_and_flip(&mut self, state: &mut ServerState) -> io::Result<()> {
    if state.dpms.power_level != DPMS_MODE_ON {
        return Ok(());   // outputs inactive, flip would EBUSY
    }
    // ...existing path...
}
```

When `set_dpms_power(On)` runs it has already called `commit_modeset` and
set `kms_outputs_active = true`; mark the scene fully dirty so the
next tick's `composite_and_flip` sees the gate open and produces a
fresh full frame.

### `crates/yserver/src/host_x11/...` (ynest)

Defaults from the trait suffice. ynest reports `dpms_capable = false`,
so init snapshots `kms_capable = false` and starts with `enabled =
false`. Protocol-level state still runs per the rules in "Non-goals"
above: `DPMSEnable` flips `enabled = true`, `DPMSForceLevel` then
exercises the protocol-level state machine and emits notifies, but
the *idle timer* never fires (gated on `kms_capable`) and the backend
hook stays a no-op. Net effect: ynest is protocol-compliant without
doing anything to the host display.

---

## Data flow

### Idle path (yserver-hw, leapfrog cascade)

```
config: standby=300s, suspend=600s, off=900s; power_level=On at t=0
t=0:    last input
  deadline = last_activity + 300s   (smallest non-zero above On)
t=300:  poll wakes
  idle=300s → next_level(On, 300s) = Standby
  apply_dpms_transition(Standby):
    power_level = Standby           ← state first (page-flip gate trips
                                       immediately so any tick between
                                       here and the backend call doesn't
                                       try to submit a flip)
    backend.set_dpms_power(1):
      want_active=false, was active → for each output: disable_output()
      kms_outputs_active = false
    notify (Standby, enabled=true)
  deadline = last_activity + 600s
t=600:  poll wakes
  idle=600s → next_level(Standby, 600s) = Suspend
  apply_dpms_transition(Suspend):
    power_level = Suspend
    backend.set_dpms_power(2): want_active=false, already inactive → no-op
    notify
  …
t=900:  → Off, same KMS no-op.

Equal-timeout case: standby=suspend=off=600
  deadline = +600s; at t=600 next_level returns Off directly
  (highest-first evaluation); single transition, one notify.

Zero-skip case: standby=0, suspend=900, off=0; power_level=On
  deadline = +900s; at t=900 next_level returns Suspend; no further deadline.
```

### Wake path (any non-On → On)

```
HostInputEvent arrives
  → {key,pointer}_event_fanout_to_state(state, backend, ev)
      → state.dpms.last_activity = now
      → if power_level != On:
            apply_dpms_transition(state, backend, On)
                → power_level = On                  ← state first
                → backend.set_dpms_power(0):
                    want_active=true, was inactive → for each output:
                      commit_modeset() with current scanout fb
                    kms_outputs_active = true
                    mark scene fully dirty
                → notify (On, enabled=true)
      → existing event fanout proceeds normally
      → next composite_and_flip tick sees the gate open and paints a full frame
```

### Explicit request path (DPMSForceLevel)

```
client → opcode 134, minor 6, level=3
  → handle_dpms_request
  → if !state.dpms.enabled: BadMatch
  → if level > 3: BadValue
  → apply_dpms_transition(state, backend, level)
  → no reply (it's a void request)
```

### Notify subscription path

```
client → opcode 134, minor 8, event_mask=1
  → handle_dpms_request
  → if event_mask & !1 != 0: BadValue
  → if event_mask == 1: state.dpms.selected_by.insert(client_id)
    else:                 state.dpms.selected_by.remove(&client_id)
  → no reply

client disconnect:
  → process_disconnect (crates/yserver-core/src/core_loop/process_disconnect.rs:81)
    adds state.dpms.selected_by.remove(&client_id)
```

---

## Error handling

| Condition | Behavior |
|-----------|----------|
| `DPMSForceLevel` while `!enabled` | `BadMatch` (Xorg `Xext/dpms.c:428`). |
| `DPMSForceLevel(level > 3)` | `BadValue`. |
| `DPMSSetTimeouts` where any non-zero pair violates `off ≥ suspend ≥ standby` | `BadValue` (Xorg `:370-376`). Zero values are sentinels for "this level disabled" and bypass the ordering check. |
| `DPMSSelectInput(mask & !1 != 0)` | `BadValue`. |
| Unknown minor opcode | Sequence-numbered `BadRequest`, per existing extension fallback. |
| `set_dpms_power` returns `Err` from atomic commit | Log `error!`, but advance the in-memory state and fan out the notify anyway (matches Xorg `Xext/dpms.c:262-293`). The client's view tracks "what was requested"; recovery is the kernel's job, and a permanent failure would brick `xset dpms`. `apply_dpms_transition` does **not** propagate the error to its caller — see "Backend hook / Error contract". |
| ynest `DPMSCapable` | Returns `false`. `enabled` starts `false`, so `DPMSForceLevel` returns `BadMatch` unless a client first calls `DPMSEnable` (matches Xorg `enabled = DPMSSupported()` at init). After `DPMSEnable`, requests still run the protocol state machine and emit notifies, but the idle timer doesn't fire (gated on `kms_capable`) and the `set_dpms_power` backend hook is a no-op. |
| Client disconnects while in `selected_by` | Removed in `process_disconnect` (`crates/yserver-core/src/core_loop/process_disconnect.rs:81`), alongside the existing per-resource cleanup. DPMS selection is global server state (not resource-keyed), so this is a single line added to that function — easy to miss if you only look at the resource-deletion paths. |
| VT-leave with `power_level != On` | In-memory state reset to On (and `last_activity = now`) in the VT-leave hook; no KMS commit, no notify. Post-resume modeset comes up On naturally. See "VT switching interaction". |

---

## Testing

### Unit tests

`crates/yserver-protocol/src/x11/dpms.rs::tests`:

1. `parse_set_timeouts_round_trip` (proptest) — encode/decode wire round-trip for the three CARD16 fields in both byte orders.
2. `set_timeouts_validates_ordering` — `off >= suspend >= standby` must hold for non-zero values; document zero as "disabled".
3. `info_reply_shape` — encode known values, assert byte layout: 1=reply, sequence at 2..4, length at 4..8 = 0, power_level at 8..10, state at 10, 21 bytes pad.
4. `info_notify_event_shape` — encode known values, assert XGE header (35, extension=134, sequence, length=0, event_type=0) plus payload (timestamp at 12..16, power_level at 16..18, state at 18, pad).
5. `force_level_parses_all_four_levels` and `force_level_rejects_above_3`.

`crates/yserver-core/src/server.rs::tests` (alongside the `subscribers` tests):

6. `dpms_transition_deadline_picks_smallest_non_zero` — On with `standby_ms=300_000, suspend_ms=600_000, off_ms=900_000` → deadline = `last_activity + 300s`. Standby with same config → deadline = `last_activity + 600s`. Suspend → `last_activity + 900s`. Off → `None`.
7. `dpms_transition_deadline_returns_none_when_disabled` — `enabled=false` → `None` regardless of timeouts.
8. `dpms_transition_deadline_zero_skips_not_halts` — `standby_ms=0, suspend_ms=900_000, off_ms=0` from On → `Some(last_activity + 900s)` (Suspend's timeout); same config from Suspend → `None`.
9. `next_dpms_level_leapfrogs_on_equal_timeouts` — `standby=suspend=off=600_000`, On, idle=600_000ms → returns Off (highest expired wins, matches Xorg).
10. `next_dpms_level_skips_zero_levels` — `standby=0, suspend=900_000, off=0`, On, idle=900_000ms → Suspend (off=0 skipped).
11. `next_dpms_level_stable_when_under_threshold` — any level, idle=0 → same level.

`crates/yserver-core/src/core_loop/key_fanout.rs::tests`:

12. `key_event_resets_dpms_last_activity` — pre-set `last_activity` to 10s ago; inject key event; assert `last_activity` is now within 100ms of `Instant::now()`.
13. `key_event_during_off_calls_set_dpms_power_with_on` — `MockBackend` that records `set_dpms_power` calls; set `power_level = Off`; inject key event; assert backend got `set_dpms_power(0)` exactly once, *before* the fanout writes any bytes to the client streams.
14. `key_event_during_off_with_backend_error_still_fans_out` — `MockBackend` that returns `Err` from `set_dpms_power`; assert `power_level` advances to On in state, the input event is still fanned out, and a notify is still queued.

`crates/yserver-core/src/core_loop/pointer_fanout.rs::tests`: same three as #12, #13, #14 for pointer.

`crates/yserver-core/src/core_loop/process_disconnect.rs::tests`:

15. `disconnect_removes_client_from_dpms_selected_by` — add a client to `state.dpms.selected_by`, run `process_disconnect`, assert it's gone.

DPMS dispatch / two-notify case:

16. `dpms_disable_from_off_emits_two_notifies` — set state `enabled=true, power_level=Off`; run the DPMSDisable handler; assert two `DPMSInfoNotify` writes to the subscribed test client: first `(On, true)` then `(On, false)`.
17. `dpms_disable_from_on_emits_one_notify` — `enabled=true, power_level=On`; DPMSDisable; assert exactly one notify `(On, false)` (no level change → no level-transition notify).
18. `dpms_enable_when_already_enabled_emits_no_notify` — `enabled=true`; DPMSEnable; zero notifies.
19. `dpms_force_level_rejects_when_disabled` — `enabled=false`; ForceLevel(Off); assert BadMatch and no state change.

### Integration (smoke) tests

Run on `bee` from `just startx` (per
`project_startx_recipe`, `feedback_hw_recipes_user_only`):

- `xset q` — reports the timeouts yserver returns.
- `xset dpms 5 10 15` then `xset q` — round-trips the new timeouts.
- `xset dpms force off` — screen blanks within ~1s; mouse motion brings it back.
- Idle for `standby_ms + 1s` with no input — screen transitions. Verify via the `target/rc-logs/` log capturing the `DPMSInfoNotify` emissions, or with a small `xinput`-style helper that subscribes to the event.

(Visible smoke is required for the wake path per
`feedback_tests_are_not_visible_evidence` — the test-green won't tell you
whether the kernel actually blanked the panel.)

### Expected counts

| Crate              | Before | After |
|--------------------|--------|-------|
| `yserver-core`     | tbd    | +5    |
| `yserver-protocol` | tbd    | +5    |

---

## Implementation staging

Each commit compiles, passes its tests, ends with `cargo +nightly fmt`,
`cargo clippy`, `cargo test`. Per
`feedback_clippy_pedantic_default`, plain clippy only.

1. **Protocol wire codecs.** Add `yserver-protocol/src/x11/dpms.rs` with
   parsers, encoders, constants, and tests #1–#5. No callers yet.
2. **Backend trait + ynest default.** Add `dpms_capable` and
   `set_dpms_power` to `Backend` with the no-op defaults. Touch nothing else.
3. **DpmsState + deadline/next-level helpers + ServerState integration.**
   Add the struct, embed in `ServerState`, initialize in `ServerState::new`,
   add `dpms_transition_deadline` and `next_dpms_level`, ship tests #6–#11.
4. **KMS backend implementation + page-flip gate.** Override
   `dpms_capable` and `set_dpms_power` on the KMS backend, calling
   `drm::modeset::disable_output` / `commit_modeset`. Add the
   `kms_outputs_active` cache. In the v2 backend's compositing path, add
   the `power_level != On` short-circuit. Add the "mark scene fully dirty"
   call on On-transition. Visible smoke `xset dpms force off` happens
   *after* commit 6 (the dispatcher) lands.
5. **Fanout integration + apply_dpms_transition + emit_dpms_notify.**
   Thread `backend` through `key_event_fanout_to_state` and
   `pointer_event_fanout_to_state`, write the wake path with the
   error-tolerant transition helper. Add tests #12–#14 + pointer twins.
   Update all callers.
6. **Extension registration + request dispatcher.** Add the
   `DPMS_MAJOR_OPCODE` const, the `EXTENSIONS` entry, the GenericEvent
   registration (matching how Present plugs into `GE_MAJOR_OPCODE`), and
   `handle_dpms_request` with the per-minor-opcode dispatch + the
   two-notify Disable behaviour. Wire opcode 134 in `process_request`.
   Add `selected_by.remove(&client_id)` in `process_disconnect.rs:81`.
   Tests #15–#19.
7. **Core-loop idle deadline + cascade evaluator.** Add
   `dpms_transition_deadline()` to the `run.rs` poll-deadline
   computation and the post-poll `next_dpms_level` check with
   `apply_dpms_transition` call. After this, `xset dpms 5 10 15` + idle
   blanks the screen.
8. **VT-leave hook.** Add the in-memory DPMS reset
   (`power_level = On`, `last_activity = now`, no backend call, no notify)
   to whichever VT-leave function lands as part of
   `docs/superpowers/plans/2026-05-28-vt-switching.md`. If VT switching
   hasn't merged yet when DPMS work ships, defer this commit and note
   the hook in the VT plan.

Seven (or eight) functional commits. Commit 4+6 together land the
visible behaviour for forced levels; commit 7 lands it for idle.
Squash to one PR at merge per `AGENTS.md`.
