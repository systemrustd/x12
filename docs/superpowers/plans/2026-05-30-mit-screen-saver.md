# MIT-SCREEN-SAVER Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the X11 MIT-SCREEN-SAVER extension end-to-end on yserver — six requests, one sequential event, per-client idle tracking, replacement of the stubbed core opcodes 107/108/115, `XScreenSaverSuspend` refcounting for media players, and Xorg-faithful DPMS↔SS coupling — so `mate-screensaver`, `xscreensaver`, and `mpv`/`vlc` work the way they do on Xorg.

**Architecture:** A new `ScreenSaverState` on `ServerState` carries protocol fields (timeouts, prefer_blanking, allow_exposures, activation state, per-client subscriber masks, per-client suspend refcounts, next cycle deadline). The core loop's poll-deadline computation grows two SS branches (idle activation, cycle re-fire) that piggyback on the existing `last_activity` clock owned by `DpmsState`. Two new helpers — `apply_screen_saver_transition` and `emit_screen_saver_notify` — mirror the DPMS pair shipped on this same branch. The existing `apply_dpms_transition` is restructured so its in-memory level write fires the SS↔DPMS coupling block before the backend hook and DPMS notify, matching `Xext/dpms.c:262-293` ordering. The existing `dpms_transition_deadline()` grows a single new gate clause on `screensaver.suspend_counts.is_empty()` — Xorg's unified-timer rule (`os/WaitFor.c:519`) means `XScreenSaverSuspend(True)` inhibits both the SS timer AND DPMS firing.

**Tech Stack:** Rust 2024, the existing yserver-protocol wire encoders (`fixed_reply`, `write_u16`, `write_u32` in `wire.rs`; single bytes are pushed directly with `Vec::push`), the `Backend` trait (no new hooks — SS is purely server-side bookkeeping), the `RecordingBackend` test double already extended for DPMS, `fanout_event_to_clients` in `core_loop/fanout.rs`.

**Spec reference:** [docs/superpowers/specs/2026-05-30-mit-screen-saver-design.md](../specs/2026-05-30-mit-screen-saver-design.md). De-facto behavioural reference: `/home/jos/Projects/xserver/Xext/saver.c`, `/home/jos/Projects/xserver/os/WaitFor.c`, `/home/jos/Projects/xserver/dix/window.c`, and `/usr/include/X11/extensions/saver.h` + `saverproto.h`.

---

## File map

| Path | Action | Responsibility |
|------|--------|----------------|
| `crates/yserver-protocol/src/x11/screensaver.rs` | **create** | Wire codecs (parse_*, encode_*), opcode + mask + state constants, event encoder. |
| `crates/yserver-protocol/src/x11/mod.rs` | modify | Add `pub mod screensaver;` next to `pub mod dpms;`. |
| `crates/yserver-core/src/server.rs` | modify | `ScreenSaverActive` enum + `ScreenSaverState` struct, embed in `ServerState`, init in `with_geometry`, add `screensaver_idle_deadline()` and `screensaver_cycle_deadline()` helpers + tests, **augment existing `dpms_transition_deadline()` with the suspend gate**. |
| `crates/yserver-core/src/nested.rs` | modify | Add `MIT_SCREEN_SAVER_MAJOR_OPCODE` + `MIT_SCREEN_SAVER_FIRST_EVENT` consts and an `ExtensionMetadata` entry in `EXTENSIONS`. |
| `crates/yserver-core/src/core_loop/process_request.rs` | modify | `apply_screen_saver_transition` + `emit_screen_saver_notify` helpers; restructure `apply_dpms_transition` to fire SS coupling before backend hook + DPMS notify; replace 107 `SetScreenSaver`, 108 `GetScreenSaver`, 115 `ForceScreenSaver`; add 150 dispatcher arm; `handle_screen_saver_request` with the six minor-opcode arms; tests. |
| `crates/yserver-core/src/core_loop/key_fanout.rs` | modify | After the existing DPMS prologue, add the SS-only sibling check (DPMS On + SS On → flip SS Off). Tests. |
| `crates/yserver-core/src/core_loop/pointer_fanout.rs` | modify | Same SS-only sibling check. Tests. |
| `crates/yserver-core/src/core_loop/run.rs` | modify | Chain `state.screensaver_idle_deadline()` and `state.screensaver_cycle_deadline()` into the `.min()` at `:396`. Add post-poll evaluator blocks (activation + cycle re-fire) after the DPMS cascade at `:664`. |
| `crates/yserver-core/src/core_loop/process_disconnect.rs` | modify | Drop client from `screensaver.selected_by` and `screensaver.suspend_counts` alongside the existing `dpms.selected_by` cleanup at `:245`; conditional `last_activity` restart if this client was the last suspender. |

No backend trait changes. No KMS backend changes. No `crates/yserver/src/lib.rs` changes.

---

## Pre-existing patterns this plan matches (not regressions)

These are gaps in yserver's broader extension plumbing — they apply to DPMS already on `feat/dpms` and will apply to SS the same way. **Do not** "fix" them inside this plan; track them as follow-ups so DPMS and SS can be uplifted together.

1. **Little-endian-hardcoded request parsers.** `crates/yserver-protocol/src/x11/dpms.rs:38-43` defines `read_u16_le`/`read_u32_le` and parses request bodies LE-only. `crates/yserver-protocol/src/x11/request_swap.rs:24-34` has no entry for opcode 134 (DPMS) or 150 (SS) — extension request bodies arrive in client byte order and are decoded as host (LE) bytes. Big-endian clients on either extension will decode wrong. This plan's `screensaver.rs` parsers follow the DPMS shape verbatim. A unified follow-up should add an extension-aware swap path (or rewrite both parsers to take a `ClientByteOrder`).
2. **Minimum-length parsing, not exact-match.** Xorg uses `REQUEST_SIZE_MATCH` (`Xext/saver.c:610, :633, :701, :1167, :1198`) and rejects bodies that are too long. yserver's DPMS handler only enforces the minimum length each parser requires. This plan matches that — `parse_query_info_request` accepts any body ≥ 4 bytes, `parse_suspend_request` accepts any non-empty body, etc. A unified follow-up should add `REQUEST_SIZE_MATCH`-equivalent exact-length validation at the dispatcher prologue for both extensions.
3. **Single-screen `drawable` arguments are accepted but not validated.** Xorg looks up the drawable via `dixLookupDrawable` to pick the per-screen saver state and to return `BadDrawable` on bogus ids (`Xext/saver.c:633-637, :701-705, :1063-1065`). yserver is single-screen so the screen lookup is moot; the plan's SS handlers parse the drawable for length validation and ignore the value. This is SS-specific (DPMS has no drawable-bearing requests), but the rationale is the same shape as items 1 and 2 — match the existing yserver posture of "accept the field, defer validation to the multi-screen rework". If multi-screen lands later, SS needs real per-screen state and `BadDrawable` rejection at the same time.

---

## Task 1: Protocol wire codecs

**Files:**
- Create: `crates/yserver-protocol/src/x11/screensaver.rs`
- Modify: `crates/yserver-protocol/src/x11/mod.rs`

Self-contained — no callers yet. Wire layout per `/usr/include/X11/extensions/saverproto.h`. Replies are 32 bytes total (8-byte fixed reply header + payload + pad). `ScreenSaverNotify` is a 32-byte **sequential** event (NOT XGE — Xorg `Xext/saver.c:381`).

- [ ] **Step 1: Write the failing tests**

Create `crates/yserver-protocol/src/x11/screensaver.rs` with constants, types, and a `#[cfg(test)]` module that references the not-yet-implemented parsers and encoders:

```rust
//! MIT-SCREEN-SAVER extension wire codecs.
//!
//! Spec: `/usr/include/X11/extensions/saver.h` + `saverproto.h`.
//! Behaviour reference: `/home/jos/Projects/xserver/Xext/saver.c`.

use super::{
    ClientByteOrder, SequenceNumber,
    wire::{fixed_reply, write_u16, write_u32},
};

// Minor opcodes.
pub const QUERY_VERSION: u8 = 0;
pub const QUERY_INFO: u8 = 1;
pub const SELECT_INPUT: u8 = 2;
pub const SET_ATTRIBUTES: u8 = 3;
pub const UNSET_ATTRIBUTES: u8 = 4;
pub const SUSPEND: u8 = 5;

// Protocol version reported by QueryVersion.
pub const SERVER_MAJOR_VERSION: u16 = 1;
pub const SERVER_MINOR_VERSION: u16 = 1;

// SelectInput event-mask bits.
pub const SCREEN_SAVER_NOTIFY_MASK: u32 = 0x0000_0001;
pub const SCREEN_SAVER_CYCLE_MASK: u32 = 0x0000_0002;

// Notify state values.
pub const SCREEN_SAVER_OFF: u8 = 0;
pub const SCREEN_SAVER_ON: u8 = 1;
pub const SCREEN_SAVER_CYCLE: u8 = 2;
pub const SCREEN_SAVER_DISABLED: u8 = 3;

// Kind values.
pub const SCREEN_SAVER_BLANKED: u8 = 0;
pub const SCREEN_SAVER_INTERNAL: u8 = 1;
pub const SCREEN_SAVER_EXTERNAL: u8 = 2;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x11::{ClientByteOrder::LittleEndian, SequenceNumber};

    #[test]
    fn parse_query_info_extracts_drawable() {
        let body = [0xab, 0xcd, 0xef, 0x12];
        assert_eq!(parse_query_info_request(&body), Some(0x12ef_cdab));
    }

    #[test]
    fn parse_select_input_extracts_drawable_and_mask() {
        // drawable:u32, event_mask:u32
        let body = [
            0x44, 0x33, 0x22, 0x11, 0x03, 0x00, 0x00, 0x00,
        ];
        assert_eq!(
            parse_select_input_request(&body),
            Some((0x1122_3344, 0x0000_0003))
        );
    }

    #[test]
    fn parse_suspend_extracts_bool() {
        assert_eq!(parse_suspend_request(&[1, 0, 0, 0]), Some(true));
        assert_eq!(parse_suspend_request(&[0, 0, 0, 0]), Some(false));
        assert_eq!(parse_suspend_request(&[]), None);
    }

    #[test]
    fn encode_query_info_reply_shape() {
        // saverproto.h: state(1) window(8) til_or_since(12) idle(16)
        // event_mask(20) kind(24) pads to 32.
        let buf = encode_query_info_reply(
            LittleEndian,
            SequenceNumber(0x5555),
            SCREEN_SAVER_ON,
            0xdead_beef,
            12345,
            67890,
            SCREEN_SAVER_NOTIFY_MASK | SCREEN_SAVER_CYCLE_MASK,
            SCREEN_SAVER_BLANKED,
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 1, "reply tag");
        assert_eq!(buf[1], SCREEN_SAVER_ON, "state at offset 1");
        assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0x5555, "sequence");
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 0, "length");
        assert_eq!(
            u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            0xdead_beef,
            "window at offset 8"
        );
        assert_eq!(
            u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            12345,
            "til_or_since at offset 12"
        );
        assert_eq!(
            u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
            67890,
            "idle at offset 16"
        );
        assert_eq!(
            u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]),
            SCREEN_SAVER_NOTIFY_MASK | SCREEN_SAVER_CYCLE_MASK,
            "event_mask at offset 20"
        );
        assert_eq!(buf[24], SCREEN_SAVER_BLANKED, "kind at offset 24");
    }

    #[test]
    fn encode_screen_saver_notify_event_shape() {
        // saverproto.h: type(0) state(1) seq(2-3) timestamp(4-7)
        // root(8-11) window(12-15) kind(16) forced(17) pads to 32.
        let mut buf = Vec::new();
        encode_screen_saver_notify_event(
            &mut buf,
            LittleEndian,
            SequenceNumber(0xabcd),
            162,
            SCREEN_SAVER_ON,
            0x1234_5678,
            0xcafe_f00d,
            0,
            SCREEN_SAVER_BLANKED,
            true,
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 162, "event type = first_event + 0");
        assert_eq!(buf[1], SCREEN_SAVER_ON, "state at offset 1");
        assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0xabcd, "sequence");
        assert_eq!(
            u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            0x1234_5678,
            "timestamp at offset 4"
        );
        assert_eq!(
            u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            0xcafe_f00d,
            "root at offset 8"
        );
        assert_eq!(
            u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            0,
            "window at offset 12 (always 0 — no SetAttributes path)"
        );
        assert_eq!(buf[16], SCREEN_SAVER_BLANKED, "kind at offset 16");
        assert_eq!(buf[17], 1, "forced at offset 17");
    }
}
```

