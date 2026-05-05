# Single-threaded core — design

## Motivation

Two pieces of state — `ServerState` and the `Backend` (`KmsBackend` /
`HostX11Backend`) — both sit behind `Arc<Mutex<…>>` and are touched by
several threads (per-client request handlers, libinput input thread,
DRM page-flip handler, host-X11 pump). Different paths take the locks
in different orders:

- **Input / DRM / host-X11 pump threads** want backend state first
  (cursor / window geometry / coord translation), then need to deliver
  through `pointer_event_fanout`, which acquires `server.lock()`.
- **Per-client request handlers** want server state first (resource
  lookup, grab tables, atom tables), then need to call into the backend
  for rendering / window ops.

Two locks, taken in opposite directions. Phase 6.6 fixed one inversion
(commit `6f7754b`). Phase 6.7 added crossing-event emission, which
tripled per-motion fanout calls and surfaced a second latent inversion
as a routinely-reproducible e16 freeze. The current state — buffering
pointer events in `pending_pointer_events` and dispatching after the
backend mutex is dropped — papers over the symptom but leaves the
deadlock class intact for any future code path that takes
server-then-backend.

This design eliminates the deadlock class entirely by making the core
single-threaded. All other threads are I/O-only and communicate via a
single `mpsc` channel.

## Architecture

One **core thread** owns:

- `state: ServerState` (no `Arc`, no `Mutex`).
- `backend: Box<dyn Backend>` — `KmsBackend` or `HostX11Backend`,
  with all internal `Mutex`/`Arc<Mutex>` over its own fields stripped.
- `clients: HashMap<ClientId, ClientState>` — write-half of each
  client's `UnixStream`, byte order, sequence counter (plain `u16`),
  event masks. Subsumes `ClientHandle`.

Other threads, all I/O-only:

- **Listener thread**: accepts new connections on the Unix socket,
  splits each `UnixStream` (via `try_clone`), spawns a reader thread
  for it, sends `Message::ClientConnected{ id, writer }` carrying the
  write-half.
- **N per-client reader threads** (one per connected client): blocking
  reads request frames off the read-half, sends
  `Message::Request{ id, header, body }`.
- **Input thread** (KMS only): polls libinput fd, sends
  `Message::HostInput(InputEvent)`.
- **DRM thread** (KMS only): polls drm fd, sends
  `Message::PageFlipReady`.
- **Signal thread**: polls signalfd, sends `Message::Shutdown` on
  SIGINT/SIGTERM.
- **Host-X11 pump thread** (ynest only): replaces `host_x11/pump.rs`'s
  thread; sends `Message::HostX11(HostEvent)`.

All I/O threads share one `mpsc::Sender<Message>`. The core holds the
single `Receiver<Message>` and runs:

```rust
loop {
    match rx.recv() {
        Ok(Message::ClientConnected{id, writer}) => …,
        Ok(Message::Request{id, header, body}) => process_request(…),
        Ok(Message::HostInput(ev))             => process_host_input(…),
        Ok(Message::PageFlipReady)             => backend.on_page_flip_ready(),
        Ok(Message::ClientDisconnected{id, …}) => state.clients.remove(&id),
        Ok(Message::Shutdown) | Err(_)         => break,
    }
}
```

Replies and events flow back via the per-client write-half stored on
the core. Single-threaded ⇒ no mutex on the writer. The X11 wire is
inherently asynchronous (clients demux replies/events by sequence
number and event type), so reader threads never block on the core —
they parse the next request immediately after sending the previous
one.

## Message enum

```rust
pub enum Message {
    ClientConnected { id: ClientId, writer: UnixStream },
    Request         { id: ClientId, header: RequestHeader, body: Vec<u8> },
    ClientDisconnected { id: ClientId, reason: io::Error },
    HostInput(HostInputEvent),
    HostX11Other(HostEvent),
    PageFlipReady,
    Shutdown,
}
```

Backends post events to the same channel rather than going through a
`BackendEventSink` callback. Removes one layer of indirection.

## What goes away

