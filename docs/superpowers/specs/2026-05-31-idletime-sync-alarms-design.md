# IDLETIME counter + SYNC alarm firing — Design

**Goal:** Fix yserver's stubbed `IDLETIME` system counter (currently aliased to SERVERTIME) and wire SYNC alarm firing for IDLETIME counters end-to-end, so MATE's idle-detection (`mate-screensaver`, `mate-power-manager`) actually triggers — closing the visible-smoke gap that the MIT-SCREEN-SAVER and DPMS work alone could not. After this lands, the user's "set 1 minute in MATE Power preferences → display blanks after 1 minute" works.

**Non-goals:**

- `Await` request semantics. `Await` is currently a no-op stub at `process_request.rs:2312-2314`; it's used by GTK/clutter frame sync, not by IDLETIME. Out of scope.
- Per-slave-device IDLETIME counters. We add per-master-device IDLETIME (VCP=2, VCK=3); per-slave (touchpad, individual USB devices) is XInput2-internal device-bookkeeping we don't yet track and isn't needed by MATE.
- XSync extension version bump. We stay at the version we already report.

**Spec reference:** `/usr/include/X11/extensions/syncproto.h` for wire layouts; `/home/jos/Projects/xserver/Xext/sync.c` for behavioural truth (especially `IdleTimeQueryValue:2627`, `SyncCheckTrigger*:258-304`, `SyncSendAlarmNotifyEvents:428`, `SyncAlarmTriggerFired:540-606`, `IdleTimeBlockHandler:2647`, `IdleTimeWakeupHandler:2750`).

---

## Architecture

Mirror the DPMS+SS evaluator pattern that just shipped on `feat/mit-screen-saver`: per-counter alarm-bracket deadlines feed the poll-deadline `.min()` chain in `run.rs`, and a post-poll evaluator + fanout-prologue handler walk the alarm tables. No per-alarm timer threads, no event-driven async — the same lazy-deadline shape DPMS and SS use.

Five components, each with one clear responsibility:

1. **IDLETIME counter value plumbing.** `process_request.rs::QUERY_COUNTER` for IDLETIME counters returns `(now - baseline_last_activity).as_millis()`. The current stub returns `state.timestamp_now()` which is SERVERTIME and grows monotonically regardless of input — that's the bug we're fixing.

2. **Per-device input tracking.** `ServerState` gains `pub per_device_last_activity: HashMap<u8, Instant>` keyed by XInput2 master device id (VCP=2, VCK=3 in our setup). Key and pointer fanouts update both `dpms.last_activity` (global) and the per-device entry.

3. **Per-counter alarm bracket deadline.** `ServerState::idletime_alarm_deadline() -> Option<Instant>` returns the earliest instant any active Positive-* alarm on an IDLETIME counter could fire. Negative-* alarms are excluded (they only fire on input). Chained into `run.rs`'s `.min()` next to the SS deadlines.

4. **Post-poll alarm evaluator.** `run.rs::evaluate_idletime_alarms_post_poll(state, backend)`. For each IDLETIME counter, compute current idle; for each Active alarm referencing that counter, run the test-type-specific `CheckTrigger(old_idle, new_idle)`; if true, fire via `fire_sync_alarm`. Caches per-counter `last_evaluated_idle` so the `old_idle` value passed to `CheckTrigger` reflects the actual transition.

5. **Fanout-prologue Negative-transition firing.** Right after `last_activity = now` in `key_fanout_to_state` / `pointer_event_fanout_to_state` (BEFORE the SS-off-on-input sibling check we added in MIT-SCREEN-SAVER Task 3), walk active Negative-* alarms with `old_idle = (now - prior_last_activity)` and `new_idle = 0`. Fire any that cross. AlarmNotify arrives on the client wire BEFORE the corresponding input event — predictable ordering, no test flake.

### State model

```rust
// crates/yserver-core/src/server.rs (alongside the existing sync_alarms field)
pub struct ServerState {
    // ... existing ...
    /// Per-XInput2-master-device idle clock. Key = device id (VCP=2, VCK=3).
    /// Updated by key/pointer fanouts on each input event for the affected
    /// device. `dpms.last_activity` continues to track "any device" — that's
    /// the global IDLETIME baseline.
    pub per_device_last_activity: HashMap<u8, Instant>,

    /// Per-counter cache of the idle value at the last evaluator pass. Used to
    /// compute (old, new) transitions for `CheckTrigger`. Keyed by counter id.
    /// Cleared / re-set when alarms on the counter are created or destroyed.
    pub idletime_last_evaluated: HashMap<u32, i64>,
}
```