Wire the module up so the tests are reachable. In `crates/yserver-protocol/src/x11/mod.rs`, alongside `pub mod dpms;`:

```rust
pub mod screensaver;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yserver-protocol screensaver`
Expected: compilation error — `parse_query_info_request`, `parse_select_input_request`, `parse_suspend_request`, `encode_query_info_reply`, `encode_screen_saver_notify_event` not defined.

- [ ] **Step 3: Implement the parsers and encoders**

Append below the constants in `crates/yserver-protocol/src/x11/screensaver.rs`:

```rust
fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[must_use]
pub fn parse_query_info_request(body: &[u8]) -> Option<u32> {
    // Layout: drawable:u32
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

#[must_use]
pub fn parse_select_input_request(body: &[u8]) -> Option<(u32, u32)> {
    // Layout: drawable:u32 event_mask:u32
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..])))
}

#[must_use]
pub fn parse_unset_attributes_request(body: &[u8]) -> Option<u32> {
    // Layout: drawable:u32
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

#[must_use]
pub fn parse_suspend_request(body: &[u8]) -> Option<bool> {
    // Layout: suspend:BOOL pad[3]
    if body.is_empty() {
        return None;
    }
    Some(body[0] != 0)
}

#[must_use]
pub fn encode_query_version_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    server_major: u16,
    server_minor: u16,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0, 0);
    write_u16(byte_order, &mut out, server_major);
    write_u16(byte_order, &mut out, server_minor);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// QueryInfo reply: state(1) window(8) til_or_since(12) idle(16)
/// event_mask(20) kind(24) pads to 32. `state` lands in the
/// fixed_reply's per-request byte slot (`data` field at offset 1).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn encode_query_info_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    state: u8,
    window: u32,
    til_or_since: u32,
    idle: u32,
    event_mask: u32,
    kind: u8,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, state, 0);
    write_u32(byte_order, &mut out, window);
    write_u32(byte_order, &mut out, til_or_since);
    write_u32(byte_order, &mut out, idle);
    write_u32(byte_order, &mut out, event_mask);
    out.push(kind);
    out.extend_from_slice(&[0u8; 7]); // pad to 32
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encode `ScreenSaverNotify` as a sequential event (NOT XGE).
/// 32 bytes total: type(0) state(1) seq(2-3) timestamp(4-7) root(8-11)
/// window(12-15) kind(16) forced(17) pad to 32.
#[allow(clippy::too_many_arguments)]
pub fn encode_screen_saver_notify_event(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    first_event: u8,
    state: u8,
    timestamp: u32,
    root: u32,
    window: u32,
    kind: u8,
    forced: bool,
) {
    let start = out.len();
    out.push(first_event);
    out.push(state);
    write_u16(byte_order, out, sequence.0);
    write_u32(byte_order, out, timestamp);
    write_u32(byte_order, out, root);
    write_u32(byte_order, out, window);
    out.push(kind);
    out.push(u8::from(forced));
    out.extend_from_slice(&[0u8; 14]); // pad to 32
    debug_assert_eq!(out.len() - start, 32);
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p yserver-protocol screensaver`
Expected: 5 passed.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver-protocol
git add crates/yserver-protocol/src/x11/screensaver.rs crates/yserver-protocol/src/x11/mod.rs
git commit -m "feat(screensaver): protocol wire codecs

Adds parsers for QueryInfo/SelectInput/UnsetAttributes/Suspend
and encoders for QueryVersion + QueryInfo replies and the
ScreenSaverNotify sequential event. Constants for the six
minor opcodes, the two SelectInput mask bits, the four state
values, and the three kind values."
```

---

## Task 2: `ScreenSaverState`, deadlines, and DPMS-suspend gate

**Files:**
- Modify: `crates/yserver-core/src/server.rs`

Adds the protocol state, the two SS deadline helpers, and a single new gate clause on the existing `dpms_transition_deadline()` (Xorg unified-timer rule: `XScreenSaverSuspend` inhibits both SS and DPMS firing).

- [ ] **Step 1: Write failing tests**

In `crates/yserver-core/src/server.rs`, find the existing tests module that already contains `dpms_transition_deadline_picks_smallest_non_zero_above_current` (around `:3368`). Append:

```rust
    #[test]
    fn screensaver_idle_deadline_none_when_timeout_zero() {
        let mut state = ServerState::new();
        state.screensaver.timeout_ms = 0;
        assert!(state.screensaver_idle_deadline().is_none());
    }

    #[test]
    fn screensaver_idle_deadline_none_when_suspended() {
        let mut state = ServerState::new();
        state.screensaver.timeout_ms = 60_000;
        state.screensaver.suspend_counts.insert(ClientId(7), 1);
        assert!(state.screensaver_idle_deadline().is_none());
    }

    #[test]
    fn dpms_transition_deadline_none_when_screensaver_suspended() {
        // Xorg WaitFor.c:519 — one timer drives BOTH SS and DPMS, and
        // it isn't armed when screenSaverSuspended.  XScreenSaverSuspend
        // therefore inhibits DPMS firing, which mpv/Firefox rely on.
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.standby_ms = 300_000;
        state.screensaver.suspend_counts.insert(ClientId(99), 1);
        assert!(state.dpms_transition_deadline().is_none());
    }

    #[test]
    fn screensaver_idle_deadline_none_when_active() {
        let mut state = ServerState::new();
        state.screensaver.timeout_ms = 60_000;
        state.screensaver.active = ScreenSaverActive::On;
        assert!(state.screensaver_idle_deadline().is_none());
    }

    #[test]
    fn screensaver_idle_deadline_none_when_dpms_blanked() {
        // Xorg WaitFor.c:457 — when DPMS already blanked the panel
        // the SS idle timer is suppressed (DPMS→SS coupling will
        // have already activated SS on the DPMS transition).
        let mut state = ServerState::new();
        state.screensaver.timeout_ms = 60_000;
        state.dpms.power_level = 1;
        assert!(state.screensaver_idle_deadline().is_none());
    }

    #[test]
    fn screensaver_idle_deadline_returns_last_activity_plus_timeout() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        let baseline = Instant::now();
        state.dpms.last_activity = baseline;
        state.screensaver.timeout_ms = 60_000;
        assert_eq!(
            state.screensaver_idle_deadline(),
            Some(baseline + Duration::from_millis(60_000))
        );
    }

    #[test]
    fn screensaver_cycle_deadline_none_when_off() {
        let state = ServerState::new();
        assert!(state.screensaver_cycle_deadline().is_none());
    }

    #[test]
    fn screensaver_cycle_deadline_some_when_on() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        state.screensaver.active = ScreenSaverActive::On;
        state.screensaver.interval_ms = 600_000;
        let fire_at = Instant::now() + Duration::from_millis(600_000);
        state.screensaver.next_cycle = Some(fire_at);
        assert_eq!(state.screensaver_cycle_deadline(), Some(fire_at));
    }

    #[test]
    fn screensaver_cycle_deadline_none_when_interval_zero() {
        let mut state = ServerState::new();
        state.screensaver.active = ScreenSaverActive::On;
        state.screensaver.interval_ms = 0;
        state.screensaver.next_cycle = None;
        assert!(state.screensaver_cycle_deadline().is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yserver-core screensaver_idle_deadline screensaver_cycle_deadline dpms_transition_deadline_none_when_screensaver_suspended`
Expected: compile error — `ScreenSaverActive`, `state.screensaver`, `screensaver_idle_deadline`, `screensaver_cycle_deadline` not defined.

- [ ] **Step 3: Declare `ScreenSaverActive` and `ScreenSaverState` and embed in `ServerState`**

In `crates/yserver-core/src/server.rs`, just below the `DpmsState` block (~`:222`), add:

```rust
/// Activation state of the screensaver. `Cycle` is used only as the
/// `notify_state` argument to `emit_screen_saver_notify` from the
/// periodic cycle path; it never appears in
/// `ScreenSaverState.active`, which only holds `Off` or `On`.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ScreenSaverActive {
    Off,
    On,
    Cycle,
}

/// Global MIT-SCREEN-SAVER state. Mirrors Xorg's per-server (not
/// per-screen) saver data model. The idle clock lives on `DpmsState`
/// (`last_activity`) — both extensions read the same "time of last
/// input" baseline.
#[derive(Debug, Clone)]
pub struct ScreenSaverState {
    /// `SetScreenSaver` `timeout` field, in milliseconds. 0 = idle
    /// timer disabled.
    pub timeout_ms: u32,
    /// `SetScreenSaver` `interval` field. We don't implement Internal
    /// saver tiling, but `GetScreenSaver` echoes the stored value and
    /// `interval_ms` drives the `ScreenSaverNotify(state=Cycle)` re-fire
    /// while active.
    pub interval_ms: u32,
    /// Echo-only — `GetScreenSaver` round-trip. No behavioural effect.
    pub prefer_blanking: bool,
    /// Echo-only — `GetScreenSaver` round-trip.
    pub allow_exposures: bool,

    /// Current activation. Holds only `Off` / `On`; never `Cycle`.
    pub active: ScreenSaverActive,
    /// True when the most recent transition came from
    /// `ForceScreenSaver` or from DPMS→SS coupling. Mirrors the
    /// `forced` byte on `ScreenSaverNotify` wire events.
    pub forced: bool,

    /// Per-client `SelectInput` mask. OR of `SCREEN_SAVER_NOTIFY_MASK`
    /// (0x01) and `SCREEN_SAVER_CYCLE_MASK` (0x02). Xorg's
    /// `ProcScreenSaverSelectInput` (`saver.c:695-713`) does NOT
    /// validate bits — any value is stored verbatim; only the two
    /// mask bits gate delivery. `mask == 0` removes the entry.
    /// QueryInfo's `event_mask` reply field is the CALLING client's
    /// mask (`saver.c:220-231`), not the union.
    pub selected_by:
        std::collections::HashMap<yserver_protocol::x11::ClientId, u32>,

    /// Per-client outstanding `Suspend(true)` count. Effective
    /// "suspended" = `!suspend_counts.is_empty()`. `Suspend(false)`
    /// decrements saturating to 0 (matches Xorg's silent
    /// `FreeResource` on spurious free); on hitting 0 the entry is
    /// dropped. `process_disconnect` drops the entry entirely.
    pub suspend_counts:
        std::collections::HashMap<yserver_protocol::x11::ClientId, u32>,

    /// Instant the next `ScreenSaverNotify(state=Cycle)` should fire.
    /// Set to `Some(now + interval_ms)` whenever `active` transitions
    /// to `On` (when `interval_ms > 0`); advanced each cycle fire;
    /// cleared when `active` returns to `Off`. Mirrors Xorg
    /// `WaitFor.c:473-476`.
    pub next_cycle: Option<Instant>,
}

impl ScreenSaverState {
    /// Defaults match Xorg `dix/globals.c:96-99`:
    /// `defaultScreenSaverTime` = `defaultScreenSaverInterval` = 600s,
    /// `defaultScreenSaverBlanking = PreferBlanking`,
    /// `defaultScreenSaverAllowExposures = AllowExposures`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            timeout_ms: 600_000,
            interval_ms: 600_000,
            prefer_blanking: true,
            allow_exposures: true,
            active: ScreenSaverActive::Off,
            forced: false,
            selected_by: std::collections::HashMap::new(),
            suspend_counts: std::collections::HashMap::new(),
            next_cycle: None,
        }
    }
}

