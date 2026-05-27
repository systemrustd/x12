# VT Switching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the user Ctrl-Alt-F<N> away from a running yserver to another VT and back without wedging the screen, losing clients, or leaving stuck keys — matching Xorg behaviour.

**Architecture:** Use **libseat** as the session/seat manager, ported faithfully from the wlroots model. When `libseat_open_seat` succeeds we run in **Libseat mode**: the `libseat::Seat` (which is `!Send`) lives inside the KMS backend on the core-loop thread, libinput moves onto the core loop (via the already-reserved `LIBINPUT_TOKEN` hook) and opens its device fds through `seat.open_device()`, and VT switching is enabled. When libseat is unavailable we stay in **Direct mode**: today's separate libinput thread + direct device opens, VT switching disabled. `kms/console.rs` TTY takeover is unchanged in both modes.

**Tech Stack:** Rust, `libseat = "0.2"` (Smithay's libseat-rs, links system `/usr/lib/libseat.so`), `drm` 0.15, `input` (libinput), `xkbcommon` 0.9, `mio`, `crossbeam-channel`, `nix`.

---

## Deviations from the spec (read first)

The spec (`docs/superpowers/specs/2026-05-27-vt-switching-design.md`) was written assuming yserver keeps its **separate input thread** and bolts on a `Stop`/`Resume` control channel, an input-quiesce barrier, and a cross-thread fd handoff. After mapping the code we confirmed against the authoritative reference (`/home/jos/Projects/wlroots/backend/libinput/backend.c`) that **wlroots has no input thread** — libinput runs on the one event loop, `open_restricted` calls `wlr_session_open_file` → `libseat_open_device` (`backend.c:18-26`), and suspend/resume are inline on the session-active signal (`backend.c:162-176`). The spec literally says "this is the model we follow," so this plan follows wlroots, not the spec's threading sketch. Concrete consequences:

1. **Libseat mode runs libinput on the core thread**, not a side thread. The reserved `LIBINPUT_TOKEN` arm in `core_loop/run.rs:467` (today a warn) becomes a real dispatch path.
2. **No cross-thread input-quiesce barrier, no `Stop`/`Resume`/`Ack` control channel, no cross-thread fd handoff.** Because input is processed inline on the core thread, the spec's cross-thread barrier (and Risk #2's timeout) are unnecessary. It is replaced by a cheap **deterministic single-thread drain**: `run_suspend` does a final `libinput.dispatch()` drain (Task 11 step 2) before snapshotting held keys, closing the race where mio delivers `SEAT_TOKEN` before `LIBINPUT_TOKEN` in the same poll batch.
3. **The `Seat` lives in the KMS backend**, not as a bare field on the core loop — `run_core` is generic over `dyn Backend` in `yserver-core` and cannot hold a yserver-crate `!Send` type. Integration is via a new `BackendFdKind::Seat` + default-no-op `Backend` trait methods, so **ynest / host-X11 / recording backends are completely untouched** (VT switching is meaningless for a nested server).
4. **Direct mode is exactly today's behaviour** — separate libinput thread (`SendContext` + `input_thread::run`), direct `open_restricted`, VT switching disabled. We do not touch that path beyond extracting a shared hotkey detector.
5. **The DRM fd is opened once and kept stable across VT switches — NOT reopened on resume, and we never call `drmSetMaster`/`drmDropMaster` in libseat mode.** The spec's resume steps 2-3 ("reopen via `seat.open_device`, then `drmSetMaster`") do not match wlroots and would force fd re-registration with the poller. wlroots' `handle_session_active` (`backend/drm/backend.c:107-127`) on resume *only* re-scans connectors; the DRM fd is opened once (`backend/session/session.c:338`) and closed only at teardown; `drmSetMaster` appears nowhere in wlroots' DRM backend. libseat/logind drops and restores DRM master at the kernel level around the disable/enable signals. Consequences: (a) the DRM poll-source registered at startup stays valid forever — no re-registration machinery; (b) `Seat::open` must dispatch until the initial `Enable` (session active) arrives before opening devices, so master is present when we enable atomic caps; (c) resume = re-scan connectors + modeset on the *existing* device + re-arm cursor + repaint.

Everything else (state machine, held-key release, render-node/master reasoning, no explicit `drmDropMaster`, gating submits, resume modeset + RandR, risks) follows the spec.

---

## File structure

### New files (all in the `yserver` crate — it owns `drm/`, `kms/`, `input/`)

- `crates/yserver/src/seat/mod.rs` — `Seat` enum (`Libseat`/`Direct`), `LibseatInner` (the `!Send` core: `libseat::Seat` + `Vec<ManagedDevice>`), `ManagedDevice`, `DeviceKind`, `open()` with Direct fallback, `open_device`/`close_device`/`switch_session`/`dispatch`. One responsibility: own libseat + the managed-device list.
- `crates/yserver/src/seat/state.rs` — pure `SeatState` machine: `SeatState`, `SeatPending`, `SeatEventKind`, `SeatAction`, transition + completion-boundary functions. No I/O; exhaustively unit-tested.
- `crates/yserver/src/input/hotkey.rs` — `HotkeyDetector` + `Hotkey` enum, extracted from `input_thread.rs` so both Direct mode (input thread) and Libseat mode (backend, on core) share one detector. Adds `Hotkey::SwitchVt(u32)`.

### Modified files

**yserver-core (the generic seam — defaults keep non-KMS backends untouched):**
- `crates/yserver-core/src/backend/trait_def.rs` — add `BackendFdKind::Seat`; add three default-no-op `Backend` methods: `on_seat_ready`, `on_libinput_ready`, `set_input_sender`.
- `crates/yserver-core/src/core_loop/poll_tokens.rs` — add `SEAT_TOKEN = Token(7)`; extend the two token-array tests.
- `crates/yserver-core/src/core_loop/run.rs` — add `SEAT_TOKEN => backend.on_seat_ready(state)`; change the `LIBINPUT_TOKEN` arm from warn to `backend.on_libinput_ready(state)`.

**yserver (the implementation):**
- `crates/yserver/Cargo.toml` — add `libseat = "0.2"`.
- `crates/yserver/src/drm/device.rs` — add `Device::from_owned_fd(fd, path)` (wrap a seat-provided fd: acquire master + enable atomic caps; no path `open`).
- `crates/yserver/src/input/context.rs` — add `LibseatInterface` (routes `open_restricted`/`close_restricted` through the shared `Seat`) and `Context::new_libseat(seat)`. Existing `Interface` / `Context::new` / `SendContext` stay for Direct mode.
- `crates/yserver/src/input_thread.rs` — replace inline hotkey logic with the extracted `HotkeyDetector`; behaviour identical in Direct mode.
- `crates/yserver/src/kms/core.rs` — add `down_keys: HashSet<u8>` to `KmsCore`, maintained in the key-cooking path; helper to snapshot held keys + button mask.
- `crates/yserver/src/kms/v2/backend.rs` — `KmsBackendV2` owns the `Seat` (`Rc<RefCell<LibseatInner>>`), `seat_state: SeatState`, `pending: Rc<Cell<SeatPending>>`, optional core-thread `Context`, optional `CoreSender`; implement `poll_fds` (+Seat,+Libinput), `on_seat_ready`, `on_libinput_ready`, `set_input_sender`, suspend/resume sequences, synthetic releases; gate submit/modeset/pageflip on `seat_state`.
- `crates/yserver/src/kms/v2/platform.rs` — accept a seat-provided DRM fd at init; resume helpers (requery connectors, redo modeset on the existing fd, re-arm cursor) — no reopen, no `drmSetMaster` (Deviation #5).
- `crates/yserver/src/kms/backend.rs` (`platform_init`) — variant that wraps a seat-provided fd instead of `drm::Device::open`.
- `crates/yserver/src/lib.rs` — open the `Seat` first; branch: Libseat mode → build libinput on the core thread, register seat fd; Direct mode → today's input thread. `YSERVER_SIMULATE_VT_SWITCH` debug knob.
- `docs/status.md` — record the feature.

---

## Conventions for every task

- Format: `cargo +nightly fmt`
- Lint: `cargo clippy --all-targets -- -D warnings` (NOT pedantic — repo convention, `AGENTS.md:11`)
- Build: `cargo build --locked`
- Test: `cargo test --all-targets --locked`
- Commit after each task with the message shown. These are unpublished feature-branch commits; squash at PR.

---

## Task 1: Add the libseat dependency

**Files:**
- Modify: `crates/yserver/Cargo.toml`

- [ ] **Step 1: Add the dependency**

In `crates/yserver/Cargo.toml`, under `[dependencies]`, add:

```toml
libseat = "0.2"
```

- [ ] **Step 2: Verify it links against the system library**

Run: `cargo build -p yserver --locked`
Expected: builds. The crate links `/usr/lib/libseat.so` (confirmed present). If link fails with "cannot find -lseat", the system `seatd`/`libseat` package is missing — stop and report; do not vendor.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/Cargo.toml Cargo.lock
git commit -m "build(yserver): add libseat dependency for VT switching"
```

---

## Task 2: The `SeatState` state machine (pure, fully tested)

This is the heart of the suspend/resume coordination and the only part fully unit-testable without hardware. No libseat, no I/O — just the (state × event) transition table from the spec plus the two completion-boundary re-checks.

**Files:**
- Create: `crates/yserver/src/seat/state.rs`
- Modify: `crates/yserver/src/seat/mod.rs` (add `pub mod state;` — created in Task 4; for now create `mod.rs` with just the module line)
- Modify: `crates/yserver/src/lib.rs` (add `mod seat;` near the other `mod` declarations)

- [ ] **Step 1: Write the failing tests**

Create `crates/yserver/src/seat/state.rs`:

```rust
//! Pure VT-switch session state machine. No I/O — drives the
//! suspend/resume coordination from libseat enable/disable events.
//!
//! Spec: docs/superpowers/specs/2026-05-27-vt-switching-design.md §"State machine".

/// Server-wide seat session state. `Suspending`/`Resuming` are transient
/// states bracketing the (possibly long) sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatState {
    Active,
    Suspending,
    Suspended,
    Resuming,
}

/// Coalesced counter-events. We never queue more than one of each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeatPending {
    pub pending_enable: bool,
    pub pending_disable: bool,
}

/// A libseat session event surfaced by `seat.dispatch()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatEventKind {
    Enable,
    Disable,
}

/// What the caller must do after applying an event to the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatAction {
    /// Run the suspend sequence (then call [`SeatState::suspend_complete`]).
    BeginSuspend,
    /// Run the resume sequence (then call [`SeatState::resume_complete`]).
    BeginResume,
    /// Do nothing this turn.
    Nothing,
}