### IDLETIME counter IDs

The protocol module already defines `SERVERTIME_COUNTER = 0x106` and `IDLETIME_COUNTER = 0x107`. We extend with two more for the master-device IDLETIME counters:

```rust
// crates/yserver-protocol/src/x11/sync.rs
pub const IDLETIME_DEVICE_VCP: u32 = 0x108;  // pointer device idle
pub const IDLETIME_DEVICE_VCK: u32 = 0x109;  // keyboard device idle
```

`LIST_SYSTEM_COUNTERS` reply must include all four. Names per Xorg convention:
`SERVERTIME`, `IDLETIME`, `DEVICEIDLETIME` (with the device id appended; e.g. `DEVICEIDLETIME-2`, `DEVICEIDLETIME-3`).

### Alarm-firing semantics

Implements all four test types per Xorg `Xext/sync.c:258-304` exactly:

| Test type | Fires when |
|---|---|
| `PositiveTransition` | `oldval < test_value && newval >= test_value` (edge-triggered, only on crossing up) |
| `NegativeTransition` | `oldval > test_value && newval <= test_value` (edge-triggered, only on crossing down — i.e. on input wake) |
| `PositiveComparison` | `newval >= test_value` (level-triggered) |
| `NegativeComparison` | `newval <= test_value` (level-triggered) |

`fire_sync_alarm(state, alarm_id, counter_value)` mirrors Xorg `SyncAlarmTriggerFired:540-606`:

1. If alarm state != Active, return.
2. If `counter == None` OR (`delta == 0` AND test_type ∈ {Pos/Neg Comparison}) → state → Inactive.
3. Else: re-arm by repeatedly adding `delta` to `test_value` until `CheckTrigger` returns false; if `checked_add` overflows during this loop → state → Inactive.
4. Build AlarmNotify event carrying:
   - `alarm_state` = the NEW state (post-transition)
   - `alarm_value` = the OLD test_value (pre-re-arm)
   - `counter_value` = current counter value
   - `timestamp` = `state.timestamp_now()`
5. If `alarm.events == true`, fan out AlarmNotify to the owning client AND any clients in the alarm's event-client list. (Currently yserver doesn't track an "event client list" separate from the owner; this design ships owner-only delivery and notes the gap.)
6. Store the (possibly re-armed) `test_value` back on the alarm.

### Backend hooks

None. SYNC alarm firing is purely server-side bookkeeping (same as MIT-SCREEN-SAVER).

### Suspend interaction

The MIT-SCREEN-SAVER Suspend mechanism (XScreenSaverSuspend) inhibits DPMS firing via the unified-timer rule shipped on `feat/mit-screen-saver` (`dpms_transition_deadline` checks `screensaver.suspend_counts.is_empty()`). For IDLETIME alarms, we mirror the same gate: `idletime_alarm_deadline()` returns `None` when `screensaver.suspend_counts` is non-empty. The fanout-prologue Negative-trigger evaluator also bails when suspended (idle is conceptually frozen).

Reasoning: `mate-power-manager` uses an IDLETIME alarm at 10s to mute audio / dim brightness (xtrace `021:<:00a9` value=10000), and a longer alarm to blank the screen via `DPMSForceLevel`. While Firefox holds an `XScreenSaverSuspend`, MATE should not see idle alarms — otherwise the screen blanks mid-video even with our DPMS suspend gate in place. This matches Xorg's `WaitFor.c:519` semantics for the unified timer.

### Bracket-based scheduling vs Xorg

Xorg maintains explicit `value_less` and `value_greater` brackets per counter, recomputed by `SyncComputeBracketValues:1042` whenever a trigger changes. yserver's deadline-computation does the equivalent work on-demand in `idletime_alarm_deadline()` — for each Active Positive-* alarm, compute `counter.baseline_last_activity + Duration::from_millis(test_value)`, take the min. This is O(n) per deadline computation (n = active idletime alarms; in practice n < 10). The deadline is recomputed every poll iteration via the `.min()` chain, so there's no bracket-cache to invalidate.

For Negative-* alarms, no positive deadline is needed — they only fire on input. The fanout-prologue handler walks them once per input event.

---

## Components

### `crates/yserver-protocol/src/x11/sync.rs`