impl Default for ScreenSaverState {
    fn default() -> Self {
        Self::new()
    }
}
```

Add a field to `ServerState` (anywhere in the `pub struct ServerState { ... }` declaration, alongside `pub dpms: DpmsState`):

```rust
    pub screensaver: ScreenSaverState,
```

Initialise it in `ServerState::with_geometry` (around `:498`, inside the `Self { ... }` literal, next to `dpms: DpmsState::new(false),`):

```rust
            screensaver: ScreenSaverState::new(),
```

`with_randr_outputs` delegates to `with_geometry`, so the field flows through.

- [ ] **Step 4: Add the two SS deadline helpers AND augment `dpms_transition_deadline`**

Find `dpms_transition_deadline` at `:542` and add the suspend-count gate immediately after the enabled/kms_capable guard:

```rust
    #[must_use]
    pub fn dpms_transition_deadline(&self) -> Option<std::time::Instant> {
        if !self.dpms.enabled || !self.dpms.kms_capable {
            return None;
        }
        // Xorg WaitFor.c:519 — single timer drives both SS and DPMS,
        // not armed when screenSaverSuspended. XScreenSaverSuspend
        // inhibits BOTH the SS timer and the DPMS cascade (mpv /
        // Firefox / vlc rely on this for fullscreen-video-inhibit).
        if !self.screensaver.suspend_counts.is_empty() {
            return None;
        }
        // ...existing smallest-non-zero-timeout body unchanged...
```

Then, inside the same `impl ServerState { ... }` block, append the two SS deadlines after the existing `dpms_transition_deadline` body:

```rust
    /// Instant the SS idle timer should fire next. None when:
    /// - the timer is disabled (`timeout_ms == 0`),
    /// - a client has suspended via `XScreenSaverSuspend`,
    /// - SS is already active, or
    /// - DPMS has already blanked the panel (Xorg `WaitFor.c:457` —
    ///   the DPMS→SS coupling already handled it; firing the idle
    ///   timer now would be a redundant no-op transition).
    #[must_use]
    pub fn screensaver_idle_deadline(&self) -> Option<std::time::Instant> {
        if self.screensaver.timeout_ms == 0
            || !self.screensaver.suspend_counts.is_empty()
            || matches!(self.screensaver.active, ScreenSaverActive::On)
            || self.dpms.power_level != 0
        {
            return None;
        }
        Some(
            self.dpms.last_activity
                + std::time::Duration::from_millis(u64::from(self.screensaver.timeout_ms)),
        )
    }

    /// Instant the next `ScreenSaverNotify(state=Cycle)` should fire.
    /// None when SS is Off, when a client has suspended, when DPMS
    /// has blanked, or when `next_cycle` is `None` (no cycle
    /// scheduled — `interval_ms == 0` at the activation transition).
    #[must_use]
    pub fn screensaver_cycle_deadline(&self) -> Option<std::time::Instant> {
        if !matches!(self.screensaver.active, ScreenSaverActive::On)
            || !self.screensaver.suspend_counts.is_empty()
            || self.dpms.power_level != 0
        {
            return None;
        }
        self.screensaver.next_cycle
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p yserver-core screensaver_idle_deadline screensaver_cycle_deadline dpms_transition_deadline_none_when_screensaver_suspended`
Expected: 9 passed.

- [ ] **Step 6: Format, lint, full test, commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver-core
cargo test -p yserver-core
git add crates/yserver-core/src/server.rs
git commit -m "feat(screensaver): ScreenSaverState + deadlines + DPMS-suspend gate

Embeds ScreenSaverState on ServerState (defaults match Xorg
dix/globals.c:96-99: 600s × 2, prefer_blanking on,
allow_exposures on). Adds screensaver_idle_deadline and
screensaver_cycle_deadline. Augments dpms_transition_deadline
with the unified-timer suspend gate (Xorg WaitFor.c:519) so
XScreenSaverSuspend inhibits both SS and DPMS firing —
matches Firefox/mpv/vlc fullscreen-video-inhibit semantics."
```

---

## Task 3: `apply_screen_saver_transition`, `emit_screen_saver_notify`, DPMS-coupling restructure, fanout SS-only sibling check

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs`
- Modify: `crates/yserver-core/src/core_loop/key_fanout.rs`
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs`

After this commit, DPMS→SS coupling fires on every DPMS transition and SS-Off-on-input works for the standalone-activation case. Idle activation itself depends on Task 6's run-loop wiring; no client can yet enable/configure SS until the dispatcher lands in Task 5.

- [ ] **Step 1: Write the failing fanout + coupling tests**

In `crates/yserver-core/src/core_loop/process_request.rs`'s `#[cfg(test)] mod tests` block (~`:15399`), append:

```rust
    #[test]
    fn dpms_off_drives_screensaver_on_with_forced_true() {
        // Xorg dpms.c:269-279 — DPMS Non-On + SS Off →
        // dixSaveScreens(SCREEN_SAVER_FORCER, ScreenSaverActive)
        // → SendScreenSaverNotify(... forced=true).
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        let mut peer = install_client(&mut state, 1);
        state
            .screensaver
            .selected_by
            .insert(ClientId(1), x11screensaver::SCREEN_SAVER_NOTIFY_MASK);
        let mut backend = RecordingBackend::new();

        apply_dpms_transition(&mut state, &mut backend, 3); // Off

        assert_eq!(state.screensaver.active, ScreenSaverActive::On);
        assert!(state.screensaver.forced);

        let bytes = read_all_available(&mut peer);
        // Sequential event tag is first_event + 0 = 162.
        let idx = bytes
            .iter()
            .position(|&b| b == 162)
            .expect("ScreenSaverNotify must be present");
        assert_eq!(bytes[idx + 1], x11screensaver::SCREEN_SAVER_ON, "state=On");
        assert_eq!(bytes[idx + 17], 1, "forced byte = 1");
    }

    #[test]
    fn dpms_on_drives_screensaver_off_with_forced_false_and_no_activity_reset() {
        // Xorg dpms.c:275-278 + window.c:3187-3193 — DPMS On + SS On
        // takes the SCREEN_SAVER_OFF (not FORCER) path, so forced=0.
        // NoticeTime only fires on the FORCER+Reset combination, so
        // last_activity must NOT be touched by the coupling itself.
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.power_level = 3;
        state.screensaver.active = ScreenSaverActive::On;
        let prior = state.dpms.last_activity;

        let mut peer = install_client(&mut state, 1);
        state
            .screensaver
            .selected_by
            .insert(ClientId(1), x11screensaver::SCREEN_SAVER_NOTIFY_MASK);
        let mut backend = RecordingBackend::new();

        apply_dpms_transition(&mut state, &mut backend, 0);

        assert_eq!(state.screensaver.active, ScreenSaverActive::Off);
        assert!(!state.screensaver.forced, "DPMS-On→SS-Off is non-FORCER path");
        assert_eq!(state.dpms.last_activity, prior, "coupling must NOT reset");

        let bytes = read_all_available(&mut peer);
        let idx = bytes.iter().position(|&b| b == 162).unwrap();
        assert_eq!(bytes[idx + 1], x11screensaver::SCREEN_SAVER_OFF);
        assert_eq!(bytes[idx + 17], 0, "forced byte = 0");
    }

    #[test]
    fn dpms_coupling_emits_screensaver_notify_before_dpms_notify() {
        // SS notify (sequential event tag 162) must appear at a lower
        // wire offset than the DPMS XGE notify (GenericEvent tag 35)
        // — matches Xorg ordering in DPMSSet (dpms.c:262-293).
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        let mut peer = install_client(&mut state, 1);
        state.dpms.selected_by.insert(ClientId(1));
        state
            .screensaver
            .selected_by
            .insert(ClientId(1), x11screensaver::SCREEN_SAVER_NOTIFY_MASK);
        let mut backend = RecordingBackend::new();

        apply_dpms_transition(&mut state, &mut backend, 3);

        let bytes = read_all_available(&mut peer);
        let ss_pos = bytes.iter().position(|&b| b == 162).unwrap();
        let dpms_pos = bytes.iter().position(|&b| b == 35).unwrap();
        assert!(
            ss_pos < dpms_pos,
            "SS notify (offset {ss_pos}) must precede DPMS notify (offset {dpms_pos})"
        );
    }

    #[test]
    fn force_screen_saver_activate_emits_notify_with_forced_true() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        state
            .screensaver
            .selected_by
            .insert(ClientId(1), x11screensaver::SCREEN_SAVER_NOTIFY_MASK);
        let mut backend = RecordingBackend::new();

        apply_screen_saver_transition(
            &mut state,
            &mut backend,
            ScreenSaverActive::On,
            /*forced=*/ true,
        );

        let bytes = read_all_available(&mut peer);
        let idx = bytes.iter().position(|&b| b == 162).unwrap();
        assert_eq!(bytes[idx + 1], x11screensaver::SCREEN_SAVER_ON);
        assert_eq!(bytes[idx + 17], 1, "forced=1");
    }

    #[test]
    fn apply_screen_saver_transition_does_not_touch_last_activity() {
        // last_activity update is the *handler's* responsibility
        // (FORCER+Reset path runs Xorg's NoticeTime, window.c:3187-3193).
        // The pure helper must NOT touch last_activity — otherwise the
        // input-fanout SS-only sibling check (Task 3 step 5) would
        // double-bump it, and the DPMS coupling would too.
        let mut state = ServerState::new();
        state.screensaver.active = ScreenSaverActive::On;
        let prior = state.dpms.last_activity;
        let mut backend = RecordingBackend::new();

        apply_screen_saver_transition(
            &mut state,
            &mut backend,
            ScreenSaverActive::Off,
            /*forced=*/ true,
        );

        assert_eq!(state.screensaver.active, ScreenSaverActive::Off);
        assert_eq!(
            state.dpms.last_activity, prior,
            "helper alone must not touch last_activity"
        );
    }
```

`x11screensaver` is the local alias for `yserver_protocol::x11::screensaver`. Add to the existing `use` block at the top of `mod tests` (around `:15400`):

```rust
use yserver_protocol::x11::screensaver as x11screensaver;
```

And ensure `ScreenSaverActive`, `apply_dpms_transition`, `apply_screen_saver_transition` are visible: `apply_dpms_transition` is already `pub(crate)`; the new helpers below will be too.

In `crates/yserver-core/src/core_loop/key_fanout.rs`, inside `#[cfg(test)] mod tests` (~`:520`), first ensure the imports near `:315-318` cover `ScreenSaverActive` and `ClientId`:

```rust
use crate::server::{ScreenSaverActive, ServerState};
use yserver_protocol::x11::ClientId;
```

(If those imports already exist for other tests on this branch, skip; otherwise add them once.)

Then append:

```rust
    #[test]
    fn key_event_during_screen_saver_on_flips_off_via_independent_path() {
        // Pre-state: DPMS On (so the existing DPMS-wake prologue
        // doesn't fire), SS On (activated standalone via idle timer
        // or ForceScreenSaver). Input must flip SS Off with forced=0.
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        // dpms.power_level already 0 from new()
        state.screensaver.active = ScreenSaverActive::On;
        state
            .screensaver
            .selected_by
            .insert(ClientId(1), 0x01);
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = key_event_fanout_to_state(&mut state, &mut backend, key_event(true, 33));

        assert_eq!(state.screensaver.active, ScreenSaverActive::Off);
        assert!(!state.screensaver.forced, "input-driven Off is non-forced");
    }
```

In `crates/yserver-core/src/core_loop/pointer_fanout.rs`'s test module (current imports at `:856-858` cover only `ServerState`; add `ScreenSaverActive` and `ClientId` the same way), append the analogous `pointer_event_during_screen_saver_on_flips_off_via_independent_path` test using the file's existing pointer-event fixture (the same shape `pointer_event_resets_dpms_last_activity` at `:877` uses).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yserver-core force_screen_saver dpms_off_drives_screensaver dpms_on_drives_screensaver key_event_during_screen_saver pointer_event_during_screen_saver`
Expected: compile error — `apply_screen_saver_transition`, `ScreenSaverActive` not in scope.

- [ ] **Step 3: Add `apply_screen_saver_transition` and `emit_screen_saver_notify` to `process_request.rs`**

In `crates/yserver-core/src/core_loop/process_request.rs`, immediately after the existing `emit_dpms_notify` body (~`:5187`), add:

```rust
/// Transition the screensaver to `new` (must be `Off` or `On`).
/// `Cycle` is an event-only value — it never appears in
/// `screensaver.active`. Passing it here is a programmer error; the
/// helper debug-asserts and no-ops in release builds. Idempotent on
/// same-state. On every Off↔On transition updates
/// `screensaver.active`, `screensaver.forced`, and
/// `screensaver.next_cycle`, then fires `ScreenSaverNotify` to
/// `SCREEN_SAVER_NOTIFY_MASK` subscribers. The `backend` parameter
/// is reserved for signature parity with `apply_dpms_transition`
/// and is currently unused (SS is purely server-side bookkeeping).
pub(crate) fn apply_screen_saver_transition(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    new: ScreenSaverActive,
    forced: bool,
) {
    debug_assert!(
        !matches!(new, ScreenSaverActive::Cycle),
        "Cycle is event-only and must never be written into screensaver.active — \
         route it through emit_screen_saver_notify instead"
    );
    if matches!(new, ScreenSaverActive::Cycle) {
        return; // release-build safety net
    }
    if state.screensaver.active == new {
        return;
    }
    let _ = backend; // reserved for future coupling
    state.screensaver.active = new;
    state.screensaver.forced = forced;
    state.screensaver.next_cycle = match new {
        ScreenSaverActive::On if state.screensaver.interval_ms > 0 => Some(
            std::time::Instant::now()
                + std::time::Duration::from_millis(u64::from(state.screensaver.interval_ms)),
        ),
        _ => None,
    };
    emit_screen_saver_notify(state, new, forced);
}

/// Fan a `ScreenSaverNotify` event out to subscribers. `notify_state`
/// and `forced` are passed explicitly so the cycle-timer path can
/// fire `Cycle` events without mutating `screensaver.active`. Cycle
/// events deliver to `CYCLE_MASK` subscribers; Off/On events deliver
/// to `NOTIFY_MASK` subscribers (Xorg `saver.c:389-391`).
pub(crate) fn emit_screen_saver_notify(
    state: &mut ServerState,
    notify_state: ScreenSaverActive,
    forced: bool,
) {
    use yserver_protocol::x11::screensaver as x11ss;
    const SCREEN_SAVER_FIRST_EVENT: u8 = 162;
    let (active_state, deliver_mask) = match notify_state {
        ScreenSaverActive::Off => (x11ss::SCREEN_SAVER_OFF, x11ss::SCREEN_SAVER_NOTIFY_MASK),
        ScreenSaverActive::On => (x11ss::SCREEN_SAVER_ON, x11ss::SCREEN_SAVER_NOTIFY_MASK),
        ScreenSaverActive::Cycle => (x11ss::SCREEN_SAVER_CYCLE, x11ss::SCREEN_SAVER_CYCLE_MASK),
    };
    let subs: Vec<ClientId> = state
        .screensaver
        .selected_by
        .iter()
        .filter(|(_, mask)| **mask & deliver_mask != 0)
        .map(|(c, _)| *c)
        .collect();
    if subs.is_empty() {
        return;
    }
    let ts = state.timestamp_now();
    let root = crate::resources::ROOT_WINDOW.0;
    let kind = if state.screensaver.prefer_blanking {
        x11ss::SCREEN_SAVER_BLANKED
    } else {
        x11ss::SCREEN_SAVER_INTERNAL
    };
    let dropped = crate::core_loop::fanout::fanout_event_to_clients(
        state,
        &subs,
        |buf, seq, order| {
            x11ss::encode_screen_saver_notify_event(
                buf,
                order,
                seq,
                SCREEN_SAVER_FIRST_EVENT,
                active_state,
                ts,
                root,
                0, // window — always 0 (no SetAttributes path)
                kind,
                forced,
            );
        },
    );
    for cid in dropped {
        state.screensaver.selected_by.remove(&cid);
    }
}
```

`ScreenSaverActive` must be re-exported into this module's namespace. Add to the existing `use crate::server::...` line at the top of `process_request.rs`:

```rust
use crate::server::{..., ScreenSaverActive};
```

(The existing import lives near `:30-40`; check `rg "use crate::server" crates/yserver-core/src/core_loop/process_request.rs | head` to find the exact line and just append `ScreenSaverActive`.)

- [ ] **Step 4: Restructure `apply_dpms_transition` to fire SS coupling between the level write and the backend hook**

Find `apply_dpms_transition` at `:5114`. After the `state.dpms.power_level = new_level;` line (currently followed by `if let Err(e) = backend.set_dpms_power(...)`), insert the coupling block:

```rust
pub(crate) fn apply_dpms_transition(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    new_level: u8,
) {
    let old = state.dpms.power_level;
    if new_level != old {
        log::info!(
            "dpms: apply_dpms_transition {old} → {new_level} (enabled={}, kms_capable={})",
            state.dpms.enabled,
            state.dpms.kms_capable,
        );
    }
    state.dpms.power_level = new_level;

    // SS coupling — fires BEFORE backend hook and BEFORE DPMS notify
    // (Xorg dpms.c:262-279 ordering).
    //   non-On + SS Off → SCREEN_SAVER_FORCER + Active → forced=true
    //   On     + SS On  → SCREEN_SAVER_OFF    + Reset  → forced=false
    // Neither path resets last_activity (Xorg's NoticeTime only runs
    // for the FORCER+Reset combination, window.c:3187-3193).
    let dpms_on = new_level == 0;
    match (dpms_on, state.screensaver.active) {
        (false, ScreenSaverActive::Off) => {
            apply_screen_saver_transition(state, backend, ScreenSaverActive::On, true);
        }
        (true, ScreenSaverActive::On) => {
            apply_screen_saver_transition(state, backend, ScreenSaverActive::Off, false);
        }
        _ => {}
    }

    // Backend hook (unchanged).
    if let Err(e) = backend.set_dpms_power(new_level) {
        log::error!("set_dpms_power({new_level}) failed: {e}");
    }
    if new_level != old {
        emit_dpms_notify(state);
    }
}
```

- [ ] **Step 5: Add the SS-only sibling check in `key_fanout.rs`**

In `crates/yserver-core/src/core_loop/key_fanout.rs:39-50`, the existing prologue is:

```rust
pub fn key_event_fanout_to_state(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    event: HostKeyEvent,
) -> Vec<ClientId> {
    state.dpms.last_activity = std::time::Instant::now();
    if state.dpms.enabled && state.dpms.power_level != 0 {
        crate::core_loop::process_request::apply_dpms_transition(state, backend, 0);
    }
    // ...
```

Append the SS-only sibling check immediately after the DPMS wake:

```rust
    state.dpms.last_activity = std::time::Instant::now();
    if state.dpms.enabled && state.dpms.power_level != 0 {
        crate::core_loop::process_request::apply_dpms_transition(state, backend, 0);
        // DPMS coupling tail already flipped SS Off if it was On.
    }
    if matches!(
        state.screensaver.active,
        crate::server::ScreenSaverActive::On
    ) {
        // Standalone SS activation (DPMS was On already; SS came up
        // via idle timer or ForceScreenSaver) — input wakes it.
        crate::core_loop::process_request::apply_screen_saver_transition(
            state,
            backend,
            crate::server::ScreenSaverActive::Off,
            /*forced=*/ false,
        );
    }
```

- [ ] **Step 6: Same SS-only sibling check in `pointer_fanout.rs`**

In `crates/yserver-core/src/core_loop/pointer_fanout.rs:45-55`, the existing DPMS prologue is identical. Apply the same `if matches!(state.screensaver.active, ...) { apply_screen_saver_transition(...) }` block immediately after the DPMS wake.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p yserver-core force_screen_saver dpms_off_drives_screensaver dpms_on_drives_screensaver dpms_coupling_emits_screensaver_notify_before_dpms_notify key_event_during_screen_saver pointer_event_during_screen_saver`
Expected: 7 passed.

- [ ] **Step 8: Format, lint, full test, commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver-core
cargo test --workspace
git add crates/yserver-core/src/core_loop/process_request.rs crates/yserver-core/src/core_loop/key_fanout.rs crates/yserver-core/src/core_loop/pointer_fanout.rs
git commit -m "feat(screensaver): transition helpers + DPMS coupling + fanout wake

Adds apply_screen_saver_transition + emit_screen_saver_notify
(mirrors the DPMS pair). Restructures apply_dpms_transition so
SS coupling fires between the in-memory level write and the
backend hook — matches Xorg dpms.c:262-293 ordering so a client
subscribed to both sees SS notify on the wire before DPMS notify.
Key + pointer fanouts gain an SS-only sibling check after the
DPMS wake for the standalone-activation case (DPMS On + SS On
via idle timer)."
```

---

## Task 4: Replace core opcodes 107, 108, 115

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs`

Today, `107 SetScreenSaver` and `115 ForceScreenSaver` are `log_void` at `:205-206`; `108 GetScreenSaver` returns hardcoded `(600, 0, 1, 0)` at `:10637-10660`. Replace all three with real handlers backed by `ScreenSaverState`.

- [ ] **Step 1: Write the failing test for invalid mode**

In `crates/yserver-core/src/core_loop/process_request.rs`'s `#[cfg(test)] mod tests`, append:

```rust
    #[test]
    fn force_screen_saver_invalid_mode_returns_bad_value() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        let header = RequestHeader {
            opcode: 115,
            data: 2, // mode=2 — only 0 (Reset) and 1 (Activate) are valid
            length_units: 1,
        };
        let _ = handle_force_screen_saver(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(1),
            header,
        );
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[0], 0, "error reply tag");
        assert_eq!(bytes[1], x11::error::BAD_VALUE);
        // bad_value at offset 4 = 2
        assert_eq!(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]), 2);
    }

    #[test]
    fn force_screen_saver_reset_via_handler_advances_last_activity() {
        // Reset is the FORCER+Reset path Xorg runs NoticeTime on
        // (window.c:3187-3193) — the handler must bump last_activity.
        use std::time::Duration;
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        state.screensaver.active = ScreenSaverActive::On;
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(120);
        let stale = state.dpms.last_activity;
        let mut backend = RecordingBackend::new();
        let header = RequestHeader { opcode: 115, data: 0, length_units: 1 }; // mode=0 (Reset)

        let _ = handle_force_screen_saver(&mut state, &mut backend, ClientId(1),
                                          SequenceNumber(1), header);

        assert_eq!(state.screensaver.active, ScreenSaverActive::Off);
        assert!(state.dpms.last_activity > stale,
                "handler must bump last_activity on Reset");
    }

    #[test]
    fn set_screen_saver_stores_fields() {
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        let header = RequestHeader { opcode: 107, data: 0, length_units: 3 };
        // timeout=300, interval=900, prefer_blanking=0 (no),
        // allow_exposures=1 (yes), pad:u16
        let body = [
            44, 1,    // 300 (LE)
            132, 3,   // 900 (LE)
            0,        // prefer_blanking
            1,        // allow_exposures
            0, 0,     // pad
        ];
        let _ = handle_set_screen_saver(&mut state, ClientId(1),
                                        SequenceNumber(1), header, &body);
        assert_eq!(state.screensaver.timeout_ms, 300_000);
        assert_eq!(state.screensaver.interval_ms, 900_000);
        assert!(!state.screensaver.prefer_blanking);
        assert!(state.screensaver.allow_exposures);
    }

    #[test]
    fn set_screen_saver_minus_one_restores_default() {
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        state.screensaver.timeout_ms = 12_345;
        state.screensaver.interval_ms = 67_890;
        let header = RequestHeader { opcode: 107, data: 0, length_units: 3 };
        // -1 (0xffff), -1, default(2), default(2), pad
        let body = [0xff, 0xff, 0xff, 0xff, 2, 2, 0, 0];
        let _ = handle_set_screen_saver(&mut state, ClientId(1),
                                        SequenceNumber(1), header, &body);
        // defaults: timeout=600s, interval=600s, prefer=true, allow=true
        assert_eq!(state.screensaver.timeout_ms, 600_000);
        assert_eq!(state.screensaver.interval_ms, 600_000);
        assert!(state.screensaver.prefer_blanking);
        assert!(state.screensaver.allow_exposures);
    }

    #[test]
    fn set_screen_saver_invalid_prefer_blanking_returns_bad_value() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let header = RequestHeader { opcode: 107, data: 0, length_units: 3 };
        // prefer_blanking=3 is invalid (only 0/1/2 valid).
        let body = [60, 0, 60, 0, 3, 1, 0, 0];
        let _ = handle_set_screen_saver(&mut state, ClientId(1),
                                        SequenceNumber(1), header, &body);
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[0], 0);
        assert_eq!(bytes[1], x11::error::BAD_VALUE);
        assert_eq!(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]), 3);
    }

    #[test]
    fn get_screen_saver_round_trips_set_screen_saver() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        state.screensaver.timeout_ms = 120_000;
        state.screensaver.interval_ms = 240_000;
        state.screensaver.prefer_blanking = false;
        state.screensaver.allow_exposures = true;
        let _ = handle_get_screen_saver(&mut state, ClientId(1), SequenceNumber(1));
        let bytes = read_all_available(&mut peer);
        // Reply layout: tag(0) data(1) seq(2-3) length(4-7) timeout(8-9)
        // interval(10-11) prefer(12) allow(13) pad to 32.
        assert_eq!(bytes[0], 1, "reply tag");
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 120, "timeout in s");
        assert_eq!(u16::from_le_bytes([bytes[10], bytes[11]]), 240, "interval in s");
        assert_eq!(bytes[12], 0, "prefer_blanking = false");
        assert_eq!(bytes[13], 1, "allow_exposures = true");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p yserver-core force_screen_saver_invalid_mode_returns_bad_value`
Expected: compile error — `handle_force_screen_saver` not defined.

- [ ] **Step 3: Replace the 107/108/115 handlers**

In `crates/yserver-core/src/core_loop/process_request.rs:205-206`, replace the two `log_void` arms with handler calls:

```rust
        // Before:
        107 => log_void(client_id, sequence, "SetScreenSaver"),
        115 => log_void(client_id, sequence, "ForceScreenSaver"),
        // After:
        107 => handle_set_screen_saver(state, client_id, sequence, header, body),
        115 => handle_force_screen_saver(state, backend, client_id, sequence, header),
```

Replace the body of `handle_get_screen_saver` at `:10637`:

```rust
/// GetScreenSaver (108): reflects ScreenSaverState back to the client.
fn handle_get_screen_saver(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} GetScreenSaver", client_id.0, sequence.0);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf = x11::fixed_reply(byte_order, sequence, 0, 0);
    #[allow(clippy::cast_possible_truncation)]
    let timeout = (state.screensaver.timeout_ms / 1000) as u16;
    #[allow(clippy::cast_possible_truncation)]
    let interval = (state.screensaver.interval_ms / 1000) as u16;
    x11::write_u16(byte_order, &mut buf, timeout);
    x11::write_u16(byte_order, &mut buf, interval);
    buf.push(u8::from(state.screensaver.prefer_blanking));
    buf.push(u8::from(state.screensaver.allow_exposures));
    buf.resize(32, 0);
    Ok(write_to_client(client, client_id, &buf))
}
```

Add the two new handlers near `handle_get_screen_saver`:

```rust
/// SetScreenSaver (107): body is `timeout:i16 interval:i16
/// prefer_blanking:u8 allow_exposures:u8 pad:u16`.
///
/// Sentinel resolution matches Xorg `dix/globals.c:96-99`:
///   -1 (timeout/interval)            → restore 600s default
///    2 (prefer_blanking/allow_expo)  → restore the default value
fn handle_set_screen_saver(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    // Body layout: timeout:i16 interval:i16 prefer_blanking:u8
    // allow_exposures:u8 pad:u16 = 8 bytes.
    if body.len() < 8 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            header.opcode,
        );
    }
    let timeout = i16::from_le_bytes([body[0], body[1]]);
    let interval = i16::from_le_bytes([body[2], body[3]]);
    let prefer_blanking = body[4];
    let allow_exposures = body[5];

    if !(-1..=0x7fff).contains(&i32::from(timeout)) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(timeout as u16),
            header.opcode,
        );
    }
    if !(-1..=0x7fff).contains(&i32::from(interval)) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(interval as u16),
            header.opcode,
        );
    }
    if prefer_blanking > 2 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(prefer_blanking),
            header.opcode,
        );
    }
    if allow_exposures > 2 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(allow_exposures),
            header.opcode,
        );
    }

    state.screensaver.timeout_ms = match timeout {
        -1 => 600_000,
        n => u32::from(n as u16) * 1000, // n is already ≥ 0
    };
    state.screensaver.interval_ms = match interval {
        -1 => 600_000,
        n => u32::from(n as u16) * 1000,
    };
    state.screensaver.prefer_blanking = match prefer_blanking {
        0 => false,
        1 => true,
        _ => true, // 2 = Default; Xorg defaultScreenSaverBlanking = PreferBlanking
    };
    state.screensaver.allow_exposures = match allow_exposures {
        0 => false,
        1 => true,
        _ => true, // 2 = Default; Xorg defaultScreenSaverAllowExposures = AllowExposures
    };

    // No last_activity reset — Xorg's ProcSetScreenSaver only calls
    // SetScreenSaverTimer(), which recomputes the deadline from the
    // unchanged LastEventTime. yserver computes the deadline lazily
    // at every poll iteration; the next iteration sees the new
    // timeout_ms against the unchanged last_activity.

    Ok(RequestOutcome::Handled)
}