- `Arc<Mutex<ServerState>>` everywhere
- `Arc<Mutex<dyn Backend>>` everywhere
- `lock_server()` helper
- All `host.lock()?` calls in `nested.rs`
- `ClientHandle::writer: Arc<Mutex<UnixStream>>` mutex (just a `UnixStream`)
- `ClientHandle::last_sequence: Arc<AtomicU16>` (just `u16`)
- The per-client keyboard-forwarder thread in `nested.rs` — key events
  flow through `Message::HostInput` like everything else; the core
  fans them out to clients
- `BackendEventSink` trait (replaced by direct `Sender<Message>`)
- `KmsBackend::pending_pointer_events` and
  `drain_pending_pointer_events` — the deadlock-workaround we just
  added becomes unnecessary

## Migration order (single landing)

Compile breakage spans the whole `yserver-core` crate during the
refactor. Accepted as the cost of doing this in one PR.

1. Add `Message` enum + `Sender<Message>` plumbing; stub
   `run_core(state, backend, rx)` looping on `rx` doing nothing yet.
2. Replace per-client `handle_client` with
   `client_reader_thread(stream, id, tx)`. Reader parses one request
   frame at a time, sends `Message::Request`. `ClientConnected`
   carries the write-half.
3. Lift `handle_client`'s opcode `match` into a free
   `process_request(&mut ServerState, &mut dyn Backend, id, header, body)`.
   Mechanically rewrite every `lock_server(server)?` → `state` and
   every `host.lock()?` → `backend`. **Bulk of the diff** —
   hundreds of edit sites in `nested.rs`.
4. Convert KMS input / DRM / signal threads to senders. Strip
   `Arc<Mutex>` from `KmsBackend` field accesses. Delete
   `pending_pointer_events`.
5. Convert `host_x11::pump` to a sender for ynest. Strip `Arc<Mutex>`
   from `HostX11Backend`.
6. Wire `pointer_event_fanout`, `expose_event_fanout`, etc. to take
   `&mut ServerState`; they write directly to
   `state.clients[id].writer`.
7. Delete the per-client keyboard-forwarder thread.
8. Delete `lock_server`, all `Arc<Mutex<ServerState>>` /
   `Arc<Mutex<dyn Backend>>` typedefs. Adjust tests to build
   `ServerState` directly.
9. Delete `BackendEventSink` if step 4-5 had backends post `Message`s
   directly.

## Testing

- The 249 `yserver-core` unit tests must keep passing throughout.
  Most just construct a `ServerState` and call into handlers, so the
  signature change is mechanical for them.
- Smoke matrix after the refactor:
  - ynest: fvwm3, wmaker, e16, GTK3 apps
  - yserver: fvwm3, wmaker, e16
  Same WMs that work today must still work.
- Stress test for the deadlock: extended e16 session with rapid clicks
  across multiple windows, the exact pattern that triggered the
  recent freeze. Must run for ≥5 minutes without locking up.

## Risk

Step 3 — the `process_request` lift — is the bulk of the work and
will leave the workspace in a non-compiling state for some time. Once
it compiles end-to-end, the unit-test suite either tells us we got it
right or there's a long debug tail. Estimate 2-3 days of focused work
for the lift, plus 1-2 days for the surrounding migration steps and
smoke validation.

## Why the alternatives were rejected

- **Strict lock-ordering discipline (backend-before-server, audited)**
  was rejected because the rule has to be hand-enforced — clippy
  can't catch a violation. Every new piece of input or request
  handling is a deadlock-in-waiting until the audit, and the audit
  isn't a permanent fix.
- **Merging the two mutexes** was rejected because it throws away the
  parallelism the input thread was added for (commit `877a399`),
  without making the type story significantly cleaner.
- **Staged refactor** was rejected because stages 2-4 each contain
  their own internal "big-bang" — once you start lifting
  `lock_server(server)?` → `server` across `nested.rs`, you can't
  stop halfway. The intermediate states are no easier to validate
  than the final one. Single landing avoids carrying vestigial
  `Arc<Mutex>` types in the codebase indefinitely.