impl SeatState {
    /// True only when master-requiring I/O (modeset, pageflip, submit)
    /// is allowed. Gate every such operation on this.
    #[must_use]
    pub fn allows_scanout(self) -> bool {
        matches!(self, SeatState::Active)
    }

    /// Apply a libseat event. Mutates `pending`, returns the action the
    /// caller must perform. Mirrors the spec's event×state matrix.
    pub fn on_event(&mut self, pending: &mut SeatPending, ev: SeatEventKind) -> SeatAction {
        match (*self, ev) {
            (SeatState::Active, SeatEventKind::Disable) => {
                *self = SeatState::Suspending;
                SeatAction::BeginSuspend
            }
            (SeatState::Suspended, SeatEventKind::Enable) => {
                *self = SeatState::Resuming;
                SeatAction::BeginResume
            }
            // Coalesce a counter-event that arrives mid-sequence.
            (SeatState::Suspending, SeatEventKind::Enable)
            | (SeatState::Resuming, SeatEventKind::Enable) => {
                pending.pending_enable = true;
                SeatAction::Nothing
            }
            (SeatState::Resuming, SeatEventKind::Disable) => {
                pending.pending_disable = true;
                SeatAction::Nothing
            }
            // Everything else is a no-op (log warn at the call site):
            // Active+Enable, Suspended+Disable, Suspending+Disable.
            _ => SeatAction::Nothing,
        }
    }

    /// Call after the suspend sequence finishes (libseat ack done).
    /// Commits to `Suspended`. If an enable arrived meanwhile, the
    /// pending flag is left set so the next real `Enable` acts at once.
    pub fn suspend_complete(&mut self, _pending: &SeatPending) {
        debug_assert_eq!(*self, SeatState::Suspending);
        *self = SeatState::Suspended;
    }