/// ForceScreenSaver (115): `mode` lives in the request header's
/// `data` byte (Fixed(1) in the core length table — no body bytes).
/// mode=0 (Reset) → force Off + bump last_activity.
/// mode=1 (Activate) → force On.
fn handle_force_screen_saver(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
) -> io::Result<RequestOutcome> {
    let mode = header.data; // u8 stashed in the request header's data byte
    if mode > 1 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(mode),
            header.opcode,
        );
    }
    if mode == 0 {
        apply_screen_saver_transition(
            state,
            backend,
            ScreenSaverActive::Off,
            /*forced=*/ true,
        );
        state.dpms.last_activity = std::time::Instant::now();
    } else {
        apply_screen_saver_transition(
            state,
            backend,
            ScreenSaverActive::On,
            /*forced=*/ true,
        );
    }
    Ok(RequestOutcome::Handled)
}
```

(`header.data` is the same byte the DPMS dispatcher reads for the minor opcode; for core requests it's the byte stashed at offset 1 of the request header — for ForceScreenSaver this is the `mode` field. Verify by reading the existing core-request shape at `rg "header\.data" crates/yserver-core/src/core_loop/process_request.rs | head -10`.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p yserver-core force_screen_saver_invalid_mode_returns_bad_value`
Expected: 1 passed.

