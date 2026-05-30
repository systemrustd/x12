# IDLETIME counter + SYNC alarm firing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make yserver's `IDLETIME` system counter actually track idle time (currently aliased to SERVERTIME), and wire SYNC alarm evaluation for IDLETIME counters end-to-end — so MATE's `mate-screensaver` lockscreen and `mate-power-manager` display-blank trigger on real user idle.

**Architecture:** Mirror the DPMS+SS evaluator pattern just shipped on `feat/mit-screen-saver`. Replace the IDLETIME branch of `QUERY_COUNTER` to return `(now - last_activity_ms)`; add per-master-device (VCP=2, VCK=3) `last_activity` tracking on `ServerState`; add `idletime_alarm_deadline()` that feeds the poll-deadline `.min()` chain in `run.rs`; add `evaluate_idletime_alarms_post_poll` that walks alarms on each IDLETIME counter; fix `evaluate_alarms_for_counter`'s broken `delta == 0 + Transition` branch (it currently goes Inactive; Xorg keeps it Active per `sync.c:548-555`); fanout-prologue Negative-trigger firing on input wake.

**Tech Stack:** Rust 2024, yserver-protocol's existing SYNC wire encoders (`encode_alarm_notify_event`, `encode_list_system_counters_reply`, `encode_query_counter_reply`), the existing `SyncAlarm` struct (`server.rs:739`), the existing `evaluate_alarms_for_counter` helper (`process_request.rs:2563`), the `RecordingBackend` test double.

**Spec reference:** [docs/superpowers/specs/2026-05-31-idletime-sync-alarms-design.md](../specs/2026-05-31-idletime-sync-alarms-design.md). Behavioural truth: `/home/jos/Projects/xserver/Xext/sync.c` (`IdleTimeQueryValue:2627`, `SyncCheckTrigger*:258-304`, `SyncSendAlarmNotifyEvents:428`, `SyncAlarmTriggerFired:540-606`, `IdleTimeBlockHandler:2647`).

---

## File map

| Path | Action | Responsibility |
|------|--------|----------------|
| `crates/yserver-protocol/src/x11/sync.rs` | modify | Add `IDLETIME_DEVICE_VCP` + `IDLETIME_DEVICE_VCK` const; extend `encode_list_system_counters_reply` to advertise all four counters; add new tests. |
| `crates/yserver-core/src/server.rs` | modify | Add `per_device_last_activity: HashMap<u8, Instant>` and `idletime_last_evaluated: HashMap<u32, i64>` fields on `ServerState`; init in `with_geometry`. Add `idletime_baseline(counter)` and `idletime_alarm_deadline()` helpers + tests. |
| `crates/yserver-core/src/core_loop/process_request.rs` | modify | Replace IDLETIME branch in `QUERY_COUNTER` (`:2292-2299`). Extend `CREATE_ALARM` create-time fire path (`:2330`) to recognise IDLETIME counters. Fix the `delta == 0 + Transition` bug in `evaluate_alarms_for_counter` (`:2588-2589`). Add tests. |
| `crates/yserver-core/src/core_loop/run.rs` | modify | Add `pub(crate) fn evaluate_idletime_alarms_post_poll`. Chain `state.idletime_alarm_deadline()` into the `.min()` poll-deadline at `:393-407`. Call evaluator after `evaluate_screen_saver_post_poll` at `:684`. Add tests. |
| `crates/yserver-core/src/core_loop/key_fanout.rs` | modify | Update `per_device_last_activity[3]` on every key event (prologue, before SS-off sibling check). Add `evaluate_idletime_negative_alarms_on_input_wake` call. Add tests. |
| `crates/yserver-core/src/core_loop/pointer_fanout.rs` | modify | Same shape for `per_device_last_activity[2]`. Add tests. |

No file is created. All edits are additive to existing files.

---

## Pre-existing bug discovered during planning

`evaluate_alarms_for_counter` at `process_request.rs:2588-2589` flips `delta == 0` alarms to `ALARM_STATE_INACTIVE` regardless of test type:

```rust
let (new_wait, new_state) = if delta == 0 {
    (fired_wait, x11sync::ALARM_STATE_INACTIVE)
} else { ... };
```

