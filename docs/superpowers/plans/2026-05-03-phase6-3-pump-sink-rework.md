# Phase 6.3 — Pump / Sink Rework Plan

Consolidate host X11 connections and implement a unified dispatch loop.

## Step 1: `BackendEventSink` and `OriginContext`
Define the core structures for async event and error delivery.

- [x] Add `OriginContext` to `crates/yserver-core/src/backend/mod.rs`.
- [x] Add `BackendEvent` and `BackendEventSink` trait to `crates/yserver-core/src/backend/trait_def.rs`.
- [x] Add `set_event_sink` to the `Backend` trait.
- [x] Update all `Backend` trait methods to accept `origin: Option<OriginContext>`.
- [x] Update all call sites in `nested.rs` to pass `Some(OriginContext { ... })`.

## Step 2: Sequence Map and Background Dispatcher
Implement the robust tracker and the event-draining thread.

- [x] Create `crates/yserver-core/src/host_x11/sequence_map.rs`.
- [x] Implement `SequenceMap<T>` with a sliding window of ~32k entries.
- [x] Implement 16-bit to 64-bit sequence promotion logic.
- [x] Implement the background `dispatch_thread` in `HostX11Backend`.
- [x] Use a `crossbeam-channel` (or similar) to send `BackendEvent` to the sink.

## Step 3: `HostX11Backend` Refactor
Implement synchronized reply waiting and unified mask management.

- [x] Add `PendingReplies` and `PendingErrors` map with `Condvar` synchronization.
- [x] Implement `wait_for_reply(u64) -> Result<Vec<u8>, HostError>`.
- [x] Update all trait methods in `trait_impl.rs` to record context and use `wait_for_reply`.
- [x] Implement `update_host_event_mask(host_xid, mask_bit, enabled)` helper using a backend-owned registry.

## Step 4: Connection Merge (The "Big Flip")
Remove the separate pump connections and unify event selection.

- [x] Remove `HostInputPump` and its separate connection. (Step 4 commit:
      `HostInputPump` deleted. `HostInputPumpHandle` retained as a thin
      wrapper that delegates to `Backend::register_top_level` /
      `register_subwindow` / `unregister_host_window`. Per-client kb
      pump deleted; per-client keyboard forwarders now read from a
      crossbeam channel fed by the merged dispatcher. Close-watcher
      thread deleted; `HostEvent::Closed` arrives via the dispatcher.)
- [x] Update `HostX11Backend` to select `PointerEventMask | ExposureMask` on the container during init.
      (Step 4: container `CreateWindow` value-list now sets the unioned
      `CONTAINER_EVENT_MASK` = KeyPress | KeyRelease | ButtonPress |
      ButtonRelease | EnterWindow | LeaveWindow | PointerMotion |
      Exposure | StructureNotify so the merged dispatcher sees every
      class of event we care about on one connection.)
- [x] Remove per-client keyboard pump connection logic in `nested.rs`.
      (Step 4: `nested.rs:612` no longer opens a host connection per
      client; instead it calls `Backend::add_key_subscriber(tx)` and
      hands the matching `Receiver` to `spawn_keyboard_forwarder`.)
- [~] Implement `Backend::select_keyboard_events` to manage focus masks on the container.
      (Step 4: deferred — instead of toggling kb event-masks per focus
      change, the container holds the union mask permanently and the
      merged dispatcher fans key events out to *every* client's
      forwarder, which applies its own focus state. This matches the
      pre-Step-4 shape where every kb pump received every key event
      regardless of focus, and avoids the lifetime cost of a focus
      tracker on the backend. If a future need surfaces — e.g. true
      "mute non-focused clients" semantics — `select_keyboard_events`
      can be added without breaking the current contract.)

## Step 5: Sink Integration
Wire the backend events back into the nested server.

- [x] Implement `BackendEventSink` in `crates/yserver-core/src/server.rs`.
- [x] Implement a server-side loop that drains the backend channel and drives the fanouts.
- [x] Wire the sink to the `HostX11Backend` in `nested.rs`.

## Step 6: Cleanup and Validation
Remove obsolete code and verify correctness.

- [x] Remove `sync_main_connection` and other multi-connection fences.
      (Step 6: `sync_main_connection` deleted from `host_x11/request.rs`;
      the in-line GetInputFocus round-trip after `create_subwindow` is
      gone. With the merged main connection the follow-up
      `ChangeWindowAttributes` selecting `ExposureMask` travels on the
      same socket as the `CreateWindow`, so wire ordering is naturally
      sequential — no fence needed. Also dropped `HostInputPumpHandle`
      from `host_x11/pump.rs` and inlined every `nested.rs` call site
      onto `Backend::register_top_level` / `register_subwindow` /
      `unregister_host_window`.)
- [x] Remove the `reply_buffer` from `HostX11Backend`.
      (Step 6 audit: the bare `reply_buffer: Vec<HostResponse>` field
      was already gone — replaced by `PendingReplies` + `PendingErrors`
      Condvar maps in Step 3. The remaining `read_until_response` and
      `stash_or_log_response` helpers are retained as synchronous
      fallbacks for the init phase only — `init_render`, `init_xkb`,
      and `query_extension_opcode` run during `open_from_env` *before*
      the dispatcher is spawned, and synchronous reads are the only
      option there. The doc comments on `wait_for_reply` make the
      init-only invariant explicit.)
- [x] Run the Phase 3.x WM smoke tests.
      (Step 6: wmaker, fvwm3, openbox screenshots captured at
      `/tmp/phase6-3-{wmaker,fvwm3,openbox}.png`. All three render
      chrome, frames, and clients correctly. Zero panics, zero ERRORs;
      WARNs are pre-existing async host errors carried with
      `OriginContext`. XTEST-driven keyboard validation deferred to
      the user on real hardware — bwrap sandbox lacks XTEST.)
- [x] Verify that async errors are logged/handled correctly via `OriginContext`.
