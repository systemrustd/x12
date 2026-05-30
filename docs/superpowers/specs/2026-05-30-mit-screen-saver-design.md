# MIT-SCREEN-SAVER Design

**Goal:** implement the MIT-SCREEN-SAVER extension on yserver end-to-end —
extension shape (six requests, one event), per-client idle tracking,
core-opcode wiring (107/108/115), media-player Suspend support, and
Xorg-faithful coupling with DPMS — so `mate-screensaver`, `xscreensaver`,
and media players like `mpv`/`vlc` behave the way they do on Xorg.

The previous DPMS work [`2026-05-30-dpms-design.md`](2026-05-30-dpms-design.md)
explicitly scoped SCREEN-SAVER as a non-goal but noted "the idle timer
added here is reusable when SCREEN-SAVER lands" — this spec lands it.

**Non-goals:**

- Server-side bitmap saver. `dixSaveScreens`'s tiled random-position
  saver is not implemented. Activations report `kind=Blanked` when
  `prefer_blanking` is set (the default), otherwise `kind=Internal` —
  matching Xorg's reporting (`saver.c:396-401`) given we have no client
  registered via `SetAttributes`. We never report `kind=External`
  because that requires a client to have actually called `SetAttributes`
  (which we always reject with `BadAccess`).
- `ScreenSaverSetAttributes` returns `BadAccess` unconditionally. Real
  saver-window plumbing (which would let us report `kind=External` and
  install a client-provided saver window) is out of scope. Xorg returns
  `BadAccess` only on ownership conflict or XAce denial (`saver.c:717-830`);
  we return it for every call. xscreensaver issues this on startup
  expecting Success; the failure is non-fatal — xscreensaver falls back
  to its own override-redirect saver window, which is exactly the
  user-visible behaviour anyway. `ScreenSaverUnsetAttributes` returns
  `Success` as a no-op (matching Xorg's silent-no-op for "no matching
  attrs" at `saver.c:1057-1073`, which is our universal case).
- **SS → DPMS coupling.** The Xorg coupling is one-way in the other
  direction (DPMS → SS); confirmed at `Xext/dpms.c:262-279` and
  `os/WaitFor.c:457`. ScreenSaver activation does NOT call `DPMSSet`.
  This is the consequential gotcha for "`xset s 60` alone won't power
  off the panel" — see "Smoke matrix" below. Matches Xorg.
(none beyond the SetAttributes deviation above).

**Spec reference:** `/usr/include/X11/extensions/saver.h` for constants,
`/usr/include/X11/extensions/saverproto.h` for wire layouts.
`/home/jos/Projects/xserver/Xext/saver.c` and
`/home/jos/Projects/xserver/dix/window.c` for the de-facto behaviour
([[feedback_match_xorg_clients_dont_get_patched]]).

---

## Architecture

### Extension shape (matches Xorg)

Extension name `MIT-SCREEN-SAVER`. Internal yserver major opcode **150**
(next free slot after X-Resource at 149). One sequential event type:
`ScreenSaverNotify` at first event = **162** (next after GLX at 161).
No errors of its own; reuses core `BadAccess` / `BadValue`.

| Minor | Request | Semantics |
|------:|---------|-----------|
| 0 | `ScreenSaverQueryVersion(c_major, c_minor)` | Replies `(server_major=1, server_minor=1)`. |
| 1 | `ScreenSaverQueryInfo(drawable)` | Replies `state, window=0, til_or_since, idle, event_mask, kind` (Blanked or Internal — see "kind" below). See "QueryInfo" below for field semantics. |
| 2 | `ScreenSaverSelectInput(drawable, event_mask)` | Stores the mask verbatim. Xorg's `ProcScreenSaverSelectInput` (`saver.c:695-713`) does NOT validate bits — any value goes; only `NotifyMask` / `CycleMask` affect delivery. `event_mask == 0` removes the client's entry. |
| 3 | `ScreenSaverSetAttributes(...)` | `BadAccess`. Documented deviation from Xorg, which only returns `BadAccess` on ownership conflict or XAce denial (`saver.c:717-830`). We cannot implement the saver-window machinery without `kind=External` plumbing, so we reject every call. Real-world impact: xscreensaver issues this on startup expecting Success; the failure is non-fatal — xscreensaver falls back to its own override-redirect saver window. |
| 4 | `ScreenSaverUnsetAttributes(drawable)` | Returns `Success` as a no-op. Xorg's `ProcScreenSaverUnsetAttributes` is silent when no matching attrs exist (`saver.c:1057-1073`) — that's our universal case since `SetAttributes` always fails. |
| 5 | `ScreenSaverSuspend(suspend: bool)` | Refcounted per-client; see "Suspend" below. |

**`ScreenSaverNotify`** wire layout (sequential event, NOT XGE), 32 bytes:

```
0     type        = first_event + 0 (= 162)
1     state       : u8   (Off=0, On=1, Cycle=2, Disabled=3)
2-3   sequence    : u16
4-7   timestamp   : u32  (state.timestamp_now())
8-11  root        : u32  (= ROOT_WINDOW xid)
12-15 window      : u32  (saver window — always 0 here; no SetAttributes path)
16    kind        : u8   (Blanked=0 or Internal=1; see "kind" below)
17    forced      : u8   (1 iff transition came from ForceScreenSaver OR
                          from DPMS-coupling; 0 from idle timer)
18-19 pad
20-31 pad (12 bytes)
```

Event fires whenever `screensaver.active` changes. We never emit `Cycle`
(no Internal saver) and never emit `Disabled` as an event (`Disabled` is
a QueryInfo-state-only report — Xorg matches).

### State model

```rust
// crates/yserver-core/src/server.rs (alongside DpmsState)
pub struct ScreenSaverState {
    /// Set by SetScreenSaver (core opcode 107). 0 = disabled.
    pub timeout_ms: u32,
    /// Internal-saver cycle period. We don't implement Internal, but
    /// GetScreenSaver echoes the stored value, so we hold it.
    pub interval_ms: u32,
    /// `prefer_blanking` and `allow_exposures` are protocol fields we
    /// echo from GetScreenSaver. No behavioural effect.
    pub prefer_blanking: bool,
    pub allow_exposures: bool,

    /// Current activation state. `forced=true` when the most recent
    /// transition came from ForceScreenSaver or from DPMS coupling.
    pub active: ScreenSaverActive,
    pub forced: bool,

    /// Subscribers per client. Mask is OR of `NotifyMask` (0x01) and
    /// `CycleMask` (0x02). QueryInfo reports the CALLING client's mask
    /// from this table (`saver.c:220-231`), not the union.
    pub selected_by: HashMap<ClientId, u32>,

    /// Per-client tally of outstanding `Suspend(true)` calls (mirrors
    /// Xorg's per-client resource records, simpler bookkeeping). Effective
    /// "suspended" = `!suspend_counts.is_empty()`.
    pub suspend_counts: HashMap<ClientId, u32>,

    /// Next time a `ScreenSaverNotify(state=Cycle)` event should fire.
    /// Set to `Some(now + interval_ms)` whenever `active` transitions to
    /// `On` (when `interval_ms > 0`); advanced each time the cycle event
    /// fires. Cleared when `active` returns to `Off`. Mirrors Xorg's
    /// `WaitFor.c:473-476` re-scheduling logic.
    pub next_cycle: Option<Instant>,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ScreenSaverActive {
    Off,
    On,
    /// Periodic event-only variant; never stored in
    /// `ScreenSaverState.active` (which only holds Off/On). Used as the
    /// `notify_state` argument to `emit_screen_saver_notify` from the
    /// cycle-timer path.
    Cycle,
}
```

Defaults at init: `timeout_ms = 600_000`, `interval_ms = 600_000`,
`prefer_blanking = true`, `allow_exposures = true`, `active = Off`,
`forced = false`, `next_cycle = None`, both `HashMap`s empty. Matches
Xorg `dix/globals.c:96-99` (`PreferBlanking`, `AllowExposures`).

`last_activity` continues to live on `DpmsState` — both extensions
read the same "time of last input" clock. Existing fanout-prologue
write at [DPMS Task 5] keeps it fresh.

### QueryInfo field semantics

Mirror Xorg's `ProcScreenSaverQueryInfo` (`saver.c:623-692`) exactly:

```rust
let last_input = state.dpms.last_activity.elapsed().as_millis() as u32;
let timeout = state.screensaver.timeout_ms;

let (reply_state, til_or_since) = match state.screensaver.active {
    ScreenSaverActive::On => {
        // CARD32 underflow on ForceScreenSaver-while-not-idle matches Xorg.
        let ts = last_input.wrapping_sub(timeout);
        (ScreenSaverOn, if timeout > 0 { ts } else { 0 })
    }
    ScreenSaverActive::Off => {
        if timeout == 0 {
            (ScreenSaverDisabled, 0)
        } else if timeout < last_input {
            (ScreenSaverOff, 0)
        } else {
            (ScreenSaverOff, timeout - last_input)
        }
    }
};

// eventMask is the CALLING client's mask (not the union).
let event_mask = state.screensaver.selected_by.get(&client_id).copied().unwrap_or(0);
```

Reply window field is always 0 (no SetAttributes path → no saver window).

### kind

`kind` is derived the same way in both `QueryInfoReply` and
`ScreenSaverNotify` events. Matches `Xext/saver.c:396-401`:

```rust
let kind: u8 = if state.screensaver.prefer_blanking {
    SCREEN_SAVER_BLANKED   // 0
} else {
    SCREEN_SAVER_INTERNAL  // 1
};
```

Never `External` (2) — that requires a client to have registered a
saver window via `SetAttributes`, which we always reject with
`BadAccess`. Real clients like mate-screensaver and xscreensaver
don't care about the kind value; they activate their lockscreen on
any `ScreenSaverNotify(state=On)`.

### State machine

Six event sources affect screen-saver state:

1. **Idle timer** (post-poll cascade evaluator in `run.rs`):
   transitions Off→On with `forced=false`. Guarded by
   `screensaver_idle_deadline()` returning `Some`. On the transition,
   `next_cycle = Some(now + interval_ms)` if `interval_ms > 0`.
2. **Input event** (fanout prologue): transitions On→Off with
   `forced=false`. No-op when active is already Off. Clears
   `next_cycle = None`.
3. **DPMS coupling via `apply_dpms_transition`** (mirrors
   `Xext/dpms.c:269-279`):
   - DPMS non-On AND SS=Off → flip SS On with `forced=true` (Xorg uses
     `dixSaveScreens(SCREEN_SAVER_FORCER, ScreenSaverActive)` —
     `dix/window.c:3122` routes that through the FORCER path, which sets
     `forced=true` in `SendScreenSaverNotify`).
   - DPMS On AND SS=On → flip SS Off with **`forced=false`** (Xorg uses
     `dixSaveScreens(SCREEN_SAVER_OFF, ScreenSaverReset)` — note this
     is NOT the FORCER path; `on=SCREEN_SAVER_OFF`, so `forced=false`).
     Do **not** reset `last_activity` — Xorg's `NoticeTime` only runs
     when `on == SCREEN_SAVER_FORCER` (`dix/window.c:3187-3193`), which
     is not this path. The input fanout prologue already updated
     `last_activity = now` for the input event that triggered this DPMS
     wake, so no additional reset is needed.
4. **`ForceScreenSaver(Activate)`**: transitions Off→On with
   `forced=true`. No-op when already On.
5. **`ForceScreenSaver(Reset)`**: transitions On→Off with `forced=true`
   AND `last_activity = Instant::now()`. No-op when already Off.
   Clears `next_cycle = None`.
6. **Cycle timer** (post-poll evaluator, fires only while `active==On`):
   when `now >= next_cycle`, emit `ScreenSaverNotify(state=Cycle,
   forced=false)` to subscribers whose mask has `CycleMask` set, then
   advance `next_cycle = Some(now + interval_ms)`. Mirrors Xorg's
   `WaitFor.c:470-476` (the same timer that drives idle activation re-
   fires every `interval_ms` while `screenIsSaved==SCREEN_SAVER_ON`, and
   `dixSaveScreens` promotes the type to `SCREEN_SAVER_CYCLE` because
   `what==screenIsSaved` at `window.c:3107-3111`).

Each transition fires `ScreenSaverNotify` via the
`fanout_event_to_clients` helper. Sequence + byte-order handled
per-client by the helper. **Subscriber filtering depends on event
type**: Off/On transitions go to subscribers with `NotifyMask` set
(bit 0); Cycle events go to subscribers with `CycleMask` set (bit 1).
Matches Xorg `saver.c:389-391` (`mask = ScreenSaverNotifyMask; if
(state == ScreenSaverCycle) mask = ScreenSaverCycleMask;`).

### Idle timer

Add `screensaver_idle_deadline` on `ServerState`:

```rust
pub fn screensaver_idle_deadline(&self) -> Option<Instant> {
    if self.screensaver.timeout_ms == 0
        || !self.screensaver.suspend_counts.is_empty()
        || matches!(self.screensaver.active, ScreenSaverActive::On)
        || self.dpms.power_level != 0  // Xorg WaitFor.c:457 skip
    {
        return None;
    }
    Some(self.dpms.last_activity
        + Duration::from_millis(u64::from(self.screensaver.timeout_ms)))
}
```

The `dpms.power_level != 0` clause matches Xorg: when DPMS has already
blanked the panel, the SS idle timer is suppressed — the DPMS→SS
coupling has already handled it.

**Cycle deadline** is separate. While `active==On`, `next_cycle` is
the moment we should fire the next `Cycle` event:

```rust
pub fn screensaver_cycle_deadline(&self) -> Option<Instant> {
    if !matches!(self.screensaver.active, ScreenSaverActive::On)
        || !self.screensaver.suspend_counts.is_empty()
        || self.dpms.power_level != 0
    {
        return None;
    }
    self.screensaver.next_cycle
}
```

Add both deadlines to the `.min()` chain in `run.rs:386` alongside
`dpms_transition_deadline()`. Post-poll, after the DPMS cascade
evaluator, run two sibling blocks:

```rust
// Idle activation
if let Some(deadline) = state.screensaver_idle_deadline() {
    if Instant::now() >= deadline {
        apply_screen_saver_transition(state, backend, On, /*forced=*/false);
    }
}
// Cycle re-fire (only fires when active==On per the deadline gate)
if let Some(deadline) = state.screensaver_cycle_deadline() {
    let now = Instant::now();
    if now >= deadline {
        emit_screen_saver_notify(state, ScreenSaverActive::Cycle, /*forced=*/false);
        state.screensaver.next_cycle =
            Some(now + Duration::from_millis(u64::from(state.screensaver.interval_ms)));
    }
}
```

(Note: `emit_screen_saver_notify` is generalized to take an explicit
state value — see the helper definition below — so the cycle path can
pass `Cycle` without flipping `state.screensaver.active`.)

### Suspend

Per-client tally on `ScreenSaverState`. `Suspend(true)` increments
the client's entry; `Suspend(false)` decrements (saturating to 0,
matching Xorg's silent-on-spurious-free `FreeResource` semantics);
if a client's count hits 0, drop its `HashMap` entry. `process_disconnect`
drops the entry entirely.

On any transition from suspended → unsuspended (last `Suspend(false)`
drains the count, OR the last suspending client disconnects), match
Xorg `ScreenSaverFreeSuspend` (`saver.c:343-378`):

```rust
if state.screensaver.suspend_counts.is_empty()
    && matches!(state.screensaver.active, ScreenSaverActive::Off)
    && state.dpms.power_level == 0
{
    state.dpms.last_activity = Instant::now();
}
```

This is the "media player ends a 2-hour movie, restart the idle
timer from now" path. No notify fires (no state change).

Suspend does NOT block `ForceScreenSaver(Activate)` — Xorg's saver.c
comment is explicit: "suspending it (by design) doesn't prevent it
from being forcibly activated".

### Backend hooks

None. ScreenSaver is purely server-side bookkeeping. The
`apply_screen_saver_transition` helper takes a `&mut dyn Backend`
parameter for signature parity with `apply_dpms_transition` (and so
future coupling paths can reuse the same shape), but ignores it.

### Core opcode wiring (107, 108, 115)

Today: 107 (`SetScreenSaver`) and 115 (`ForceScreenSaver`) are
`log_void` at `process_request.rs:205-206`; 108 (`GetScreenSaver`)
returns hardcoded `(600, 0, 1, 0)` at `:213`. Replace all three.

**107 `SetScreenSaver`** — body: `timeout:i16, interval:i16,
prefer_blanking:u8, allow_exposures:u8, pad:u16`. Total body 8 bytes.

- Range validation:
  - `timeout`, `interval` ∈ `[-1, 0x7FFF]` (i16). Per spec `-1` = restore
    default; positive = seconds; 0 = disabled. Otherwise `BadValue`.
  - `prefer_blanking`, `allow_exposures` ∈ `[0, 2]`. `2` = `Default` sentinel.
    Otherwise `BadValue`.
- Sentinel resolution — matches Xorg's `defaultScreenSaver*` globals
  (`dix/globals.c:96-99`):
  - `timeout == -1` → restore `600_000ms` (Xorg `defaultScreenSaverTime`,
    10 minutes in ms).
  - `interval == -1` → restore `600_000ms` (Xorg
    `defaultScreenSaverInterval`).
  - `prefer_blanking == 2` → restore `true` (Xorg's
    `defaultScreenSaverBlanking == PreferBlanking`).
  - `allow_exposures == 2` → restore `true` (Xorg's
    `defaultScreenSaverAllowExposures == AllowExposures`, value `1`).
- Store: `state.screensaver.timeout_ms = (timeout as u32) * 1000` (etc).
- **No `last_activity` reset.** Xorg's `ProcSetScreenSaver`
  (`dix/dispatch.c:3211-3220`) only calls `SetScreenSaverTimer()`, which
  recomputes the next-fire deadline from the existing
  `LastEventTime` — it does NOT update `last_event_time` itself. In
  yserver our deadline is computed dynamically from `last_activity` at
  every poll, so no equivalent of `SetScreenSaverTimer` is needed; the
  next loop iteration's `screensaver_idle_deadline()` will use the new
  `timeout_ms` against the unchanged `last_activity`.

**108 `GetScreenSaver`** — current handler at `:10617` returns hardcoded
values. Replace to read from `state.screensaver`:

```rust
let mut buf = x11::fixed_reply(byte_order, sequence, 0, 0);
write_u16(byte_order, &mut buf, (state.screensaver.timeout_ms / 1000) as u16);
write_u16(byte_order, &mut buf, (state.screensaver.interval_ms / 1000) as u16);
buf.push(u8::from(state.screensaver.prefer_blanking));
buf.push(u8::from(state.screensaver.allow_exposures));
buf.resize(32, 0);
```

**115 `ForceScreenSaver`** — body: `mode:u8, pad:u8, pad:u16`. 4 bytes.

- `mode > 1` → `BadValue` with `bad_value = mode as u32`.
- `mode == 0` (Reset) → `apply_screen_saver_transition(state, backend,
  Off, /*forced=*/true)` AND `state.dpms.last_activity = Instant::now()`.
- `mode == 1` (Activate) → `apply_screen_saver_transition(state, backend,
  On, /*forced=*/true)`.

### `apply_screen_saver_transition` helper

Lives in `process_request.rs` alongside `apply_dpms_transition`:

```rust
pub(crate) fn apply_screen_saver_transition(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    new: ScreenSaverActive,
    forced: bool,
) {
    if state.screensaver.active == new { return; }
    let was_off = matches!(state.screensaver.active, ScreenSaverActive::Off);
    state.screensaver.active = new;
    state.screensaver.forced = forced;
    // Manage the cycle deadline alongside the activation transition.
    state.screensaver.next_cycle = match new {
        ScreenSaverActive::On if state.screensaver.interval_ms > 0 => {
            Some(Instant::now() + Duration::from_millis(u64::from(state.screensaver.interval_ms)))
        }
        _ => None,
    };
    let _ = (backend, was_off);  // backend reserved for future coupling; was_off unused but documents intent
    emit_screen_saver_notify(state, new, forced);
}

/// `notify_state` and `forced` are passed explicitly so the cycle-timer
/// path can fire `Cycle` events without mutating `state.screensaver.active`.
/// `Cycle` events go to `CycleMask` subscribers; `Off`/`On` events go to
/// `NotifyMask` subscribers (Xorg `saver.c:389-391`).
pub(crate) fn emit_screen_saver_notify(
    state: &mut ServerState,
    notify_state: ScreenSaverActive,
    forced: bool,
) {
    const SCREEN_SAVER_FIRST_EVENT: u8 = 162;
    // Notify events carry On/Off/Cycle. Disabled is QueryInfo-only per
    // `saver.c:381-414` — SendScreenSaverNotify never sees ScreenSaverDisabled.
    let (active_state, deliver_mask): (u8, u32) = match notify_state {
        ScreenSaverActive::Off   => (SCREEN_SAVER_OFF,   SCREEN_SAVER_NOTIFY_MASK),
        ScreenSaverActive::On    => (SCREEN_SAVER_ON,    SCREEN_SAVER_NOTIFY_MASK),
        ScreenSaverActive::Cycle => (SCREEN_SAVER_CYCLE, SCREEN_SAVER_CYCLE_MASK),
    };
    let subs: Vec<ClientId> = state
        .screensaver
        .selected_by
        .iter()
        .filter(|(_, mask)| *mask & deliver_mask != 0)
        .map(|(c, _)| *c)
        .collect();
    if subs.is_empty() { return; }
    let ts = state.timestamp_now();
    let kind: u8 = if state.screensaver.prefer_blanking {
        SCREEN_SAVER_BLANKED  // 0
    } else {
        SCREEN_SAVER_INTERNAL // 1
    };
    let dropped = fanout_event_to_clients(state, &subs, |buf, seq, order| {
        encode_screen_saver_notify_event(
            buf, order, seq,
            SCREEN_SAVER_FIRST_EVENT,
            active_state, ts, ROOT_WINDOW.0, 0 /*window*/,
            kind,
            forced,
        );
    });
    for c in dropped { state.screensaver.selected_by.remove(&c); }
}
```

Note: `ScreenSaverActive::Cycle` is a value used only as the
`notify_state` argument — it never appears in
`state.screensaver.active` (which only holds `Off`/`On`). Cycle is a
periodic event, not a persistent state. The enum gets a `Cycle`
variant solely so the same `emit_screen_saver_notify` helper covers
all three notify-event types.

### DPMS coupling

Restructure `apply_dpms_transition` so the SS coupling runs **between**
the in-memory level write and the backend call + DPMS notify — matching
Xorg's `DPMSSet` order (`Xext/dpms.c:262-293`):

```rust
pub(crate) fn apply_dpms_transition(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    new_level: u8,
) {
    let old = state.dpms.power_level;
    state.dpms.power_level = new_level;

    // (1) ScreenSaver coupling — fires BEFORE backend hook and BEFORE
    //     DPMS notify, matching Xorg dpms.c:269-279.
    //
    //   non-On && SS=Off  → SCREEN_SAVER_FORCER + Active → forced=true
    //   On     && SS=On   → SCREEN_SAVER_OFF    + Reset  → forced=false
    //
    // Neither coupling resets last_activity: Xorg's NoticeTime only
    // fires for the FORCER+Reset combination (window.c:3187-3193),
    // which neither of these paths is.
    let dpms_on = new_level == 0;
    match (dpms_on, state.screensaver.active) {
        (false, ScreenSaverActive::Off) => {
            apply_screen_saver_transition(state, backend, On, /*forced=*/true);
        }
        (true, ScreenSaverActive::On) => {
            apply_screen_saver_transition(state, backend, Off, /*forced=*/false);
        }
        _ => {}
    }

    // (2) Backend hook (Xorg's per-screen DPMS DDX call).
    if let Err(e) = backend.set_dpms_power(new_level) {
        log::error!("set_dpms_power({new_level}) failed: {e}");
        // state still advances — Xorg's DPMSSet doesn't unwind on error either
    }

    // (3) DPMS notify (Xorg dpms.c:289-290).
    if new_level != old {
        emit_dpms_notify(state);
    }
}
```

Order matters: a client subscribed to BOTH `ScreenSaverNotify` and
`DPMSInfoNotify` sees the SS notify first when a DPMS transition
happens, matching Xorg's wire ordering exactly.

### Input fanout

The DPMS Task 5 fanout prologue already calls `apply_dpms_transition`
when power_level is non-On. The DPMS-coupling block above will then
fire the SS-Off side-effect for free. The remaining case is:
DPMS=On AND SS=On (saver activated via idle timer or ForceScreenSaver
without DPMS having fired). For that case, add a sibling check after
the DPMS wake block:

```rust
// crates/yserver-core/src/core_loop/key_fanout.rs and pointer_fanout.rs
state.dpms.last_activity = Instant::now();
if state.dpms.enabled && state.dpms.power_level != 0 {
    apply_dpms_transition(state, backend, 0);
    // DPMS coupling already flipped SS Off if needed.
}
if matches!(state.screensaver.active, ScreenSaverActive::On) {
    // Independent SS-only path (DPMS was On, SS activated on its own).
    apply_screen_saver_transition(state, backend, Off, /*forced=*/false);
}
```

---

## Components

### `crates/yserver-protocol/src/x11/screensaver.rs` (new)

Wire codecs and constants:

```rust
pub const QUERY_VERSION: u8 = 0;
pub const QUERY_INFO: u8 = 1;
pub const SELECT_INPUT: u8 = 2;
pub const SET_ATTRIBUTES: u8 = 3;
pub const UNSET_ATTRIBUTES: u8 = 4;
pub const SUSPEND: u8 = 5;

pub const SERVER_MAJOR_VERSION: u16 = 1;
pub const SERVER_MINOR_VERSION: u16 = 1;

pub const SCREEN_SAVER_NOTIFY_MASK: u32 = 0x0000_0001;
pub const SCREEN_SAVER_CYCLE_MASK:  u32 = 0x0000_0002;

pub const SCREEN_SAVER_OFF: u8       = 0;
pub const SCREEN_SAVER_ON: u8        = 1;
pub const SCREEN_SAVER_CYCLE: u8     = 2;
pub const SCREEN_SAVER_DISABLED: u8  = 3;

pub const SCREEN_SAVER_BLANKED: u8   = 0;
pub const SCREEN_SAVER_INTERNAL: u8  = 1;
pub const SCREEN_SAVER_EXTERNAL: u8  = 2;

pub fn parse_query_info_request(body: &[u8]) -> Option<u32>; // drawable
pub fn parse_select_input_request(body: &[u8]) -> Option<(u32, u32)>; // drawable, mask
pub fn parse_unset_attributes_request(body: &[u8]) -> Option<u32>; // drawable
pub fn parse_suspend_request(body: &[u8]) -> Option<bool>;

pub fn encode_query_version_reply(byte_order, seq, server_major: u16, server_minor: u16) -> Vec<u8>;
pub fn encode_query_info_reply(byte_order, seq, state: u8, window: u32,
                               til_or_since: u32, idle: u32, event_mask: u32,
                               kind: u8) -> Vec<u8>;
pub fn encode_screen_saver_notify_event(out: &mut Vec<u8>, byte_order, seq,
                                        first_event: u8, state: u8,
                                        timestamp: u32, root: u32, window: u32,
                                        kind: u8, forced: bool);
```

### `crates/yserver-core/src/nested.rs`

Add the major-opcode and event-base consts at `:21+`:

```rust
const MIT_SCREEN_SAVER_MAJOR_OPCODE: u8 = 150;
const MIT_SCREEN_SAVER_FIRST_EVENT: u8 = 162;
```

Add an `ExtensionMetadata` entry to `EXTENSIONS` next to DPMS:

```rust
ExtensionMetadata {
    name: "MIT-SCREEN-SAVER",
    major_opcode: MIT_SCREEN_SAVER_MAJOR_OPCODE,
    first_event: MIT_SCREEN_SAVER_FIRST_EVENT,
    event_count: 1, // ScreenSaverNotify
    first_error: 0,
    availability: ExtensionAvailability::Always,
    unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
},
```

### `crates/yserver-core/src/server.rs`

Add `ScreenSaverState` (struct shown above) and embed in `ServerState`:

```rust
pub screensaver: ScreenSaverState,
```

Initialize in `with_geometry` with the defaults above.

Add `screensaver_idle_deadline(&self) -> Option<Instant>` and
`screensaver_cycle_deadline(&self) -> Option<Instant>` methods on the
existing `impl ServerState` block.

### `crates/yserver-core/src/core_loop/process_request.rs`

- Add `apply_screen_saver_transition` and `emit_screen_saver_notify`
  helpers (bodies above).
- Extend `apply_dpms_transition` with the DPMS-coupling tail block.
- Replace the `107 => log_void` arm with `107 => handle_set_screen_saver(...)`.
- Replace the `115 => log_void` arm with `115 => handle_force_screen_saver(...)`.
- Replace the existing `handle_get_screen_saver` body to read from
  `state.screensaver`.
- Add `150 => handle_screen_saver_request(...)` alongside the other
  extension dispatchers.
- Implement `handle_screen_saver_request` with the six minor-opcode
  arms (matching the table above).

### `crates/yserver-core/src/core_loop/{key,pointer}_fanout.rs`

Extend the existing DPMS prologue with the SS-only sibling check (body
above).

### `crates/yserver-core/src/core_loop/run.rs`

Chain both `state.screensaver_idle_deadline()` and
`state.screensaver_cycle_deadline()` into the `.min()` at `:386`.
After the DPMS cascade evaluator block, add two sibling blocks:

```rust
// Idle activation
if let Some(deadline) = state.screensaver_idle_deadline() {
    if Instant::now() >= deadline {
        apply_screen_saver_transition(state, backend, ScreenSaverActive::On, /*forced=*/false);
    }
}
// Cycle re-fire
if let Some(deadline) = state.screensaver_cycle_deadline() {
    let now = Instant::now();
    if now >= deadline {
        emit_screen_saver_notify(state, ScreenSaverActive::Cycle, /*forced=*/false);
        state.screensaver.next_cycle =
            Some(now + Duration::from_millis(u64::from(state.screensaver.interval_ms)));
    }
}
```

### `crates/yserver-core/src/core_loop/process_disconnect.rs`

Alongside the existing DPMS cleanup added in [DPMS Task 6], add:

```rust
state.screensaver.selected_by.remove(&client_id);
let was_suspending = state.screensaver.suspend_counts.remove(&client_id).is_some();
if was_suspending
    && state.screensaver.suspend_counts.is_empty()
    && matches!(state.screensaver.active, ScreenSaverActive::Off)
    && state.dpms.power_level == 0
{
    state.dpms.last_activity = Instant::now();
}
```

---

## Data flow

### Idle path with DPMS coupling (Xorg-faithful)

```
config: xset s 60, xset dpms 120 120 120
t=0:   last input. last_activity=0. screensaver.active=Off, dpms.power_level=On.
       deadline_chain = min(SS@60, DPMS@120) = 60.
t=60:  poll wakes (SS deadline). DPMS deadline not reached.
         Post-poll DPMS cascade: power_level still On (no transition).
         Post-poll SS evaluator: SS deadline passed, DPMS still On.
           apply_screen_saver_transition(state, On, forced=false)
             → screensaver.active = On
             → emit ScreenSaverNotify(state=On, forced=0)
       deadline_chain recomputed = DPMS@120 (SS deadline now None — active=On).
t=120: poll wakes (DPMS deadline).
         Post-poll DPMS cascade: idle=120 ≥ off_ms=120 (in the example)
           apply_dpms_transition(state, OFF)
             → power_level = OFF
             → backend.set_dpms_power(OFF)
             → emit DPMSInfoNotify(level=Off)
             → DPMS-coupling tail: new_level=OFF, SS=On → no SS transition (Xorg matches).
       deadline_chain = None (SS deadline blocked by active=On, DPMS deadline blocked by power_level=Off).
```

### Wake path

```
HostInputEvent arrives.
  → fanout prologue:
      last_activity = now
      if power_level != On: apply_dpms_transition(On)
        → power_level = On
        → backend.set_dpms_power(On)
        → emit DPMSInfoNotify(level=On)
        → coupling tail: new_level=On, SS=On → apply_screen_saver_transition(Off, forced=true)
            → screensaver.active = Off
            → emit ScreenSaverNotify(state=Off, forced=1)
            → last_activity = now (already set above)
      if SS=On (independent activation case): apply_screen_saver_transition(Off, forced=false)
        → … (this case fires only when DPMS was already On and SS activated on its own)
      existing event fanout
```

### Suspend path (media player)

```
Frame 1: mpv calls XScreenSaverSuspend(True).
  → state.screensaver.suspend_counts.insert(mpv_client_id, 1).
  → screensaver_idle_deadline() now returns None.
Frame N (video playing): user does not touch mouse for 90 minutes.
  → last_activity stale by 90 min, but no SS deadline → no transition.
  → DPMS deadline_chain still uses normal DPMS timeouts.
End of video: mpv calls XScreenSaverSuspend(False).
  → state.screensaver.suspend_counts.entry(mpv_client_id) -> 0 → entry removed.
  → suspend_counts now empty.
  → guard: SS=Off, DPMS=On → state.dpms.last_activity = Instant::now()  (restart timer)
  → No notify fires (no state change).
```

### DPMS-induced SS activation (Xorg coupling)

```
DPMS standby_ms=60, off_ms=120, SS timeout_ms=300 (longer than DPMS).
t=0:   input. last_activity=0.
t=60:  DPMS cascade: power_level=Standby
         coupling tail: new_level=Standby (≠On), SS=Off → apply_screen_saver_transition(On, forced=true)
           → emit ScreenSaverNotify(state=On, forced=1)
t=300: SS deadline would have fired, but screensaver_idle_deadline() returned None
       since active=On already. (Also blocked by power_level != On in the gate.)
```

### Explicit-request paths

```
ForceScreenSaver(mode=1, Activate):
  → handle_force_screen_saver
  → apply_screen_saver_transition(On, forced=true)
  → emit ScreenSaverNotify(state=On, forced=1)

ForceScreenSaver(mode=0, Reset):
  → apply_screen_saver_transition(Off, forced=true)
  → last_activity = now
  → emit ScreenSaverNotify(state=Off, forced=1)

ScreenSaverSelectInput(drawable, mask=NotifyMask):
  → handle_screen_saver_request
  → state.screensaver.selected_by.insert(client_id, mask)

ScreenSaverSelectInput(drawable, mask=0):
  → state.screensaver.selected_by.remove(&client_id)

Client disconnect:
  → process_disconnect
  → selected_by.remove(&client_id)
  → suspend_counts.remove(&client_id) (+ timer-restart guard above)
```

---

## Error handling

| Condition | Behavior |
|-----------|----------|
| `SelectInput` with any mask bits | `Success`. Xorg's `ProcScreenSaverSelectInput` (`saver.c:695-713`) does NOT validate bits — any value is stored verbatim. Only `NotifyMask` and `CycleMask` gate delivery; other bits are inert. |
| `Suspend` body parse failure | `BadLength`. |
| `SetAttributes` (any args) | `BadAccess`. Documented deviation: Xorg allows this for the first/owning client (`saver.c:717-830`), we never do. xscreensaver tolerates this — falls back to its own override-redirect saver window. |
| `UnsetAttributes` (any drawable) | `Success` no-op. Matches Xorg's silent-no-op when no matching attrs exist (`saver.c:1057-1073`) — our universal case. |
| `SetScreenSaver(timeout > 0x7FFF \|\| timeout < -1)` | `BadValue` with `bad_value = u32::from(timeout as u16)`. |
| `SetScreenSaver(interval > 0x7FFF \|\| interval < -1)` | `BadValue` with `bad_value = interval`. |
| `SetScreenSaver(prefer_blanking > 2)` | `BadValue` with `bad_value = prefer_blanking`. |
| `SetScreenSaver(allow_exposures > 2)` | `BadValue` with `bad_value = allow_exposures`. |
| `ForceScreenSaver(mode > 1)` | `BadValue` with `bad_value = mode`. |
| Unknown minor opcode | `BadRequest` (sequence-numbered). |
| Client disconnects while in `selected_by` and/or `suspend_counts` | Both entries removed in `process_disconnect`; if dropping the last suspender, restart the idle timer per the Xorg-matching guard. |

---

## Testing

### Unit tests

**`crates/yserver-protocol/src/x11/screensaver.rs::tests`** (5 tests):

1. `parse_query_info_extracts_drawable` — `[0xab, 0xcd, 0xef, 0x12]` → `Some(0x12efcdab)`.
2. `parse_select_input_extracts_drawable_and_mask` — round-trip.
3. `parse_suspend_extracts_bool` — `[1, 0, 0, 0]` → `Some(true)`; `[0, …]` → `Some(false)`.
4. `encode_query_info_reply_shape` — assert byte offsets per saverproto.h: state at 1, window at 8, til_or_since at 12, idle at 16, event_mask at 20, kind at 24, pad to 32.
5. `encode_screen_saver_notify_event_shape` — assert byte offsets per saverproto.h: type at 0, state at 1, sequence at 2-3, timestamp at 4-7, root at 8-11, window at 12-15, kind at 16, forced at 17, pads to 32.

**`crates/yserver-core/src/server.rs::tests`** (8 tests):

6. `screensaver_idle_deadline_none_when_timeout_zero`.
7. `screensaver_idle_deadline_none_when_suspended` — insert a fake client into `suspend_counts`; deadline → None.
8. `screensaver_idle_deadline_none_when_active`.
9. `screensaver_idle_deadline_none_when_dpms_blanked` — `dpms.power_level = 1` → None (Xorg `WaitFor.c:457`).
10. `screensaver_idle_deadline_returns_last_activity_plus_timeout` — basic Some-path.
10a. `screensaver_cycle_deadline_none_when_off`.
10b. `screensaver_cycle_deadline_some_when_on` — after `apply_screen_saver_transition(On)` with interval_ms > 0, `next_cycle` is set and deadline returns it.
10c. `screensaver_cycle_deadline_none_when_interval_zero` — even with `active=On`, deadline is None when `interval_ms=0` (no Cycle events).

**`crates/yserver-core/src/core_loop/process_request.rs::tests`** (8 tests):

11. `dpms_off_drives_screensaver_on_with_forced_true` — subscribed client gets ScreenSaverNotify with state=On, forced=1 after `apply_dpms_transition(state, backend, OFF)`.
12. `dpms_on_drives_screensaver_off_with_forced_false_and_no_activity_reset` — reverse direction: `apply_dpms_transition(state, backend, ON)` with SS=On fires SS notify state=Off, **forced=0** (non-FORCER path, Xorg `dpms.c:275-278` + `window.c:3187-3193`). `last_activity` is NOT reset by the coupling itself.
13. `dpms_coupling_emits_screensaver_notify_before_dpms_notify` — wire-byte sequencing test: client subscribed to both extensions sees SS notify byte (type=162) appear at a lower offset than the DPMS XGE notify byte (type=35) in the per-client outbound buffer.
14. `force_screen_saver_activate_emits_notify_with_forced_true` — direct ForceScreenSaver(1) path; verify wire-level byte 17 = 1.
15. `force_screen_saver_reset_resets_last_activity` — Reset path; verify `last_activity` advanced (this IS the FORCER+Reset combination Xorg runs `NoticeTime` on).
16. `force_screen_saver_invalid_mode_returns_bad_value` — mode=2 → BadValue with `bad_value=2`.

**Suspend semantics** (`process_request.rs::tests`):

17. `suspend_per_client_refcount_stacks` — Suspend(true) twice from same client; idle deadline returns None; takes two Suspend(false) calls to drain.
18. `suspend_release_last_resets_last_activity_when_screensaver_off_and_dpms_on` — drop last suspender via the Suspend(false) path; assert `last_activity` advanced AND no notify emitted.
19. `force_screen_saver_activate_still_works_while_suspended` — client A suspends, client B forces activate; SS transitions On regardless.

**`crates/yserver-core/src/core_loop/process_disconnect.rs::tests`** (1 test):

20. `disconnect_removes_client_from_screensaver_state_and_restarts_timer_if_last_suspender` — pre-seed both `selected_by` and `suspend_counts` with the client; run `process_disconnect`; assert both clean AND `last_activity` advanced (since this client was the last suspender, SS=Off, DPMS=On).

**`crates/yserver-core/src/core_loop/{key,pointer}_fanout.rs::tests`** (2 tests, one each):

21. `key_event_during_screen_saver_on_flips_off_via_independent_path` — pre-set SS=On, DPMS=On (so DPMS-wake path is skipped). Inject key. Assert SS=Off and ScreenSaverNotify fired with forced=0.
22. `pointer_event_during_screen_saver_on_flips_off_via_independent_path` — same but motion event.

**Dispatcher / extension shape** (`process_request.rs::tests`):

23. `screen_saver_query_version_returns_one_one` — QueryVersion → server_major=1, server_minor=1.
24. `screen_saver_query_info_returns_disabled_when_timeout_zero` — state byte = `SCREEN_SAVER_DISABLED` (3).
25. `screen_saver_select_input_accepts_unknown_mask_bits` — mask=0x04 → Success; client added to `selected_by` with mask=0x04; subsequent Off→On transition does NOT deliver an event to this client (NotifyMask bit not set). Matches Xorg `saver.c:695-713`.
26. `screen_saver_set_attributes_returns_bad_access` — opcode dispatch returns BadAccess; no state side-effects.
27. `screen_saver_unset_attributes_returns_success_noop` — opcode dispatch returns Success with no error reply; no state side-effects. Matches Xorg `saver.c:1057-1073`.
28. `cycle_event_delivered_only_to_cycle_mask_subscribers` — two subscribers: client A with `mask=NotifyMask`, client B with `mask=CycleMask`. Force SS active, then advance time past `interval_ms`. Run the cycle evaluator. Client A receives nothing (already got the On notify from the activation); client B receives a ScreenSaverNotify with `state=Cycle` and `forced=0`. Matches Xorg `saver.c:389-391`.
29. `cycle_event_advances_next_cycle_deadline` — after firing a cycle event, `state.screensaver.next_cycle` advances by exactly `interval_ms`.

### Integration (smoke) tests

User-driven on `bee` from `just startx` (per [[feedback_hw_recipes_user_only]]):

- `xset s 60` then idle 60s: ScreenSaverNotify subscribers (e.g. `xev -event screensaver`) see state=On after 60s. Visible panel-blank only if DPMS is ALSO configured (SS→DPMS coupling does not exist in Xorg).
- `mate-screensaver` running with default config: lockscreen activates after the configured "blank screen after N" timeout. This is the bug fix this whole spec exists for.
- `xscreensaver` running with default config: activates normally; respects its own `dpms*` settings.
- `xset s 0`: idle timer disabled; mate-screensaver / xscreensaver fall back to their own polling (still works on yserver since they have non-MIT-SCREEN-SAVER paths).
- `mpv --loop video.mp4` playing: no SS activation during playback. After mpv exits, idle timer restarts from the exit moment.
- `xset s 60; xset dpms 120 120 120; xset dpms force on` then idle: at t=60 ScreenSaverNotify(state=On) fires; at t=120 DPMS fires AND SS stays On (already activated). At input: DPMS wakes + emits SS-Off notify (forced=1) via coupling.

**Visible smoke is required for the activation path** per
[[feedback_tests_are_not_visible_evidence]] — unit tests verify the
protocol invariants, but only smoke confirms mate-screensaver's
lockscreen actually appears.

### Expected counts

| Crate              | Before | After |
|--------------------|--------|-------|
| `yserver-core`     | +13 from DPMS | +27 |
| `yserver-protocol` | +11 from DPMS | +5  |

---

## Implementation staging

Each commit compiles, passes its tests, ends with `cargo +nightly fmt`,
`cargo clippy`, `cargo test`. Per [[feedback_clippy_pedantic_default]],
plain clippy only.

1. **Protocol wire codecs.** Add `yserver-protocol/src/x11/screensaver.rs`
   with parsers, encoders, constants, and tests #1–#5. No callers yet.
2. **`ScreenSaverState` + `screensaver_idle_deadline` helper.** Add the
   struct, embed in `ServerState`, init in `with_geometry`, add the
   deadline helper. Tests #6–#10.
3. **`apply_screen_saver_transition` + `emit_screen_saver_notify` +
   DPMS coupling restructure.** Add the helpers; restructure
   `apply_dpms_transition` so SS coupling fires between the in-memory
   level write and the backend call (Xorg ordering); add the SS-only
   sibling check in the input fanouts. Tests #11–#15, #21, #22.
4. **Core opcode wiring (107, 108, 115).** Replace the `log_void` /
   hardcoded handlers with real implementations calling
   `apply_screen_saver_transition`. Test #16 (invalid mode).
5. **Extension dispatcher + Suspend semantics.** Add
   `MIT_SCREEN_SAVER_MAJOR_OPCODE` const, `EXTENSIONS` entry,
   `handle_screen_saver_request` with the six minor-opcode arms,
   opcode-150 dispatch in `process_request`. Tests #17, #18, #19,
   #23–#27.
6. **Core-loop idle cascade + cycle fire.** Add both
   `screensaver_idle_deadline()` and `screensaver_cycle_deadline()` to
   the `.min()` chain in `run.rs`, add the post-poll evaluator blocks
   for activation and cycle re-fire. Tests #28, #29.
7. **Disconnect cleanup.** Add the SS lines to `process_disconnect`,
   plus the conditional timer-restart guard. Test #20.

Seven commits. Squash to one PR at merge per [[reference_xephyr_source]]
conventions — actually per `AGENTS.md`.