- `pub const IDLETIME_DEVICE_VCP: u32 = 0x108;`
- `pub const IDLETIME_DEVICE_VCK: u32 = 0x109;`
- Verify `encode_alarm_notify_event` exists; if not, add it. Per `syncproto.h`: 32-byte sequential event, `type=SYNC_FIRST_EVENT+1` (= 84), `kind=AlarmNotify(1)`, `alarm:u32` at offset 4, `counter_value_lo:u32` at 8, `counter_value_hi:u32` at 12, `alarm_value_lo:u32` at 16, `alarm_value_hi:u32` at 20, `time:u32` at 24, `state:u8` at 28, 3 bytes pad.
- Update `encode_list_system_counters_reply` to advertise all four counters with their names.

### `crates/yserver-core/src/server.rs`

- Add `pub per_device_last_activity: HashMap<u8, Instant>` to `ServerState`; init empty in `with_geometry`.
- Add `pub idletime_last_evaluated: HashMap<u32, i64>` to `ServerState`; init empty.
- Add `pub fn idletime_alarm_deadline(&self) -> Option<Instant>`:
  - Returns `None` when `screensaver.suspend_counts` is non-empty.
  - For each alarm in `sync_alarms` with `state == Active` and `trigger.counter` ∈ {IDLETIME, IDLETIME_DEVICE_VCP, IDLETIME_DEVICE_VCK} and `test_type` ∈ {PosTransition, PosComparison}: compute the counter's baseline last_activity, add `Duration::from_millis(test_value)`. Take the min across all such alarms.
- Add a small helper `pub fn idletime_baseline(&self, counter: u32) -> Instant` returning the `Instant` for each known IDLETIME counter id, falling back to `dpms.last_activity` for unknown ids.

### `crates/yserver-core/src/core_loop/process_request.rs`

- Replace the IDLETIME branch in `QUERY_COUNTER` (`:2292-2299`):
  ```rust
  let value = match counter {
      x11sync::SERVERTIME_COUNTER => i64::from(state.timestamp_now()),
      x11sync::IDLETIME_COUNTER
      | x11sync::IDLETIME_DEVICE_VCP
      | x11sync::IDLETIME_DEVICE_VCK => {
          let baseline = state.idletime_baseline(counter);
          i64::from(Instant::now()
              .duration_since(baseline)
              .as_millis()
              .min(u128::from(u32::MAX)) as u32)
      }
      _ => state.sync_counters.get(&counter).map_or(0, |c| c.value),
  };
  ```
- Add `pub(crate) fn fire_sync_alarm(state: &mut ServerState, alarm_id: ResourceId, counter_value: i64)` implementing the 6-step recipe from §"Alarm-firing semantics".
- Adjust `LIST_SYSTEM_COUNTERS` reply to include the new per-device counters.
- The existing `CREATE_ALARM` and `CHANGE_ALARM` paths already call `evaluate_alarms_for_counter` — extend that helper (or add a sibling) to recognise IDLETIME counters and route through `fire_sync_alarm`. The create-time immediate-fire check (`:2330`) needs to use `idletime_baseline` for IDLETIME counters and skip the `state.sync_counters.get(&counter).map_or(0, ...)` path that returns 0 for IDLETIME today.

### `crates/yserver-core/src/core_loop/run.rs`

- Add `pub(crate) fn evaluate_idletime_alarms_post_poll(state: &mut ServerState, backend: &mut dyn Backend)`:
  - For each IDLETIME counter id (3 of them):
    - Compute `current_idle` from `state.idletime_baseline(counter)`
    - Get `old_idle` from `state.idletime_last_evaluated.get(&counter).copied().unwrap_or(0)`
    - For each Active alarm referencing this counter:
      - Run `check_trigger(test_type, old_idle, current_idle, test_value)`
      - If true: call `fire_sync_alarm(state, alarm_id, current_idle)`
    - Update `state.idletime_last_evaluated.insert(counter, current_idle)`
- Chain `state.idletime_alarm_deadline()` into the `.min()` chain at `:393-407`.
- Call the evaluator immediately after `evaluate_screen_saver_post_poll` at the same `:664-681` location.

### `crates/yserver-core/src/core_loop/key_fanout.rs` and `pointer_fanout.rs`

Both files gain the same prologue extension (right after `last_activity = now`, BEFORE the SS-off sibling check):

