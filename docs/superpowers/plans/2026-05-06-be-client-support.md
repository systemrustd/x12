# Big-endian client support — implementation plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Accept big-endian X11 clients end-to-end. The X11 spec lets a server pick whichever byte order is convenient internally, but every wire byte sent to a client and every wire byte received from one must be in *that client's* declared byte order. Today ynest accepts only little-endian (the setup gate at `crates/yserver-core/src/core_loop/setup_thread.rs:128` rejects `b'B'` outright). xts5 opens a reversed-byte-sex probe connection on every test that has a "BE" sub-check, which is essentially every test, so the entire xts pass count is currently gated on this.

**Tech stack:** Rust 2024. The byte-order-aware wire helpers (`x11::wire::{read_u16, write_u16, write_i16, write_u32}`) already exist; what's missing is (a) plumbing them through every reply/event/error encoder that today hard-codes LE, and (b) byte-swapping inbound request bodies before dispatch. `ClientByteOrder` is already on `Client`/`ClientState`.

**Branch:** `be-client-support` (already created off master after BadLength landed).

**Atomic merge.** This branch lands as a single squash to master. Phases land as commits *within* the branch but the branch itself merges atomically — a half-deployed BE path (e.g. setup succeeds but replies still LE) leaves a real BE client in an unrecoverable state. xts is itself the integration probe; we re-run after each phase to confirm forward motion but don't merge until the full pass completes Phase E.

**Out of scope:** Anything that's not on the wire. Internal `ServerState` numbers stay native-endian. The host X11 connection (when used as a backend) is unaffected — that's our own connection, opened LE, and untouched.

---

## Spec recap

X11 byte-order rules:

1. **Setup request.** The first byte is `b'l'` (LE) or `b'B'` (BE). All multi-byte fields in the setup request and reply use that byte order. A BE client expects the `ConnectionSetup` reply (success or failed) in BE.
2. **All subsequent traffic** in either direction uses the declared client byte order. This includes request *headers*, request bodies, replies, events, and errors. The 1-byte fields (opcode, error code, response type, byte_order_value) stay as-is.
3. **String / bytes payloads** inside requests and replies (font names, atom names, property data with `format=8`, image data) are **not** byte-swapped — they're already byte streams. Only the typed fields (CARD16/CARD32/INT16/INT32) are swapped. Property data with `format=16` is treated as an array of CARD16 values and *is* swapped per element.
4. **Image data** for `PutImage`/`GetImage` follows the `image-byte-order` declared in the setup reply. We currently echo back the client's order, which is correct: data flows through unchanged.

---

## Approach decisions

### Replies / events / errors: thread `byte_order` through encoders

These are constructed in the server: we know the client byte order at encoding time. The cheapest fix is to thread `client.byte_order` into each `encode_*_reply` / event / error builder and replace hard-coded `ClientByteOrder::LittleEndian` arguments to `write_u16` / `write_u32`. There are roughly 79 reply encoders and 34 event/notify encoders (`grep -rE "fn (encode|write|emit)_[a-z_]+_(reply|notify|event)"`).

There is **not one** `fixed_reply` helper. Each of `wire.rs`, `randr.rs`, `shape.rs`, and `xfixes.rs` defines its own private `fixed_reply`, and `present.rs` writes replies directly with `to_le_bytes` (no helper at all). The audit must be per-file — Phase C lists each one explicitly.

### Inbound framing: byte-order-aware `read_request`

`read_request` (`crates/yserver-protocol/src/x11/mod.rs:758`) currently decodes the 16-bit length field and the BIG-REQUESTS 32-bit extended length with `from_le_bytes`. Body swap can't fix this — the framing layer itself misreads BE clients before any body parse runs.

The new contract: `read_request(reader, byte_order, big_requests_enabled) -> (RequestHeader, Vec<u8>)`. The setup handshake establishes `byte_order`; the per-client reader thread (`core_loop/client_reader.rs:95`) holds the value and passes it on every call.

### Requests: byte-swap inbound body before dispatch

Request handlers are extensive (over 100 functions, many with their own private `read_u32_le` / `read_u16_le` decoders). Swapping at every call site would touch ~272 reader call sites and is error-prone.

Instead: at the I/O boundary, after `read_request` returns the body, if the client is BE, walk a per-opcode field-table that lists the offsets of CARD16/CARD32/INT16/INT32 fields and byte-swap them in place. After the swap, the body is in LE form and dispatchers run unchanged.