Per Xorg `Xext/sync.c:548-555`, only `delta == 0 + (PositiveComparison | NegativeComparison)` should go Inactive. `delta == 0 + (PositiveTransition | NegativeTransition)` must stay Active (the trigger is edge-triggered; once the edge passes, the alarm sits quiescent waiting for the next crossing). MATE's two alarms at value=59999 are both `PositiveTransition` and `NegativeTransition` with `delta=0` — under the current bug, they'd fire once and then go Inactive forever (and we'd never see the NegativeTransition fire because the counter going back below 59999 requires the alarm to be Active to be re-evaluated).

**Task 2 fixes this.** It is a precondition for IDLETIME alarms working at all under MATE.

---

## Task 1: IDLETIME counter values + per-device counter IDs + per-device tracking

**Files:**
- Modify: `crates/yserver-protocol/src/x11/sync.rs` (add 2 consts + extend ListSystemCounters reply + 1 test)
- Modify: `crates/yserver-core/src/server.rs` (add 2 fields, init, add `idletime_baseline` helper + 3 tests)
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (replace IDLETIME branch in `QUERY_COUNTER` at `:2292-2299` + 3 tests)
- Modify: `crates/yserver-core/src/core_loop/key_fanout.rs` (update `per_device_last_activity[3]` in prologue + 1 test)
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (update `per_device_last_activity[2]` in prologue + 1 test)

After this commit, `xset q` style clients still see the old behaviour (alarms still don't fire on IDLETIME because nothing evaluates them) but `XSyncQueryCounter(IDLETIME)` now returns the right value. This makes the regression visible to clients that read the counter directly.

- [ ] **Step 1: Add per-device counter consts to `sync.rs`**

In `crates/yserver-protocol/src/x11/sync.rs`, after the existing `pub const IDLETIME_COUNTER: u32 = 0x107;` at `:28`, add:

```rust
/// Per-master-device IDLETIME counters. yserver hard-codes the XI2
/// master pair: VCP=2, VCK=3 (see `key_fanout.rs:29`,
/// `pointer_fanout.rs:30`). Counter IDs picked to avoid collision
/// with anything in `resources.rs`.
pub const IDLETIME_DEVICE_VCP: u32 = 0x108;
pub const IDLETIME_DEVICE_VCK: u32 = 0x109;
```

- [ ] **Step 2: Extend `encode_list_system_counters_reply` to advertise all four counters**

In the same file, find `encode_list_system_counters_reply` at `:325`. Update the `COUNTERS` table:

```rust
    const COUNTERS: &[(u32, i64, &[u8])] = &[
        (SERVERTIME_COUNTER, 4, b"SERVERTIME"),
        (IDLETIME_COUNTER, 4, b"IDLETIME"),
        (IDLETIME_DEVICE_VCP, 4, b"DEVICEIDLETIME 2"),
        (IDLETIME_DEVICE_VCK, 4, b"DEVICEIDLETIME 3"),
    ];
```

- [ ] **Step 3: Update / add the corresponding ListSystemCounters test**

Find the existing test at `:443` (`encode_list_system_counters_reply` + `:449-462` assertions). The test asserts SERVERTIME at one offset and IDLETIME at another. Extend it to also assert the two new counters:

```rust
    #[test]
    fn list_system_counters_advertises_four_counters_with_device_idletime() {
        let reply = encode_list_system_counters_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(0x88),
        );
        // header: tag(1B) data(1B) seq(2B) length(4B) counters_len(4B) pad(20B) = 32B
        assert_eq!(
            u32::from_le_bytes([reply[32], reply[33], reply[34], reply[35]]),
            SERVERTIME_COUNTER,
            "first entry counter id"
        );
        // each entry: counter(4) resolution(8) name_len(2) name(padded to 4)
        // SERVERTIME entry: 14 + 10 = 24 bytes, padded to 24.
        // IDLETIME entry starts at byte 32 + 24 = 56.
        assert_eq!(
            u32::from_le_bytes([reply[56], reply[57], reply[58], reply[59]]),
            IDLETIME_COUNTER,
            "second entry counter id"
        );
        // IDLETIME entry: 14 + 8 = 22, padded to 24. Next at 80.
        assert_eq!(
            u32::from_le_bytes([reply[80], reply[81], reply[82], reply[83]]),
            IDLETIME_DEVICE_VCP,
            "third entry: per-pointer IDLETIME"
        );
        assert_eq!(&reply[94..110], b"DEVICEIDLETIME 2");
        // Per-VCP entry: 14 + 16 = 30, padded to 32. Next at 112.
        assert_eq!(
            u32::from_le_bytes([reply[112], reply[113], reply[114], reply[115]]),
            IDLETIME_DEVICE_VCK,
            "fourth entry: per-keyboard IDLETIME"
        );
        assert_eq!(&reply[126..142], b"DEVICEIDLETIME 3");
    }
```

If a single existing test was named differently and asserts the 2-counter shape, **replace** its body with this 4-counter shape (don't keep two conflicting tests).

- [ ] **Step 4: Run the protocol test to verify the wire layout**

```bash
cargo test -p yserver-protocol list_system_counters
```

Expected: pass (or fail with offset mismatches if the byte arithmetic is off — fix by reading the byte buffer manually with `eprintln!`).

- [ ] **Step 5: Add the per-device tracking + helper to `server.rs`**

In `crates/yserver-core/src/server.rs`, add two fields to `ServerState` (alongside `pub sync_alarms`):

```rust
    /// Per-XI2-master-device idle clock. Key = device id (VCP=2, VCK=3
    /// hard-coded in key_fanout.rs:29 / pointer_fanout.rs:30). Updated
    /// by the fanouts on each input event for the affected device.
    /// `dpms.last_activity` continues to track "any device" — that's the
    /// global IDLETIME baseline.
    pub per_device_last_activity: HashMap<u8, Instant>,

    /// Per-counter cache of the IDLETIME value at the last evaluator
    /// pass. Lets the post-poll evaluator compute `(old, new)`
    /// transitions for `trigger_fires`. Keyed by counter id (one of
    /// IDLETIME_COUNTER / IDLETIME_DEVICE_VCP / IDLETIME_DEVICE_VCK).
    pub idletime_last_evaluated: HashMap<u32, i64>,
```

Init both in `with_geometry` (alongside `sync_alarms: HashMap::new()` at `:575`):

```rust
            per_device_last_activity: HashMap::new(),
            idletime_last_evaluated: HashMap::new(),
```

Add a helper method on `impl ServerState` (near `timestamp_now` at `:528`):

```rust
    /// Baseline `Instant` for an IDLETIME-family counter. Falls back to
    /// `dpms.last_activity` (global) for unknown counters so that a
    /// per-device counter query before any device-specific input has
    /// landed still returns a sensible "any device" idle.
    #[must_use]
    pub fn idletime_baseline(&self, counter: u32) -> Instant {
        use yserver_protocol::x11::sync as x11sync;
        match counter {
            x11sync::IDLETIME_DEVICE_VCP => self
                .per_device_last_activity
                .get(&2)
                .copied()
                .unwrap_or(self.dpms.last_activity),
            x11sync::IDLETIME_DEVICE_VCK => self
                .per_device_last_activity
                .get(&3)
                .copied()
                .unwrap_or(self.dpms.last_activity),
            // Global IDLETIME (and any unknown counter routed here).
            _ => self.dpms.last_activity,
        }
    }
```

- [ ] **Step 6: Add server-state tests for `idletime_baseline`**

In `server.rs`'s `#[cfg(test)] mod tests`, append:

```rust
    #[test]
    fn idletime_baseline_global_returns_dpms_last_activity() {
        use std::time::Instant;
        let mut state = ServerState::new();
        let baseline = Instant::now();
        state.dpms.last_activity = baseline;
        assert_eq!(
            state.idletime_baseline(yserver_protocol::x11::sync::IDLETIME_COUNTER),
            baseline
        );
    }

    #[test]
    fn idletime_baseline_per_device_uses_per_device_entry() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        let global = Instant::now() - Duration::from_secs(60);
        let pointer = Instant::now() - Duration::from_secs(5);
        state.dpms.last_activity = global;
        state.per_device_last_activity.insert(2, pointer);
        assert_eq!(
            state.idletime_baseline(yserver_protocol::x11::sync::IDLETIME_DEVICE_VCP),
            pointer
        );
        // VCK has no per-device entry; falls back to global.
        assert_eq!(
            state.idletime_baseline(yserver_protocol::x11::sync::IDLETIME_DEVICE_VCK),
            global
        );
    }

    #[test]
    fn idletime_baseline_unknown_counter_falls_back_to_global() {
        let mut state = ServerState::new();
        let baseline = state.dpms.last_activity;
        assert_eq!(state.idletime_baseline(0xdead_beef), baseline);
    }
```

Run: `cargo test -p yserver-core idletime_baseline`
Expected: 3 passed.

- [ ] **Step 7: Replace the IDLETIME branch in `QUERY_COUNTER`**

In `crates/yserver-core/src/core_loop/process_request.rs:2292-2299`, the current code is:

```rust
        x11sync::QUERY_COUNTER => {
            let counter = x11sync::parse_resource(body).unwrap_or(0);
            let value = if matches!(
                counter,
                x11sync::SERVERTIME_COUNTER | x11sync::IDLETIME_COUNTER
            ) {
                i64::from(state.timestamp_now())
            } else {
                state.sync_counters.get(&counter).map_or(0, |c| c.value)
            };
```

Replace with:

```rust
        x11sync::QUERY_COUNTER => {
            let counter = x11sync::parse_resource(body).unwrap_or(0);
            let value = match counter {
                x11sync::SERVERTIME_COUNTER => i64::from(state.timestamp_now()),
                x11sync::IDLETIME_COUNTER
                | x11sync::IDLETIME_DEVICE_VCP
                | x11sync::IDLETIME_DEVICE_VCK => {
                    let baseline = state.idletime_baseline(counter);
                    // X11 timestamps are 32-bit ms; saturate at u32::MAX
                    // (~49 days idle) per X11 spec.
                    let elapsed_ms = std::time::Instant::now()
                        .duration_since(baseline)
                        .as_millis()
                        .min(u128::from(u32::MAX)) as u64;
                    elapsed_ms as i64
                }
                _ => state.sync_counters.get(&counter).map_or(0, |c| c.value),
            };
```

- [ ] **Step 8: Add `QUERY_COUNTER` tests for IDLETIME**

In `process_request.rs`'s `#[cfg(test)] mod tests`, append:

```rust
    #[test]
    fn query_counter_idletime_returns_elapsed_since_last_activity_not_uptime() {
        use std::time::Duration;
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(30);

        let header = RequestHeader {
            opcode: 142,
            data: x11sync::QUERY_COUNTER,
            length_units: 2,
        };
        let body = x11sync::IDLETIME_COUNTER.to_le_bytes();
        let _ = handle_sync_request(&mut state, &mut backend, ClientId(1),
                                    SequenceNumber(1), header, &body);

        let bytes = read_all_available(&mut peer);
        // QueryCounter reply: tag(1) data(1) seq(2) length(4) value:i64(8) pad(16)
        assert_eq!(bytes[0], 1, "reply tag");
        let value = i64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11],
            bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        assert!(
            value >= 29_000 && value <= 35_000,
            "IDLETIME ≈ 30_000ms ± scheduling slack; got {value}"
        );
    }

    #[test]
    fn query_counter_idletime_device_vcp_uses_per_device_baseline() {
        use std::time::Duration;
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        // Global was idle 60s ago, but the pointer device is fresh.
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(60);
        state.per_device_last_activity.insert(2,
            std::time::Instant::now() - Duration::from_millis(500));

        let header = RequestHeader {
            opcode: 142,
            data: x11sync::QUERY_COUNTER,
            length_units: 2,
        };
        let body = x11sync::IDLETIME_DEVICE_VCP.to_le_bytes();
        let _ = handle_sync_request(&mut state, &mut backend, ClientId(1),
                                    SequenceNumber(1), header, &body);

        let bytes = read_all_available(&mut peer);
        let value = i64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11],
            bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        assert!(
            value >= 400 && value <= 5_000,
            "VCP IDLETIME ≈ 500ms; got {value}"
        );
    }

    #[test]
    fn query_counter_idletime_device_vck_uses_per_device_baseline() {
        use std::time::Duration;
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(60);
        state.per_device_last_activity.insert(3,
            std::time::Instant::now() - Duration::from_millis(200));

        let header = RequestHeader {
            opcode: 142,
            data: x11sync::QUERY_COUNTER,
            length_units: 2,
        };
        let body = x11sync::IDLETIME_DEVICE_VCK.to_le_bytes();
        let _ = handle_sync_request(&mut state, &mut backend, ClientId(1),
                                    SequenceNumber(1), header, &body);

        let bytes = read_all_available(&mut peer);
        let value = i64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11],
            bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        assert!(
            value >= 100 && value <= 3_000,
            "VCK IDLETIME ≈ 200ms; got {value}"
        );
    }
```

If `x11sync` is not yet imported in the test module, add:

```rust
    use yserver_protocol::x11::sync as x11sync;
```

to the existing test-module `use` block.

- [ ] **Step 9: Update fanouts to maintain `per_device_last_activity`**

In `crates/yserver-core/src/core_loop/key_fanout.rs:47`, the existing prologue line is:

```rust
    state.dpms.last_activity = std::time::Instant::now();
```

Change to:

```rust
    let now = std::time::Instant::now();
    state.dpms.last_activity = now;
    state
        .per_device_last_activity
        .insert(XI2_MASTER_KEYBOARD_DEVICE_ID as u8, now);
```

(Yes, `XI2_MASTER_KEYBOARD_DEVICE_ID` is `u16` but the value is always 3; the cast is safe and the comment in `server.rs` references the u8 keying.)

In `crates/yserver-core/src/core_loop/pointer_fanout.rs:53`, the existing line is the same shape. Change to:

```rust
    let now = std::time::Instant::now();
    state.dpms.last_activity = now;
    state
        .per_device_last_activity
        .insert(XI2_MASTER_POINTER_DEVICE_ID as u8, now);
```

- [ ] **Step 10: Add fanout per-device tracking tests**

In `key_fanout.rs`'s `#[cfg(test)] mod tests`, append:

```rust
    #[test]
    fn key_event_updates_global_and_per_device_vck_last_activity() {
        use std::time::Duration;
        let mut state = ServerState::new();
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(30);
        let stale = state.dpms.last_activity;
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        assert!(state.dpms.last_activity > stale, "global last_activity advanced");
        let vck = state.per_device_last_activity.get(&3).copied()
            .expect("VCK per-device entry inserted");
        assert!(vck > stale, "VCK per-device last_activity advanced");
    }
```

In `pointer_fanout.rs`'s test module, append the analogue (use the existing `motion_event()` or similar test helper — find it via `rg motion_event crates/yserver-core/src/core_loop/pointer_fanout.rs | head` — and assert `per_device_last_activity.get(&2)`).

- [ ] **Step 11: Format, lint, full test, commit**

```bash
cargo +nightly fmt
cargo clippy --workspace
cargo test --workspace
git add crates/yserver-protocol/src/x11/sync.rs crates/yserver-core/src/server.rs crates/yserver-core/src/core_loop/process_request.rs crates/yserver-core/src/core_loop/key_fanout.rs crates/yserver-core/src/core_loop/pointer_fanout.rs
git commit -m "feat(sync): IDLETIME values + per-device counters + per-device tracking

QUERY_COUNTER for IDLETIME now returns (now - last_activity)
instead of (now - server_start). Adds two per-master-device
counters (IDLETIME_DEVICE_VCP=0x108, IDLETIME_DEVICE_VCK=0x109),
advertised in ListSystemCounters as `DEVICEIDLETIME 2`/`DEVICEIDLETIME 3` per Xorg
convention. ServerState gains per_device_last_activity (keyed by
XI2 master device id) which the key/pointer fanouts update
alongside the existing global last_activity.

Alarms on IDLETIME counters still don't fire — that arrives in
Tasks 3 and 4. After this commit, clients QueryCounter-ing
IDLETIME directly (e.g. xset diagnostics) see the right value.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 2: Fix `delta == 0 + Transition` alarm-state bug

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:2588-2589` (`evaluate_alarms_for_counter`)
- Add 1 test (regression)

Pre-existing bug independent of IDLETIME — but a precondition for IDLETIME alarms to work under MATE. Currently every `delta == 0` alarm transitions to Inactive on fire; Xorg only does this for `Comparison` test types. Transitions with `delta == 0` must stay Active so the next crossing re-fires the alarm.

- [ ] **Step 1: Write the failing regression test**

In `process_request.rs`'s `#[cfg(test)] mod tests`, append:

```rust
    #[test]
    fn evaluate_alarms_positive_transition_with_delta_zero_stays_active() {
        // Regression: Xorg sync.c:548-555 — only delta=0 + Comparison
        // goes Inactive. Transitions with delta=0 must stay Active so
        // the next crossing re-fires.
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        let counter = 0x1000;
        state.sync_counters.insert(counter, crate::server::SyncCounter {
            owner: ClientId(1),
            value: 0,
        });
        let alarm_id = 0x2000;
        state.sync_alarms.insert(alarm_id, crate::server::SyncAlarm {
            owner: ClientId(1),
            counter,
            wait_value: 100,
            delta: 0,
            test_type: x11sync::TEST_POSITIVE_TRANSITION as u8,
            events: true,
            state: x11sync::ALARM_STATE_ACTIVE,
        });

        // Counter transition from 50 → 150 crosses the trigger.
        evaluate_alarms_for_counter(&mut state, counter, 50, 150);

        let after = &state.sync_alarms[&alarm_id];
        assert_eq!(
            after.state, x11sync::ALARM_STATE_ACTIVE,
            "PositiveTransition + delta=0 must stay Active (Xorg sync.c:548-555)"
        );
        assert_eq!(after.wait_value, 100, "wait_value unchanged for delta=0 Transition");
    }

    #[test]
    fn evaluate_alarms_positive_comparison_with_delta_zero_goes_inactive() {
        // Companion test: PositiveComparison + delta=0 SHOULD go Inactive.
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        let counter = 0x1000;
        state.sync_counters.insert(counter, crate::server::SyncCounter {
            owner: ClientId(1),
            value: 0,
        });
        let alarm_id = 0x2000;
        state.sync_alarms.insert(alarm_id, crate::server::SyncAlarm {
            owner: ClientId(1),
            counter,
            wait_value: 100,
            delta: 0,
            test_type: x11sync::TEST_POSITIVE_COMPARISON as u8,
            events: true,
            state: x11sync::ALARM_STATE_ACTIVE,
        });

        evaluate_alarms_for_counter(&mut state, counter, 50, 150);

        let after = &state.sync_alarms[&alarm_id];
        assert_eq!(after.state, x11sync::ALARM_STATE_INACTIVE);
    }

    #[test]
    fn evaluate_alarms_re_arm_overflow_transitions_alarm_to_inactive() {
        // Xorg sync.c:589-597 — re-arm overflow on i64::checked_add
        // must transition the alarm to Inactive and leave wait_value
        // unchanged.
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        let counter = 0x1000;
        state.sync_counters.insert(counter, crate::server::SyncCounter {
            owner: ClientId(1),
            value: 0,
        });
        let alarm_id = 0x2000;
        // Start near i64::MAX so adding delta overflows on the very
        // first re-arm step. Use PositiveTransition so the trigger fires
        // when new >= wait_value.
        let near_max = i64::MAX - 10;
        state.sync_alarms.insert(alarm_id, crate::server::SyncAlarm {
            owner: ClientId(1),
            counter,
            wait_value: near_max,
            delta: 100, // would overflow on first add
            test_type: x11sync::TEST_POSITIVE_TRANSITION as u8,
            events: true,
            state: x11sync::ALARM_STATE_ACTIVE,
        });

        // Crossing: old < wait_value, new >= wait_value → trigger fires.
        evaluate_alarms_for_counter(&mut state, counter, near_max - 1, near_max + 5);

        let after = &state.sync_alarms[&alarm_id];
        assert_eq!(after.state, x11sync::ALARM_STATE_INACTIVE,
                   "overflow on re-arm → Inactive");
        assert_eq!(after.wait_value, near_max,
                   "wait_value unchanged on overflow (Xorg sync.c:589-597)");
    }
```

Run: `cargo test -p yserver-core evaluate_alarms_` (shared prefix catches all three new tests; multi-positional cargo-test args don't filter additively).
Expected: the Transition test FAILS (regression — state is currently Inactive); the Comparison test passes; the overflow test FAILS (saturating_add doesn't transition to Inactive).

- [ ] **Step 2: Apply the fix (delta=0 state-transition + overflow→Inactive + pub(crate) visibility)**

In `process_request.rs:2563`, change the function signature from `fn evaluate_alarms_for_counter` to `pub(crate) fn evaluate_alarms_for_counter` so `run.rs` (Task 4) can call it.

In the same function at `:2588-2605`, the current body is:

```rust
        let (new_wait, new_state) = if delta == 0 {
            (fired_wait, x11sync::ALARM_STATE_INACTIVE)
        } else {
            // Advance past the current value so the next crossing fires
            // again instead of re-triggering on this same value. Bounded
            // in case delta points away from the test direction.
            let mut w = fired_wait;
            let mut guard = 0u32;
            while x11sync::comparison_satisfied(test_type, new, w) && guard < 1_000_000 {
                let next = w.saturating_add(delta);
                if next == w {
                    break;
                }
                w = next;
                guard += 1;
            }
            (w, x11sync::ALARM_STATE_ACTIVE)
        };
```

Replace with the corrected three-branch logic AND `checked_add`-based overflow→Inactive (per Xorg `sync.c:589-597`):

```rust
        // Per Xorg sync.c:548-555: state → Inactive when
        // (counter==None) OR (delta==0 AND test_type is Comparison).
        // Transitions with delta=0 stay Active — once the edge passes,
        // the alarm sits quiescent waiting for the next crossing.
        // Per Xorg sync.c:589-597: re-arm overflow → state Inactive
        // (test_value left unmodified).
        let is_comparison = matches!(
            test_type,
            x11sync::TEST_POSITIVE_COMPARISON | x11sync::TEST_NEGATIVE_COMPARISON
        );
        let (new_wait, new_state) = if delta == 0 && is_comparison {
            (fired_wait, x11sync::ALARM_STATE_INACTIVE)
        } else if delta == 0 {
            // Transition + delta=0: stay Active, wait_value unchanged.
            (fired_wait, x11sync::ALARM_STATE_ACTIVE)
        } else {
            // delta != 0: re-arm by adding delta until trigger stops firing.
            // Overflow on the addition → state Inactive, wait_value reverts
            // to fired_wait (Xorg sync.c:589-597).
            let mut w = fired_wait;
            let mut guard = 0u32;
            let mut overflowed = false;
            while x11sync::comparison_satisfied(test_type, new, w) && guard < 1_000_000 {
                match w.checked_add(delta) {
                    Some(next) if next != w => {
                        w = next;
                        guard += 1;
                    }
                    Some(_) => break, // delta=0 inside this branch can't happen, defensive
                    None => {
                        overflowed = true;
                        break;
                    }
                }
            }
            if overflowed {
                (fired_wait, x11sync::ALARM_STATE_INACTIVE)
            } else {
                (w, x11sync::ALARM_STATE_ACTIVE)
            }
        };
```

- [ ] **Step 3: Run the three regression tests to verify they pass**

```bash
cargo test -p yserver-core evaluate_alarms_
```

Expected: all three pass (Transition stays Active, Comparison goes Inactive, overflow goes Inactive).

- [ ] **Step 4: Run full crate test to check for regressions in other alarm tests**

```bash
cargo test -p yserver-core
```

Expected: all green. If any existing test asserted the buggy `delta == 0 + Transition → Inactive` behaviour, **fix the test** (it was asserting the bug). Comment why in the commit message.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver-core
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "fix(sync): keep Transition alarms Active when delta=0

evaluate_alarms_for_counter was sending every delta=0 alarm to
ALARM_STATE_INACTIVE on fire. Xorg sync.c:548-555 only does
this for the two Comparison test types — Transitions with
delta=0 must stay Active so the next crossing re-fires.

Precondition for IDLETIME alarms working under MATE: MATE's
mate-power-manager creates two delta=0 alarms at value=59999
(PositiveTransition for idle, NegativeTransition for wake);
under the old code the wake alarm went Inactive after the
idle alarm fired and never re-fired on input.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 3: `idletime_alarm_deadline` + chain into poll-deadline `.min()`

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (add `idletime_alarm_deadline()` + 5 tests)
- Modify: `crates/yserver-core/src/core_loop/run.rs` (chain into `.min()` at `:393-407`)

After this commit, the poll-deadline wakes when an IDLETIME PositiveTransition / PositiveComparison alarm would fire — but the evaluator that actually fires it lands in Task 4.

- [ ] **Step 1: Write failing tests**

In `server.rs`'s `#[cfg(test)] mod tests`, append:

```rust
    #[test]
    fn idletime_alarm_deadline_none_when_no_alarms() {
        let state = ServerState::new();
        assert!(state.idletime_alarm_deadline().is_none());
    }

    #[test]
    fn idletime_alarm_deadline_picks_smallest_active_pos_alarm() {
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let baseline = std::time::Instant::now();
        state.dpms.last_activity = baseline;

        for (id, wait) in &[(1u32, 60_000i64), (2, 30_000), (3, 90_000)] {
            state.sync_alarms.insert(*id, crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_COUNTER,
                wait_value: *wait,
                delta: 0,
                test_type: x11sync::TEST_POSITIVE_TRANSITION as u8,
                events: true,
                state: x11sync::ALARM_STATE_ACTIVE,
            });
        }

        let deadline = state.idletime_alarm_deadline().expect("Some");
        let expected = baseline + Duration::from_millis(30_000);
        // Allow ±1ms for monotonic-clock resolution.
        let diff = if deadline > expected {
            deadline - expected
        } else {
            expected - deadline
        };
        assert!(diff < Duration::from_millis(2),
                "deadline ~ baseline + 30_000ms; got diff {diff:?}");
    }

    #[test]
    fn idletime_alarm_deadline_ignores_negative_alarms() {
        // Negative-* alarms only fire on input wake, not on a positive
        // deadline. They must not be considered when computing the
        // poll-deadline `.min()`.
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        state.sync_alarms.insert(1, crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_COUNTER,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_NEGATIVE_TRANSITION as u8,
            events: true,
            state: x11sync::ALARM_STATE_ACTIVE,
        });
        assert!(state.idletime_alarm_deadline().is_none());
    }

    #[test]
    fn idletime_alarm_deadline_ignores_inactive_alarms() {
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        state.sync_alarms.insert(1, crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_COUNTER,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_POSITIVE_TRANSITION as u8,
            events: true,
            state: x11sync::ALARM_STATE_INACTIVE,
        });
        assert!(state.idletime_alarm_deadline().is_none());
    }

    #[test]
    fn idletime_alarm_deadline_none_when_screensaver_suspended() {
        // Mirrors the dpms_transition_deadline suspend gate
        // (server.rs ~:542). XScreenSaverSuspend inhibits both the
        // DPMS cascade AND IDLETIME alarms so fullscreen video
        // (Firefox / mpv / vlc) doesn't blank the screen.
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        state.screensaver.suspend_counts.insert(ClientId(99), 1);
        state.sync_alarms.insert(1, crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_COUNTER,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_POSITIVE_TRANSITION as u8,
            events: true,
            state: x11sync::ALARM_STATE_ACTIVE,
        });
        assert!(state.idletime_alarm_deadline().is_none());
    }
```

(`ClientId` is already imported into the test module from Task 1's tests; if not, add `use yserver_protocol::x11::ClientId;`.)

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p yserver-core idletime_alarm_deadline
```

Expected: compile error — `idletime_alarm_deadline` not defined.

- [ ] **Step 3: Implement `idletime_alarm_deadline`**

In `server.rs`'s `impl ServerState`, append (near `idletime_baseline` from Task 1):

```rust
    /// Earliest instant any Active IDLETIME alarm could fire from idle
    /// progression alone. Negative-* alarms fire on input wake (handled
    /// by the fanouts), so they don't contribute to this deadline.
    /// Returns `None` when no eligible alarm exists, or when
    /// `XScreenSaverSuspend` has gated the unified timer (Xorg
    /// WaitFor.c:519).
    ///
    /// **Quiescent-state handling.** A `PositiveTransition + delta=0`
    /// alarm that has already fired stays Active but is *quiescent*
    /// until the counter drops below `wait_value` and crosses up again
    /// (which requires an input event resetting `last_activity`). For
    /// such alarms, the deadline only contributes when current idle is
    /// strictly below `wait_value`. Without this check the poll-min
    /// would lock at a past `Instant` forever and spin with
    /// `Duration::ZERO`. `PositiveComparison` is level-triggered: a
    /// `delta=0` Comparison transitions to Inactive on fire (Xorg
    /// `sync.c:548-555`) so it never re-enters this path; a
    /// `delta != 0` Comparison re-arms `wait_value` past the current
    /// value, so by construction `current_idle < wait_value` and the
    /// deadline is in the future.
    #[must_use]
    pub fn idletime_alarm_deadline(&self) -> Option<std::time::Instant> {
        use yserver_protocol::x11::sync as x11sync;
        if !self.screensaver.suspend_counts.is_empty() {
            return None;
        }
        let now = std::time::Instant::now();
        let mut earliest: Option<std::time::Instant> = None;
        for alarm in self.sync_alarms.values() {
            if alarm.state != x11sync::ALARM_STATE_ACTIVE {
                continue;
            }
            if !matches!(
                alarm.counter,
                x11sync::IDLETIME_COUNTER
                | x11sync::IDLETIME_DEVICE_VCP
                | x11sync::IDLETIME_DEVICE_VCK
            ) {
                continue;
            }
            let test_type = u32::from(alarm.test_type);
            if !matches!(
                test_type,
                x11sync::TEST_POSITIVE_TRANSITION | x11sync::TEST_POSITIVE_COMPARISON
            ) {
                continue;
            }
            if alarm.wait_value < 0 {
                continue; // negative wait_value can't be reached by idle (unsigned ms)
            }
            let baseline = self.idletime_baseline(alarm.counter);
            // Quiescent-state skip: drop alarms whose threshold is already
            // at-or-below current idle. They've already fired (Transition)
            // or would re-fire every poll (Comparison) — neither shape
            // contributes a future-instant deadline. They re-enter the
            // deadline only after an input event resets `baseline`, at
            // which point `current_idle < wait_value` again.
            #[allow(clippy::cast_sign_loss)]
            let current_idle_ms = now
                .duration_since(baseline)
                .as_millis()
                .min(u128::from(u32::MAX)) as i64;
            if current_idle_ms >= alarm.wait_value {
                continue;
            }
            #[allow(clippy::cast_sign_loss)]
            let fire_at =
                baseline + std::time::Duration::from_millis(alarm.wait_value as u64);
            earliest = Some(earliest.map_or(fire_at, |e| e.min(fire_at)));
        }
        earliest
    }
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p yserver-core idletime_alarm_deadline
```

Expected: 5 passed.

- [ ] **Step 5: Chain `idletime_alarm_deadline()` into the poll-deadline `.min()`**

In `crates/yserver-core/src/core_loop/run.rs:393-407`, the existing chain (after MIT-SCREEN-SAVER landed) is:

```rust
            let repeat_deadline = state.repeat_state.as_ref().map(|r| r.next_fire);
            let backend_deadline = backend.next_wakeup();
            let dpms_deadline = state.dpms_transition_deadline();
            let ss_idle_deadline = state.screensaver_idle_deadline();
            let ss_cycle_deadline = state.screensaver_cycle_deadline();
            repeat_deadline
                .into_iter()
                .chain(backend_deadline)
                .chain(dpms_deadline)
                .chain(ss_idle_deadline)
                .chain(ss_cycle_deadline)
                .min()
                .map(|deadline| {
                    deadline
                        .checked_duration_since(now)
                        .unwrap_or(Duration::ZERO)
                })
```

Add the IDLETIME deadline:

```rust
            let repeat_deadline = state.repeat_state.as_ref().map(|r| r.next_fire);
            let backend_deadline = backend.next_wakeup();
            let dpms_deadline = state.dpms_transition_deadline();
            let ss_idle_deadline = state.screensaver_idle_deadline();
            let ss_cycle_deadline = state.screensaver_cycle_deadline();
            let idletime_alarm_deadline = state.idletime_alarm_deadline();
            repeat_deadline
                .into_iter()
                .chain(backend_deadline)
                .chain(dpms_deadline)
                .chain(ss_idle_deadline)
                .chain(ss_cycle_deadline)
                .chain(idletime_alarm_deadline)
                .min()
                .map(|deadline| {
                    deadline
                        .checked_duration_since(now)
                        .unwrap_or(Duration::ZERO)
                })
```

- [ ] **Step 6: Workspace test + format + lint + commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver-core
cargo test -p yserver-core
git add crates/yserver-core/src/server.rs crates/yserver-core/src/core_loop/run.rs
git commit -m "feat(sync): idletime_alarm_deadline + chain into poll-min

ServerState::idletime_alarm_deadline returns the earliest instant
any Active Positive-* alarm on an IDLETIME counter could fire
from idle progression. Negative-* alarms only fire on input wake
(handled in Task 4's fanout integration). Suspended via
XScreenSaverSuspend → None (mirrors dpms_transition_deadline's
unified-timer gate).

Chained into run.rs's poll-deadline .min() alongside the existing
DPMS + SS deadlines. The evaluator that turns this wake into
AlarmNotify lands in Task 4.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 4: `evaluate_idletime_alarms_post_poll` evaluator + create-time fire path

**Files:**
- Modify: `crates/yserver-core/src/core_loop/run.rs` (add `evaluate_idletime_alarms_post_poll` + 2 tests + call from outer loop)
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (extend `CREATE_ALARM` create-time fire at `:2330` to use `idletime_baseline` for IDLETIME counters)

After this commit, idle-progression PositiveTransition/PositiveComparison alarms fire correctly. NegativeTransition firing on input wake still requires Task 5.

- [ ] **Step 1: Write the failing run-loop tests**

In `crates/yserver-core/src/core_loop/run.rs`'s test module (existing — see the SS evaluator tests added in MIT-SCREEN-SAVER Task 6), append:

```rust
    #[test]
    fn idletime_evaluator_fires_pos_transition_when_deadline_elapsed() {
        use std::time::Duration;
        use yserver_protocol::x11::{ClientId, sync as x11sync};
        let mut state = ServerState::new();
        // Pre-arm: a PositiveTransition alarm at 60_000ms, last_activity 61s ago.
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(61);
        let alarm_id = 0x2000;
        state.sync_alarms.insert(alarm_id, crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_COUNTER,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_POSITIVE_TRANSITION as u8,
            events: false,  // skip wire delivery; assert state mutation only
            state: x11sync::ALARM_STATE_ACTIVE,
        });
        let mut backend = RecordingBackend::default();

        super::evaluate_idletime_alarms_post_poll(&mut state, &mut backend);

        // PositiveTransition + delta=0 stays Active (Task 2 fix).
        let after = &state.sync_alarms[&alarm_id];
        assert_eq!(after.state, x11sync::ALARM_STATE_ACTIVE);
        // last_evaluated cache updated for global IDLETIME.
        assert!(
            state.idletime_last_evaluated
                .get(&x11sync::IDLETIME_COUNTER).copied()
                .unwrap_or(0) >= 60_000,
            "last_evaluated cache should advance past the trigger value"
        );
    }

    #[test]
    fn idletime_evaluator_skips_when_no_idletime_alarms() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::default();
        // No alarms at all — must not panic, must not insert spurious cache entries.
        super::evaluate_idletime_alarms_post_poll(&mut state, &mut backend);
        assert!(state.idletime_last_evaluated.is_empty());
    }
```

If `RecordingBackend` / `ServerState` / `ScreenSaverActive` imports were already added to this test module by MIT-SCREEN-SAVER Task 6, reuse them; otherwise add the same imports as Task 6 used.

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p yserver-core idletime_evaluator
```

Expected: compile error — `evaluate_idletime_alarms_post_poll` not defined.

- [ ] **Step 3: Implement `evaluate_idletime_alarms_post_poll` in `run.rs`**

In `run.rs` (alongside `evaluate_screen_saver_post_poll`):

```rust
/// Post-poll IDLETIME alarm evaluator. For each IDLETIME counter,
/// compute the current idle, walk Active alarms referencing the
/// counter, run the test-type check against the cached
/// `(last_evaluated, current)` pair, and fire via
/// `evaluate_alarms_for_counter` (which handles re-arm + emission).
/// Mirrors Xorg's `IdleTimeBlockHandler` + `IdleTimeWakeupHandler`
/// (sync.c:2647, :2750).
pub(crate) fn evaluate_idletime_alarms_post_poll(
    state: &mut ServerState,
    _backend: &mut dyn crate::backend::Backend,
) {
    use yserver_protocol::x11::sync as x11sync;
    // Suspend gate (Xorg WaitFor.c:519 unified-timer rule) — mirrors
    // `idletime_alarm_deadline`. Skip the whole evaluator when any
    // client holds XScreenSaverSuspend; otherwise an unrelated wake
    // could still fire Positive alarms mid-fullscreen-video.
    if !state.screensaver.suspend_counts.is_empty() {
        return;
    }
    const IDLETIME_COUNTERS: &[u32] = &[
        x11sync::IDLETIME_COUNTER,
        x11sync::IDLETIME_DEVICE_VCP,
        x11sync::IDLETIME_DEVICE_VCK,
    ];
    let now = Instant::now();
    for &counter in IDLETIME_COUNTERS {
        // Skip if no alarms reference this counter.
        let has_alarm = state.sync_alarms.values().any(|a| {
            a.counter == counter && a.state == x11sync::ALARM_STATE_ACTIVE
        });
        if !has_alarm {
            continue;
        }
        let baseline = state.idletime_baseline(counter);
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
        let current_idle = now
            .duration_since(baseline)
            .as_millis()
            .min(u128::from(u32::MAX)) as i64;
        let old_idle = state
            .idletime_last_evaluated
            .get(&counter)
            .copied()
            .unwrap_or(0);
        // Run the existing evaluator helper — it walks Active alarms,
        // calls trigger_fires, applies the Task 2 state-transition fix,
        // emits AlarmNotify, and updates wait_value.
        crate::core_loop::process_request::evaluate_alarms_for_counter(
            state, counter, old_idle, current_idle,
        );
        state.idletime_last_evaluated.insert(counter, current_idle);
    }
}
```

- [ ] **Step 4: Call the evaluator from the outer loop**

In `run.rs`, immediately after the existing `evaluate_screen_saver_post_poll(state, backend);` call (added by MIT-SCREEN-SAVER Task 6 at `:688`), add:

```rust
        evaluate_idletime_alarms_post_poll(state, backend);
```

- [ ] **Step 5a: Fix relative `wait_value` resolution for IDLETIME counters in `apply_alarm_attributes`**

`apply_alarm_attributes` at `process_request.rs:2544-2549` resolves Relative-value alarms by reading the current counter value from `state.sync_counters`, which doesn't contain IDLETIME counters. So a relative IDLETIME alarm currently resolves against `0` instead of "current idle". Xorg's resolver queries the system-counter value before resolving relative triggers (`/home/jos/Projects/xserver/Xext/sync.c:337-366`).

Replace the body of the `if relative { ... }` branch:

```rust
    if attrs.value.is_some() || attrs.value_type.is_some() {
        let value = attrs.value.unwrap_or(0);
        let relative = attrs.value_type == Some(x11sync::VALUE_TYPE_RELATIVE);
        alarm.wait_value = if relative {
            let current = match alarm.counter {
                x11sync::IDLETIME_COUNTER
                | x11sync::IDLETIME_DEVICE_VCP
                | x11sync::IDLETIME_DEVICE_VCK => {
                    let baseline = state.idletime_baseline(alarm.counter);
                    #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
                    let v = std::time::Instant::now()
                        .duration_since(baseline)
                        .as_millis()
                        .min(u128::from(u32::MAX)) as i64;
                    v
                }
                _ => state.sync_counters.get(&alarm.counter).map_or(0, |c| c.value),
            };
            current.saturating_add(value)
        } else {
            value
        };
    }
```

(`x11sync` is already imported at the top of `apply_alarm_attributes`; check with `rg "use yserver_protocol::x11::sync" crates/yserver-core/src/core_loop/process_request.rs | head` — should be in scope from the parent handler.)

Add a regression test in the same test module:

```rust
    #[test]
    fn relative_alarm_value_on_idletime_resolves_against_current_idle() {
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        // Pre-condition: already idle 5s.
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(5);

        // CreateAlarm relative-value=10_000 → wait_value should resolve
        // to current_idle (~5_000) + 10_000 = ~15_000.
        let alarm_id: u32 = 0x2000_0002;
        let mut body = Vec::new();
        body.extend_from_slice(&alarm_id.to_le_bytes());
        let mask: u32 = x11sync::CA_COUNTER | x11sync::CA_VALUE_TYPE | x11sync::CA_VALUE
            | x11sync::CA_TEST_TYPE | x11sync::CA_DELTA | x11sync::CA_EVENTS;
        body.extend_from_slice(&mask.to_le_bytes());
        body.extend_from_slice(&x11sync::IDLETIME_COUNTER.to_le_bytes());
        body.extend_from_slice(&x11sync::VALUE_TYPE_RELATIVE.to_le_bytes()); // u32
        body.extend_from_slice(&10_000i64.to_le_bytes()); // value = 10_000 relative
        body.extend_from_slice(&(x11sync::TEST_POSITIVE_TRANSITION as u32).to_le_bytes());
        body.extend_from_slice(&0i64.to_le_bytes()); // delta = 0
        body.push(1); // events = true
        body.extend_from_slice(&[0u8; 3]);

        let header = RequestHeader {
            opcode: 142,
            data: x11sync::CREATE_ALARM,
            length_units: u16::try_from(2 + body.len() / 4).unwrap(),
        };
        let _ = handle_sync_request(&mut state, &mut backend, ClientId(1),
                                    SequenceNumber(1), header, &body);

        let alarm = state.sync_alarms.get(&alarm_id).expect("alarm exists");
        assert!(
            alarm.wait_value >= 14_500 && alarm.wait_value <= 16_000,
            "wait_value ≈ current_idle(5000) + 10000 = 15000; got {}",
            alarm.wait_value
        );
    }
```

- [ ] **Step 5b: Extend the `CREATE_ALARM` create-time fire path**

In `process_request.rs` at `:2329-2330`, the current code is:

```rust
                // Per the X Synchronization Extension spec the trigger is
                // tested at creation: a comparison alarm whose condition
                // already holds fires immediately.
                let now = state.sync_counters.get(&counter).map_or(0, |c| c.value);
                evaluate_alarms_for_counter(state, counter, now, now);
```

That reads `state.sync_counters` which doesn't contain IDLETIME counters — so an IDLETIME alarm created when the user has already been idle past its threshold wouldn't fire at create time. Fix by branching on counter id:

```rust
                use yserver_protocol::x11::sync as x11sync;
                // Per the X Synchronization Extension spec the trigger is
                // tested at creation: a comparison alarm whose condition
                // already holds fires immediately. For IDLETIME-family
                // counters the value is derived from last_activity rather
                // than `sync_counters`.
                let now_value = if matches!(
                    counter,
                    x11sync::IDLETIME_COUNTER
                    | x11sync::IDLETIME_DEVICE_VCP
                    | x11sync::IDLETIME_DEVICE_VCK
                ) {
                    let baseline = state.idletime_baseline(counter);
                    #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
                    let v = std::time::Instant::now()
                        .duration_since(baseline)
                        .as_millis()
                        .min(u128::from(u32::MAX)) as i64;
                    v
                } else {
                    state.sync_counters.get(&counter).map_or(0, |c| c.value)
                };
                evaluate_alarms_for_counter(state, counter, now_value, now_value);
```

(The duplicate `use yserver_protocol::x11::sync as x11sync;` is fine — the surrounding handler already has it.)

Apply the same change in `CHANGE_ALARM`'s analogous create-time-fire path. Find it via `rg -n "evaluate_alarms_for_counter\(state, counter" crates/yserver-core/src/core_loop/process_request.rs` — should be 2 call sites within ~50 lines of each other.

- [ ] **Step 6: Add a create-time-fire test for IDLETIME**

In `process_request.rs`'s `#[cfg(test)] mod tests`, append:

```rust
    #[test]
    fn create_alarm_on_idletime_with_comparison_already_true_fires_immediately() {
        // Xorg sync.c:1772-1775 — create-time trigger eval passes
        // (old=current, new=current). PositiveTransition requires
        // `old < wait <= new` so it cannot fire on a same-value pair;
        // only Comparison test types can fire at create time.
        // PositiveComparison fires when `new >= wait`, which is
        // exactly the "client opens an alarm while user is already
        // idle past the threshold" smoke we want to verify.
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        // Pre-condition: already idle past the alarm threshold.
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(120);

        let alarm_id: u32 = 0x2000_0001;
        let mut body = Vec::new();
        body.extend_from_slice(&alarm_id.to_le_bytes());
        let mask: u32 = x11sync::CA_COUNTER | x11sync::CA_VALUE_TYPE | x11sync::CA_VALUE
            | x11sync::CA_TEST_TYPE | x11sync::CA_DELTA | x11sync::CA_EVENTS;
        body.extend_from_slice(&mask.to_le_bytes());
        body.extend_from_slice(&x11sync::IDLETIME_COUNTER.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // value_type = Absolute
        body.extend_from_slice(&60_000i64.to_le_bytes()); // value = 60_000
        body.extend_from_slice(&(x11sync::TEST_POSITIVE_COMPARISON as u32).to_le_bytes());
        body.extend_from_slice(&0i64.to_le_bytes()); // delta = 0 → goes Inactive on fire
        body.push(1); // events = true
        body.extend_from_slice(&[0u8; 3]);

        let header = RequestHeader {
            opcode: 142,
            data: x11sync::CREATE_ALARM,
            length_units: u16::try_from(2 + body.len() / 4).unwrap(),
        };
        let _ = handle_sync_request(&mut state, &mut backend, ClientId(1),
                                    SequenceNumber(1), header, &body);

        let bytes = read_all_available(&mut peer);
        // AlarmNotify event: type = SYNC_FIRST_EVENT(83) + ALARM_NOTIFY_KIND(1) = 84
        assert!(
            bytes.iter().any(|&b| b == 84),
            "AlarmNotify must fire at create-time for an already-idle PositiveComparison alarm; got {:?}",
            bytes
        );
        // Comparison + delta=0 transitions to Inactive on fire.
        assert_eq!(
            state.sync_alarms[&alarm_id].state,
            x11sync::ALARM_STATE_INACTIVE
        );
    }

    #[test]
    fn create_alarm_on_idletime_with_pos_transition_does_not_fire_at_create() {
        // Companion test: PositiveTransition cannot fire at create-time
        // (Xorg sync.c:1772-1775 — old==new). The alarm sits Active
        // waiting for the post-poll evaluator or input wake to drive
        // the (old, new) pair across the threshold.
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(120);

        let alarm_id: u32 = 0x2000_0003;
        let mut body = Vec::new();
        body.extend_from_slice(&alarm_id.to_le_bytes());
        let mask: u32 = x11sync::CA_COUNTER | x11sync::CA_VALUE_TYPE | x11sync::CA_VALUE
            | x11sync::CA_TEST_TYPE | x11sync::CA_DELTA | x11sync::CA_EVENTS;
        body.extend_from_slice(&mask.to_le_bytes());
        body.extend_from_slice(&x11sync::IDLETIME_COUNTER.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&60_000i64.to_le_bytes());
        body.extend_from_slice(&(x11sync::TEST_POSITIVE_TRANSITION as u32).to_le_bytes());
        body.extend_from_slice(&0i64.to_le_bytes());
        body.push(1);
        body.extend_from_slice(&[0u8; 3]);

        let header = RequestHeader {
            opcode: 142,
            data: x11sync::CREATE_ALARM,
            length_units: u16::try_from(2 + body.len() / 4).unwrap(),
        };
        let _ = handle_sync_request(&mut state, &mut backend, ClientId(1),
                                    SequenceNumber(1), header, &body);

        let bytes = read_all_available(&mut peer);
        assert!(
            !bytes.iter().any(|&b| b == 84),
            "PositiveTransition with old==new at create-time must not fire"
        );
        assert_eq!(
            state.sync_alarms[&alarm_id].state,
            x11sync::ALARM_STATE_ACTIVE,
            "alarm stays Active, waiting for evaluator/input to drive the transition"
        );
    }
```

If `CA_*` constants are not all in `x11sync`, check the existing `parse_alarm_attributes` in `sync.rs` for the actual constant names; the mask bit layout is canonical XSync (XCB calls them `XSyncCA*`).

- [ ] **Step 7: Run all Task 4 tests + workspace test**

```bash
cargo test -p yserver-core idletime_evaluator
cargo test -p yserver-core create_alarm_on_idletime
cargo test -p yserver-core relative_alarm_value_on_idletime
cargo test -p yserver-core
```

Expected: 3 new tests pass; full crate green.

- [ ] **Step 8: Format, lint, commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver-core
git add crates/yserver-core/src/core_loop/run.rs crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(sync): IDLETIME post-poll evaluator + create-time fire

evaluate_idletime_alarms_post_poll runs each loop iteration after
evaluate_screen_saver_post_poll. For each IDLETIME counter it
computes current idle, looks up the cached last-evaluated value,
and delegates to the existing evaluate_alarms_for_counter
(extended in Task 2 to handle delta=0 Transitions correctly).

CREATE_ALARM and CHANGE_ALARM's create-time fire paths now branch
on counter type — IDLETIME counters derive the current value from
last_activity instead of looking up sync_counters (which never
contains the system counters). Without this branch, an alarm
created while the user is already idle past its threshold would
not fire until the next idle re-arm.

NegativeTransition firing on input wake still requires Task 5.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 5: Fanout-prologue NegativeTransition firing on input wake

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (factor a small helper `evaluate_idletime_negative_alarms_on_input` callable from both fanouts)
- Modify: `crates/yserver-core/src/core_loop/key_fanout.rs` (call the helper in the prologue, BEFORE the SS-off sibling check)
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (same)

After this commit, IDLETIME alarms fire correctly in both directions: idle-progression Pos alarms via the post-poll evaluator (Task 4), input-wake Neg alarms via this task's fanout-prologue handler. End-to-end MATE smoke works.

- [ ] **Step 1: Write the failing fanout tests**

In `key_fanout.rs`'s `#[cfg(test)] mod tests`, append:

```rust
    #[test]
    fn key_event_fires_neg_transition_alarm_when_prior_idle_crosses_threshold() {
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        // User idle for 90s, NegativeTransition alarm at 60s.
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
        state.per_device_last_activity.insert(3,
            std::time::Instant::now() - Duration::from_secs(90));
        let alarm_id = 0x2000;
        state.sync_alarms.insert(alarm_id, crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_COUNTER,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_NEGATIVE_TRANSITION as u8,
            events: false, // assert state mutation, not wire delivery
            state: x11sync::ALARM_STATE_ACTIVE,
        });
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        // Alarm stays Active (Transition + delta=0 — Task 2 fix), but
        // last_evaluated cache should advance to 0 (current idle after
        // last_activity reset).
        let after = &state.sync_alarms[&alarm_id];
        assert_eq!(after.state, x11sync::ALARM_STATE_ACTIVE);
        // last_evaluated[IDLETIME] should now reflect post-wake idle = 0
        assert_eq!(
            state.idletime_last_evaluated
                .get(&x11sync::IDLETIME_COUNTER).copied(),
            Some(0),
            "post-wake last_evaluated should be 0"
        );
    }
```

In `pointer_fanout.rs`'s test module, append the pointer analogue (use the file's existing pointer-event fixture; key device id is 3 → swap to 2 for VCP).

Append one more test in `key_fanout.rs`'s test module covering per-device IDLETIME (the bug-fix above's regression target):

```rust
    #[test]
    fn key_event_fires_neg_transition_alarm_on_per_device_idletime_vck() {
        // Regression for the prior `unwrap_or(0)` bug: a NegativeTransition
        // alarm on IDLETIME_DEVICE_VCK must fire on the very first input
        // even if `per_device_last_activity[3]` has no entry yet (the
        // first-input case). Without the fallback-to-global fix, the
        // computed `prior_device` would be 0 and the trigger
        // `old > wait_value && new <= wait_value` would not hold.
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
        // Deliberately do NOT insert into per_device_last_activity[3] —
        // simulate the first-input-for-this-device case.
        assert!(state.per_device_last_activity.get(&3).is_none());

        let alarm_id = 0x3000;
        state.sync_alarms.insert(alarm_id, crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_DEVICE_VCK,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_NEGATIVE_TRANSITION as u8,
            events: false,
            state: x11sync::ALARM_STATE_ACTIVE,
        });
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        // Alarm stays Active (Transition + delta=0 — Task 2 fix), and the
        // per-device last_evaluated cache should reflect post-wake idle = 0.
        assert_eq!(
            state.sync_alarms[&alarm_id].state,
            x11sync::ALARM_STATE_ACTIVE
        );
        assert_eq!(
            state.idletime_last_evaluated
                .get(&x11sync::IDLETIME_DEVICE_VCK).copied(),
            Some(0)
        );
    }
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test -p yserver-core key_event_fires_neg_transition pointer_event_fires_neg_transition
```

Expected: tests pass on the "state stays Active" assertion (the fanout already exists) but FAIL on the `last_evaluated` assertion (no wake handler yet inserts the entry).

- [ ] **Step 3: Add the helper in `process_request.rs`**

Just below `apply_screen_saver_transition` (or any nearby spot — `process_request.rs` is large, anywhere alongside the other `pub(crate)` helpers is fine):

```rust
/// Evaluate Negative-* alarms on IDLETIME-family counters after input
/// wake. Called from the key + pointer fanout prologues immediately
/// after `last_activity` is updated to "now". The semantic is:
/// `old_idle = (now - prior_last_activity)`, `new_idle = 0`. Fires any
/// alarms whose trigger crosses on this transition.
///
/// `device_id` identifies which per-device counter to evaluate; pass
/// 2 for pointer events (drives IDLETIME_DEVICE_VCP), 3 for key events
/// (drives IDLETIME_DEVICE_VCK). The global IDLETIME counter is
/// evaluated unconditionally because any input resets the global
/// last_activity.
pub(crate) fn evaluate_idletime_negative_alarms_on_input_wake(
    state: &mut ServerState,
    device_id: u8,
    prior_global_idle_ms: i64,
    prior_device_idle_ms: i64,
) {
    use yserver_protocol::x11::sync as x11sync;
    // Suspend gate (Xorg WaitFor.c:519). When XScreenSaverSuspend is
    // held, the unified timer is not armed in either direction —
    // including the input-wake firing of Negative-* alarms. Without
    // this gate, an input event during fullscreen video would still
    // fire MATE's wake alarms and could prompt mate-power-manager to
    // re-arm screen-blanking too aggressively.
    if !state.screensaver.suspend_counts.is_empty() {
        return;
    }
    // Global IDLETIME: always reset on any input.
    evaluate_alarms_for_counter(
        state,
        x11sync::IDLETIME_COUNTER,
        prior_global_idle_ms,
        0,
    );
    state.idletime_last_evaluated.insert(x11sync::IDLETIME_COUNTER, 0);

    // Per-device IDLETIME: only the affected device resets.
    let device_counter = match device_id {
        2 => x11sync::IDLETIME_DEVICE_VCP,
        3 => x11sync::IDLETIME_DEVICE_VCK,
        _ => return,
    };
    evaluate_alarms_for_counter(state, device_counter, prior_device_idle_ms, 0);
    state.idletime_last_evaluated.insert(device_counter, 0);
}
```

- [ ] **Step 4: Wire the helper into `key_fanout.rs`**

In `key_fanout.rs`, the prologue (added in Task 1, Step 9) now reads approximately:

```rust
    let now = std::time::Instant::now();
    state.dpms.last_activity = now;
    state.per_device_last_activity.insert(XI2_MASTER_KEYBOARD_DEVICE_ID as u8, now);
    if state.dpms.enabled && state.dpms.power_level != 0 {
        crate::core_loop::process_request::apply_dpms_transition(state, backend, 0);
    }
    if matches!(state.screensaver.active, crate::server::ScreenSaverActive::On) {
        crate::core_loop::process_request::apply_screen_saver_transition(
            state, backend, crate::server::ScreenSaverActive::Off, /*forced=*/false,
        );
    }
```

Insert the IDLETIME wake handler RIGHT AFTER the `state.per_device_last_activity.insert(...)` line and BEFORE the DPMS wake check (this places it before SS-off too, per the spec). Capture priors before mutating `last_activity` — wait, that's already done in step 9 of Task 1. Update step 9's edit pattern accordingly. **The correct prologue shape is:**

```rust
    let now = std::time::Instant::now();
    // Capture priors BEFORE mutating; needed by the IDLETIME wake handler.
    let prior_global = now.duration_since(state.dpms.last_activity)
        .as_millis().min(u128::from(u32::MAX)) as i64;
    // Per-device prior: fall back to global if no per-device entry yet.
    // Matches `idletime_baseline`'s fallback (server.rs Task 1) — without
    // this, the very first input event for a device whose baseline isn't
    // recorded would compute prior_device=0 and a per-device Negative
    // alarm (whose wait_value > 0) would not see the `old > wait` half of
    // its trigger.
    let prior_device = state.per_device_last_activity
        .get(&(XI2_MASTER_KEYBOARD_DEVICE_ID as u8))
        .copied()
        .map(|t| now.duration_since(t).as_millis().min(u128::from(u32::MAX)) as i64)
        .unwrap_or(prior_global);

    state.dpms.last_activity = now;
    state.per_device_last_activity.insert(XI2_MASTER_KEYBOARD_DEVICE_ID as u8, now);

    // IDLETIME wake: fires Negative-* alarms before the input event itself
    // reaches clients (predictable ordering).
    crate::core_loop::process_request::evaluate_idletime_negative_alarms_on_input_wake(
        state,
        XI2_MASTER_KEYBOARD_DEVICE_ID as u8,
        prior_global,
        prior_device,
    );

    if state.dpms.enabled && state.dpms.power_level != 0 {
        crate::core_loop::process_request::apply_dpms_transition(state, backend, 0);
    }
    if matches!(state.screensaver.active, crate::server::ScreenSaverActive::On) {
        crate::core_loop::process_request::apply_screen_saver_transition(
            state, backend, crate::server::ScreenSaverActive::Off, /*forced=*/false,
        );
    }
```

(This supersedes the simpler edit in Task 1 Step 9. When implementing Task 1 you can either land the simpler shape and grow it here, OR land the full shape upfront and skip this widening in Task 5. The plan presents them as separate so each commit's diff is self-contained.)

- [ ] **Step 5: Wire the helper into `pointer_fanout.rs`**

Apply the same shape, with `XI2_MASTER_POINTER_DEVICE_ID` (value 2) instead of `XI2_MASTER_KEYBOARD_DEVICE_ID`.

- [ ] **Step 6: Suspend-release reset for IDLETIME state**

When `XScreenSaverSuspend` drains to empty, existing MIT-SS code (in TWO places — `process_request.rs:5696` SUSPEND handler and `process_disconnect.rs:263`) bumps `state.dpms.last_activity = now`. That's enough for DPMS and the global IDLETIME *value* to behave correctly, BUT:

- `idletime_last_evaluated` may hold stale-high values from before suspend. Next post-poll evaluator pass sees `(old=stale, new=fresh)` and the `old < wait <= new` half of a `PositiveTransition` test never holds — the crossing is missed forever.
- `per_device_last_activity` entries are also stale. A fresh device baseline matters for per-device alarms post-resume.

Add a small helper in `process_request.rs` (alongside `evaluate_idletime_negative_alarms_on_input_wake`):

```rust
/// Reset IDLETIME bookkeeping when the suspend-counts table drains to
/// empty. Must be called at the SAME spot the existing MIT-SCREEN-SAVER
/// code resets `state.dpms.last_activity` (process_request.rs:5696
/// SUSPEND handler + process_disconnect.rs:263 last-suspender cleanup).
///
/// Without this, IDLETIME alarms can be skipped post-suspend because
/// `idletime_last_evaluated` holds stale-high values from before the
/// suspend, causing `evaluate_alarms_for_counter`'s
/// `old < wait <= new` Transition check to never hold.
pub(crate) fn reset_idletime_state_after_suspend_release(state: &mut ServerState) {
    let now = std::time::Instant::now();
    // dpms.last_activity is already reset by the caller (preserve
    // existing behaviour). Reset the per-device baselines so post-resume
    // per-device IDLETIME computation is consistent.
    for entry in state.per_device_last_activity.values_mut() {
        *entry = now;
    }
    // Clear the evaluator's last-seen cache so the next post-poll pass
    // computes `(old=0, new=current)` from a clean slate, preserving
    // the Transition `old < wait <= new` invariant.
    state.idletime_last_evaluated.clear();
}
```

Wire into the SS SUSPEND handler. In `process_request.rs` at `:5690-5697` (the existing `drained && suspend_counts.is_empty() && Off && On` guard), the current code reads:

```rust
                if drained
                    && state.screensaver.suspend_counts.is_empty()
                    && matches!(state.screensaver.active, ScreenSaverActive::Off)
                    && state.dpms.power_level == 0
                {
                    // Mirrors ScreenSaverFreeSuspend (saver.c:343-378):
                    // restart the idle clock from now. No notify fires.
                    state.dpms.last_activity = std::time::Instant::now();
                }
```

Change to:

```rust
                if drained
                    && state.screensaver.suspend_counts.is_empty()
                    && matches!(state.screensaver.active, ScreenSaverActive::Off)
                    && state.dpms.power_level == 0
                {
                    // Mirrors ScreenSaverFreeSuspend (saver.c:343-378):
                    // restart the idle clock from now. No notify fires.
                    state.dpms.last_activity = std::time::Instant::now();
                    reset_idletime_state_after_suspend_release(state);
                }
```

Same edit in `process_disconnect.rs:263`:

```rust
    if was_suspending
        && state.screensaver.suspend_counts.is_empty()
        && matches!(state.screensaver.active, crate::server::ScreenSaverActive::Off)
        && state.dpms.power_level == 0
    {
        // Mirrors ScreenSaverFreeSuspend (saver.c:343-378): on the last
        // suspender going away, restart the idle clock so the saver
        // doesn't immediately fire from a stale baseline.
        state.dpms.last_activity = std::time::Instant::now();
        crate::core_loop::process_request::reset_idletime_state_after_suspend_release(state);
    }
```

Add a regression test in `process_request.rs::tests`:

```rust
    #[test]
    fn suspend_release_resets_idletime_last_evaluated_and_per_device_baselines() {
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();

        // Pre-seed: stale idletime cache from before suspend.
        state.idletime_last_evaluated.insert(x11sync::IDLETIME_COUNTER, 999_999);
        state.per_device_last_activity.insert(
            3,
            std::time::Instant::now() - Duration::from_secs(120),
        );

        // Insert a suspending client, then drain via Suspend(false).
        state.screensaver.suspend_counts.insert(ClientId(1), 1);
        let header = RequestHeader { opcode: 150, data: x11screensaver::SUSPEND, length_units: 2 };
        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(1), header, &[0u8, 0, 0, 0]);

        // The drain hit the (drained && empty && SS=Off && DPMS=On) guard
        // and must have reset both IDLETIME bookkeeping fields.
        assert!(
            state.idletime_last_evaluated.is_empty(),
            "idletime_last_evaluated must be cleared on suspend release"
        );
        let vck_baseline = state.per_device_last_activity.get(&3).copied()
            .expect("per_device_last_activity[3] still present");
        let elapsed = std::time::Instant::now().duration_since(vck_baseline);
        assert!(
            elapsed < Duration::from_millis(100),
            "per_device_last_activity[3] must be reset to ~now; got elapsed {elapsed:?}"
        );
    }
```

- [ ] **Step 7: Run tests + workspace test**

```bash
cargo test -p yserver-core key_event_fires_neg_transition
cargo test -p yserver-core pointer_event_fires_neg_transition
cargo test -p yserver-core suspend_release_resets_idletime
cargo test -p yserver-core
```

Expected: all new tests pass; full crate green.

- [ ] **Step 8: Smoke-test on `just startx` (user-driven)**

Per [[feedback_hw_recipes_user_only]] the user drives this. Restart yserver to pick up the fix, then:

```bash
gsettings set org.mate.power-manager sleep-display-ac 60
gsettings set org.mate.session idle-delay 1
# (then restart mate-power-manager + mate-screensaver, or relog into MATE)
# leave keyboard/mouse alone for >60s
# expect: panel blanks at 1min via mate-power-manager DPMSForceLevel
#         mate-screensaver lockscreen at 1min
# move mouse: panel restores AND lockscreen unlocks
```

If the smoke doesn't trip, capture xtrace and look for `AlarmNotify` events at type 84 — if absent, the fanout-prologue path didn't fire.

- [ ] **Step 9: Format, lint, commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver-core
git add crates/yserver-core/src/core_loop/process_request.rs crates/yserver-core/src/core_loop/key_fanout.rs crates/yserver-core/src/core_loop/pointer_fanout.rs crates/yserver-core/src/core_loop/process_disconnect.rs
git commit -m "feat(sync): IDLETIME NegativeTransition firing on input wake

Key + pointer fanouts now capture the prior idle BEFORE mutating
last_activity, then call evaluate_idletime_negative_alarms_on_input_wake
to fire any Negative-* alarms on the global IDLETIME and on the
affected per-device counter. AlarmNotify arrives on the client wire
BEFORE the corresponding input event (predictable ordering, same
shape as the SS-off-on-input sibling check we already ship).

After this lands, MATE's idle-detection (mate-power-manager's
DPMSForceLevel + mate-screensaver's lockscreen) works on yserver
for the first time, closing the visible-smoke gap that the DPMS
and MIT-SCREEN-SAVER work alone could not.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Final verification

After Task 5 lands:

- [ ] **Step 1: Workspace-wide build, format, lint, test**

```bash
cargo build --workspace
cargo +nightly fmt --check
cargo clippy --workspace
cargo test --workspace
```

Expected: clean. Test count should grow by ~22 across the modules.

- [ ] **Step 2: Smoke matrix on `just startx` (user-driven)**

Per [[feedback_hw_recipes_user_only]] and [[feedback_tests_are_not_visible_evidence]]:

| Scenario | Expected |
|---|---|
| `xset s 60` + idle 60s with `xev -event screensaver` | `ScreenSaverNotify(state=On)` fires AND any IDLETIME alarm at 60_000 fires AlarmNotify. |
| Default MATE config (1-min idle in mate-screensaver-preferences, 1-min display sleep in mate-power-preferences) | Lockscreen activates at 1 min; panel blanks (DPMSForceLevel) at 1 min via mate-power-manager. **This is the user-visible bug the whole DPMS + MIT-SS + IDLETIME-fix arc exists for.** |
| `mpv --loop video.mp4` fullscreen + 1-min MATE display sleep | Panel does NOT blank during playback (XScreenSaverSuspend gates both DPMS firing AND IDLETIME alarm firing via the unified-timer rule). |
| xtrace recapture | After the fix, `mate.xtrace` should show `Event Sync AlarmNotify` (event type 84) at the configured idle thresholds. Compare against the pre-fix `mate.xtrace` which had zero AlarmNotify. |

- [ ] **Step 3: Update `docs/status.md`**

Add a short bullet to the existing DPMS + MIT-SCREEN-SAVER section noting that the IDLETIME fix closes the visible-smoke gap.

- [ ] **Step 4: Squash and ship**

Per `AGENTS.md`, squash to one PR at merge (ask user for confirmation per [[feedback_confirm_each_master_push]]).

---

## Risk index

- **The fanout prologue widening in Task 1 vs Task 5.** Task 1 lands a simpler prologue (`state.dpms.last_activity = now; per_device_last_activity.insert(...)`); Task 5 widens it to capture priors BEFORE the mutation. If Task 1 is implemented exactly as shown in Task 1 Step 9, Task 5 Step 4 has to refactor the same lines. Cleaner alternative: land Task 5's full prologue shape in Task 1 directly and skip Task 5 Step 4's widening. Either is fine; pick one when implementing and document the choice in the commit message.
- **`CA_*` mask bit names in the Task 4 create-time-fire test.** The test relies on constants like `CA_COUNTER`, `CA_VALUE_TYPE`, etc. yserver's `parse_alarm_attributes` already decodes these — read its body in `crates/yserver-protocol/src/x11/sync.rs` to confirm the const names match before writing the test. If the constants are private to the parser, expose them as `pub`.
- **Pre-existing tests for `evaluate_alarms_for_counter` may assert the buggy `delta=0 + Transition → Inactive` behaviour.** Task 2 Step 4 explicitly says to fix any such test; if the existing test was loadbearing for some other code path, investigate before silently changing it. Check `rg "ALARM_STATE_INACTIVE.*test_type|test_type.*ALARM_STATE_INACTIVE" crates/yserver-core/src/` for prior assertions.
- **Per-device counter advertising in `LIST_SYSTEM_COUNTERS`.** Some clients enumerate the system-counter list once at startup and only use counters they recognise by name. The names match Xorg's `DEVICEIDLETIME %d` format (space, decimal device id). MATE's xtrace only ever uses global IDLETIME (counter 0x107), so per-device-name compatibility is low-risk for the immediate smoke target; verify with a fresh xtrace after the fix.
- **`idletime_alarm_deadline` returns None when suspended, but the post-poll evaluator runs unconditionally.** This is correct (the evaluator should be cheap when there are no eligible alarms, and `evaluate_alarms_for_counter` is no-op when none match the counter), but if profiling shows the unconditional walk is hot under suspended-with-many-alarms workloads, add a top-of-function suspend short-circuit.