```rust
// IDLETIME: update per-device baseline + fire Negative-transition alarms
// for the affected counters. Runs BEFORE the SS-off check because
// AlarmNotify must arrive on the wire before the input event itself
// (predictable ordering for clients subscribed to both).
let device_id = /* 3 for keys, 2 for pointer */;
let prior_global = now.duration_since(prior_global_last_activity)
    .as_millis().min(u128::from(u32::MAX)) as u32;
let prior_device = state.per_device_last_activity
    .get(&device_id).copied()
    .map(|t| now.duration_since(t).as_millis().min(u128::from(u32::MAX)) as u32)
    .unwrap_or(0);

state.dpms.last_activity = now;  // already there in current code
state.per_device_last_activity.insert(device_id, now);

evaluate_idletime_negative_alarms_on_input(state, backend, device_id,
                                           prior_global, prior_device);
```

The free function `evaluate_idletime_negative_alarms_on_input` walks active Negative-* alarms on the relevant IDLETIME counters and fires those whose `(old=prior, new=0)` crosses their `test_value`.

### `crates/yserver-core/src/core_loop/process_disconnect.rs`

No new cleanup required. `sync_alarms` is keyed by alarm resource id, and the existing disconnect path already iterates through `state.sync_alarms` and removes alarms owned by the departing client (verify; if not, add). `per_device_last_activity` and `idletime_last_evaluated` are server-global and outlive any single client.

---

## Data flow

### Idle path (PositiveTransition alarm fires after threshold)

```
config: client calls SYNC.CreateAlarm{counter=IDLETIME, value=59999, test=PosTransition, delta=0}

t=0     user input → fanouts set last_activity=0 + per_device_last_activity[2|3]=0
        no IDLETIME alarm in sync_alarms → idletime_alarm_deadline()=None
        poll deadline_chain = min(SS, DPMS, other)

t=X     client sends CreateAlarm. state.sync_alarms.insert(alarm, ...)
        idletime_alarm_deadline() = Some(t=0 + 59999ms)
        poll deadline shrinks to min(prev, 59999)

t=59999 poll wakes (idletime deadline). evaluate_idletime_alarms_post_poll:
          current_idle = 59999
          old_idle (cached, defaults to 0) = 0
          CheckTriggerPositiveTransition(0, 59999, 59999) → true
          fire_sync_alarm(alarm, counter_value=59999):
            delta=0 + test=PosTransition → state stays Active (transitions stay Active w/ delta=0)
            emit AlarmNotify(alarm_id, counter=59999, alarm_value=59999,
                             state=Active, time=now_ms)
          idletime_last_evaluated[IDLETIME] = 59999
        idletime_alarm_deadline() now returns None (no Pos alarm can fire from current state)
        Active alarm "quiescent" until counter drops below test_value and crosses back up
```

### Wake path (NegativeTransition alarm fires on input)

```
state: alarm{counter=IDLETIME, value=59999, test=NegTransition, delta=0} Active
       user idle for 90s, last_activity = now-90s

input event arrives:
  fanout prologue:
    prior_global = 90_000ms
    prior_device = 90_000ms (matching device)
    state.dpms.last_activity = now           # idle resets to 0
    state.per_device_last_activity[3] = now
    evaluate_idletime_negative_alarms_on_input(device=3, prior_global=90_000, prior_device=90_000):
      walk Neg alarms on IDLETIME and IDLETIME_DEVICE_VCK
      CheckTriggerNegativeTransition(old=90_000, new=0, test=59999)
        → 90_000 > 59999 AND 0 <= 59999 → true
      fire_sync_alarm: delta=0 + test=NegTransition → stays Active
      emit AlarmNotify(state=Active, alarm_value=59999, counter=0)
  [SS-off-on-input sibling check runs after — unchanged from MIT-SCREEN-SAVER]
  [DPMS-wake check runs after — unchanged]
  continue normal event fanout (KeyPress / Motion etc.)
```

### Re-arm path (delta > 0)

```
CreateAlarm{value=10000, delta=10000, test=PosTransition, Events=true}

idle reaches 10000 → fire:
  test_value += delta (now 20000)
  CheckTrigger(old=10000, new=10000, test_value=20000) → false → stop re-arm loop
  AlarmNotify(alarm_value=10000 [OLD], counter=10000, state=Active)
  alarm.trigger.test_value = 20000

idletime_alarm_deadline() now schedules next wake at last_activity + 20s
```

### Suspended path (XScreenSaverSuspend gates IDLETIME alarms)