The field table representation must explicitly support:
- `Fixed(offset, kind)` — a typed field at a known offset.
- `OpaqueTail(from_offset)` — everything from offset to end-of-body is bytes (font names, glyph blobs, image data).
- `ElementArrayTail(from_offset, element_size)` — uniform `u16`/`u32` array at the tail (e.g., `FreeColors` pixel array).
- `LengthPrefixedBytes(length_offset)` — a u16 length followed by that many opaque bytes (then padding) — needed for `InternAtom`, font names.
- `Custom(handler_fn)` — a per-opcode swapper for irregular layouts. Used for `ChangeProperty`/`GetProperty` (format-aware payload) and for extension dispatchers whose minor opcodes have wildly different shapes.

A flat offset list is *not* sufficient: RENDER `AddGlyphs`/`CompositeGlyphs`/`FillRectangles`, RANDR list payloads, PRESENT/XFIXES region lists all have mixed prefixes + opaque or element-array tails.

The field tables also unlock the variable-length BadLength cases (the residual 30-ish FAILs from the BadLength bucket): once we know the field layout, we can compute the actual required `length_units` from the body content and reject mismatches.

### Strings / opaque bytes inside request bodies

Font names, atom names, property payloads (format=8), image data — these segments are NOT swapped. Captured by the `OpaqueTail` and `LengthPrefixedBytes` table entries above.

### `ChangeProperty` / `GetProperty` payload: `format`-aware

The `data` portion of `ChangeProperty` is swapped per `format`: format=8 is bytes (no swap), format=16 is u16[] (swap each), format=32 is u32[] (swap each). Same for `GetProperty` reply payload. This is the only opcode where payload swap depends on a value inside the request — handled with a `Custom` table entry rather than via a generic table.

### Raw event templates (every `fanout_raw_event_to_clients` source)

Today's path forwards a 32-byte event template **as raw LE bytes**, regardless of recipient byte order. This affects **every** caller of `fanout_raw_event_to_clients` (`core_loop/fanout.rs:318`), not just `SendEvent`:

- `SendEvent` (opcode 25): parses the template from the client, flips the send-event bit, fans out — only the sequence field is patched per recipient.
- `encode_selection_request_event` / `encode_selection_clear_event` (`mod.rs:2610-2632`) take an `_order` parameter and ignore it — hard-coded to `to_le_bytes`.
- **Backend-originated events** (host-X11 pump events, KMS-side input events that fan out through the same raw-template helper) — same problem.

Phase D2 must convert *all* raw-template fanout sources to per-recipient re-encoding, not only `SendEvent`. The conversion uses a shared event field-table (defined in the new `wire_swap` module described below).

### Shared field-kind module

Phases D2 and E both need the same `FieldKind`/`FieldEntry` enum (one for the inbound request swap, one for the outbound event re-encoding). The shared types live in a new module **`crates/yserver-protocol/src/x11/wire_swap.rs`**, defined and committed *before* D2. Both phases consume it.

---

## Phases

Each phase ends with `cargo +nightly fmt && cargo clippy && cargo test` (regular clippy, not pedantic — per AGENTS.md). Phase A is no longer the merge-safety boundary; the branch lands atomically.

### Phase 0 — Byte-order-aware request reader

**Goal:** `read_request` correctly parses request headers (including BIG-REQUESTS extended length) for BE clients.

**Tasks:**

1. Change `read_request` signature to `(reader, byte_order, big_requests_enabled)`. Replace `from_le_bytes` for both length fields:
   - The 16-bit length at offset 2 of the 4-byte header. `body_len = length_units * 4 - 4`.
   - The 32-bit BIG-REQUESTS extended length read **immediately after** the 4-byte header (so total prefix is 8 bytes), entered when `length_units == 0` and BIG-REQUESTS is enabled. `body_len = length_units * 4 - 8`.
   - Both reads must use `byte_order` (the existing `read_u16` already takes it; add or use `read_u32(byte_order, …)`).
2. Update `core_loop/client_reader.rs:95` to pass `client.byte_order`.
3. Unit tests: drive both LE and BE request frames including a BIG-REQUESTS extended-length frame; assert correct `body_len` and field values for each combination.

**Stop sign:** This must land before any later phase; otherwise BE clients can't even be framed.

### Phase A — Setup handshake

**Goal:** A BE client can complete the setup handshake.

**Tasks:**

1. Make `write_setup_success` and `write_screen` (`crates/yserver-protocol/src/x11/mod.rs:579, :677`) take `byte_order: ClientByteOrder` and pass it to every `write_u16` / `write_u32` call inside (currently hard-coded LE). Update `setup_thread.rs:177` call site to pass `setup.byte_order`.
2. `write_setup_failed` (`mod.rs:544`) already honours `byte_order` — no change.
3. Remove the BE rejection at `crates/yserver-core/src/core_loop/setup_thread.rs:128`. Replace with `info!` log noting which byte order the client declared.

