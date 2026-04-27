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

- Selections / clipboard (Phase 2).
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

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  ynest::run                                                 │
│    ┌─────────────────────────────────────────────────────┐  │
│    │  Arc<Mutex<ServerState>>   (shared by all clients)  │  │
│    │   ├─ atoms:        AtomTable                        │  │
│    │   ├─ resources:    ResourceTable                    │  │
│    │   │     ├─ windows  (each Window owns properties +  │  │
│    │   │     │            owner: ClientId)               │  │
│    │   │     ├─ pixmaps                                  │  │
│    │   │     ├─ gcs                                      │  │
│    │   │     ├─ fonts                                    │  │
│    │   │     └─ cursors                                  │  │
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

### Module layout (`crates/yserver-core/src`)

- `server.rs` *(new)* — `ServerState`, `ClientHandle`, `IdAllocator`, the
  `subscribers(window, mask_bit)` query.
- `properties.rs` *(new)* — pure types and helpers: `PropertyValue`,
  `PropertyFormat`, `ChangeMode`, `apply_change`, `slice_for_get`. No
  locking, no I/O.
- `resources.rs` — keep, but `ResourceTable` becomes a struct held inside
  `ServerState`. `Window` gains `properties: HashMap<AtomId, PropertyValue>`
  and `owner: ClientId`. The existing `event_mask: u32` field is removed
  in favor of per-(client, window) masks on `ClientHandle`.
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
// Default: base = 0x0000_0100 + n * 0x0010_0000, mask = 0x000F_FFFF.

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
    let targets = state.lock().unwrap().subscribers(window, mask_bit);
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
    pub const BAD_REQUEST: u8 = 1;
    pub const BAD_VALUE:   u8 = 2;
    pub const BAD_WINDOW:  u8 = 3;
    pub const BAD_ATOM:    u8 = 5;
    pub const BAD_MATCH:   u8 = 8;
    pub const BAD_ALLOC:   u8 = 11;
    pub const BAD_LENGTH:  u8 = 16;
}
```

## Data flow

### Server startup (`ynest::run`)

1. Build `ServerState` (empty atoms, `ResourceTable` with root window only,
   empty clients, `id_allocator` starting at `0x0000_0100`,
   `start_instant = Instant::now()`).
2. Wrap in `Arc<Mutex<_>>` and clone into each accepted client thread.

### Client connect (`handle_client`)

1. Lock state → `id_allocator.allocate()` → use the returned
   `(resource_id_base, resource_id_mask)` in the setup_success reply.
2. Register `ClientHandle { writer, byte_order, last_sequence,
   event_masks: HashMap::new() }` in `clients[client_id]`.
3. Drop lock, enter request loop.

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
     - Type matches (or AnyPropertyType): `BadValue` if `offset*4 > len`;
       else `len_to_return = min(remaining, long_length*4)`,
       `bytes_after = remaining - len_to_return`. `len_to_return` is
       rounded down to a multiple of `format.bytes()` for the reported
       slice; `value_len` is reported in format units.
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

### Client disconnect

1. Lock state.
2. Walk windows where `owner == client_id` → recursively destroy.
   For each destroyed window, emit `DestroyNotify` to:
   - clients that selected `StructureNotify` (bit 0x20000) on the window itself, and
   - clients that selected `SubstructureNotify` (bit 0x80000) on the parent.
   `subscribers()` is parameterised by `(window, mask_bit)`, so this is two calls
   per destroyed window. Lands here because the routing primitive is now in
   place; without it we couldn't deliver the event correctly.
3. Walk all other clients' `event_masks` and drop entries for any
   destroyed window.
4. Remove this client from `clients`.

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
| GetProperty `long_offset * 4 > existing length` | BadValue | `long_offset` |

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

Wire-format round-trips:

- **`ChangePropertyRequest`**: ∀ valid (mode ∈ 0..=2, format ∈ {8,16,32},
  length ≤ small bound, data of correct length, atoms, window),
  `parse(encode(req)) == Some(req)`.
- **`GetPropertyRequest`**: same.
- **`GetPropertyReply`**: ∀ valid reply, byte-checking fixed-offset fields
  reproduces inputs.
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

This design intentionally bundles three concerns (shared `ServerState`,
per-(client, window) event masks, property storage) plus a tag-along
(`DestroyNotify` on disconnect). They are bundled because property
correctness depends on each of the others — but they decompose into
sequenced stages that the implementation plan should respect:

1. **`ServerState` skeleton + `IdAllocator`.** Move `ResourceTable` and
   `AtomTable` into a shared `Arc<Mutex<ServerState>>`. Switch every
   handler that uses them to lock for the duration of its update. Hand
   out non-overlapping resource-ID ranges per client. No behavior change
   visible to clients yet (single-client scenarios still work).
2. **Client registry + per-(client, window) event masks.** Register
   `ClientHandle` on connect; deregister on disconnect. Move existing
   event-emission sites (Expose, MapNotify, ConfigureNotify, FocusIn/Out)
   to the new `subscribers()` + `emit_window_event` primitive. Behavior
   should be unchanged for the existing single-client case but
   architecturally correct for multi-client.
3. **`PropertyValue`, `apply_change`, `slice_for_get` (pure).** Land
   `properties.rs` with full unit + proptest coverage. No wiring yet.
4. **Wire-format additions + handler implementations.** Implement
   parsers, reply/event encoders, and the `ChangeProperty` /
   `DeleteProperty` / `GetProperty` request handlers, including X11
   error emission. PropertyNotify integrates via the routing primitive
   from stage 2.
5. **`DestroyNotify` on disconnect.** Emit on client disconnect; this is
   small once stages 1–2 land.

Each stage compiles, passes its tests, and leaves the server in a
working state. Whether they ship as one PR or several is a separate
call.
