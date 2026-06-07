//! Monotonic millisecond timestamp shared between the libinput thread
//! and the KMS backend.
//!
//! X11 event timestamps are 32-bit milliseconds; clients (notably
//! window managers like marco / xfwm) compare them against the
//! server's own notion of "current time" returned by
//! `ServerState::timestamp_now` (used in `UngrabPointer`/
//! `AllowEvents`/`SetInputFocus` time-check arms — Xorg
//! `dix/events.c::ProcUngrabPointer:5152`). Both clocks must share
//! the SAME `Instant` origin or the spec time checks reject
//! legitimate client requests carrying genuine input-event
//! timestamps.
//!
//! Before this redesign `START` was a `LazyLock<Instant>` initialised
//! on the first `server_time_ms()` call — which in practice was the
//! libinput thread's first dispatch, ~1.8 s *after*
//! `ServerState::start_instant`. With the two clocks diverged that
//! far, every input event delivered to clients carried a timestamp
//! ~1.8 s behind `state.timestamp_now()`, and XFCE's menu-close path
//! (UngrabPointer with the saved press timestamp) silently failed
//! the `time >= grabTime` gate for seconds at a stretch.
//!
//! Now: `START` is a `OnceLock<Instant>` that the binary's `run`
//! initialises from `state.start_instant` (`clock::init`) immediately
//! after `ServerState::new`. Subsequent callers see the same Instant
//! both threads agree on. The lazy fallback (`get_or_init(Instant::now)`)
//! exists for test/benchmark binaries that don't call `init` — they
//! get the prior behaviour without coupling to ServerState.

use std::{sync::OnceLock, time::Instant};

static START: OnceLock<Instant> = OnceLock::new();

/// Pin the clock baseline to `start_instant`. Must be called once,
/// right after `ServerState::new`, and before the libinput thread
/// touches `server_time_ms`. If `init` is skipped (test binaries,
/// `recording.rs` smoke), the first `server_time_ms` falls back to
/// a fresh `Instant::now()` — the pre-fix behaviour, just without
/// the cross-thread skew dependency.
pub fn init(start_instant: Instant) {
    let _ = START.set(start_instant);
}

#[must_use]
pub fn server_time_ms() -> u32 {
    let start = START.get_or_init(Instant::now);
    #[allow(clippy::cast_possible_truncation)]
    let ms = start.elapsed().as_millis() as u32;
    ms
}