- [ ] **Step 5: Format, lint, full test, commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver-core
cargo test --workspace
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(screensaver): core opcodes 107/108/115

Replaces the SetScreenSaver / ForceScreenSaver log_void stubs
and the hardcoded GetScreenSaver reply with real handlers
backed by ScreenSaverState. SetScreenSaver implements the -1
and 2 sentinels per Xorg dix/globals.c:96-99. ForceScreenSaver
Reset bumps last_activity (the FORCER+Reset path that runs
Xorg's NoticeTime, window.c:3187-3193); Activate does not."
```

---

## Task 5: Extension dispatcher + Suspend semantics

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-core/src/core_loop/process_request.rs`

Adds the major-opcode 150 registration and the six-minor-opcode dispatcher. After this commit, a client can issue all six requests — including `XScreenSaverSuspend` from Firefox/mpv. The idle timer doesn't yet fire (Task 6) and disconnect cleanup is in Task 7.

- [ ] **Step 1: Register the extension**

In `crates/yserver-core/src/nested.rs`, alongside `const DPMS_MAJOR_OPCODE` at `:68`, add:

```rust
const MIT_SCREEN_SAVER_MAJOR_OPCODE: u8 = 150;
const MIT_SCREEN_SAVER_FIRST_EVENT: u8 = 162;
```

Then add an `ExtensionMetadata` entry to the `EXTENSIONS` array at `:115`. Place it next to `DPMS`:

```rust
    ExtensionMetadata {
        name: "MIT-SCREEN-SAVER",
        major_opcode: MIT_SCREEN_SAVER_MAJOR_OPCODE,
        first_event: MIT_SCREEN_SAVER_FIRST_EVENT,
        event_count: 1, // ScreenSaverNotify
        first_error: 0, // uses core BadValue / BadAccess / BadLength
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
```

- [ ] **Step 2: Write the failing dispatcher tests**

In `crates/yserver-core/src/core_loop/process_request.rs`'s `#[cfg(test)] mod tests`, append:

```rust
    #[test]
    fn suspend_per_client_refcount_stacks() {
        // Two Suspend(true) from the same client stacks the refcount.
        // It takes two Suspend(false) calls to drain.
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();

        let true_body = [1u8, 0, 0, 0];
        let false_body = [0u8, 0, 0, 0];
        let header = RequestHeader { opcode: 150, data: x11screensaver::SUSPEND, length_units: 2 };

        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(1), header, &true_body);
        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(2), header, &true_body);
        assert!(state.screensaver_idle_deadline().is_none(), "suspended");
        assert_eq!(state.screensaver.suspend_counts.get(&ClientId(1)).copied(), Some(2));

        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(3), header, &false_body);
        assert!(state.screensaver_idle_deadline().is_none(), "still suspended");

        state.screensaver.timeout_ms = 60_000; // arm the timer
        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(4), header, &false_body);
        assert!(state.screensaver_idle_deadline().is_some(), "drained");
        assert!(!state.screensaver.suspend_counts.contains_key(&ClientId(1)));
    }

    #[test]
    fn suspend_release_last_resets_last_activity_when_screensaver_off_and_dpms_on() {
        // Mirrors Xorg ScreenSaverFreeSuspend (saver.c:343-378): on
        // suspended→unsuspended with SS Off and DPMS On, restart the
        // idle clock. No notify fires (no state change).
        use std::time::Duration;
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        state.screensaver.timeout_ms = 60_000;
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(120);
        let stale = state.dpms.last_activity;
        state.screensaver.suspend_counts.insert(ClientId(1), 1);
        let mut backend = RecordingBackend::new();

        let header = RequestHeader { opcode: 150, data: x11screensaver::SUSPEND, length_units: 2 };
        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(1), header, &[0u8, 0, 0, 0]);

        assert!(state.dpms.last_activity > stale, "last_activity must advance");
    }

    #[test]
    fn force_screen_saver_activate_still_works_while_suspended() {
        // Xorg saver.c: "suspending it (by design) doesn't prevent it
        // from being forcibly activated".
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        state.screensaver.suspend_counts.insert(ClientId(2), 1);
        let mut backend = RecordingBackend::new();

        let header = RequestHeader { opcode: 115, data: 1, length_units: 1 }; // Activate
        let _ = handle_force_screen_saver(&mut state, &mut backend, ClientId(1),
                                          SequenceNumber(1), header);

        assert_eq!(state.screensaver.active, ScreenSaverActive::On);
    }

    #[test]
    fn screen_saver_query_version_returns_one_one() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        // Body: client_major:u8 client_minor:u8 pad:u16
        let header = RequestHeader { opcode: 150, data: x11screensaver::QUERY_VERSION,
                                     length_units: 2 };
        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(1), header, &[1, 1, 0, 0]);

        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[0], 1, "reply tag");
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), x11screensaver::SERVER_MAJOR_VERSION);
        assert_eq!(u16::from_le_bytes([bytes[10], bytes[11]]), x11screensaver::SERVER_MINOR_VERSION);
    }

    #[test]
    fn screen_saver_query_info_returns_disabled_when_timeout_zero() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        state.screensaver.timeout_ms = 0; // disabled
        let mut backend = RecordingBackend::new();
        let header = RequestHeader { opcode: 150, data: x11screensaver::QUERY_INFO,
                                     length_units: 2 };
        let drawable = ROOT_WINDOW.0.to_le_bytes();
        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(1), header, &drawable);

        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[1], x11screensaver::SCREEN_SAVER_DISABLED);
        // Layout invariants per saverproto.h, regardless of state:
        assert_eq!(
            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            0,
            "window field is always 0 (no SetAttributes path)"
        );
        assert_eq!(
            u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            0,
            "til_or_since = 0 in Disabled"
        );
        assert_eq!(bytes[24], x11screensaver::SCREEN_SAVER_BLANKED,
                   "kind = Blanked (prefer_blanking default true)");
    }

    #[test]
    fn screen_saver_query_info_off_carries_til_remaining_and_caller_mask() {
        // active=Off, timeout=60s, last_input recent (small idle) → til = 60s - idle (ms).
        // event_mask reflects the CALLING client's subscription, not the union.
        use std::time::Duration;
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let _ = install_client(&mut state, 2); // other subscriber; must not influence reply
        state.screensaver.timeout_ms = 60_000;
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_millis(20_000);
        state
            .screensaver
            .selected_by
            .insert(ClientId(1), x11screensaver::SCREEN_SAVER_NOTIFY_MASK);
        state
            .screensaver
            .selected_by
            .insert(ClientId(2), x11screensaver::SCREEN_SAVER_CYCLE_MASK);
        let mut backend = RecordingBackend::new();

        let header = RequestHeader { opcode: 150, data: x11screensaver::QUERY_INFO,
                                     length_units: 2 };
        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(1), header,
                                            &ROOT_WINDOW.0.to_le_bytes());

        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[1], x11screensaver::SCREEN_SAVER_OFF, "state Off");
        let til = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        assert!(til > 30_000 && til <= 40_000,
                "til_remaining ≈ 60_000 - idle(~20_000); got {til}");
        let idle = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        assert!(idle >= 19_000 && idle <= 30_000,
                "idle≈20_000ms (slack for test scheduling); got {idle}");
        let mask = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        assert_eq!(mask, x11screensaver::SCREEN_SAVER_NOTIFY_MASK,
                   "event_mask is caller's only, not union with client 2");
    }

    #[test]
    fn screen_saver_query_info_on_state_uses_til_since_underflow_when_idle_lt_timeout() {
        // active=On + idle < timeout (e.g. ForceScreenSaver before idle expired) →
        // Xorg wraps last_input - timeout as CARD32 underflow. Spec section
        // "QueryInfo field semantics" calls this out explicitly.
        use std::time::Duration;
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        state.screensaver.active = ScreenSaverActive::On;
        state.screensaver.timeout_ms = 60_000;
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_millis(5_000);
        let mut backend = RecordingBackend::new();

        let header = RequestHeader { opcode: 150, data: x11screensaver::QUERY_INFO,
                                     length_units: 2 };
        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(1), header,
                                            &ROOT_WINDOW.0.to_le_bytes());

        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[1], x11screensaver::SCREEN_SAVER_ON);
        let til = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        // 5_000 - 60_000 wraps to near u32::MAX
        assert!(til > 0xff00_0000,
                "underflow wrap expected when idle < timeout; got 0x{til:08x}");
    }

    #[test]
    fn screen_saver_query_info_kind_reflects_prefer_blanking() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        state.screensaver.prefer_blanking = false; // → kind=Internal
        let mut backend = RecordingBackend::new();
        let header = RequestHeader { opcode: 150, data: x11screensaver::QUERY_INFO,
                                     length_units: 2 };
        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(1), header,
                                            &ROOT_WINDOW.0.to_le_bytes());
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[24], x11screensaver::SCREEN_SAVER_INTERNAL);
    }

    #[test]
    fn screen_saver_select_input_accepts_unknown_mask_bits() {
        // Xorg saver.c:695-713 — any mask value is stored verbatim;
        // only NOTIFY/CYCLE bits gate delivery.
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        let header = RequestHeader { opcode: 150, data: x11screensaver::SELECT_INPUT,
                                     length_units: 3 };
        // drawable:u32 mask:u32 — mask=0x04 has no defined meaning.
        let mut body = Vec::new();
        body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&0x0000_0004u32.to_le_bytes());
        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(1), header, &body);

        let bytes = read_all_available(&mut peer);
        assert!(!bytes.iter().any(|&b| b == 0), "no error reply");
        assert_eq!(state.screensaver.selected_by.get(&ClientId(1)).copied(), Some(0x04));

        // Now force a transition Off→On — client 1 (mask=0x04, no
        // NOTIFY bit) must NOT receive a notify.
        apply_screen_saver_transition(&mut state, &mut backend,
                                      ScreenSaverActive::On, /*forced=*/false);
        let bytes2 = read_all_available(&mut peer);
        assert!(!bytes2.iter().any(|&b| b == 162),
                "client without NOTIFY_MASK bit must not receive notify");
    }

    #[test]
    fn screen_saver_set_attributes_returns_bad_access() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        let header = RequestHeader { opcode: 150, data: x11screensaver::SET_ATTRIBUTES,
                                     length_units: 4 };
        // Body content irrelevant — handler rejects unconditionally.
        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(1), header, &[0u8; 12]);

        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[0], 0, "error reply");
        assert_eq!(bytes[1], x11::error::BAD_ACCESS);
    }

    #[test]
    fn screen_saver_unset_attributes_returns_success_noop() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        let header = RequestHeader { opcode: 150, data: x11screensaver::UNSET_ATTRIBUTES,
                                     length_units: 2 };
        let drawable = ROOT_WINDOW.0.to_le_bytes();
        let _ = handle_screen_saver_request(&mut state, &mut backend, ClientId(1),
                                            SequenceNumber(1), header, &drawable);

        let bytes = read_all_available(&mut peer);
        assert!(bytes.is_empty(), "UnsetAttributes must be a silent no-op");
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p yserver-core suspend_per_client_refcount_stacks suspend_release_last_resets_last_activity force_screen_saver_activate_still_works_while_suspended screen_saver_query screen_saver_select_input screen_saver_set_attributes screen_saver_unset_attributes`
Expected: compile error — `handle_screen_saver_request` not defined.

- [ ] **Step 4: Implement `handle_screen_saver_request`**

In `crates/yserver-core/src/core_loop/process_request.rs`, near `handle_dpms_request` at `:5189`, add:

```rust
fn handle_screen_saver_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::{ClientByteOrder, screensaver as x11ss};
    const MAJOR: u8 = 150;
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    let minor_u16 = u16::from(minor);
    debug!(
        "client {} #{} ScreenSaver::minor={} body_len={}",
        client_id.0, sequence.0, minor, body.len()
    );

    match minor {
        x11ss::QUERY_VERSION => {
            let reply = x11ss::encode_query_version_reply(
                byte_order, sequence,
                x11ss::SERVER_MAJOR_VERSION,
                x11ss::SERVER_MINOR_VERSION,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11ss::QUERY_INFO => {
            // Body validation: drawable:u32 (4 bytes). We don't
            // actually use the drawable — it's a per-screen selector
            // and yserver is single-screen (see "Pre-existing
            // patterns" §3 in the plan header).
            if x11ss::parse_query_info_request(body).is_none() {
                return emit_x11_error_with_minor(state, client_id, sequence,
                                                 x11::error::BAD_LENGTH, 0, minor_u16, MAJOR);
            }
            #[allow(clippy::cast_possible_truncation)]
            let last_input = state.dpms.last_activity.elapsed().as_millis() as u32;
            let timeout = state.screensaver.timeout_ms;
            let (reply_state, til_or_since) = match state.screensaver.active {
                ScreenSaverActive::On => {
                    let ts = last_input.wrapping_sub(timeout);
                    (x11ss::SCREEN_SAVER_ON, if timeout > 0 { ts } else { 0 })
                }
                ScreenSaverActive::Off | ScreenSaverActive::Cycle => {
                    if timeout == 0 {
                        (x11ss::SCREEN_SAVER_DISABLED, 0)
                    } else if timeout < last_input {
                        (x11ss::SCREEN_SAVER_OFF, 0)
                    } else {
                        (x11ss::SCREEN_SAVER_OFF, timeout - last_input)
                    }
                }
            };
            let event_mask = state
                .screensaver
                .selected_by
                .get(&client_id)
                .copied()
                .unwrap_or(0);
            let kind = if state.screensaver.prefer_blanking {
                x11ss::SCREEN_SAVER_BLANKED
            } else {
                x11ss::SCREEN_SAVER_INTERNAL
            };
            let reply = x11ss::encode_query_info_reply(
                byte_order, sequence, reply_state, 0 /*window*/,
                til_or_since, last_input, event_mask, kind,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11ss::SELECT_INPUT => {
            let Some((_drawable, mask)) = x11ss::parse_select_input_request(body) else {
                return emit_x11_error_with_minor(state, client_id, sequence,
                                                 x11::error::BAD_LENGTH, 0, minor_u16, MAJOR);
            };
            // Xorg saver.c:695-713 stores any mask value verbatim;
            // 0 removes the entry.
            if mask == 0 {
                state.screensaver.selected_by.remove(&client_id);
            } else {
                state.screensaver.selected_by.insert(client_id, mask);
            }
        }
        x11ss::SET_ATTRIBUTES => {
            // Documented deviation from Xorg — we always reject; the
            // saver-window plumbing required to honour this is out
            // of scope. xscreensaver tolerates the failure (falls
            // back to its own override-redirect saver window).
            return emit_x11_error_with_minor(state, client_id, sequence,
                                             x11::error::BAD_ACCESS, 0, minor_u16, MAJOR);
        }
        x11ss::UNSET_ATTRIBUTES => {
            // Matches Xorg saver.c:1057-1073 — silent no-op when no
            // matching attrs exist (our universal case).
            if x11ss::parse_unset_attributes_request(body).is_none() {
                return emit_x11_error_with_minor(state, client_id, sequence,
                                                 x11::error::BAD_LENGTH, 0, minor_u16, MAJOR);
            }
        }
        x11ss::SUSPEND => {
            let Some(suspend) = x11ss::parse_suspend_request(body) else {
                return emit_x11_error_with_minor(state, client_id, sequence,
                                                 x11::error::BAD_LENGTH, 0, minor_u16, MAJOR);
            };
            if suspend {
                *state
                    .screensaver
                    .suspend_counts
                    .entry(client_id)
                    .or_insert(0) += 1;
            } else {
                let drained = match state.screensaver.suspend_counts.get_mut(&client_id) {
                    Some(c) if *c > 1 => {
                        *c -= 1;
                        false
                    }
                    Some(_) => {
                        state.screensaver.suspend_counts.remove(&client_id);
                        true
                    }
                    // Saturating: Xorg silently no-ops a free of a
                    // non-existent resource (saver.c FreeResource).
                    None => false,
                };
                if drained
                    && state.screensaver.suspend_counts.is_empty()
                    && matches!(state.screensaver.active, ScreenSaverActive::Off)
                    && state.dpms.power_level == 0
                {
                    // Mirrors ScreenSaverFreeSuspend (saver.c:343-378):
                    // restart the idle clock from now. No notify fires.
                    state.dpms.last_activity = std::time::Instant::now();
                }
            }
        }
        _ => {
            return emit_x11_error_with_minor(state, client_id, sequence,
                                             x11::error::BAD_REQUEST, 0, minor_u16, MAJOR);
        }
    }
    Ok(RequestOutcome::Handled)
}
```

- [ ] **Step 5: Wire opcode 150 into the request dispatcher**

In the same file at `:383` (alongside the other extension dispatch arms), add:

```rust
        // ── MIT-SCREEN-SAVER extension dispatcher ──
        150 => handle_screen_saver_request(state, backend, client_id, sequence, header, body),
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p yserver-core suspend_per_client_refcount_stacks suspend_release_last_resets_last_activity force_screen_saver_activate_still_works_while_suspended screen_saver_query screen_saver_select_input screen_saver_set_attributes screen_saver_unset_attributes`
Expected: 8 passed.

- [ ] **Step 7: Format, lint, full test, commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver-core
cargo test --workspace
git add crates/yserver-core/src/nested.rs crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(screensaver): extension dispatcher + Suspend

Registers MIT-SCREEN-SAVER (major opcode 150, first_event 162)
and dispatches the six minor opcodes. Suspend uses per-client
refcounts; the suspended→unsuspended transition restarts the
idle clock per Xorg ScreenSaverFreeSuspend (saver.c:343-378).
SetAttributes returns BadAccess (documented deviation —
xscreensaver tolerates this); UnsetAttributes is a silent
no-op (matches Xorg's no-matching-attrs path)."
```

---

## Task 6: Core-loop idle cascade + cycle fire

**Files:**
- Modify: `crates/yserver-core/src/core_loop/run.rs`

After this commit, idle activation works end-to-end: with `xset s 60`, 60 s of no input fires `ScreenSaverNotify(state=On)` and (if `interval_ms > 0`) periodic `Cycle` events follow.

- [ ] **Step 1: Write the failing tests**

The post-poll evaluator runs inside `run.rs`'s outer loop which is hard to drive from a unit test (mio poll, real clocks, etc.). To make it testable, extract the evaluator body into a free function in `run.rs` named `evaluate_screen_saver_post_poll(state, backend)` that the outer loop calls. Tests then drive the same function directly with pre-armed state.

In `crates/yserver-core/src/core_loop/process_request.rs`'s `#[cfg(test)] mod tests`, append (after the existing dispatcher tests):

```rust
    #[test]
    fn cycle_event_delivered_only_to_cycle_mask_subscribers() {
        // Two clients: A subscribed with NOTIFY_MASK, B with CYCLE_MASK.
        // After SS goes Active and we fire a Cycle event, only B sees it.
        let mut state = ServerState::new();
        let mut peer_a = install_client(&mut state, 1);
        let mut peer_b = install_client(&mut state, 2);
        state
            .screensaver
            .selected_by
            .insert(ClientId(1), x11screensaver::SCREEN_SAVER_NOTIFY_MASK);
        state
            .screensaver
            .selected_by
            .insert(ClientId(2), x11screensaver::SCREEN_SAVER_CYCLE_MASK);
        let mut backend = RecordingBackend::new();

        apply_screen_saver_transition(&mut state, &mut backend,
                                      ScreenSaverActive::On, /*forced=*/false);
        // A received the activation Notify; drain so the next read is clean.
        let _ = read_all_available(&mut peer_a);
        // B was NOT a NOTIFY subscriber; it should have received nothing yet.
        assert!(read_all_available(&mut peer_b).is_empty());

        emit_screen_saver_notify(&mut state, ScreenSaverActive::Cycle, /*forced=*/false);
        assert!(read_all_available(&mut peer_a).is_empty(),
                "Cycle must NOT deliver to NOTIFY_MASK subscriber");
        let b = read_all_available(&mut peer_b);
        let idx = b.iter().position(|&x| x == 162).expect("B receives Cycle");
        assert_eq!(b[idx + 1], x11screensaver::SCREEN_SAVER_CYCLE);
    }
```

In `crates/yserver-core/src/core_loop/run.rs`'s test module (or a fresh `#[cfg(test)] mod tests` if none exists), add tests that drive `evaluate_screen_saver_post_poll` end-to-end — these are what guard the run-loop wiring. These tests assert state transitions, not wire delivery, so they don't need an installed client — `emit_screen_saver_notify` short-circuits on empty `selected_by` (see helper body in Task 3 step 3). Add imports at the top of the test module:

```rust
use crate::{
    backend::recording::RecordingBackend,
    server::{ScreenSaverActive, ServerState},
};
```

Then append:

```rust
    #[test]
    fn evaluator_fires_idle_activation_when_deadline_elapsed() {
        use std::time::Duration;
        let mut state = ServerState::new();
        state.screensaver.timeout_ms = 60_000;
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(61);
        // No client installed — emit_screen_saver_notify short-circuits
        // on empty selected_by; we're asserting state transition only.
        let mut backend = RecordingBackend::default();

        super::evaluate_screen_saver_post_poll(&mut state, &mut backend);

        assert_eq!(state.screensaver.active, ScreenSaverActive::On,
                   "elapsed idle deadline must drive SS On");
    }

    #[test]
    fn evaluator_fires_cycle_and_advances_next_cycle() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        state.screensaver.active = ScreenSaverActive::On;
        state.screensaver.interval_ms = 60_000;
        let past = Instant::now() - Duration::from_millis(1);
        state.screensaver.next_cycle = Some(past);
        let mut backend = RecordingBackend::default();

        super::evaluate_screen_saver_post_poll(&mut state, &mut backend);

        let next = state.screensaver.next_cycle.expect("re-armed by evaluator");
        assert!(next > past, "next_cycle must advance past the prior deadline");
    }

    #[test]
    fn evaluator_idle_path_skipped_while_dpms_blanked() {
        // Xorg WaitFor.c:457 — when DPMS is non-On the SS idle timer
        // is suppressed; the DPMS→SS coupling already handled it.
        use std::time::Duration;
        let mut state = ServerState::new();
        state.screensaver.timeout_ms = 60_000;
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(120);
        state.dpms.power_level = 3; // Off
        let mut backend = RecordingBackend::default();

        super::evaluate_screen_saver_post_poll(&mut state, &mut backend);

        assert_eq!(state.screensaver.active, ScreenSaverActive::Off,
                   "evaluator must not fire SS when DPMS is blanked");
    }
```

(`evaluate_screen_saver_post_poll`'s body is the same two-block code shipped in Step 4 below; the only change is that it lives in a function the outer loop calls instead of inlined into the loop body.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yserver-core cycle_event_delivered evaluator_fires evaluator_idle_path`
Expected: compile error — `evaluate_screen_saver_post_poll` does not exist; the in-process_request.rs test will pass already (helpers from Task 3).

- [ ] **Step 3: Chain the SS deadlines into the poll-deadline `.min()`**

In `crates/yserver-core/src/core_loop/run.rs:393-407`, the poll-timeout block currently chains `repeat_deadline + backend_deadline + dpms_deadline`. Add the two SS deadlines:

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

- [ ] **Step 4: Extract the evaluator into a testable function and call it from the loop**

In `crates/yserver-core/src/core_loop/run.rs`, add (alongside other free functions in the file):

```rust
/// Post-poll screen-saver evaluator. Drives idle activation and the
/// periodic Cycle event re-fire. Extracted from the outer loop body
/// so unit tests (Task 6 Step 1) can drive it directly with pre-
/// armed state.
pub(crate) fn evaluate_screen_saver_post_poll(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
) {
    // SS: idle activation. Mirrors Xorg WaitFor.c:441 timing.
    if let Some(deadline) = state.screensaver_idle_deadline() {
        if Instant::now() >= deadline {
            crate::core_loop::process_request::apply_screen_saver_transition(
                state,
                backend,
                crate::server::ScreenSaverActive::On,
                /*forced=*/ false,
            );
        }
    }
    // SS: cycle re-fire. Mirrors Xorg WaitFor.c:470-476.
    if let Some(deadline) = state.screensaver_cycle_deadline() {
        let now = Instant::now();
        if now >= deadline {
            crate::core_loop::process_request::emit_screen_saver_notify(
                state,
                crate::server::ScreenSaverActive::Cycle,
                /*forced=*/ false,
            );
            state.screensaver.next_cycle = Some(
                now + Duration::from_millis(u64::from(state.screensaver.interval_ms)),
            );
        }
    }
}
```

Then, **immediately after** the DPMS cascade evaluator at `:664-681`, call it:

```rust
        evaluate_screen_saver_post_poll(state, backend);
```

- [ ] **Step 5: Run the tests + workspace build**

```bash
cargo build --workspace
cargo test --workspace
```

Expected: clean — Task 6's four unit tests pass (one helper-level + three run.rs-level); all earlier tests still pass.

- [ ] **Step 6: Smoke-test the idle activation under `just startx` (user-driven)**

Per [[feedback_hw_recipes_user_only]], the user drives this from a `just startx` session:

```bash
xset s 60                # arm SS at 60s
xset q | grep -i 'screen' # confirm value
# leave the keyboard/mouse alone for 60s
# Run `xev -event screensaver` (or any client subscribed via
# XScreenSaverSelectInput) in another terminal; expect a
# ScreenSaverNotify(state=On) at t=60. Visible panel-blank
# requires xset dpms ALSO configured (no SS→DPMS coupling).
```

- [ ] **Step 7: Format, lint, commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver-core
git add crates/yserver-core/src/core_loop/run.rs crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(screensaver): idle cascade + cycle re-fire

The core loop's poll-deadline minimum now includes both SS
deadlines (idle activation, Cycle re-fire). Post-poll evaluators
fire ScreenSaverNotify(On) on idle expiry and re-fire
ScreenSaverNotify(Cycle) every interval_ms while active — the
last bit that makes 'mate-screensaver lockscreen comes up after
N seconds' work end-to-end."
```

---

## Task 7: Disconnect cleanup

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_disconnect.rs`

After this commit, a disconnecting client is dropped from both `selected_by` and `suspend_counts`. If the client was the last suspender (and SS is Off + DPMS is On), the idle clock restarts — matches the Xorg `ScreenSaverFreeSuspend` semantics.

- [ ] **Step 1: Write the failing test**

In `crates/yserver-core/src/core_loop/process_disconnect.rs`'s `#[cfg(test)] mod tests` (the existing `disconnect_removes_client_from_dpms_selected_by` is at `:609`), append:

```rust
    #[test]
    fn disconnect_removes_client_from_screensaver_state_and_restarts_timer_if_last_suspender() {
        use std::time::Duration;
        let mut state = ServerState::new();
        install_client(&mut state, 7);
        state
            .screensaver
            .selected_by
            .insert(ClientId(7), 0x01);
        state
            .screensaver
            .suspend_counts
            .insert(ClientId(7), 1);
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(120);
        let stale = state.dpms.last_activity;

        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(7));

        assert!(!state.screensaver.selected_by.contains_key(&ClientId(7)));
        assert!(!state.screensaver.suspend_counts.contains_key(&ClientId(7)));
        assert!(state.dpms.last_activity > stale,
                "last_activity must advance — client 7 was the last suspender");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p yserver-core disconnect_removes_client_from_screensaver_state_and_restarts_timer_if_last_suspender`
Expected: fail — `screensaver.selected_by` and `screensaver.suspend_counts` still contain `ClientId(7)` after `process_disconnect`.

- [ ] **Step 3: Add the cleanup lines**

In `crates/yserver-core/src/core_loop/process_disconnect.rs`, immediately after the existing DPMS cleanup at `:245`:

```rust
    state.dpms.selected_by.remove(&client_id);
    state.screensaver.selected_by.remove(&client_id);
    let was_suspending = state
        .screensaver
        .suspend_counts
        .remove(&client_id)
        .is_some();
    if was_suspending
        && state.screensaver.suspend_counts.is_empty()
        && matches!(
            state.screensaver.active,
            crate::server::ScreenSaverActive::Off
        )
        && state.dpms.power_level == 0
    {
        // Mirrors ScreenSaverFreeSuspend (saver.c:343-378): on the
        // last suspender going away, restart the idle clock so the
        // saver doesn't immediately fire from a stale baseline.
        state.dpms.last_activity = std::time::Instant::now();
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p yserver-core disconnect_removes_client_from_screensaver_state_and_restarts_timer_if_last_suspender`
Expected: 1 passed.

- [ ] **Step 5: Format, lint, full test, commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver-core
cargo test --workspace
git add crates/yserver-core/src/core_loop/process_disconnect.rs
git commit -m "feat(screensaver): disconnect cleanup

Drop client from screensaver.selected_by and
screensaver.suspend_counts on disconnect. If the disconnecting
client was the last suspender, restart the idle clock — matches
Xorg ScreenSaverFreeSuspend (saver.c:343-378), so a media
player crashing mid-playback doesn't leave the saver firing
against a stale 2-hour baseline."
```

---

## Final verification

After all seven tasks land:

- [ ] **Step 1: Workspace-wide build, format, lint, test**

```bash
cargo build --workspace
cargo +nightly fmt --check
cargo clippy --workspace
cargo test --workspace
```

Expected: clean. Test count should grow by ~40 (5 protocol + 9 server.rs + 27 process_request.rs + 1 process_disconnect.rs + 2 fanout + 3 run.rs — adjust for any tests that turned out to overlap with DPMS tests).

- [ ] **Step 2: Smoke matrix on `just startx` (user-driven)**

Per [[feedback_hw_recipes_user_only]] and [[feedback_tests_are_not_visible_evidence]], the user drives this from `just startx` — test-green corroborates protocol invariants only; visible-symptom coverage needs smoke.

| Command / scenario | Expected |
|---|---|
| `xset q` | `Screen Saver:` section present with timeout=600, interval=600, prefer_blanking=yes, allow_exposures=yes. |
| `xset s 60` then `xset q` | timeout=60 (sec) round-trips. |
| `xset s 60` then idle 60s with `xev -event screensaver` running | `ScreenSaverNotify state=On forced=0` fires at t=60. |
| `xset s 60; xset dpms 120 120 120` then idle 60s, then idle 120s | t=60: `ScreenSaverNotify(On forced=0)`. t=120: visible panel blank + `DPMSInfoNotify(level=Off)`. SS stays On (no second notify). |
| At any point during the previous scenario: mouse motion | Panel restores. Per the restructured `apply_dpms_transition` (Task 3) the wire ordering is `ScreenSaverNotify(Off forced=0)` THEN `DPMSInfoNotify(level=On)` — DPMS-On→SS-Off takes the non-FORCER path (Xorg `dpms.c:275-278`) so `forced=0`, and the SS notify fires BEFORE the DPMS notify (Xorg `dpms.c:262-293` ordering). |
| `mate-screensaver` running with default config | Lockscreen activates after the configured "blank after N" timeout. **This is the bug fix this whole spec exists for.** |
| `xscreensaver` running with default config | Activates normally (also issues SetAttributes during startup, gets BadAccess, falls back to its own override-redirect saver window — non-fatal). |
| `mpv --loop video.mp4` playing for >timeout seconds | No `ScreenSaverNotify(On)` arrives during playback. After mpv exits, the idle timer restarts from the exit moment (next notify fires `timeout_ms` later, not immediately). |
| `xset s 60; xset dpms 60 60 60` then `mpv --loop video.mp4` | Panel does NOT blank mid-video. `xset dpms` showing a 60s timeout is irrelevant — Suspend inhibits BOTH the SS and DPMS timers via the unified-timer gate. |
| `xset s 0` | Idle timer disabled; `screensaver_idle_deadline()` returns None; no notify fires from idle path. ForceScreenSaver still works. |
| Concurrent `xev -event screensaver` subscribed | Notifies arrive on every Off↔On transition; type byte = 162, layout matches saverproto.h. |

- [ ] **Step 3: Update `docs/status.md`**

Per `AGENTS.md` ("current status is in docs/status.md and should be kept up to date"). Add a one-line bullet to the appropriate section noting MIT-SCREEN-SAVER (major opcode 150, first_event 162) is implemented with the documented `SetAttributes → BadAccess` deviation; cross-link to this plan. Include in the squash commit.

- [ ] **Step 4: Squash and ship**

Per `AGENTS.md`, squash to one PR at merge (ask user for confirmation per [[feedback_confirm_each_master_push]]). Commit-message convention follows Tasks 1–7 (`feat(screensaver): …`).

---

## Risk index

- **The DPMS-coupling restructure in `apply_dpms_transition`** is load-bearing for wire ordering (SS notify must precede DPMS notify on the same client's socket). The test `dpms_coupling_emits_screensaver_notify_before_dpms_notify` is the regression guard. Don't weaken it.
- **The augmented `dpms_transition_deadline` suspend gate** is what makes Firefox/mpv fullscreen-video-inhibit work. If a client reports "panel blanks mid-video", first check `dpms_transition_deadline_none_when_screensaver_suspended` is still passing.
- **`ScreenSaverActive::Cycle` is event-only, never stored** in `screensaver.active`. The helper `apply_screen_saver_transition` enforces this with `debug_assert!` + a release-build early-return; `emit_screen_saver_notify` takes `notify_state` as a separate argument so Cycle bypasses the field. Code review check: nothing should ever write `Cycle` into `state.screensaver.active` — if you find yourself wanting to, you want a `Cycle` event instead, fire it via `emit_screen_saver_notify`.
- **Per-client SUSPEND refcount must saturate, not panic**: `Suspend(false)` on a non-existent entry is a silent no-op (matches Xorg's `FreeResource` on a non-existent resource). Mishandling this turns a quirky-but-spec-conformant client into a crash trigger.
- **`SetAttributes → BadAccess` is the documented deviation** from Xorg. xscreensaver issues this on startup expecting Success; the failure is non-fatal — xscreensaver falls back to its own override-redirect saver window. If a future client breaks, the answer is "see [[feedback_match_xorg_clients_dont_get_patched]] — diff Xorg's saver.c:717-830 and implement the real `kind=External` plumbing" — NOT "make our handler lie about success".
- **No SS→DPMS coupling.** The smoke matrix's `xset s 60` (alone) does NOT power off the panel — `mate-screensaver`'s lockscreen activates, but the display stays lit. This is correct Xorg behaviour (the coupling is one-way, DPMS→SS, confirmed at `Xext/dpms.c:262-279`). If a user reports "screen doesn't blank with `xset s`", the answer is "you also need `xset dpms`" — don't add a SS→DPMS hook to "fix" it.
