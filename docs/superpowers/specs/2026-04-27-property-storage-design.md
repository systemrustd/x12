# Property storage — design

Phase 1 punch-list item: implement real per-window property storage so
`ChangeProperty` / `DeleteProperty` / `GetProperty` work and `PropertyNotify`
is delivered to interested clients.

This is also where shared server state lands. Property semantics are
fundamentally cross-client (window managers ↔ clients), so the per-client
`ClientState` model has to break apart now rather than later.

## Goals

- `ChangeProperty` (Replace / Prepend / Append) updates a real per-window
  property table, with full X11 spec validation (BadWindow, BadAtom,
  BadValue, BadMatch, BadAlloc, BadLength).
- `DeleteProperty` removes a property and emits `PropertyNotify(Deleted)`
  iff the property existed.
- `GetProperty` returns real data, including `AnyPropertyType`, type-mismatch
  semantics, the `delete` flag, and `long_offset` / `long_length` partial
  reads with correct `bytes_after` and format-unit accounting.
- `PropertyNotify` is delivered to every client that selected
  `PropertyChange` on the affected window — including clients other than
  the one issuing the request. Timestamps are real (`Instant::now() -
  server_start`).
- All resource state moves out of `ClientState` into a server-shared
  `ServerState` behind a single `Mutex`. Per-(client, window) event masks
  replace the existing single `Window.event_mask`. Existing event-emission
  sites (Expose, MapNotify, ConfigureNotify, FocusIn/Out) switch to the
  same routing primitive.

## Non-goals

- Selections / clipboard (Phase 2). Selections will share `ServerState` but
  need a separate routing primitive (single-owner direct delivery, not
  mask-based fanout via `subscribers()`).
- `SubstructureRedirect` and `ResizeRedirect` — these are *exclusive*
  selections (one owning client; second selector → `BadAccess`) and
  redirect normal request execution into `MapRequest` / `ConfigureRequest`
  events. They do not fit the broadcast `subscribers()` model and need a
  separate "redirect owner" mechanism. Phase 2.
- WM-specific lifecycle events beyond what falls out of the routing primitive
  this design lands. `DestroyNotify` ships with this work because client
  disconnect needs it; `UnmapNotify` / `ReparentNotify` / `ClientMessage`
  remain Phase 1 punch-list items but stay separate PRs.
- BIG-REQUESTS extension (max property size still 64 MiB; per-request size
  bounded by `maximum_request_length`).
- Big-endian clients (already out of scope for ynest).
- Cross-client visibility of GCs / pixmaps / fonts (their tables become
  server-shared but cross-client access patterns aren't a Phase 1 concern;
  what matters is that windows, atoms, and properties are coherent).
- Atom garbage collection. Atoms are never freed in core X11; this design
  follows that — `AtomTable` grows until server shutdown.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  ynest::run                                                 │
│    ┌─────────────────────────────────────────────────────┐  │
│    │  Arc<Mutex<ServerState>>   (shared by all clients)  │  │
│    │   ├─ atoms:        AtomTable                        │  │
│    │   ├─ resources:    ResourceTable                    │  │
│    │   │     ├─ windows  (each Window owns properties)  │  │
│    │   │     ├─ pixmaps                                  │  │
│    │   │     ├─ gcs                                      │  │
│    │   │     ├─ fonts                                    │  │
│    │   │     └─ cursors                                  │  │
│    │   │       (every resource carries owner: ClientId)  │  │
│    │   ├─ clients:      HashMap<ClientId, ClientHandle>  │  │
│    │   │     └─ ClientHandle { writer, byte_order,       │  │
│    │   │                       last_sequence,            │  │
│    │   │                       event_masks }             │  │
│    │   ├─ id_allocator: hands out non-overlapping        │  │
│    │   │                resource-ID ranges per client    │  │
│    │   └─ start_instant (PropertyNotify timestamps)      │  │
│    └─────────────────────────────────────────────────────┘  │
│                                                             │
│  per-client thread:                                         │
│    ClientState { client_id, byte_order, sequence, ... }     │
│    (no resource tables — those live in ServerState now)     │
└─────────────────────────────────────────────────────────────┘
```

Lock-discipline rule: never hold the `ServerState` mutex across a
host-X11 forwarding call or a per-client-writer write. Pattern is:

```
lock → mutate state → collect work to do (writers + byte buffers)
  → drop lock → perform I/O.