**Verification:**
- A `cargo test` integration test driving setup with `b'B'` and asserting the reply bytes are BE-encoded.
- The setup-success encoding has 5 pixmap formats and a screen body, easy to miss a hard-coded LE — re-grep `LittleEndian` and `to_le_bytes` in `mod.rs` setup-related functions.

### Phase B — Error encoding

**Goal:** Every X11 error reply is encoded in the client's byte order.

**Tasks:**

1. Change `x11::write_error` (`mod.rs:560`) signature to take `byte_order: ClientByteOrder`. Replace its hard-coded LE writes.
2. Update `emit_x11_error` in `process_request.rs:3893` to look up `client.byte_order` and pass it through.
3. Audit any other code that builds error responses by hand (grep `error_code` usages).

**Verification:** Add a unit test that builds an error for a BE client and checks the sequence/bad_value bytes are BE.

### Phase C — Reply encoder pass

**Goal:** All reply encoders honour `byte_order`.

This is the bulkiest phase. **Audit per file** — there is no single `fixed_reply` and `present.rs` doesn't use a helper at all.

**Tasks:**

1. **`wire.rs:62` `fixed_reply`** — add `byte_order` parameter; ~70 call sites updated mechanically.
2. **`randr.rs:66` `fixed_reply`** (private) — add `byte_order` parameter; update all RANDR encoders (`encode_query_version_reply`, `encode_get_screen_resources_current_reply`, `encode_get_output_info_reply`, `encode_get_crtc_info_reply`, `encode_get_monitors_reply`, etc. — see `randr.rs:192–649` for the full list).
3. **`shape.rs:83` `fixed_reply`** (private) — same treatment; update `encode_query_version_reply`, `encode_query_extents_reply`, `encode_input_selected_reply`, `encode_get_rectangles_reply`.
4. **`xfixes.rs:68` `fixed_reply`** (private) — same; update `encode_query_version_reply`, `encode_get_cursor_image_empty_reply`, `encode_fetch_region_reply`.
5. **`present.rs:146,160`** — no helper, raw `to_le_bytes` calls in `encode_query_version_reply` and `encode_query_capabilities_reply`. Rewrite to use `write_u16`/`write_u32` with the client byte order.
6. **`composite.rs`, `damage.rs`, `mit_shm.rs`, `sync.rs`, `xtest.rs`** — convert each `encode_*_reply` to take `byte_order`.
7. **`mod.rs`** core-protocol encoders — `write_*_reply` for InternAtom, GetAtomName, GetGeometry, QueryTree, GetWindowAttributes, GetProperty, ListProperties, GetSelectionOwner, QueryPointer, GetMotionEvents, TranslateCoordinates, GetInputFocus, QueryKeymap, QueryFont, QueryTextExtents, ListFonts, ListFontsWithInfo (multi-reply), GetFontPath, AllocColor, AllocNamedColor, AllocColorCells, AllocColorPlanes, QueryColors, LookupColor, QueryBestSize, ListExtensions, QueryExtension, GetKeyboardMapping, GetModifierMapping, GetPointerMapping, GetKeyboardControl, GetPointerControl, GetScreenSaver, ListHosts, GetImage. Inline encoders in `process_request.rs` too.
8. **Render reply encoders** in `mod.rs` (write_render_query_version_reply etc.).

For each encoder: function gains `byte_order: ClientByteOrder` parameter, every call site is updated to pass `client.byte_order`. The compile errors after step 1 surface most of them; the remaining files require explicit walking.

**Specific care:** the multi-reply encoders for `ListFontsWithInfo` and `GetProperty` (format-aware payload).

**Verification:**
- Unit tests for at least 5 representative encoders (a fixed-reply opcode, a variable-length list reply, an extension reply, an error, and the setup-success).
- xts Xproto run to confirm no regressions on LE-only path.

### Phase D — Event encoder pass

**Goal:** All events delivered to a BE client are in BE.

There are ~34 event/notify encoders. Each is small (32 bytes, fixed format).

**Tasks:**

