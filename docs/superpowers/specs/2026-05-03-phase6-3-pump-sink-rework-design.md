# Phase 6.3 — Pump / Sink Rework Design

Goal: Fold today's three host X11 connections (main + `HostInputPump` + per-client kb pumps) into one, behind a clean `Backend::dispatch()` / `BackendEventSink` shape on the trait.

## 1. Problem Statement

The current architecture uses multiple X11 connections to the host:
1.  **Main Connection (`HostX11Backend`)**: Used for all requests (`CreateWindow`, `Draw`, etc.). Uses blocking reads for replies.
2.  **Input Pump (`HostInputPump`)**: A separate connection used solely for reading events (`Pointer`, `Expose`, `Configure`).
3.  **Per-client Keyboard Pumps**: Each client gets a dedicated connection to select `KeyPress/Release` on the host container.

### Issues:
*   **Race Conditions**: Requests on the main connection and event selections on the pump connections can race, requiring the "sync fence" (`GetInputFocus`) hack.
*   **Wasted Resources**: Multiple sockets and threads per client are inefficient.
*   **Async Errors**: Errors for void requests (most drawing ops) are currently ignored or cause the connection to drop because we don't have a way to attribute them back to the original nested request once they arrive asynchronously.
*   **Sequence Wrap**: X11's 16-bit sequence number wraps every 65k requests. Under heavy load, we could misattribute a late error to a new request with the same 16-bit sequence.

## 2. Proposed Architecture

### 2.1 Single Connection

`HostX11Backend` will own a single `UnixStream`. All requests and event-mask selections will happen over this stream.

### 2.2 Unified Dispatch Loop

The `Backend` trait will be extended to support an asynchronous dispatch model.

```rust
pub enum BackendEvent {
    X11(yserver_protocol::x11::Event),
    // Future: DrmPageFlip, LibinputEvent
}

pub trait BackendEventSink: Send + Sync {
    fn handle_event(&self, event: BackendEvent);
    fn handle_error(&self, origin: OriginContext, error: yserver_protocol::x11::Error);
}

#[derive(Clone, Copy, Debug)]
pub struct OriginContext {
    pub client_id: ClientId,
    pub nested_seq: u16,
    pub opcode: u8,
}

pub trait Backend: Send {
    // ... existing resource methods updated to take Option<OriginContext> ...

    /// Register a sink for events and async errors.
    fn set_event_sink(&mut self, sink: Arc<dyn BackendEventSink>);
}
```

### 2.3 `HostX11Backend` Implementation Details

#### 2.3.1 Background Dispatch Thread
To ensure events are always drained even when no client request is pending a reply, `HostX11Backend` will spawn a dedicated background thread on initialization.

*   **Responsibility**: Owns the `Read` half of the `UnixStream`. Blocks on `read()`, promotes 16-bit sequences to 64-bit, and classifies responses.
*   **Decoupling (Deadlock Avoidance)**: To avoid lock inversion (`Backend` -> `ServerState` vs. `ServerState` -> `Backend`), the background thread will **never** call directly into the `ServerState`. Instead, it will push events and async errors onto a **thread-safe channel**.
*   **Reply Handling**: For requests that require a reply, the background thread will push the reply bytes into a `PendingReplies` map protected by a dedicated `Mutex` (not the main `Backend` mutex) and notify a `Condvar`. The synchronous trait method on the main thread will wait on this `Condvar`.

#### 2.3.2 64-bit Sequence Tracking and Origin Map
*   The backend maintains `next_seq_full: u64`.
*   A `SequenceMap<Option<OriginContext>>` stores the context for every issued request.
*   The background thread uses the latest known reply/error sequence to safely promote the 16-bit wire sequence of incoming events/errors/replies.
*   **Error Handling**: If an error arrives for a sequence that was expected to produce a reply, the `SequenceMap` entry will be used to resolve the waiter with an `Err` result rather than `Ok(reply)`.

#### 2.3.3 Unified Mask Registry
To prevent `ChangeWindowAttributes` (CWA) from clobbering masks (e.g., focus change clobbering pointer masks), `HostX11Backend` will maintain a `HashMap<u32, u32>` (host_xid -> combined_mask).
*   Any request to update a mask for a window will go through a helper that ORs the new requirement into the registry and issues a single CWA with the full state.

### 2.4 Dissolving the Pumps

*   **Keyboard Pumps**: The `ServerState` will track the "focused client". When a client gains focus, `nested.rs` will call `backend.select_keyboard_events(container, true)`. The backend will manage the combined mask on the container host window.
*   **Input Pump**: Replaced by the background thread. The `xid_map` (host_xid -> nested_id) will be moved into the `HostX11Backend` (or remain shared via `Arc<Mutex>`) so the background thread can include the `ResourceId` in the `BackendEvent` before pushing to the channel.

## 3. Implementation Plan

### Step 1: `BackendEventSink` and `OriginContext`
Define the traits and structs. Update `Backend` trait methods to accept `OriginContext`.

### Step 2: SequenceMap and Channel Dispatcher
Implement the 64-bit promotion logic and the background thread shell.

### Step 3: `HostX11Backend` Refactor
Implement `wait_for_reply` using `Condvar` + `PendingReplies`. Update request handlers to record context.

### Step 4: Merge Connections
Select combined masks on the container. Remove `HostInputPump`.

### Step 5: Sink Integration
Implement the server-side channel consumer that drives the fanouts.

## 4. Risks & Mitigations

*   **Deadlock**: If a request handler blocks on a reply while the `dispatch` loop is also trying to lock something.
    *   *Mitigation*: Ensure the `Backend` mutex is only held briefly or use a separate lock for the sequence map.
*   **Sequence Wrap Edge Case**: A very late error arriving after the 16-bit sequence has wrapped and the original 64-bit entry was purged.
    *   *Mitigation*: Keep a large window (e.g., 32k entries). If an error arrives for an unknown sequence, log it at `DEBUG`.