```
Firefox calls XScreenSaverSuspend(True) during fullscreen video.
  state.screensaver.suspend_counts[firefox] = 1

Even though IDLETIME alarms exist:
  idletime_alarm_deadline() returns None (suspend_counts non-empty)
  fanout prologue's evaluate_idletime_negative_alarms_on_input bails early

mpv/Firefox/vlc → no AlarmNotify during fullscreen video
                → mate-power-manager doesn't see "user idle"
                → no DPMSForceLevel(Off) → screen stays lit

Firefox calls XScreenSaverSuspend(False) when video exits:
  suspend_counts drained, last_activity reset to now (per MIT-SCREEN-SAVER Task 5)
  IDLETIME alarms re-armed from now=0 idle baseline
  idletime_alarm_deadline() returns Some again, next poll picks it up
```

### Disconnect path

```
Client C with alarms A1, A2 disconnects:
  process_disconnect walks state.sync_alarms, removes A1+A2 owned by C
  idletime_alarm_deadline() recomputes naturally on next poll
  Per-device last_activity untouched (global state)
```

---

## Error handling

| Condition | Behavior |
|---|---|
| `QUERY_COUNTER` on unknown counter id | Existing: `state.sync_counters.get(...).map_or(0, ...)` → returns 0. Unchanged. |
| `CREATE_ALARM` referencing a non-existent counter id | Existing path stores alarm with `counter=0`; create-time trigger eval no-ops. Unchanged. |
| Active alarm's `test_value` re-arm overflows i64 | State → Inactive, AlarmNotify emitted with the unmodified test_value (mirrors Xorg `:597-600`). Implemented via `i64::checked_add` returning `None`. |
| Per-device input event from device id we have no `per_device_last_activity` entry for | Insert on first event. No error. |
| `idletime_alarm_deadline()` called when an alarm references a counter that was destroyed mid-evaluation | Skip that alarm (treat as Inactive). |
| `fire_sync_alarm` called but owner client has disconnected | The alarm entry persists; `fanout_event_to_clients` early-returns on the empty subscriber set. No error, no panic. |
| Alarm with `events: false` fires | State transition happens, but no AlarmNotify is sent (Xorg `:464`). Test this. |
| Alarm with NegativeTransition and `prior_idle = 0` already (i.e., back-to-back input events) | `CheckTriggerNegativeTransition(0, 0, test_value)` → false (no transition). No spurious fire. |
| Comparison alarm whose condition is already true at create time | Xorg fires immediately (`:1146` and our existing `:2330`). We continue this for IDLETIME alarms via the create-time `CheckTrigger` path. |

---

## Testing

### Protocol layer (`crates/yserver-protocol/src/x11/sync.rs::tests`)

Likely 1 test, depending on whether `encode_alarm_notify_event` already exists:

1. `encode_alarm_notify_event_shape` — verify offsets per syncproto.h (type at 0, kind at 1, sequence at 2-3, time at 4-7, counter_value at 8-15, alarm_value at 16-23, alarm at 24-27, state at 28; total 32 bytes). **Skip if encoder already exists.**

### Server state (`crates/yserver-core/src/server.rs::tests`)

5 tests:

2. `idletime_alarm_deadline_none_when_no_alarms`
3. `idletime_alarm_deadline_picks_smallest_active_pos_alarm` — three alarms at 60_000, 30_000, 90_000 → deadline = baseline + 30_000ms
4. `idletime_alarm_deadline_ignores_negative_alarms`
5. `idletime_alarm_deadline_ignores_inactive_alarms`
6. `idletime_alarm_deadline_none_when_screensaver_suspended` (Xorg-mirrored unified-timer rule)

### `QUERY_COUNTER` for IDLETIME (`process_request.rs::tests`)

3 tests:

7. `query_counter_idletime_returns_elapsed_since_last_activity_not_uptime` — **regression test for the current bug**: set `last_activity = now - 30s`, query, expect ≥ 29_000.
8. `query_counter_idletime_device_vcp_uses_per_device_baseline`
9. `query_counter_idletime_device_vck_uses_per_device_baseline`

### Alarm firing — all 4 test types (`process_request.rs::tests` via `fire_sync_alarm`)

6 tests:

10. `pos_transition_fires_once_then_quiescent_when_delta_zero` (MATE's pattern)
11. `neg_transition_fires_on_input_wake`
12. `pos_comparison_with_delta_zero_transitions_to_inactive_after_fire`
13. `neg_comparison_with_delta_zero_transitions_to_inactive_after_fire`
14. `pos_transition_with_delta_re_arms_test_value_until_check_false`
15. `re_arm_overflow_transitions_alarm_to_inactive`

### Event emission (`process_request.rs::tests`)

2 tests:

16. `alarm_with_events_false_does_not_emit_alarmnotify_but_state_still_transitions`
17. `alarm_notify_carries_old_test_value_and_new_state` (Xorg invariant `:604`)

### Core-loop integration (`run.rs::tests`)

2 tests — drive `evaluate_idletime_alarms_post_poll` directly with pre-armed state, mirroring the SS evaluator tests from MIT-SCREEN-SAVER Task 6:

18. `evaluator_fires_pos_transition_when_idle_deadline_elapsed`
19. `evaluator_skips_when_no_idletime_alarms`

### Fanout integration (`key_fanout.rs::tests` + `pointer_fanout.rs::tests`)

4 tests:

20. `key_event_fires_neg_transition_alarm_when_prior_idle_crosses_threshold`
21. `pointer_event_fires_neg_transition_alarm_when_prior_idle_crosses_threshold`
22. `key_event_updates_global_and_per_device_vck_last_activity`
23. `pointer_event_updates_global_and_per_device_vcp_last_activity`

### Integration / smoke (user-driven)

Per [[feedback_hw_recipes_user_only]] and [[feedback_tests_are_not_visible_evidence]] the visible-symptom smoke must run on hardware via `just startx`:

- **`xset s 60` + idle 60s with `xev -event screensaver` running:** alarm fires AND ScreenSaverNotify fires.
- **MATE default-config (no `xset` overrides):** `mate-screensaver` lockscreen activates after the configured "regard idle after N min" timeout; `mate-power-manager` blanks the panel via `DPMSForceLevel(Off)` after its configured "sleep display" timeout. **This is the user-visible bug the entire DPMS + MIT-SS + IDLETIME-fix arc exists for.**
- **`mpv --loop video.mp4` fullscreen + MATE configured 1-min display sleep:** panel does NOT blank during playback. Confirmed via `XScreenSaverSuspend` gating both DPMS firing AND IDLETIME alarm firing.
- **xtrace verification:** after fix, the `mate.xtrace` from the user's session should show AlarmNotify events (event type 84) fired by yserver at the configured idle thresholds; absent in the pre-fix trace.

### Expected counts

| Crate | Before | After |
|---|---|---|
| `yserver-core` | +41 from MIT-SS | +22 |
| `yserver-protocol` | +5 from MIT-SS | +0 or +1 |

---

## Implementation staging

Four tasks. Each commit compiles, passes its tests, ends with `cargo +nightly fmt` + `cargo clippy` + `cargo test`. Plain clippy per [[feedback_clippy_pedantic_default]].

1. **IDLETIME QueryCounter fix + per-device counter IDs + per-device tracking.** `QUERY_COUNTER` for IDLETIME returns real idle; per-device counter IDs reserved in protocol; `LIST_SYSTEM_COUNTERS` advertises all four; per-device `last_activity` field on `ServerState`; fanouts update both global and per-device. Tests #7-#9 + #22-#23. **No alarm-firing logic yet — alarms exist but never fire on IDLETIME.**
2. **Bracket deadline + `idletime_alarm_deadline()`.** Server-state helper, chained into `run.rs` `.min()` poll-deadline. Tests #2-#6. No firing yet (the deadline computes, but no evaluator runs).
3. **`fire_sync_alarm` + post-poll evaluator + create-time fire path.** All four test types, re-arm semantics, overflow handling, Active↔Inactive transitions, AlarmNotify emission. Wire `evaluate_idletime_alarms_post_poll` into `run.rs`. Create-time fire for newly-created Comparison alarms whose condition holds. Tests #10-#19 + #1 if needed.
4. **Fanout-prologue NegativeTransition firing.** Both key and pointer fanouts gain `evaluate_idletime_negative_alarms_on_input`. Runs BEFORE the SS-off sibling check. Tests #20-#21. After this task, end-to-end IDLETIME alarm firing works in both directions.

Optional final-verification step: update `docs/status.md` with a short entry noting the IDLETIME fix lands the visible-smoke matrix for both DPMS and MIT-SCREEN-SAVER.

Four commits. Squash to one PR at merge per `AGENTS.md`.