```

### Writer-lock discipline

Today `nested.rs:167-179` locks the per-client `Arc<Mutex<UnixStream>>`
once at request entry and holds it for the entire handler. With
cross-client event delivery, a request can produce events whose
subscriber set includes the issuing client itself — `emit_window_event`
would then try to re-lock the same writer (std `Mutex` is non-reentrant)
and deadlock.

This design therefore requires that the per-handler outer writer lock
goes away. Each call site that writes to a client's stream (reply,
event, error) acquires the writer lock locally, writes, and releases.
Equivalently: the request loop never holds a `writer.lock()` across
calls that may emit events. This refactor lands as stage 0 of the
implementation plan (a precondition), before any cross-client event
delivery is wired up. Single-client correctness is preserved because
each client still has its own writer-mutex and only its own thread
reads requests.

A consequence: `emit_window_event` is safe to call with the issuing
client among its subscribers, including `ChangeProperty` /
`DeleteProperty` / `GetProperty(delete=1)` paths.

### Module layout (`crates/yserver-core/src`)

- `server.rs` *(new)* — `ServerState`, `ClientHandle`, `IdAllocator`, the
  `subscribers(window, mask_bit)` query.
- `properties.rs` *(new)* — pure types and helpers: `PropertyValue`,
  `PropertyFormat`, `ChangeMode`, `apply_change`, `slice_for_get`. No
  locking, no I/O.
- `resources.rs` — keep, but `ResourceTable` becomes a struct held inside
  `ServerState`. `Window` gains `properties: HashMap<AtomId, PropertyValue>`
  and `owner: ClientId`. *Every* resource type (`Window`, `Pixmap`, `Gc`,
  `Font`, `Cursor`) gains an `owner: ClientId` field, since under shared
  state none of them die with the originating thread. The existing
  `event_mask: u32` field on `Window` is removed in favor of per-(client,
  window) masks on `ClientHandle`.
- `nested.rs` — every handler that touched `ClientState`'s resource tables
  takes `&Mutex<ServerState>` and locks for its duration; all event
  emission goes through one routing helper.
- `yserver-protocol/src/x11.rs` — request parsers for ChangeProperty /
  DeleteProperty / GetProperty; replace the empty `write_get_property_reply`
  with one that takes a real `GetPropertyReply`; `write_property_notify_event`;
  X11 error-code constants.

## Components

### `properties.rs`

```rust
pub struct PropertyValue {
    pub r#type: AtomId,
    pub format: PropertyFormat,
    pub data: Vec<u8>,        // length always a multiple of format.bytes()
}

pub enum PropertyFormat { F8, F16, F32 }
impl PropertyFormat {
    pub fn from_protocol(v: u8) -> Option<Self>;  // 8|16|32
    pub fn bytes(self) -> usize;                  // 1|2|4
    pub fn protocol_value(self) -> u8;
}

pub enum ChangeMode { Replace, Prepend, Append }
impl ChangeMode { pub fn from_protocol(v: u8) -> Option<Self>; }

pub enum ChangePropertyError { BadValue, BadMatch, BadAlloc }

pub fn apply_change(
    existing: Option<&PropertyValue>,
    mode: ChangeMode,
    new_type: AtomId,
    format: PropertyFormat,
    data: &[u8],
) -> Result<PropertyValue, ChangePropertyError>;

pub struct GetPropertySlice<'a> {
    pub r#type: AtomId,           // 0 == None
    pub format: u8,                // 0 if type == 0
    pub bytes_after: u32,
    pub value: &'a [u8],
}