1. Identify every event encoder via `grep -rE "fn (encode|write|emit)_[a-z_]+(notify|event)"`.
2. Each takes a `byte_order` parameter; the fan-out helpers in `core_loop/fanout.rs` look up `client.byte_order` per recipient and re-encode. Events are encoded **per-recipient** (32 bytes × N recipients is cheap; the alternative — encode once and conditionally swap — saves nothing and complicates cache invalidation). The sequence number is also assigned per recipient (it's a per-client counter), and the shared template — if any — is read-only across recipients (we never mutate in place across the iteration).
3. Fix `encode_selection_request_event` / `encode_selection_clear_event` (`mod.rs:2610-2632`) — they currently take `_order` and ignore it, hard-coding `to_le_bytes`. Rewrite to actually use `write_u16`/`write_u32` with the passed order.

**Verification:** Drive a `MapNotify` to two clients, one LE one BE, assert the bytes differ as expected.

### Phase D1 — Shared `wire_swap` module

**Goal:** Define the `FieldKind` / `FieldEntry` types used by Phases D2 and E in one place.

**Tasks:**

1. Create `crates/yserver-protocol/src/x11/wire_swap.rs` containing:

   ```rust
   pub enum FieldKind { U16, U32, I16, I32 }
   pub enum FieldEntry {
       Fixed { offset: u16, kind: FieldKind },
       OpaqueTail { from: u16 },
       ElementArrayTail { from: u16, element_size: u8 },
       LengthPrefixedBytes { length_offset: u16, length_kind: FieldKind, data_offset: u16 },
       Custom(fn(&mut [u8])),
   }
   pub fn swap_in_place(entries: &[FieldEntry], body: &mut [u8]);
   ```

2. `swap_in_place` walks `entries` and byte-swaps each typed field in place. `Custom` handlers receive the raw body and are responsible for reading any u8 discriminants *before* mutating typed fields (e.g., `ChangeProperty`'s custom handler reads `format` at body offset 1 — a u8, byte-order-irrelevant — and uses it to decide whether to swap the trailing data array per element).

3. Unit tests: each `FieldEntry` variant has a round-trip test (LE bytes → swap → BE bytes → swap back → LE bytes).

This module lands in its own commit before D2.

### Phase D2 — Raw event templates (all sources)

**Goal:** Every raw-template fanout path correctly translates from source byte order to each recipient's byte order. This covers `SendEvent` *and* every backend-originated event that flows through `fanout_raw_event_to_clients`.

`SendEvent` (opcode 25) has the sender supply a 32-byte event template; the server sets the send-event bit and delivers it to one or more recipients. The template is in the **sender's** byte order. Each recipient sees it in **their own** byte order, so the server must (a) parse the template per its event type, (b) re-encode for each recipient. Backend-originated events (host-X11 pump, libinput-driven KMS events that flow through the same helper) follow the same rule with the source byte order being LE (server-internal canonical form).

`fanout_raw_event_to_clients` (`core_loop/fanout.rs:318`) currently only patches the sequence field. The new contract: it encodes per recipient using the event field-table.

**Tasks:**

1. Add an `event_swap_table` to `wire_swap.rs` keyed by event type (1-byte response_type bottom 7 bits, with the send-event bit ignored). Coverage: events 2–35 (core), GenericEvent (35) per extension event base. Reuses `FieldEntry`.
2. In the SendEvent handler: parse template fields in the *sender's* byte order, then for each recipient call `swap_in_place` with the recipient's byte order on a per-recipient copy of the 32-byte buffer. The original template is read-only across the iteration.
3. **API change for `fanout_raw_event_to_clients`:** the function now owns the per-recipient encoding step. New signature:

   ```rust
   pub fn fanout_raw_event_to_clients(
       state: &mut ServerState,
       client_ids: &[ClientId],
       template: &[u8; 32],
       template_byte_order: ClientByteOrder, // source order of `template`
   ) -> Vec<ClientId> // dropped clients
   ```

   Internally it copies `template` per recipient, re-encodes into recipient byte order via `swap_in_place(event_swap_table[response_type], &mut copy)`, patches the sequence, and writes. Drop accounting stays inside the function — same return semantics as today, no API ambiguity.
4. Audit every existing `fanout_raw_event_to_clients` call site for the source byte order: SendEvent uses sender's byte order, backend-originated events use LE (the canonical server-internal form).

**Verification:**
- Test: client A (LE) issues `SendEvent` with a `ClientMessage` targeting client B (BE); B receives the message with fields in BE.
- Test: backend-originated event delivered to a BE client arrives in BE.

### Phase E — Inbound request body swap

**Goal:** A BE client's requests are dispatched correctly.

**Note on direction:** This phase swaps **request bodies only** (the inbound wire form). Replies and events are handled separately in Phases C and D — those use byte-order-aware encoders that build outgoing bytes directly in the client's byte order, no in-place swap. `GetProperty`'s reply payload (also format-aware) is the encoder's responsibility, not the swap table's.

**Tasks:**

1. Build `request_swap_table` in `wire_swap.rs` (extending the module from D1) — a per-opcode `&'static [FieldEntry]` for opcodes 1–127. Extension dispatchers register their own per-minor tables.

2. After `read_request` returns, if the client is BE, look up the table entry for `header.opcode` and call `swap_in_place(entries, &mut body)`. Strings and opaque payloads are skipped by construction (they use `OpaqueTail` / `LengthPrefixedBytes`).

3. For extension opcodes (≥128): the dispatcher (`handle_render_request`, `handle_xfixes_request`, …) calls a per-extension swap function keyed on `header.data` (the minor opcode) **before** the existing parse-and-dispatch logic. Each extension owns its own table.

4. **Custom handlers needed (request-body only):**
   - `ChangeProperty` (opcode 18) — format-aware payload. The custom handler reads the u8 `format` field at body offset 1 (byte-order-irrelevant), then if `format` ∈ {16, 32} swaps the data array per element. Typed fields before the data are swapped first via the standard `Fixed` entries.
   - `GetProperty` (20) is a request with no format-dependent body (just typed fields) — no custom handler needed for the inbound path. The format-aware payload swap is on the *reply* side and lives in the encoder.
   - RENDER `CompositeGlyphs8/16/32` (139.23, 139.24, 139.25) — glyph elements have variable-width entries (1 or 2 bytes for the glyph ID); swap typed prefix, then for 16/32 walk the glyphcmd stream and swap glyph IDs per element.
   - RENDER `AddGlyphs` (139.20) — typed prefix + 4-byte-per-entry typed array + raw bitmap data.
   - `ChangeKeyboardMapping` request — keysym table is u32[]; `ElementArrayTail` covers it without a custom handler. The reply for `GetKeyboardMapping` is on the encoder side.

5. Validate post-swap: combine with the field table to compute the actual required `length_units` for variable-length opcodes (Phase F).

**Verification:**
- `cargo test`: swap-table-driven round-trip tests for representative opcodes (`InternAtom`, `ChangeWindowAttributes` with a value-mask, `PutImage`, `ChangeProperty` for each format, `RenderCompositeGlyphs16`).
- xts Xproto run with the residual UNRES bucket dropping below ~50.

### Phase F — Variable-length BadLength (bonus, optional this branch)

**Goal:** Reject variable-length requests whose declared `length_units` doesn't match their content. Closes the 30-ish residual BadLength FAILs.

**Tasks:**

1. Augment the field-table from Phase E with a "minimum required units given this content" computation: count bits in value-masks, count list lengths, add string lengths.
2. Validate after swap, reject with `BadLength` on mismatch.

**Verification:** xts Xproto run; the variable-length BadLength FAILs (ChangeGC TP2-4, CreateWindow TP2-4, etc.) flip to PASS.

---

## Tally checkpoints

After each phase, run `just xts-ynest` and add a row to the run-history table in `docs/xts-baseline.md`. Expectations:

| After phase | PASS | FAIL | UNRES | Comment |
|-------------|------|------|-------|---------|
| Baseline (now) | 1 | 74 | 296 | BadLength enforced, BE blocked at setup |
| Phase 0 | 1 | 74 | 296 | Reader BE-aware but setup still rejects BE — no change yet |
| Phase A | 1 | ~74 | ~296 | Setup works but everything else still LE-only on the BE socket |
| Phase B+C | small bump | ~74 | ~290 | BE clients receive correct replies/errors; their requests still LE-decoded so most still UNRES |
| Phase D1+D+D2 | similar | similar | similar | Events landing correctly across all fanout sources, but BE clients sending requests still misparsed |
| Phase E | **major lift** | small | small | Most of the 136 "trapped behind BE" tests should flip to PASS or FAIL (revealing the next layer of native bugs) |
| Phase F | major lift | smaller | small | Variable-length BadLength FAILs PASS |

The big win is at the *end* of Phase E. Phases 0–D2 are necessary preparation but show small tally movement on their own. **Master only sees the final atomic merge** — intermediate-phase tally rows live in the branch's xts-baseline doc.

---

## Conventions

- Run `cargo +nightly fmt && cargo clippy && cargo test` after each task that ends compile-clean. **Regular clippy only**, per AGENTS.md (pedantic is opt-in for this repo).
- Commit messages: `feat(be): <phase>.<step> — <one-line summary>`. One commit per task unless the change is split across files for compile-cleanliness.
- `codex` review at end of each phase (per CLAUDE.md, codex handles review of work YOU did to save tokens).
- File-path references with `:N` are accurate at time of writing — re-grep before editing.
- Every commit must keep `cargo test` green for both binaries.
- Branch lands as a **single squash commit** on master once Phase E completes (Phase F may land in the same squash if it's done in time, or in a follow-up).