    /// Call after the resume sequence finishes but BEFORE committing to
    /// `Active`. If a disable arrived during resume, go straight back
    /// into `Suspending` (returning `BeginSuspend`) without ever
    /// becoming `Active` — avoids a visible "blink". Otherwise commit
    /// to `Active`.
    pub fn resume_complete(&mut self, pending: &mut SeatPending) -> SeatAction {
        debug_assert_eq!(*self, SeatState::Resuming);
        if pending.pending_disable {
            pending.pending_disable = false;
            *self = SeatState::Suspending;
            SeatAction::BeginSuspend
        } else {
            *self = SeatState::Active;
            SeatAction::Nothing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> SeatPending {
        SeatPending::default()
    }

    #[test]
    fn active_disable_begins_suspend() {
        let mut s = SeatState::Active;
        let mut pend = p();
        assert_eq!(s.on_event(&mut pend, SeatEventKind::Disable), SeatAction::BeginSuspend);
        assert_eq!(s, SeatState::Suspending);
    }

    #[test]
    fn suspended_enable_begins_resume() {
        let mut s = SeatState::Suspended;
        let mut pend = p();
        assert_eq!(s.on_event(&mut pend, SeatEventKind::Enable), SeatAction::BeginResume);
        assert_eq!(s, SeatState::Resuming);
    }

    #[test]
    fn enable_during_suspend_is_coalesced() {
        let mut s = SeatState::Suspending;
        let mut pend = p();
        assert_eq!(s.on_event(&mut pend, SeatEventKind::Enable), SeatAction::Nothing);
        assert!(pend.pending_enable);
        assert_eq!(s, SeatState::Suspending);
    }

    #[test]
    fn disable_during_resume_is_coalesced() {
        let mut s = SeatState::Resuming;
        let mut pend = p();
        assert_eq!(s.on_event(&mut pend, SeatEventKind::Disable), SeatAction::Nothing);
        assert!(pend.pending_disable);
    }

    #[test]
    fn double_disable_is_ignored() {
        let mut s = SeatState::Suspending;
        let mut pend = p();
        assert_eq!(s.on_event(&mut pend, SeatEventKind::Disable), SeatAction::Nothing);
        assert_eq!(s, SeatState::Suspending);
    }

    #[test]
    fn active_enable_is_ignored() {
        let mut s = SeatState::Active;
        let mut pend = p();
        assert_eq!(s.on_event(&mut pend, SeatEventKind::Enable), SeatAction::Nothing);
        assert_eq!(s, SeatState::Active);
    }

    #[test]
    fn resume_completion_bypasses_active_when_disable_pending() {
        let mut s = SeatState::Resuming;
        let mut pend = SeatPending { pending_disable: true, ..p() };
        assert_eq!(s.resume_complete(&mut pend), SeatAction::BeginSuspend);
        assert_eq!(s, SeatState::Suspending);
        assert!(!pend.pending_disable, "pending_disable consumed");
    }

    #[test]
    fn resume_completion_commits_active_when_nothing_pending() {
        let mut s = SeatState::Resuming;
        let mut pend = p();
        assert_eq!(s.resume_complete(&mut pend), SeatAction::Nothing);
        assert_eq!(s, SeatState::Active);
    }

    #[test]
    fn only_active_allows_scanout() {
        assert!(SeatState::Active.allows_scanout());
        assert!(!SeatState::Suspending.allows_scanout());
        assert!(!SeatState::Suspended.allows_scanout());
        assert!(!SeatState::Resuming.allows_scanout());
    }
}
```

Create `crates/yserver/src/seat/mod.rs` (minimal for now — `mod.rs` body comes in Task 4):

```rust
//! libseat session management + VT switching.
pub mod state;
```

Add to `crates/yserver/src/lib.rs` alongside the other `mod` lines (e.g. near `mod input_thread;`):

```rust
mod seat;
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p yserver seat::state -- --nocapture`
Expected: all `seat::state::tests::*` PASS.

- [ ] **Step 3: Lint + format**

Run: `cargo +nightly fmt && cargo clippy -p yserver --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/seat/ crates/yserver/src/lib.rs
git commit -m "feat(seat): pure VT-switch session state machine"
```

---

## Task 3: Backend trait seam (`BackendFdKind::Seat` + default-no-op hooks)

This wires the libseat fd and the on-core libinput dispatch into the generic core loop **without touching any non-KMS backend**. All new trait methods get default no-op bodies, so `recording.rs`, `host_x11/trait_impl.rs`, and ynest compile unchanged.

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs`
- Modify: `crates/yserver-core/src/core_loop/poll_tokens.rs`
- Modify: `crates/yserver-core/src/core_loop/run.rs`

- [ ] **Step 1: Add the `Seat` fd kind + trait methods**

In `crates/yserver-core/src/backend/trait_def.rs`, extend the `BackendFdKind` enum (around line 32):

```rust
pub enum BackendFdKind {
    Drm,
    Libinput,
    HostX11,
    PresentCompletion,
    /// libseat connection fd (KMS + libseat mode only). Readiness drives
    /// `Backend::on_seat_ready` → `seat.dispatch()`.
    Seat,
}
```

> The DRM fd is stable across VT switches (see Deviation #5), so no runtime fd re-registration mechanism is needed — `poll_fds()` registered once at startup remains valid for the whole session.

In the `Backend` trait body, add three methods with default no-op bodies (place them near `on_page_flip_ready`):

```rust
    /// The libseat connection fd is readable. The KMS backend dispatches
    /// libseat (which may fire enable/disable callbacks synchronously)
    /// and runs any resulting suspend/resume sequence. Default: no-op
    /// (ynest, host-X11, recording have no seat).
    fn on_seat_ready(&mut self, _state: &mut ServerState) {}

    /// The libinput fd is readable AND libinput is owned by the core
    /// loop (libseat mode). Dispatch libinput inline. Default: no-op —
    /// in Direct mode the dedicated input thread owns the fd and this is
    /// never registered.
    fn on_libinput_ready(&mut self, _state: &mut ServerState) {}

    /// Hand the backend a core-channel sender so that, when it owns
    /// input on the core thread (libseat mode), it can emit the same
    /// control Messages the input thread would (Shutdown, DumpScanout,
    /// DumpDrawables). Default: no-op.
    fn set_input_sender(&mut self, _sender: crate::core_loop::CoreSender) {}
```

(Confirm `CoreSender` is reachable from `trait_def.rs`; if not, import it or reference by full path as shown.)

- [ ] **Step 2: Add `SEAT_TOKEN`**

In `crates/yserver-core/src/core_loop/poll_tokens.rs`, after `PRESENT_COMPLETION_TOKEN` (line 45):

```rust
/// libseat connection fd; readiness drives `Backend::on_seat_ready`.
pub const SEAT_TOKEN: Token = Token(7);
```

Add `SEAT_TOKEN` to both test arrays (`system_tokens_decode_to_none` and `fixed_tokens_are_distinct`).

- [ ] **Step 3: Wire dispatch in the core loop**

In `crates/yserver-core/src/core_loop/run.rs`:

Add `SEAT_TOKEN` to the imports from `poll_tokens` and the `BackendFdKind` match (around line 360):

```rust
        let token = match kind {
            BackendFdKind::Drm => DRM_TOKEN,
            BackendFdKind::Libinput => LIBINPUT_TOKEN,
            BackendFdKind::HostX11 => HOST_X11_TOKEN,
            BackendFdKind::PresentCompletion => PRESENT_COMPLETION_TOKEN,
            BackendFdKind::Seat => SEAT_TOKEN,
        };
```

Replace the `LIBINPUT_TOKEN` arm (lines 467-477) — it currently only warns:

```rust
                LIBINPUT_TOKEN => {
                    // Libseat mode: the backend owns libinput on the core
                    // thread and dispatches it inline. Direct mode never
                    // registers this fd (the input thread owns it).
                    backend.on_libinput_ready(state);
                }
```

Add a new arm next to `PRESENT_COMPLETION_TOKEN`:

```rust
                SEAT_TOKEN => {
                    backend.on_seat_ready(state);
                }
```

(No fd re-registration: the DRM fd is stable across VT switches — Deviation #5.)

- [ ] **Step 4: Verify build + existing tests**

Run: `cargo build --locked && cargo test -p yserver-core poll_tokens --locked`
Expected: builds; `poll_tokens::tests::*` PASS (now covering `SEAT_TOKEN`).

- [ ] **Step 5: Lint, format, commit**

```bash
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings
git add crates/yserver-core/src/backend/trait_def.rs crates/yserver-core/src/core_loop/poll_tokens.rs crates/yserver-core/src/core_loop/run.rs
git commit -m "feat(core): Backend seat seam — BackendFdKind::Seat + on_seat_ready/on_libinput_ready hooks"
```

---

## Task 4: `Seat` open with Direct fallback + `ManagedDevice`

Implements the libseat wrapper. The `libseat::Seat::open` callback is a `'static FnMut` closure that cannot borrow the backend, so it writes into a shared `Rc<Cell<SeatPending>>` (the only thing it touches). The `Seat` itself + the managed-device list live behind `Rc<RefCell<LibseatInner>>` so the libinput `LibseatInterface` (Task 7) can reach `open_device` on the same thread.

**Files:**
- Modify: `crates/yserver/src/seat/mod.rs`

- [ ] **Step 1: Write the module**

Replace `crates/yserver/src/seat/mod.rs` with:

```rust
//! libseat session management + VT switching (wlroots model).
//!
//! Libseat mode: this owns `libseat::Seat` and the list of devices we
//! opened through it. Lives entirely on the core-loop thread (libseat
//! is `!Send`). Direct mode is a marker — no libseat, VT switching off.
//!
//! Spec: docs/superpowers/specs/2026-05-27-vt-switching-design.md

pub mod state;

use std::{
    cell::RefCell,
    io,
    os::fd::{AsFd, AsRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
    rc::Rc,
};

use libseat::{Seat as LibSeat, SeatEvent};

use self::state::SeatEventKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// DRM card (primary node). Master is owned by libseat/logind, not
    /// acquired by us (Deviation #5); `is_kms` distinguishes the KMS card
    /// from a render-only node for bookkeeping.
    Drm { is_kms: bool },
    Input,
}

/// Map libseat's error (`errno::Errno`, a newtype over `i32`) to
/// `io::Error`. VERIFY at impl time: the field is `.0`; the type may be
/// re-exported as `libseat::Errno` or reachable as `errno::Errno`.
fn errno_to_io(e: libseat::Errno) -> io::Error {
    io::Error::from_raw_os_error(e.0)
}

/// A device opened through libseat. We OWN the libseat `Device` — it has
/// NO `Drop` impl, so the only way to release it is `Seat::close_device`,
/// which consumes it by value. We hand our owner (drm::Device or
/// libinput) a `dup` of the fd; `handed_fd` is that dup's number, used to
/// find this entry from libinput's `close_restricted(OwnedFd)`. The dup
/// shares the same open-file-description as libseat's fd, so DRM master
/// (which is per-description) and kernel revoke-on-suspend apply to both.
pub struct ManagedDevice {
    pub device: libseat::Device,
    pub path: PathBuf,
    pub handed_fd: RawFd,
    pub kind: DeviceKind,
}

/// The `!Send` libseat core, shared (single-thread) between the KMS
/// backend and the libinput `LibseatInterface`.
pub struct LibseatInner {
    seat: LibSeat,
    pub devices: Vec<ManagedDevice>,
}

impl LibseatInner {
    /// Open a device through libseat and return an `OwnedFd` (a dup of
    /// libseat's fd) for our owner to hold. libseat keeps its own fd
    /// alive until `close_device`; the dup is a distinct fd number over
    /// the same description (see `ManagedDevice`). Mirrors wlroots'
    /// `wlr_session_open_file` (`backend/session/session.c`).
    pub fn open_device(&mut self, path: &Path, kind: DeviceKind) -> io::Result<OwnedFd> {
        let device = self.seat.open_device(&path).map_err(errno_to_io)?;
        let owned = device.as_fd().try_clone_to_owned()?; // dup(2)
        let handed_fd = owned.as_raw_fd();
        self.devices.push(ManagedDevice {
            device,
            path: path.to_path_buf(),
            handed_fd,
            kind,
        });
        Ok(owned)
    }

    /// Close the device whose handed-out fd matches `handed_fd` (called
    /// from libinput's `close_restricted`). Consumes the libseat `Device`
    /// by value. No-op if unknown.
    pub fn close_device_by_fd(&mut self, handed_fd: RawFd) {
        if let Some(idx) = self.devices.iter().position(|d| d.handed_fd == handed_fd) {
            let md = self.devices.remove(idx);
            if let Err(e) = self.seat.close_device(md.device) {
                log::warn!("seat: close_device(handed_fd={handed_fd}) failed: {e}");
            }
        }
    }

    /// Request a VT switch. Fire-and-forget: libseat does not guarantee a
    /// switch will occur, and any state transition arrives later via the
    /// disable callback.
    pub fn switch_session(&mut self, vt: u32) -> io::Result<()> {
        self.seat.switch_session(vt as i32).map_err(errno_to_io)
    }

    /// Ack a disable: tell libseat we've quiesced. Only after this does
    /// the kernel allow the VT switch to proceed.
    pub fn disable(&mut self) -> io::Result<()> {
        self.seat.disable().map_err(errno_to_io)
    }

    pub fn fd(&mut self) -> io::Result<RawFd> {
        self.seat.get_fd().map(|b| b.as_raw_fd()).map_err(errno_to_io)
    }

    /// Non-blocking dispatch. The enable/disable callback closure runs
    /// inside this call and pushes into the shared `pending_events` queue.
    pub fn dispatch(&mut self) -> io::Result<()> {
        self.seat.dispatch(0).map(|_| ()).map_err(errno_to_io)
    }
}

/// Seat mode. `Direct` is the marker variant for the no-libseat path —
/// it carries no state because device opens go through today's direct
/// code, not this module.
pub enum Seat {
    Libseat {
        inner: Rc<RefCell<LibseatInner>>,
        /// Shared with the libseat callback closure; the closure writes
        /// Enable/Disable here, the backend drains after each dispatch.
        pending_events: Rc<RefCell<Vec<SeatEventKind>>>,
    },
    Direct,
}

impl Seat {
    /// Try to open `seat0` via libseat; fall back to `Direct` on any
    /// error (matches wlroots / the spec's single rule).
    #[must_use]
    pub fn open() -> Self {
        let pending_events: Rc<RefCell<Vec<SeatEventKind>>> = Rc::new(RefCell::new(Vec::new()));
        let cb_events = Rc::clone(&pending_events);
        // The callback is `'static FnMut` and cannot borrow the backend;
        // it only pushes event kinds into the shared queue.
        let seat = LibSeat::open(move |_seat, event| {
            let kind = match event {
                SeatEvent::Enable => SeatEventKind::Enable,
                SeatEvent::Disable => SeatEventKind::Disable,
            };
            cb_events.borrow_mut().push(kind);
        });
        match seat {
            Ok(mut seat) => {
                // Block until the initial Enable (session active) before we
                // open any device, so DRM master is present when we enable
                // atomic caps. wlroots does the same in libseat_open_seat.
                // Bounded to ~5s so a broken backend can't hang startup.
                let mut active = false;
                for _ in 0..50 {
                    if let Err(e) = seat.dispatch(100) {
                        log::warn!("yserver: libseat initial dispatch failed: {e}");
                        break;
                    }
                    if pending_events
                        .borrow()
                        .iter()
                        .any(|e| *e == SeatEventKind::Enable)
                    {
                        active = true;
                        break;
                    }
                }
                if !active {
                    log::warn!("yserver: libseat opened but no initial Enable; using Direct");
                    return Seat::Direct;
                }
                // Consume the initial Enable: the backend starts in
                // SeatState::Active, so this event must not later be
                // re-interpreted as a resume.
                pending_events.borrow_mut().clear();
                log::info!("yserver: libseat session opened + active; VT switching enabled");
                Seat::Libseat {
                    inner: Rc::new(RefCell::new(LibseatInner {
                        seat,
                        devices: Vec::new(),
                    })),
                    pending_events,
                }
            }
            Err(e) => {
                log::info!(
                    "yserver: libseat unavailable ({e}); VT switching disabled, \
                     opening devices directly"
                );
                Seat::Direct
            }
        }
    }

    #[must_use]
    pub fn is_libseat(&self) -> bool {
        matches!(self, Seat::Libseat { .. })
    }
}
```

> **libseat 0.2.4 API (verified against docs.rs, 2026-05-28).** Confirmed signatures:
> - `Seat::open<C: FnMut(&mut SeatRef, SeatEvent) + 'static>(callback: C) -> Result<Seat, Errno>`
> - `open_device<P: AsRef<Path>>(&mut self, &P) -> Result<Device, Errno>`
> - `close_device(&mut self, device: Device) -> Result<(), Errno>` — **takes `Device` by value**
> - `switch_session(&mut self, i32) -> Result<(), Errno>`, `disable(&mut self) -> Result<(), Errno>`
> - `get_fd(&mut self) -> Result<BorrowedFd<'_>, Errno>`, `dispatch(&mut self, timeout: i32) -> Result<i32, Errno>`
> - `Device` impls **`AsFd`** + `Debug`. It has **no `Drop`, no `Clone`, no `AsRawFd`, no `device_id()`** — it is an opaque owned handle you hand back to `close_device`. Get the fd via `device.as_fd()`.
>
> **Still to verify at impl time (Task 1, once the crate is fetched):** (a) the `SeatEvent` enum variants are exactly `Enable`/`Disable`; (b) the error type path (`libseat::Errno` re-export vs `errno::Errno`) and that the inner i32 is field `.0`. These are the only two unconfirmed points; the wrappers above are the single place the API surface is touched, so adapt there if either differs.

- [ ] **Step 2: Build**

Run: `cargo build -p yserver --locked`
Expected: builds. (No unit test here — `Seat::open` needs a real seat manager; covered by the stub integration test in Task 13 and the hardware matrix.)

- [ ] **Step 3: Lint, format, commit**

```bash
cargo +nightly fmt && cargo clippy -p yserver --all-targets -- -D warnings
git add crates/yserver/src/seat/mod.rs
git commit -m "feat(seat): libseat Seat wrapper with Direct fallback"
```

---

## Task 5: `Device::from_owned_fd` — wrap a seat-provided DRM fd

In libseat mode the DRM card fd comes from `seat.open_device(path)`, not from opening the path ourselves. `Device::open` (`drm/device.rs:9-59`) both opens the path and acquires master. Add a sibling that takes an already-open fd and enables atomic caps **but does not acquire master** — libseat/logind owns master in libseat mode (Deviation #5).

**Files:**
- Modify: `crates/yserver/src/drm/device.rs`

- [ ] **Step 1: Read the existing `Device`**

Confirm the struct (`file: File`, `path: String`) and that `enable_atomic_capabilities()` is reachable on `&Device` (it is, via the `drm` crate's `DrmDevice` impl, used at `device.rs:57`). `from_owned_fd` does NOT use `acquire_master_lock` (that stays in the Direct-mode `Device::open` only).

- [ ] **Step 2: Add the constructor**

In `crates/yserver/src/drm/device.rs`, add:

```rust
impl Device {
    /// Wrap a DRM primary-node fd that libseat already opened for us.
    /// Unlike [`Device::open`] this does NOT open the path (libseat owns
    /// it) and does NOT call `drmSetMaster`: in libseat mode the seat
    /// manager (logind/seatd) owns DRM master and grants it to the active
    /// session. `Seat::open` blocks until the session is active before any
    /// device is opened, so we hold master here and enabling atomic caps
    /// (which requires master) succeeds. Mirrors wlroots, which never
    /// calls `drmSetMaster`.
    pub fn from_owned_fd(fd: std::os::fd::OwnedFd, path: &str) -> io::Result<Self> {
        let file = std::fs::File::from(fd);
        let device = Self {
            file,
            path: path.to_string(),
        };
        device.enable_atomic_capabilities()?;
        Ok(device)
    }
}
```

> **Note on the fd type (resolved).** `LibseatInner::open_device` (Task 4, corrected) returns an `OwnedFd` that is a `dup(2)` of libseat's fd, so Task 8's DRM path passes it straight into `Device::from_owned_fd`. No double-close: libseat's `Device` has no `Drop` and is released only via `seat.close_device`; our dup is an independent fd number that `drm::Device`'s `File` closes on drop. The dup shares libseat's open-file-description, so the kernel's suspend-time master revoke (done by logind/seatd) applies to it. The DRM device is opened **once** and kept for the session's life (Deviation #5) — it is not reopened or closed on VT switch; libseat's `Device` and our dup are released only at shutdown (process exit closes both).

- [ ] **Step 3: Build**

Run: `cargo build -p yserver --locked`
Expected: builds.

- [ ] **Step 4: Lint, format, commit**

```bash
cargo +nightly fmt && cargo clippy -p yserver --all-targets -- -D warnings
git add crates/yserver/src/drm/device.rs
git commit -m "feat(drm): Device::from_owned_fd to wrap a libseat-provided card fd"
```

---

## Task 6: Extract a shared `HotkeyDetector` (+ `SwitchVt`)

Today hotkey detection lives inside `LibinputThreadState` (`input_thread.rs:117-147`). Libseat mode needs the same detection on the core thread. Extract it into `input/hotkey.rs`, add `SwitchVt(u32)`, and have `input_thread.rs` use it (Direct-mode behaviour unchanged — `SwitchVt` is detected but, with no seat, only logged).

**Files:**
- Create: `crates/yserver/src/input/hotkey.rs`
- Modify: `crates/yserver/src/input/mod.rs` (add `pub mod hotkey;`)
- Modify: `crates/yserver/src/input_thread.rs`

- [ ] **Step 1: Write the failing tests + detector**

Create `crates/yserver/src/input/hotkey.rs`:

```rust
//! Server-internal hotkey detection on raw evdev keycodes, before XKB
//! translation. Shared by the Direct-mode input thread and the
//! Libseat-mode on-core libinput dispatch.

use crate::input::InputEvent;

// Linux evdev keycodes (raw, before the X11 +8 translation).
const LINUX_KEY_ENTER: u32 = 28;
const LINUX_KEY_BACKSPACE: u32 = 14;
const LINUX_KEY_LEFTCTRL: u32 = 29;
const LINUX_KEY_LEFTALT: u32 = 56;
const LINUX_KEY_RIGHTCTRL: u32 = 97;
const LINUX_KEY_RIGHTALT: u32 = 100;
const LINUX_KEY_F12: u32 = 88;
// F1..F10 are contiguous 59..=68. F11=87, F12=88.
const LINUX_KEY_F1: u32 = 59;
const LINUX_KEY_F10: u32 = 68;
const LINUX_KEY_F11: u32 = 87;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    /// Ctrl+Alt+Backspace — emergency shutdown.
    Zap,
    /// Ctrl+Alt+Enter — diagnostic scanout dump (SIGUSR1 path).
    DumpScanout,
    /// Ctrl+Alt+F12 — diagnostic per-drawable storage dump (SIGUSR2).
    DumpDrawables,
    /// Ctrl+Alt+F<N> — VT switch to VT N (1-based). Note F12 is claimed
    /// by DumpDrawables, so SwitchVt covers F1..F11 → VT1..VT11.
    SwitchVt(u32),
}

/// Tracks Ctrl/Alt held state off the raw kernel scancodes and matches
/// the fixed hotkey combos. Off-X-side on purpose: a grabbing client or
/// remapped keymap must not be able to swallow zap or the VT switch.
#[derive(Debug, Clone, Copy, Default)]
pub struct HotkeyDetector {
    ctrl_pressed: bool,
    alt_pressed: bool,
}

impl HotkeyDetector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Update modifier state for `ev`; return the hotkey it fires, if any.
    /// Only key *presses* fire; releases just update modifier state.
    pub fn check(&mut self, ev: &InputEvent) -> Option<Hotkey> {
        match *ev {
            InputEvent::KeyPress { keycode } => match keycode {
                LINUX_KEY_LEFTCTRL | LINUX_KEY_RIGHTCTRL => {
                    self.ctrl_pressed = true;
                    None
                }
                LINUX_KEY_LEFTALT | LINUX_KEY_RIGHTALT => {
                    self.alt_pressed = true;
                    None
                }
                _ if !(self.ctrl_pressed && self.alt_pressed) => None,
                LINUX_KEY_BACKSPACE => Some(Hotkey::Zap),
                LINUX_KEY_F12 => Some(Hotkey::DumpDrawables),
                LINUX_KEY_ENTER => Some(Hotkey::DumpScanout),
                LINUX_KEY_F1..=LINUX_KEY_F10 => Some(Hotkey::SwitchVt(keycode - LINUX_KEY_F1 + 1)),
                LINUX_KEY_F11 => Some(Hotkey::SwitchVt(11)),
                _ => None,
            },
            InputEvent::KeyRelease { keycode } => {
                match keycode {
                    LINUX_KEY_LEFTCTRL | LINUX_KEY_RIGHTCTRL => self.ctrl_pressed = false,
                    LINUX_KEY_LEFTALT | LINUX_KEY_RIGHTALT => self.alt_pressed = false,
                    _ => {}
                }
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(d: &mut HotkeyDetector, kc: u32) -> Option<Hotkey> {
        d.check(&InputEvent::KeyPress { keycode: kc })
    }
    fn release(d: &mut HotkeyDetector, kc: u32) {
        d.check(&InputEvent::KeyRelease { keycode: kc });
    }

    #[test]
    fn ctrl_alt_f2_switches_to_vt2() {
        let mut d = HotkeyDetector::new();
        assert_eq!(press(&mut d, LINUX_KEY_LEFTCTRL), None);
        assert_eq!(press(&mut d, LINUX_KEY_LEFTALT), None);
        assert_eq!(press(&mut d, 60 /* F2 */), Some(Hotkey::SwitchVt(2)));
    }

    #[test]
    fn ctrl_alt_f1_switches_to_vt1() {
        let mut d = HotkeyDetector::new();
        press(&mut d, LINUX_KEY_RIGHTCTRL);
        press(&mut d, LINUX_KEY_RIGHTALT);
        assert_eq!(press(&mut d, LINUX_KEY_F1), Some(Hotkey::SwitchVt(1)));
    }

    #[test]
    fn f_keys_without_modifiers_do_not_switch() {
        let mut d = HotkeyDetector::new();
        assert_eq!(press(&mut d, 60), None);
    }

    #[test]
    fn releasing_a_modifier_disarms() {
        let mut d = HotkeyDetector::new();
        press(&mut d, LINUX_KEY_LEFTCTRL);
        press(&mut d, LINUX_KEY_LEFTALT);
        release(&mut d, LINUX_KEY_LEFTALT);
        assert_eq!(press(&mut d, 60), None);
    }

    #[test]
    fn zap_and_dumps_still_fire() {
        let mut d = HotkeyDetector::new();
        press(&mut d, LINUX_KEY_LEFTCTRL);
        press(&mut d, LINUX_KEY_LEFTALT);
        assert_eq!(press(&mut d, LINUX_KEY_BACKSPACE), Some(Hotkey::Zap));
        assert_eq!(press(&mut d, LINUX_KEY_F12), Some(Hotkey::DumpDrawables));
        assert_eq!(press(&mut d, LINUX_KEY_ENTER), Some(Hotkey::DumpScanout));
    }

    #[test]
    fn f12_is_dump_not_vt12() {
        let mut d = HotkeyDetector::new();
        press(&mut d, LINUX_KEY_LEFTCTRL);
        press(&mut d, LINUX_KEY_LEFTALT);
        assert_eq!(press(&mut d, LINUX_KEY_F12), Some(Hotkey::DumpDrawables));
    }
}
```

Add `pub mod hotkey;` to `crates/yserver/src/input/mod.rs`.

- [ ] **Step 2: Run new tests (red→green)**

Run: `cargo test -p yserver input::hotkey -- --nocapture`
Expected: all PASS.

- [ ] **Step 3: Switch `input_thread.rs` to the shared detector**

In `crates/yserver/src/input_thread.rs`: delete the local `Hotkey` enum (lines 56-67), the `LINUX_KEY_*` consts now in `hotkey.rs` (keep any still used elsewhere), the `ctrl_pressed`/`alt_pressed` fields on `LibinputThreadState`, and the `check_hotkey` method. Replace with a `HotkeyDetector` field and delegate. At the call site in `process_batch` (the existing `check_hotkey` result match), keep the `Zap`/`DumpScanout`/`DumpDrawables` arms exactly as today and add:

```rust
                Some(Hotkey::SwitchVt(vt)) => {
                    // Direct mode (no libseat): VT switching is disabled.
                    // Log and swallow the keypress so it doesn't leak to
                    // a client.
                    log::debug!("input: Ctrl-Alt-F{vt} ignored (Direct mode, no libseat)");
                    continue;
                }
```

Import: `use crate::input::hotkey::{Hotkey, HotkeyDetector};`.

- [ ] **Step 4: Verify the input thread still builds + its tests pass**

Run: `cargo test -p yserver input_thread -- --nocapture && cargo build -p yserver --locked`
Expected: existing `input_thread` tests (e.g. `maps_relative_motion_to_clamped_absolute`) still PASS.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo +nightly fmt && cargo clippy -p yserver --all-targets -- -D warnings
git add crates/yserver/src/input/ crates/yserver/src/input_thread.rs
git commit -m "refactor(input): extract shared HotkeyDetector, add SwitchVt(N)"
```

---

## Task 7: `LibseatInterface` — route libinput device opens through libseat

Port wlroots' `libinput_open_restricted` (`backend.c:18-26`): instead of opening the path directly, call `seat.open_device`. The interface holds an `Rc<RefCell<LibseatInner>>` clone (same thread as the backend — never sent).

**Files:**
- Modify: `crates/yserver/src/input/context.rs`

- [ ] **Step 1: Add the libseat interface + constructor**

In `crates/yserver/src/input/context.rs`, add (keeping the existing `Interface`, `Context::new`, `SendContext` for Direct mode):

```rust
use std::{cell::RefCell, rc::Rc};

use crate::seat::{DeviceKind, LibseatInner};

/// libinput interface that opens evdev devices through libseat (wlroots'
/// `libinput_open_restricted` → `wlr_session_open_file`). Used only in
/// libseat mode, only on the core thread — the `Rc` never crosses a
/// thread boundary.
struct LibseatInterface {
    seat: Rc<RefCell<LibseatInner>>,
}

impl LibinputInterface for LibseatInterface {
    fn open_restricted(&mut self, path: &Path, _flags: i32) -> Result<OwnedFd, i32> {
        // libseat decides read/write; we ignore `flags` like wlroots does
        // (backend.c:18). open_device hands back an OwnedFd dup of
        // libseat's fd; libseat keeps its own handle, released later by
        // close_restricted → close_device_by_fd.
        self.seat
            .borrow_mut()
            .open_device(path, DeviceKind::Input)
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        // libinput hands back the exact OwnedFd we returned; its raw
        // number is our `handed_fd` key. Release libseat's side, then drop
        // libinput's dup. Mirrors wlroots' libinput_close_restricted
        // (backend.c:28).
        self.seat.borrow_mut().close_device_by_fd(fd.as_raw_fd());
        drop(fd);
    }
}

impl Context {
    /// Build a libinput context whose device opens route through libseat.
    /// Caller owns this on the core thread (NOT wrapped in `SendContext`).
    pub fn new_libseat(seat: Rc<RefCell<LibseatInner>>) -> io::Result<Self> {
        log_input_devnodes();
        let mut libinput = Libinput::new_with_udev(LibseatInterface { seat });
        libinput.udev_assign_seat("seat0").map_err(|()| {
            io::Error::other("libinput: udev_assign_seat(\"seat0\") failed under libseat")
        })?;
        Ok(Self { libinput })
    }
}
```

> **Re-entrancy contract (critical — codex review target).** `open_restricted`/`close_restricted` `borrow_mut()` the `LibseatInner`. They run synchronously inside `libinput.dispatch()` / `libinput.resume()` / `libinput.suspend()`. Therefore the backend MUST NOT hold a `LibseatInner` borrow across any call into libinput. The suspend/resume code in Task 11/12 enforces this: it borrows `LibseatInner` only for short libseat calls (`dispatch`, `disable`, `open_device` for DRM, `switch_session`) and never while calling `libinput.resume()`/`suspend()`/`dispatch()`. The libseat enable/disable *callback* touches only the separate `pending_events` queue, never `LibseatInner`, so it cannot deadlock against an in-flight `open_device`.

- [ ] **Step 2: Build**

Run: `cargo build -p yserver --locked`
Expected: builds. (`BorrowedFd`, `AsRawFd`, `libc` already imported at top of file; add any missing imports.)

- [ ] **Step 3: Lint, format, commit**

```bash
cargo +nightly fmt && cargo clippy -p yserver --all-targets -- -D warnings
git add crates/yserver/src/input/context.rs
git commit -m "feat(input): LibseatInterface routes libinput opens through libseat"
```

---

## Task 8: KMS backend owns the `Seat`; wire Libseat vs Direct in `lib.rs`

The biggest wiring task. `KmsBackendV2` gains seat ownership and the on-core libinput context; `poll_fds` advertises the seat fd (+ libinput fd in libseat mode); `lib.rs` opens the seat first and branches.

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs`
- Modify: `crates/yserver/src/kms/v2/platform.rs`
- Modify: `crates/yserver/src/kms/backend.rs` (`platform_init`)
- Modify: `crates/yserver/src/lib.rs`

- [ ] **Step 1: Add seat state to `KmsBackendV2`**

In `crates/yserver/src/kms/v2/backend.rs`, add fields to `KmsBackendV2` (around the existing field block near line 127):

```rust
    // VT switching (libseat mode). `Direct`/`None`/empty in Direct mode.
    seat: crate::seat::Seat,
    seat_state: crate::seat::state::SeatState,
    seat_pending: crate::seat::state::SeatPending,
    /// libinput owned on the core thread (libseat mode only). In Direct
    /// mode the dedicated input thread owns libinput and this is `None`.
    core_libinput: Option<crate::input::Context>,
    /// On-core cursor/scroll accumulator for libseat-mode input mapping
    /// (the same state the input thread holds in Direct mode). `None` in
    /// Direct mode.
    core_input_state: Option<crate::input_thread::LibinputThreadState>,
    /// Cached raw fds for `poll_fds(&self)` (which can't call the
    /// `&mut`-taking `get_fd`). The seat connection fd, the libinput fd,
    /// AND the DRM fd are all STABLE for the process lifetime (the DRM fd
    /// is opened once and never reopened — Deviation #5), so caching plus
    /// the one-shot startup registration in `run_core` are correct.
    seat_fd: std::os::fd::RawFd,
    core_libinput_fd: std::os::fd::RawFd,
    /// Shared core sender for emitting Shutdown/Dump messages from the
    /// on-core hotkey path (libseat mode).
    input_sender: Option<yserver_core::core_loop::CoreSender>,
    hotkey: crate::input::hotkey::HotkeyDetector,
```

Initialise (Direct mode): `seat: Seat::Direct`, `seat_state: SeatState::Active`, `seat_pending: default`, `core_libinput: None`, `core_input_state: None`, `seat_fd: -1`, `core_libinput_fd: -1`, `input_sender: None`, `hotkey: HotkeyDetector::new()`. Libseat-mode init (Step 4) overrides `seat`, `core_libinput`, `core_input_state`, `seat_fd`, `core_libinput_fd`.

- [ ] **Step 2: Implement `poll_fds`, `set_input_sender`**

In the `Backend` impl for `KmsBackendV2` (`backend.rs`), extend `poll_fds` so libseat mode advertises both fds:

```rust
    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, BackendFdKind)> {
        let mut fds = vec![(self.platform.drm_event_fd(), BackendFdKind::Drm)];
        if let crate::seat::Seat::Libseat { inner, .. } = &self.seat {
            // Seat fd: get_fd needs &mut; cache it at open time instead.
            fds.push((self.seat_fd, BackendFdKind::Seat));
        }
        if self.core_libinput.is_some() {
            fds.push((self.core_libinput_fd, BackendFdKind::Libinput));
        }
        fds
    }

    fn set_input_sender(&mut self, sender: yserver_core::core_loop::CoreSender) {
        self.input_sender = Some(sender);
    }
```

> **Note:** `seat.get_fd()` / `libinput.as_raw_fd()` need `&mut` / are cheap but `poll_fds` is `&self`. Cache both raw fds into `self.seat_fd: RawFd` and `self.core_libinput_fd: RawFd` when the seat/context are created (Step 4). Add those two fields too. (Keep the existing DRM fd accessor name — confirm it; the agent map cited `drm_event_fd`/page-flip readiness via `DRM_TOKEN`. Match the real accessor.)

- [ ] **Step 3: Route DRM open through the seat in `platform_init`**

In `crates/yserver/src/kms/backend.rs::platform_init` (line 428), the first line is `let device = Arc::new(drm::Device::open(device_path)?);`. Add a seat-aware path. Change the signature to accept an optional seat fd, or add a sibling `platform_init_seat`:

```rust
pub(crate) fn platform_init_with_fd(
    device_path: &str,
    card_fd: std::os::fd::OwnedFd, // from seat.open_device
    commit: fn(/* unchanged */) -> io::Result<()>,
) -> io::Result<PlatformInit> {
    let device = Arc::new(drm::Device::from_owned_fd(card_fd, device_path)?);
    // ... rest identical to platform_init from the render-node discovery
    //     line onward (copy the body; the only change is how `device` is
    //     constructed).
}
```

(Render-node discovery via `crate::kms::render_node::open_for_card` is unchanged — Mesa's render node is independent of the primary-node fd and of master.)

- [ ] **Step 4: Branch in `lib.rs`**

In `crates/yserver/src/lib.rs`, BEFORE backend construction, open the seat. Then construct the backend in seat-aware fashion, and after the channel exists, hand the sender to the backend.

Sketch (adapt to the real backend constructor + the early `composite_and_flip` at line 226):

```rust
    // Open the seat first so DRM + input device opens can route through
    // it. Direct fallback keeps today's behaviour.
    let seat = crate::seat::Seat::open();

    // Backend construction: in libseat mode, open the DRM card via the
    // seat and build libinput on the core thread; in Direct mode, use the
    // existing direct path (drm::Device::open + input thread).
    let mut backend = build_kms_backend_v2(seat /* + device_path, fb dims, … */)?;

    // ... existing socket bind, initial composite_and_flip ...

    let (poll, sender, rx) = core_loop::channel()?;

    // Libseat mode: backend owns input on the core thread, so give it the
    // sender for Shutdown/Dump messages. Direct mode: spawn the input
    // thread exactly as today.
    if backend.is_libseat_mode() {
        backend.set_input_sender(sender.clone_handle());
    } else if let Some(input_ctx) = backend.take_input_ctx() {
        // ... unchanged input-thread spawn (lib.rs:238-250) ...
    }
```

`build_kms_backend_v2` is a free fn in `lib.rs` (or `kms/v2/backend.rs`) with this concrete signature:

```rust
fn build_kms_backend_v2(
    seat: crate::seat::Seat,
    device_path: &str,
    fb_w: u16,
    fb_h: u16,
) -> io::Result<KmsBackendV2> {
    match &seat {
        crate::seat::Seat::Libseat { inner, .. } => {
            let inner = std::rc::Rc::clone(inner);
            // DRM card via libseat (OwnedFd dup). FATAL on failure — see below.
            let card_fd = inner
                .borrow_mut()
                .open_device(std::path::Path::new(device_path), DeviceKind::Drm { is_kms: true })
                .map_err(|e| io::Error::other(format!(
                    "libseat mode: opening DRM card {device_path} via seat failed: {e}"
                )))?;
            let platform = platform_init_with_fd(device_path, card_fd, v2_commit)?;
            let core_libinput = crate::input::Context::new_libseat(std::rc::Rc::clone(&inner))?;
            let core_libinput_fd = core_libinput.fd();
            let seat_fd = inner.borrow_mut().fd()?;
            Ok(KmsBackendV2::from_parts_libseat(
                platform, seat, core_libinput, core_libinput_fd, seat_fd, fb_w, fb_h,
            ))
        }
        crate::seat::Seat::Direct => {
            let platform = platform_init(device_path, v2_commit)?; // today's path
            Ok(KmsBackendV2::from_parts_direct(platform, fb_w, fb_h))
        }
    }
}
```

- Add `KmsBackendV2::is_libseat_mode(&self) -> bool { self.seat.is_libseat() }`.
- Add `Seat::libseat_inner(&self) -> Option<Rc<RefCell<LibseatInner>>>` (clone the `Rc`).
- The `from_parts_libseat` / `from_parts_direct` constructors set the fields per Step 1. (Pick names matching the existing `KmsBackendV2` constructor convention — there is already a v2 constructor in `backend.rs`; extend it with a `seat`-bearing variant rather than inventing a parallel one if cleaner.)

> **Startup device-open-fail policy (codex finding #3).** Once `Seat::open()` succeeds we are committed to libseat mode — logind/seatd has handed us the session and direct opens won't get DRM master. So if `seat.open_device(card)` (or any input open during `Context::new_libseat`) **fails at startup, it is FATAL**: propagate the error out of `run()` so yserver exits with a clear message. Do NOT silently fall back to Direct mode after libseat has taken control — that path can't acquire master and would wedge. The only fallback point is `Seat::open()` itself returning `Direct` (no libseat at all).

- [ ] **Step 5: Build + smoke (Direct mode must be unchanged)**

Run: `cargo build --locked && cargo test --all-targets --locked`
Expected: builds; all existing tests pass. Direct-mode startup (no libseat, e.g. in the sandbox) must behave exactly as before — verify with a ynest/Direct smoke if available, else confirm `Seat::open()` logs the Direct fallback and the input thread still spawns.

- [ ] **Step 6: Lint, format, commit**

```bash
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings
git add crates/yserver/src/kms/ crates/yserver/src/lib.rs
git commit -m "feat(kms): KmsBackendV2 owns libseat Seat; libseat-vs-direct wiring"
```

---

## Task 9: On-core libinput dispatch + on-core hotkeys (`on_libinput_ready`)

Port wlroots' `handle_libinput_readable` (`backend.c:49-63`): dispatch libinput, translate events, run them through the existing fanout, and detect hotkeys on the core thread. VT switch calls `seat.switch_session` inline.

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs`
- Possibly: `crates/yserver/src/input/context.rs` (expose `Context::dispatch` returning `Vec<InputEvent>` — already exists at `context.rs:108`)

- [ ] **Step 1: Implement `on_libinput_ready`**

In the `Backend` impl for `KmsBackendV2`:

```rust
    fn on_libinput_ready(&mut self, state: &mut ServerState) {
        let Some(ctx) = self.core_libinput.as_mut() else {
            return;
        };
        let events = match ctx.dispatch() {
            Ok(evs) => evs,
            Err(e) => {
                log::warn!("kms: core libinput dispatch failed: {e}");
                return;
            }
        };
        for ev in events {
            if let Some(hk) = self.hotkey.check(&ev) {
                self.handle_core_hotkey(hk);
                continue; // do not forward the hotkey keypress to clients
            }
            // Reuse the same mapping the input thread uses, then the same
            // fanout on_host_input already drives.
            let host = self.map_input_event(&ev); // factor from LibinputThreadState::map
            if let Some(host) = host {
                self.on_host_input(state, host);
            }
        }
    }
```

- [ ] **Step 2: Implement `handle_core_hotkey`**

```rust
impl KmsBackendV2 {
    fn handle_core_hotkey(&mut self, hk: crate::input::hotkey::Hotkey) {
        use crate::input::hotkey::Hotkey;
        match hk {
            Hotkey::Zap => {
                if let Some(s) = &self.input_sender {
                    let _ = s.send(yserver_core::core_loop::Message::Shutdown);
                }
            }
            Hotkey::DumpScanout => {
                if let Some(s) = &self.input_sender {
                    let _ = s.send(yserver_core::core_loop::Message::DumpScanout);
                }
            }
            Hotkey::DumpDrawables => {
                if let Some(s) = &self.input_sender {
                    let _ = s.send(yserver_core::core_loop::Message::DumpDrawables);
                }
            }
            Hotkey::SwitchVt(vt) => {
                if let crate::seat::Seat::Libseat { inner, .. } = &self.seat {
                    // Short borrow; switch_session is fire-and-forget and
                    // does NOT transition seat_state (that happens later
                    // via the disable callback).
                    if let Err(e) = inner.borrow_mut().switch_session(vt) {
                        log::warn!("kms: switch_session({vt}) failed: {e}");
                    } else {
                        log::info!("kms: requested VT switch to {vt}");
                    }
                }
            }
        }
    }
}
```

- [ ] **Step 3: Factor `map_input_event`**

Move the body of `LibinputThreadState::map` (`input_thread.rs:158+`) into a free function or a small reusable mapper so both the input thread and `KmsBackendV2` produce identical `HostInputEvent`s (cursor accumulation, scroll v120 banking, button codes). The on-core mapper must keep its own cursor accumulator + scroll banks; store them on `KmsBackendV2` (or reuse `LibinputThreadState` directly as a field — preferred, since it already encapsulates exactly this state). Recommended: give `KmsBackendV2` a `core_input_state: Option<LibinputThreadState>` (Some in libseat mode) and call `core_input_state.map(ev, now)`.

- [ ] **Step 4: Build + existing tests**

Run: `cargo build --locked && cargo test --all-targets --locked`
Expected: builds; existing tests pass.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings
git add crates/yserver/src/kms/v2/backend.rs crates/yserver/src/input_thread.rs
git commit -m "feat(kms): on-core libinput dispatch + on-core hotkeys (libseat mode)"
```

---

## Task 10: Track held keys + synthesize releases on suspend

`xkbcommon::xkb::State` (wrapped by `XkbState`, `kms/core.rs:71`) aggregates modifier masks but cannot enumerate physically-down keys, so we track them explicitly. `KmsCore.button_mask` (`core.rs:749`) already tracks held buttons.

**Files:**
- Modify: `crates/yserver/src/kms/core.rs`
- Modify: `crates/yserver/src/kms/v2/backend.rs`

- [ ] **Step 1: Add `down_keys` + maintain it**

In `crates/yserver/src/kms/core.rs`, add to `KmsCore` (near the keyboard state fields, line 729):

```rust
    /// Cooked X11 keycodes currently pressed. Maintained in the key path
    /// so suspend can synthesize a release for each (xkbcommon::State
    /// cannot enumerate down keys).
    pub(crate) down_keys: std::collections::HashSet<u8>,
```

Initialise to `HashSet::new()` in both `KmsCore` constructors (the `new`-style fns at `core.rs:818+`).

Find the key-cooking site (`cook_host_key` / the `on_host_input` `Key` arm in `kms/v2/backend.rs`). After computing the cooked X11 `keycode` and `pressed`:

```rust
        if cooked.pressed {
            self.core.down_keys.insert(cooked.keycode);
        } else {
            self.core.down_keys.remove(&cooked.keycode);
        }
```

- [ ] **Step 2: Write the synthesize-release tests + function**

Add a method on `KmsBackendV2` that, given `&mut ServerState`, emits a synthetic `KeyRelease` for every `down_keys` entry and a `ButtonRelease` for every held button, through the SAME fanout used live (`key_event_fanout_to_state` / `pointer_event_fanout_to_state`):

```rust
impl KmsBackendV2 {
    /// Synthesize releases for every held key/button so a client that
    /// owned input at switch time doesn't see stuck-down keys on resume.
    /// XI2 raw listeners are intentionally NOT updated (spec §"XI2 raw
    /// events"). Crossing events are not synthesized.
    fn synthesize_held_releases(&mut self, state: &mut ServerState) {
        // Keys.
        let keys: Vec<u8> = self.core.down_keys.drain().collect();
        for keycode in keys {
            let ev = HostKeyEvent {
                pressed: false,
                keycode,
                time: self.now_ms(),
                root_x: self.core.cursor_x as i16,
                root_y: self.core.cursor_y as i16,
                event_x: self.core.cursor_x as i16,
                event_y: self.core.cursor_y as i16,
                state: 0,
            };
            let _ = yserver_core::core_loop::key_event_fanout_to_state(state, ev);
        }
        // Buttons: held bits live in (button_mask >> 8) & 0x1f, bit n => button n+1.
        let held = (self.core.button_mask >> 8) & 0x1f;
        for n in 0..5u16 {
            if held & (1 << n) != 0 {
                let button = (n + 1) as u16;
                // Build a synthetic PointerButton release and run it
                // through pointer fanout exactly as on_host_input does.
                self.process_pointer_button(u32::from(button), false, state);
            }
        }
        self.core.button_mask = 0;
    }
}
```

Test in `crates/yserver/src/kms/v2/backend.rs` `#[cfg(test)]` (use the existing KMS test harness that builds a `ServerState` + backend; mirror the harness from the Cinnamon grab regression `0c117e7`/`key_fanout` tests):

```rust
#[test]
fn synthesize_releases_emits_one_per_held_key_and_button() {
    // GIVEN a backend with two keys down and button 1 + button 3 held.
    // WHEN synthesize_held_releases runs,
    // THEN exactly those KeyRelease/ButtonRelease are fanned out and the
    //      tracking sets are cleared. Assert no XI2 raw event is emitted.
    // (Construct via the existing KMS unit-test harness; assert on the
    //  recording/observer the harness exposes.)
}
```

> **Implementer:** wire the assertions to whatever observation hook the KMS unit harness provides (the `recording` backend or a captured client). The behavioural contract to assert: (a) one release per held key, (b) one release per held button, (c) `down_keys` empty + `button_mask == 0` after, (d) no XI2 raw event.

- [ ] **Step 3: Run tests**

Run: `cargo test -p yserver kms::v2::backend -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Lint, format, commit**

```bash
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings
git add crates/yserver/src/kms/core.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "feat(kms): track held keys + synthesize releases on suspend"
```

---

## Task 11: Gate scanout I/O on `seat_state`, then the suspend sequence

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs`
- Modify: `crates/yserver/src/kms/v2/platform.rs`

- [ ] **Step 1: Gate the three master-requiring operations**

Add `seat_state.allows_scanout()` guards (early-return / no-op) at:
- the composite/flip entry (`composite_and_flip` and the page-flip resubmit) — skip modeset + pageflip when not Active.
- `vkQueueSubmit2` via `flush_submit_group` (`platform.rs:~1518`) — when not Active, do not submit (return a benign "skipped" outcome). This is the only reason to gate submits: avoid in-flight submits racing modeset/scanout teardown (spec §"Device fd lifetime contract").

Implement as a single helper read on the backend: `fn scanout_allowed(&self) -> bool { self.seat_state.allows_scanout() }` and check it at each site.

- [ ] **Step 2: Implement the suspend sequence**

In `KmsBackendV2`, the routine invoked when `on_seat_ready` decides `BeginSuspend` (spec §Suspend, minus the dropped input-quiesce barrier):

```rust
impl KmsBackendV2 {
    fn run_suspend(&mut self, state: &mut ServerState) {
        // 1. State already set to Suspending by the state machine; gates
        //    are now closed (scanout_allowed() == false).
        // 2. DETERMINISTIC INPUT DRAIN (replaces the spec's cross-thread
        //    input-quiesce barrier). mio may deliver SEAT_TOKEN before
        //    LIBINPUT_TOKEN in the same poll batch, so libinput may hold
        //    events the kernel delivered just before the switch. Drain
        //    them now — through the SAME path on_libinput_ready uses — so
        //    `down_keys`/`button_mask` reflect every real event before we
        //    snapshot. One dispatch suffices (it reads all currently
        //    available events); loop defensively until empty.
        if self.core_libinput.is_some() {
            loop {
                let evs = match self.core_libinput.as_mut().unwrap().dispatch() {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("kms: suspend drain dispatch failed: {e}");
                        break;
                    }
                };
                if evs.is_empty() {
                    break;
                }
                for ev in evs {
                    // Hotkeys are irrelevant mid-suspend; just update input
                    // state via the normal mapping + fanout.
                    if let Some(host) = self.map_input_event(&ev) {
                        self.on_host_input(state, host);
                    }
                }
            }
        }
        // 3. Synthesize held-key / held-button releases (snapshot is now
        //    consistent with every delivered event).
        self.synthesize_held_releases(state);
        // 4. Wait for in-flight GPU work, bounded (reuse the 5s shutdown
        //    bound used by FenceTicket::wait / device_wait_idle).
        self.platform.wait_idle_bounded();
        // 5. Close input fds: suspend libinput → close_restricted →
        //    seat.close_device for each input device. MUST NOT hold a
        //    LibseatInner borrow across this call (re-entrancy contract).
        if let Some(ctx) = self.core_libinput.as_mut() {
            ctx.suspend(); // add a thin Context::suspend() → libinput.suspend()
        }
        // 6. Ack the disable. We do NOT drmDropMaster (spec §"No explicit
        //    drmDropMaster"): the gate in step 1 already stopped master
        //    ioctls; libseat/logind revoke master during this ack.
        if let crate::seat::Seat::Libseat { inner, .. } = &self.seat {
            if let Err(e) = inner.borrow_mut().disable() {
                log::warn!("kms: libseat disable() ack failed: {e}");
            }
        }
        // 7. Commit Suspended (state machine).
    }
}
```

Wrap steps 3-6 so any error still reaches the `disable()` ack (Risk #1: if we never ack, the kernel freezes the screen waiting). Use error-tolerant logging, not `?`.

- [ ] **Step 3: Add `Context::suspend`/`Context::resume`**

In `crates/yserver/src/input/context.rs`, add thin wrappers over libinput:

```rust
impl Context {
    pub fn suspend(&mut self) { self.libinput.suspend(); }
    pub fn resume(&mut self) -> io::Result<()> {
        self.libinput.resume().map_err(|()| io::Error::other("libinput resume failed"))
    }
}
```

(Confirm the `input` crate exposes `Libinput::suspend`/`resume`; if `resume` returns `Result<(), ()>` adapt the mapping.)

- [ ] **Step 4: Build**

Run: `cargo build --locked`
Expected: builds. (Suspend is exercised end-to-end by the stub test in Task 13 + hardware.)

- [ ] **Step 5: Lint, format, commit**

```bash
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings
git add crates/yserver/src/kms/ crates/yserver/src/input/context.rs
git commit -m "feat(kms): seat_state gating + suspend sequence"
```

---

## Task 12: The resume sequence + `on_seat_ready` driver

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs`
- Modify: `crates/yserver/src/kms/v2/platform.rs`

- [ ] **Step 1: Resume helpers on `PlatformBackend`**

In `crates/yserver/src/kms/v2/platform.rs`, add (note: the DRM fd is NOT reopened — Deviation #5; we keep the existing `self.device` and rely on libseat/logind having restored master before the `Enable`):
- `requery_outputs_and_modeset(&mut self) -> io::Result<Vec<RandrChange>>` — on the EXISTING `self.device`, call `drm::modeset::discover_outputs` (`modeset.rs:193`), drop outputs that disappeared (return them so the backend fires RandR change-events), redo modeset on survivors using the saved `OutputLayout`, ignore newly-appeared outputs (MVP non-goal). If every modeset commit fails (e.g. the card was hot-unplugged while suspended), surface an error so the backend can exit (Risk #4).
- `rearm_cursor(&mut self)` — `cursor_plane_rebind_visible_crtcs()` (`scene.rs:891`).
- `post_full_damage_all_outputs(&mut self)` — full-screen damage + immediate composite.

- [ ] **Step 2: Implement the resume sequence**

```rust
impl KmsBackendV2 {
    fn run_resume(&mut self, state: &mut ServerState) {
        // 1. State already Resuming.
        // 2. NO DRM reopen / no drmSetMaster (Deviation #5): the DRM fd is
        //    the same one we opened at startup; libseat/logind restored
        //    master before delivering Enable. We just re-modeset on it.
        // 3. Re-query connectors, drop missing (fire RandR change), redo
        //    modeset on the existing device. If all commits fail, the card
        //    is gone → exit (Risk #4).
        match self.platform.requery_outputs_and_modeset() {
            Ok(changes) => self.fire_randr_changes(state, changes),
            Err(e) => {
                log::error!("kms: resume modeset failed (card gone?): {e}; exiting");
                self.request_exit();
                return;
            }
        }
        // 4. Re-arm cursor.
        self.platform.rearm_cursor();
        // 5. Repaint (full damage) is deferred until the state machine
        //    commits Active so the scanout gate is open (see on_seat_ready).
        // 6. Resume input: libinput.resume() → open_restricted →
        //    seat.open_device for each device. MUST be called with NO
        //    LibseatInner borrow held (re-entrancy contract) — run_resume
        //    holds none.
        if let Some(ctx) = self.core_libinput.as_mut() {
            if let Err(e) = ctx.resume() {
                log::warn!("kms: libinput resume failed: {e}");
            }
        }
        // 7. Completion boundary handled by the on_seat_ready driver
        //    (resume_complete may bypass Active → Suspending).
    }
}
```

- [ ] **Step 3: Implement the `on_seat_ready` driver**

This ties the state machine to the sequences and drains the callback's event queue:

```rust
    fn on_seat_ready(&mut self, state: &mut ServerState) {
        let (inner, events) = match &self.seat {
            crate::seat::Seat::Libseat { inner, pending_events } => {
                (Rc::clone(inner), Rc::clone(pending_events))
            }
            crate::seat::Seat::Direct => return,
        };
        // Dispatch libseat: callback pushes Enable/Disable into `events`.
        if let Err(e) = inner.borrow_mut().dispatch() {
            log::error!("kms: libseat dispatch failed: {e}; exiting"); // Risk #7
            self.request_exit();
            return;
        }
        // Drain queued events in order; borrow released before sequences.
        let drained: Vec<SeatEventKind> = events.borrow_mut().drain(..).collect();
        for ev in drained {
            match self.seat_state.on_event(&mut self.seat_pending, ev) {
                SeatAction::BeginSuspend => {
                    self.run_suspend(state);
                    self.seat_state.suspend_complete(&self.seat_pending);
                }
                SeatAction::BeginResume => {
                    self.run_resume(state);
                    match self.seat_state.resume_complete(&mut self.seat_pending) {
                        SeatAction::BeginSuspend => {
                            self.run_suspend(state);
                            self.seat_state.suspend_complete(&self.seat_pending);
                        }
                        _ => {
                            // Committed Active: now the gate is open, repaint.
                            self.platform.post_full_damage_all_outputs();
                        }
                    }
                }
                SeatAction::Nothing => {
                    log::debug!("kms: seat event {ev:?} ignored in {:?}", self.seat_state);
                }
            }
        }
        // After a suspend completes, if pending_enable was set we stay in
        // Suspended; the next real Enable (a fresh dispatch) drives resume.
    }
```

- [ ] **Step 4: Build + full test suite**

Run: `cargo build --locked && cargo test --all-targets --locked`
Expected: builds; all pass.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings
git add crates/yserver/src/kms/
git commit -m "feat(kms): resume sequence + on_seat_ready state-machine driver"
```

---

## Task 13: Stub-backed suspend/resume integration test + `YSERVER_SIMULATE_VT_SWITCH`

The **stub integration test (Step 2) is the primary deterministic coverage** — it drives the suspend→resume sequence directly through the backend, no live poller, no real seat. The runtime knob (Step 3) is a convenience for a live single-machine smoke and reuses the existing channel wakeup.

**Prerequisite refactor (in Task 12's `on_seat_ready`):** extract the per-event body (the `match self.seat_state.on_event(...)` arm including `run_suspend`/`run_resume`/completion) into a shared method:

```rust
fn drive_seat_event(&mut self, state: &mut ServerState, ev: SeatEventKind) { /* the match body */ }
```

`on_seat_ready` then calls `self.drive_seat_event(state, ev)` per drained event; the test/knob paths call it directly without touching libseat.

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs`
- Modify: `crates/yserver-core/src/core_loop/message.rs` + `run.rs` + `backend/trait_def.rs` (debug seat-event message, Step 3)
- Create: `crates/yserver/tests/vt_switch_sim.rs` (or extend `tests/v2_acceptance.rs`)

- [ ] **Step 1: Test-only injection entry point**

Add to `KmsBackendV2` (compiled always; trivial):

```rust
/// Drive a fake seat enable/disable, bypassing libseat. Used by the
/// integration test and the YSERVER_SIMULATE_VT_SWITCH knob.
pub fn inject_seat_event_for_test(&mut self, state: &mut ServerState, enable: bool) {
    let ev = if enable { SeatEventKind::Enable } else { SeatEventKind::Disable };
    self.drive_seat_event(state, ev);
}
```

- [ ] **Step 2: Stub integration test (primary, deterministic)**

Add an integration test that builds the KMS v2 backend test harness and calls `inject_seat_event_for_test(state, false)` then `(state, true)`, asserting:
- after Disable: `seat_state == Suspended`, `scanout_allowed() == false`, held releases were emitted, GPU wait was called.
- after Enable: `seat_state == Active`, a full-damage repaint was posted on every output.
- rapid Disable-immediately-after-Enable drives `resume_complete → BeginSuspend` without passing through Active (assert no Active observed between).

Model the harness on `crates/yserver/tests/v2_acceptance.rs`. Also assert the **re-entrancy contract** (codex soft spot #2): after a full disable→enable cycle the test completes with no `RefCell` borrow panic — exercising that the resume path's `libinput.resume()` (which re-enters `open_restricted` → `seat.borrow_mut()`) never runs while the driver holds a `LibseatInner` borrow. (In the stub test, substitute a fake input context that invokes the open-callback during `resume()` to actually trigger the re-entry.)

- [ ] **Step 3: Live knob `YSERVER_SIMULATE_VT_SWITCH` (optional convenience)**

Concrete wakeup path (the bare `pending_events` push can't reach the loop on its own):
- Add a debug message to `yserver-core` that does NOT depend on the yserver `seat` type: in `message.rs`, `Message::DebugSeatEvent { enable: bool }`; in `trait_def.rs`, `fn inject_seat_event(&mut self, _state: &mut ServerState, _enable: bool) {}` (default no-op); in `run.rs`'s `NOTIFY_TOKEN` drain, `Message::DebugSeatEvent { enable } => backend.inject_seat_event(state, enable)`. `KmsBackendV2::inject_seat_event` forwards to `inject_seat_event_for_test`.
- In `lib.rs`, when `std::env::var("YSERVER_SIMULATE_VT_SWITCH").is_ok()`, spawn a thread holding `sender.clone_handle()` that, after ~3s, sends `DebugSeatEvent { enable: false }` then ~2s later `DebugSeatEvent { enable: true }`. The existing channel `Waker` (NOTIFY_TOKEN) wakes the loop; `run_core` calls `inject_seat_event`, which runs the real suspend/resume sequences. Strictly behind the env var.

- [ ] **Step 4: Run**

Run: `cargo test -p yserver --test vt_switch_sim --locked -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Lint, format, commit**

```bash
cargo +nightly fmt && cargo clippy --all-targets -- -D warnings
git add crates/yserver/src/kms/v2/backend.rs crates/yserver/tests/vt_switch_sim.rs crates/yserver-core/src/core_loop/message.rs crates/yserver-core/src/core_loop/run.rs crates/yserver-core/src/backend/trait_def.rs
git commit -m "test(kms): stub VT-switch suspend/resume integration + simulate knob"
```

---

## Task 14: Docs + hardware test checklist (user-run)

**Files:**
- Modify: `docs/status.md`

- [ ] **Step 1: Update status**

Add a VT-switching entry to `docs/status.md`: libseat-mode behaviour, Direct-mode fallback, what's covered (state machine, held-key release, resume modeset/RandR) and the MVP non-goals (hot-plug while suspended, Vulkan device-loss recovery, multi-seat).

- [ ] **Step 2: Commit**

```bash
git add docs/status.md
git commit -m "docs(status): record VT switching (libseat) feature"
```

- [ ] **Step 3: Hardware verification — HAND TO THE USER (do not run `*-hw` yourself)**

The sandbox cannot acquire DRM master / seat0, so the agent stops here and the user drives these. For each host: start yserver from a tty under a real session → Ctrl-Alt-F2 → confirm getty visible → Ctrl-Alt-F1 → confirm cursor + desktop restored, full-screen repaint, **zero `vk_device_lost`, zero `missed_pageflips` once Active**. Capture the log.

- [ ] bee (Ryzen 6900HX / RADV) — plus the rapid-double-switch capture (Ctrl-Alt-F2 then Ctrl-Alt-F1 within ~100 ms) to exercise the `resume_complete → BeginSuspend` boundary.
- [ ] yoga (Snapdragon X1 / Turnip)
- [ ] silence (rx580 + split-driver iGPU)
- [ ] iMac 19,2 (i5-8500 + Polaris / RADV)

Agent picks up from the captured logs (per the "read logs before input fixes" / "hw recipes user-only" working agreements).

---

## Self-review (against the spec)

**Spec coverage:**
- Goal (Ctrl-Alt-F<N> ↔ back, no wedge/stuck keys) → Tasks 6, 9 (detect + switch), 11-12 (suspend/resume), 10 (held-key release). ✓
- libseat as session manager + Direct fallback → Task 4. ✓
- Single rule for fallback (`open` succeeds → Libseat) → Task 4 `Seat::open`. ✓
- Threading: libseat on the main thread → Tasks 3, 8 (backend on core thread, seat fd as poll source). Spec's "main loop owns Seat" reconciled to "KMS backend owns Seat" with rationale in Deviations. ✓
- Callback semantics (enable/disable only, ack via disable) → Task 4 closure + Task 12 driver; `disable()` ack in Task 11. ✓
- Device fd lifetime: DRM master revoke by kernel, render node immune, input fds closed → Tasks 5, 7, 11. No explicit `drmDropMaster` → Task 11 step 2. ✓
- State machine + reentrancy matrix + completion boundaries → Task 2 (exhaustive tests). ✓
- Suspend steps → Task 11 (input-quiesce barrier dropped per Deviations; documented). ✓
- Resume steps → Task 12. **Deviation from spec (#5):** no DRM reopen and no `drmSetMaster` (wlroots keeps the fd and lets libseat/logind own master); resume = requery connectors + modeset on the existing fd + cursor + repaint + libinput.resume. ✓
- Multi-card/split-driver → unchanged card selection + one server-wide `SeatState` (Task 8 reuses `platform_init`). ✓
- Input: held-key release → Task 10; Ctrl-Alt-Fn pre-translation detection → Task 6; `switch_session` fire-and-forget, no state transition → Task 9. XI2 raw not updated → Task 10 contract. ✓
- Testing: unit (state machine, hotkey, held-key) → Tasks 2/6/10; stub integration + simulate knob → Task 13; hardware matrix → Task 14. **Gap vs spec:** the spec's "real-libseat-in-a-container CI" test is NOT included — there is no container CI harness today (CI is unit+lint only). Flagged as a follow-up rather than invented here.
- Risks #1-#9 → addressed in Tasks 11/12 (error-tolerant ack #1; bounded GPU wait #3; reopen-fail exit #4; modeset-fail drop output #5; double-switch #2-as-coalesce + Task 2 tests #6; dispatch-error exit #7). Render-node/nvidia reasoning needs no code. ✓
- Rollout (one PR, runtime detection, no env disable) → matches; `YSERVER_SIMULATE_VT_SWITCH` is debug-only, not a disable switch. ✓

**Placeholder scan:** No "TBD"/"handle errors appropriately". Task 10 step 2 and Task 13 step 2 describe test *assertions* in prose because they bind to the existing KMS unit harness whose exact observation hook must be read at implementation time — the behavioural contract is fully specified.

**Type consistency:** `SeatState`/`SeatPending`/`SeatEventKind`/`SeatAction` (Task 2) used consistently in Tasks 4/12. `LibseatInner`/`ManagedDevice`/`DeviceKind` (Task 4) used in Tasks 7/8/12. `HotkeyDetector`/`Hotkey` (Task 6) used in Tasks 6/9. `Device::from_owned_fd` (Task 5) used in Tasks 8/12. Trait methods `on_seat_ready`/`on_libinput_ready`/`set_input_sender` (Task 3) implemented in Tasks 9/12/8.

**Soft spots — RESOLVED after codex round 1 (gpt-5.4-mini) + libseat 0.2.4 API verification:**
1. **RawFd vs OwnedFd handoff for DRM** — resolved. The libseat `Device` has no `Drop` and is released only via `close_device(device)` (by value), so there is no early-close. `LibseatInner::open_device` hands back an `OwnedFd` dup; `ManagedDevice` owns the libseat `Device`. The DRM device is opened once and kept (no reopen — Deviation #5); input device dups are released by libinput's `close_restricted` → `close_device_by_fd`. (Task 4/5.)
2. **`Rc<RefCell<LibseatInner>>` re-entrancy** — resolved by construction + test. All libseat access is via short `inner.borrow_mut().method()` calls; the driver never holds a borrow across a `libinput.{dispatch,suspend,resume}()` call (which re-enter `open_restricted`/`close_restricted`). The libseat callback touches only the separate `pending_events` queue. Task 13's stub test asserts no borrow panic across a full resume cycle (which exercises the `libinput.resume()` re-entry).
3. **Caching `seat_fd`/`core_libinput_fd` as `RawFd`** — resolved. The seat connection fd, the libinput fd, AND the DRM fd are all stable for the process lifetime (the DRM fd is opened once and never reopened — Deviation #5), so the one-shot startup `poll_fds` registration is valid for the whole session.

**Open question (codex round-1/2 finding) — RESOLVED by following wlroots, NOT by adding machinery.** Codex flagged that reopening the DRM fd on resume needs poller re-registration, and round 2 found the re-registration itself had an fd-lifetime bug (deregister on a closed/reused fd). Root cause: the spec's "reopen + drmSetMaster on resume" is wrong per wlroots (`backend/drm/backend.c:107-127` only re-scans connectors; the fd is opened once in `session.c:338`). Fix: **keep the DRM fd stable across VT switches** (Deviation #5). This deletes the `FdUpdate`/re-registration mechanism entirely and removes the bug class — the cleaner and more correct resolution.

**Remaining acknowledged gaps (intentional, not blockers):**
- No "real-libseat container CI" test — there is no container CI harness today (CI is unit+lint). Listed as a follow-up; the stub test + hardware matrix cover the behaviour.
- Two libseat API points still need a fetch-time confirm (Task 4 note): `SeatEvent` variant names and the `Errno` path/`.0` field.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-28-vt-switching.md`. Before execution, this plan should go through codex review (gpt-5.4-mini) and iterate to convergence per the user's request — in particular the three soft spots and the open DRM re-registration question above.