pub fn slice_for_get(
    property: Option<&PropertyValue>,
    requested_type: AtomId,        // 0 = AnyPropertyType
    long_offset: u32,              // 4-byte units
    long_length: u32,              // 4-byte units
) -> Result<GetPropertySlice<'_>, ChangePropertyError /* BadValue */>;

pub const MAX_PROPERTY_BYTES: usize = 64 * 1024 * 1024;
```

### `server.rs`

```rust
pub struct ServerState {
    pub atoms: AtomTable,
    pub resources: ResourceTable,
    pub clients: HashMap<ClientId, ClientHandle>,
    pub id_allocator: IdAllocator,
    pub start_instant: Instant,
}

pub struct ClientHandle {
    pub writer: Arc<Mutex<UnixStream>>,
    pub byte_order: ClientByteOrder,
    pub last_sequence: Arc<AtomicU16>,
    pub event_masks: HashMap<ResourceId, u32>,
}

pub struct IdAllocator { next_base: u32 }
// Hands out (resource_id_base, resource_id_mask) per client.
// Server-owned IDs (ROOT_WINDOW=0x100, ROOT_COLORMAP=0x101, ROOT_VISUAL=0x102)
// occupy the range below 0x0010_0000 and are reserved.
// First client base = 0x0010_0000, subsequent clients += 0x0010_0000.
// Per-client mask = 0x000F_FFFF (1 MiB IDs per client; ample for Phase 1).
// `allocate()` returns Err on exhaustion; the request loop closes the
// listener gracefully when no further base would fit in u32.
//
// `validate_owned(id, client)` returns true iff `(id & !mask) == base`
// for that client's range — used by Create* handlers to enforce that
// new resource IDs land in the caller's allocated range.

impl ServerState {
    pub fn timestamp_now(&self) -> u32;  // (now - start) ms truncated

    pub fn subscribers(&self, window: ResourceId, mask_bit: u32)
        -> Vec<EventTarget>;
}

pub struct EventTarget {
    pub writer: Arc<Mutex<UnixStream>>,
    pub byte_order: ClientByteOrder,
    pub last_sequence: Arc<AtomicU16>,
}
```

The routing helper (lives in `server.rs` alongside `subscribers`):

```rust
fn emit_window_event(
    state: &Mutex<ServerState>,
    window: ResourceId,
    mask_bit: u32,
    encode: impl Fn(&mut Vec<u8>, SequenceNumber, ClientByteOrder),
) {
    // Snapshot subscribers under the ServerState lock; release before any I/O.
    let targets = {
        let guard = state.lock().unwrap();
        guard.subscribers(window, mask_bit)
    };
    // Per-target writer locks are non-reentrant and brief; the request loop
    // does NOT hold any writer lock at this point (see "Writer-lock
    // discipline"), so locking the issuing client's own writer here is safe.
    for target in targets {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        encode(&mut buf, seq, target.byte_order);
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }
}
```

### Wire-format additions (`yserver-protocol/src/x11.rs`)

```rust
pub struct ChangePropertyRequest {
    pub mode: u8, pub window: ResourceId,
    pub property: AtomId, pub r#type: AtomId,
    pub format: u8, pub data: Vec<u8>, pub length: u32,
}
pub fn change_property_request(body: &[u8]) -> Option<ChangePropertyRequest>;

pub struct DeletePropertyRequest { pub window: ResourceId, pub property: AtomId }
pub fn delete_property_request(body: &[u8]) -> Option<DeletePropertyRequest>;

pub struct GetPropertyRequest {
    pub delete: bool, pub window: ResourceId,
    pub property: AtomId, pub r#type: AtomId,
    pub long_offset: u32, pub long_length: u32,
}
pub fn get_property_request(body: &[u8]) -> Option<GetPropertyRequest>;

pub struct GetPropertyReply<'a> {
    pub format: u8, pub r#type: AtomId,
    pub bytes_after: u32, pub value_len: u32,
    pub value: &'a [u8],
}
pub fn write_get_property_reply(w: &mut impl Write, seq: SequenceNumber,
                                reply: GetPropertyReply<'_>) -> io::Result<()>;

pub fn write_property_notify_event(w: &mut impl Write, seq: SequenceNumber,
                                   window: ResourceId, atom: AtomId,
                                   timestamp: u32, deleted: bool) -> io::Result<()>;

pub mod error {
    pub const BAD_REQUEST:    u8 = 1;
    pub const BAD_VALUE:      u8 = 2;
    pub const BAD_WINDOW:     u8 = 3;
    pub const BAD_ATOM:       u8 = 5;
    pub const BAD_MATCH:      u8 = 8;
    pub const BAD_ALLOC:      u8 = 11;
    pub const BAD_ID_CHOICE:  u8 = 14;
    pub const BAD_LENGTH:     u8 = 16;
}
```

## Data flow

### Server startup (`ynest::run`)

1. Build `ServerState` (empty `AtomTable`, `ResourceTable` with the root
   window/colormap/visual at the existing reserved IDs `0x100..0x103`,
   empty clients, `id_allocator` with `next_base = 0x0010_0000`,
   `start_instant = Instant::now()`).
2. Wrap in `Arc<Mutex<_>>` and clone into each accepted client thread.

### Client connect (`handle_client`)

1. Lock state → `id_allocator.allocate()` → use the returned
   `(resource_id_base, resource_id_mask)` in the setup_success reply.
   The first client gets `(0x0010_0000, 0x000F_FFFF)`; the n-th gets
   `(0x0010_0000 * n, 0x000F_FFFF)`. Server-owned IDs (root window etc.)
   sit below the first client range and never collide.
2. Register `ClientHandle { writer, byte_order, last_sequence,
   event_masks: HashMap::new() }` in `clients[client_id]`.
3. Drop lock, enter request loop.

### Resource creation (`CreateWindow` / `CreatePixmap` / `CreateGC` / `OpenFont` / `CreateGlyphCursor`)

For every request that allocates a new resource ID:

1. Validate the proposed ID via
   `id_allocator.validate_owned(new_id, client_id)`.
   Outside the client's range → emit `BadIDChoice` (code 14, bad_value
   = new_id).
2. Lock state. If a resource with that ID already exists (in *any*
   table — windows, pixmaps, gcs, fonts, cursors), drop lock, emit
   `BadIDChoice`. Otherwise insert the new resource with
   `owner = client_id` and proceed with the rest of the handler's
   normal validation (parent BadWindow, drawable BadDrawable, etc.).

### `ChangeProperty`

1. Parse body → `ChangePropertyRequest` (parser returns `None` on
   length-out-of-range → emit `BadLength`).
2. Validate `mode` (BadValue), `format` (BadValue),
   `data.len() == length * format.bytes()` (BadLength).
3. Lock state:
   - Lookup window. Missing → drop lock, emit `BadWindow`.
   - Verify `property` and `type` atoms exist (predefined or in
     `AtomTable`). Missing → drop lock, emit `BadAtom`.
   - Call `apply_change(window.properties.get(property), mode, type,
     format, &data)`:
     - `Replace` ignores existing.
     - `Prepend`/`Append` require existing.type == new.type and
       existing.format == new.format → else `BadMatch`. Concatenate.
     - Total length > `MAX_PROPERTY_BYTES` → `BadAlloc`.
   - Insert into `window.properties[property]`.
   - Stamp `timestamp = state.timestamp_now()`.
   - Collect `subscribers(window, PROPERTY_CHANGE_BIT)`.
4. Drop lock. For each subscriber, write `PropertyNotify(state=NewValue)`.

### `DeleteProperty`

1. Parse → `DeletePropertyRequest`.
2. Lock state:
   - Validate window (BadWindow), atom (BadAtom).
   - `let existed = window.properties.remove(property).is_some();`
   - If `existed`: collect subscribers + timestamp.
3. Drop lock. If existed, emit `PropertyNotify(state=Deleted)` to subscribers.

### `GetProperty`

1. Parse → `GetPropertyRequest`.
2. Lock state:
   - Validate window (BadWindow), property atom (BadAtom). For `type`,
     `0` means AnyPropertyType — skip atom validation when type == 0.
   - Look up `window.properties.get(property)`.
   - `slice_for_get(..)`:
     - Property absent → `{ type: 0, format: 0, bytes_after: 0, value: &[] }`.
     - Type set + mismatch → returns `(existing.type, existing.format)` with
       empty value, `bytes_after = full byte length`.
     - Type matches (or AnyPropertyType): treat `long_offset` and
       `long_length` as `u64` (or use `checked_mul` on `u32`) when
       multiplying by 4, since either can otherwise overflow.
       `BadValue` iff `(offset as u64) * 4 > (len as u64)`. Note this is
       strict greater-than: `offset*4 == len` is the valid end-of-property
       case (returns empty slice with `bytes_after = 0`, and is exactly
       the condition that lets a `delete=1` request fire).
       Otherwise `len_to_return = min(remaining, (long_length as u64) * 4)`,
       `bytes_after = remaining - len_to_return`. `len_to_return` is
       rounded down to a multiple of `format.bytes()` for the reported
       slice; `value_len` in the reply is reported in *format units*
       (8/16/32-bit elements), while the reply's wire-level `length`
       header is the padded payload size in 4-byte units.
   - If `delete=1` AND type matched AND `bytes_after == 0`:
     remove the property and queue a `Deleted` PropertyNotify; collect
     subscribers.
   - Build owned `GetPropertyReply` (copy slice into Vec) so the lock can
     drop before I/O.
3. Drop lock. Write reply on this client's writer; if delete fired, write
   PropertyNotify to subscribers.

### `ChangeWindowAttributes` / `CreateWindow` (event_mask path)

Today writes `Window.event_mask = mask`. New behavior:
`clients[client_id].event_masks.insert(window, mask)` (or `remove(&window)`
if mask == 0). This is the existing-bug fix that PropertyNotify forces:
last-writer-wins becomes per-client.

### `GetWindowAttributes` and screen `current_input_masks`

With per-(client, window) masks, the protocol-visible mask fields are
derived rather than stored:

- `your_event_mask` = `clients[requesting_client].event_masks.get(window).copied().unwrap_or(0)`.
- `all_event_masks` = bitwise-OR of every `event_masks[window]` across
  all clients (i.e. the union of every selection on this window).
- The screen's `current_input_masks` (returned in `setup_success`) is
  the union of all clients' masks on the root window, computed at
  setup-reply time. It changes as clients connect/disconnect; we compute
  it on demand rather than caching.

These are pure read paths: they take the `ServerState` lock, compute
the union, and drop it before writing the reply.

### `DestroyWindow`

In addition to the existing recursive destruction in `ResourceTable`,
the handler must:

1. While locked: collect `subscribers(window, StructureNotify)` and
   `subscribers(parent, SubstructureNotify)`. Recursively destroy the
   subtree (depth-first; emit per-window). Snapshot the destroyed-window
   IDs.
2. While still locked: walk every other client's `event_masks` and
   remove entries keyed on any destroyed window ID. Without this, stale
   subscriptions survive into ID reuse and mis-route future events.
3. Drop lock. Emit `DestroyNotify` to each collected subscriber set.

This is the same shape as the disconnect path; in fact disconnect
calls into this primitive once per top-level window the client owned.

### Client disconnect

The disconnecting client's thread runs cleanup. All state mutation
happens under the `ServerState` lock; all event I/O happens *after*
the lock is released.

1. Lock state.
2. Collect all windows where `owner == client_id`. Compute the
   destruction order (depth-first: children before parents). For each
   window in that order:
   - Snapshot `(window_id, parent_id, StructureNotify subscribers,
     SubstructureNotify subscribers on parent)` into a pending list.
   - Remove the window (and its properties) from `ResourceTable`.
   - Walk every other client's `event_masks` and drop entries keyed on
     this window ID.
3. Free every other resource owned by this client:
   - Pixmaps (`owner == client_id`): drop from table.
   - GCs: drop from table.
   - Fonts: drop from table *and* enqueue a host-side `CloseFont` for
     each `host_xid` (executed after lock release alongside the host
     I/O for events).
   - Cursors: drop from table.
4. Remove this client from `clients` (which also drops its
   `event_masks`).
5. Drop lock.
6. For each entry in the pending destroy list, emit `DestroyNotify` to
   the collected StructureNotify (on the window) and SubstructureNotify
   (on the parent) subscribers. `DestroyNotify` bit values:
   `StructureNotify = 0x0002_0000`, `SubstructureNotify = 0x0008_0000`.
7. Run the queued host-side `CloseFont`s.

Race notes: a subscriber whose own thread also disconnected concurrently
will simply have its writer write fail — already handled silently per
the lock-discipline rule. The disconnect path itself never races with
itself for a given client (only that client's thread runs it).

## Errors

X11 error wire format already exists at `x11.rs:376` (`write_error`).
Helper for emission:

```rust
fn emit_x11_error(writer: &mut UnixStream, sequence: SequenceNumber,
                  code: u8, bad_value: u32, major_opcode: u8) -> io::Result<()> {
    x11::write_error(writer, sequence, code, bad_value, 0, major_opcode)
}
```

Error matrix:

| Condition | Code | bad_value |
|---|---|---|
| `mode ∉ {0,1,2}` (ChangeProperty) | BadValue | `mode as u32` |
| `format ∉ {8,16,32}` | BadValue | `format as u32` |
| `length * (format/8) != data.len()` | BadLength | `0` |
| Request body malformed | BadLength | `0` |
| Window ID unknown | BadWindow | `window.0` |
| Property/type atom unknown (type ≠ 0 in GetProperty) | BadAtom | `atom.0` |
| Prepend/Append type or format mismatch | BadMatch | `window.0` |
| Cumulative size > 64 MiB | BadAlloc | `0` |
| GetProperty `long_offset * 4 > existing length` (strict >) | BadValue | `long_offset` |
| `Create*` ID outside caller's allocated range | BadIDChoice | `new_id` |
| `Create*` ID already in use (any resource table) | BadIDChoice | `new_id` |

Validation runs before mutation. `apply_change` and `slice_for_get` return
`Err` rather than half-updating. On error: error reply, no other side
effects (no PropertyNotify, no state change).

Rust-level errors:

- Mutex poisoned → propagate as `io::Error(BrokenPipe)`, terminate the
  thread (matches `nested.rs:168`).
- `subscribers` failing to find a registered client (race with disconnect):
  silently skip.
- A subscriber's `writer.lock()` failure or `write_all` error: log at
  `debug` and skip. The offending client's own thread will clean up on
  next read failure.

Out of scope: `BadAccess` for cross-client property writes (X11 doesn't
require it for properties), selection-related errors (Phase 2).

## Testing

### Unit tests — example-based

`apply_change`:

- Replace on empty → returns new value with given type/format/data.
- Replace on existing with different type/format → returns new value
  (Replace ignores existing).
- Append/Prepend on empty are equivalent to Replace.
- `apply_change` exactly at `MAX_PROPERTY_BYTES` and at MAX+1.

`slice_for_get`:

- Property absent → type=0, format=0, value empty, bytes_after=0.
- Type mismatch → returns existing type/format, value empty,
  bytes_after = full byte length.
- format=16, long_length=1 → returns 4 bytes, `value_len` reported as 2.
- format=8, long_length=1 → returns 4 bytes, `value_len` reported as 4.

`subscribers()`:

- Returns clients with the bit set; omits clients with mask=0; omits
  clients that selected on a different window.
- Disconnecting a client removes its `event_masks` entries and its
  `ClientHandle`; subsequent `subscribers` calls don't return it.
- Destroying a client's window: properties go with it; other clients'
  `event_masks` entries pointing at that window are dropped.

### Unit tests — property-based (proptest)

`apply_change`:

- **Replace round-trip**: ∀ (type, format, data ≤ MAX),
  `apply_change(_, Replace, t, f, d).data == d`,
  `.type == t`, `.format == f` regardless of `existing`.
- **Append/Prepend additivity**: ∀ matching type/format and
  `existing.len() + new.len() ≤ MAX`,
  `apply_change(Some(v), Append, ...).data.len() == v.data.len() + new.len()`.
  Same for Prepend.
- **Concatenation order**: ∀ matching, Append produces `v.data ++ new`,
  Prepend produces `new ++ v.data` (byte-for-byte).
- **Type/format mismatch always BadMatch**: ∀ (v, mode ∈ {Append, Prepend},
  t' ≠ v.type) → `Err(BadMatch)`. Same for format.
- **MAX boundary**: `existing.len() + new.len() ≤ MAX ⇒ Ok`;
  `> MAX ⇒ Err(BadAlloc)`. Probe at the edge.

`slice_for_get`:

- **Read-all recovers original**: ∀ `p` with type T,
  `slice_for_get(Some(&p), T, 0, u32::MAX/4).value` equals `p.data`
  (truncated to multiple of `format.bytes()`; properties always have
  aligned data).
- **Chunked reads reassemble**: ∀ `p`, repeatedly call with
  `long_offset` advanced by previous `value.len()/4`, until
  `bytes_after == 0`. Concatenation of returned slices == `p.data`.
- **`value_len` in format units**: ∀ valid call,
  `value_len * format.bytes() == value.len()`.
- **`bytes_after + value.len() == remaining-from-offset`**: invariant on
  every successful return.
- **AnyPropertyType matches everything**: ∀ p,
  `slice_for_get(Some(&p), 0, 0, u32::MAX/4).type == p.type`.
- **Type mismatch returns metadata, not data**: ∀ p, T' ∉ {p.type, 0} →
  `value` empty, `bytes_after == p.data.len()`, `type == p.type`.
- **Offset past end is BadValue**: ∀ p, offset where
  `offset * 4 > p.data.len()` → `Err(BadValue)`.

`IdAllocator`:

- **Pairwise non-overlap**: ∀ N ∈ 1..256, ranges produced by N
  consecutive `allocate()` calls are pairwise disjoint.
- **Mask covers exactly assigned bits**: each `(base, mask)` satisfies
  `base & mask == 0`.
- **Above server-owned IDs**: every allocated `base` is `>= 0x0010_0000`,
  i.e. strictly above the reserved root-window/colormap/visual range.
- **Validation round-trip**: ∀ allocated `(base, mask)` and any `id` in
  `base..=(base|mask)`, `validate_owned(id, client) == true`. ∀ `id`
  outside that range, `validate_owned(id, client) == false`.

Wire-format round-trips:

- **`ChangePropertyRequest`**: ∀ valid (mode ∈ 0..=2, format ∈ {8,16,32},
  length ≤ small bound, data of correct length, atoms, window),
  `parse(encode(req)) == Some(req)`.
- **`GetPropertyRequest`**: same.
- **`GetPropertyReply`**: ∀ valid reply, byte-checking fixed-offset fields
  reproduces inputs. Two specific invariants the test must assert:
  the reply's wire `length` field equals
  `((value.len() + 3) / 4)` (padded payload in 4-byte units), and
  `value_len` equals `value.len() / format.bytes()` (format units).
- **`PropertyNotify` event**: ∀ inputs, encoded buffer is exactly 32 bytes
  with the spec-defined byte/word layout.

`proptest` config: 256 cases default, 1000 for the chunked-reassembly
prop. Add `proptest` to `[dev-dependencies]` of `yserver-core` and
`yserver-protocol`. No runtime dependency.

### Manual integration smoke

Documented in `docs/status.md` once it works:

```sh
# terminal 1
DISPLAY=:0 cargo run --bin ynest -- 42

# terminal 2
DISPLAY=:42 xterm &
xdotool search --name xterm    # → window id, e.g. 0x100002
xprop -display :42 -id 0x100002 -f FOO 8s -set FOO "hello"
xprop -display :42 -id 0x100002 FOO
# → FOO(STRING) = "hello"

# cross-client PropertyNotify
xev -display :42 -id 0x100002 &        # selects PropertyChange
xprop -display :42 -id 0x100002 -set FOO "world"
# xev should print:
#   PropertyNotify event ... atom = FOO ... state PropertyNewValue
```

The cross-client `xev` check is the single most valuable smoke test —
it proves the per-(client, window) event-mask model and shared
`ServerState` are wired up correctly.

### Skipped (deliberately)

- Automated end-to-end client tests — defer until enough features
  amortize the harness cost.
- Stress / fuzz beyond the proptest cases above.
- TDD specifics — those go in the implementation plan.

## Implementation staging

This design intentionally bundles four concerns (shared `ServerState`,
per-(client, window) event masks, property storage, and per-resource
owner tracking) plus a tag-along (`DestroyNotify` on disconnect/
DestroyWindow). They are bundled because property correctness depends
on each of the others — but they decompose into sequenced stages that
the implementation plan should respect:

0. **Writer-lock refactor (precondition).** Drop the per-handler
   `writer.lock()` hold in `nested.rs` (`nested.rs:167-179` today).
   Each call site that writes to a client's stream — replies, events,
   errors — locks the writer locally, writes, releases. No external
   behavior change; this is a pure refactor that unblocks every later
   stage by making `emit_window_event` safe for the issuing client.
1. **`ServerState` skeleton + `IdAllocator` + per-resource owner.**
   Move `ResourceTable` and `AtomTable` into a shared
   `Arc<Mutex<ServerState>>`. Add `owner: ClientId` to every resource
   type (Window, Pixmap, Gc, Font, Cursor). Switch every handler that
   uses them to lock for the duration of its update. Hand out
   non-overlapping resource-ID ranges per client (base ≥ 0x0010_0000).
   Add `BadIDChoice` validation to every `Create*` handler. Add
   client-disconnect cleanup that walks all five resource tables. No
   behavior change visible to single-client scenarios.
2. **Client registry + per-(client, window) event masks +
   `DestroyWindow` cleanup.** Register `ClientHandle` on connect;
   deregister on disconnect. Move existing event-emission sites
   (Expose, MapNotify, ConfigureNotify, FocusIn/Out) to the new
   `subscribers()` + `emit_window_event` primitive. Update
   `GetWindowAttributes` and `current_input_masks` to compute their
   replies from per-client masks. Add `event_masks` cleanup on
   `DestroyWindow`. Behavior should be unchanged for the existing
   single-client case but architecturally correct for multi-client.
3. **`PropertyValue`, `apply_change`, `slice_for_get` (pure).** Land
   `properties.rs` with full unit + proptest coverage. No wiring yet.
4. **Wire-format additions + handler implementations.** Implement
   parsers, reply/event encoders, and the `ChangeProperty` /
   `DeleteProperty` / `GetProperty` request handlers, including X11
   error emission. PropertyNotify integrates via the routing primitive
   from stage 2.
5. **`DestroyNotify` on disconnect / DestroyWindow.** Emit via the
   routing primitive; this is small once stages 1–2 land.

Each stage compiles, passes its tests, and leaves the server in a
working state. Whether they ship as one PR or several is a separate
call.
