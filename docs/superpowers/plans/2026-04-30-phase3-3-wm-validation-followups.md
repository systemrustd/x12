# Phase 3.3 WM Validation Follow-Ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans
> to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Close the WM-validation polish gaps from Phase 3.2 so `wmaker`,
`e16`, `fvwm3`, and `Openbox` render and behave correctly under
`ynest`, while keeping the existing regression set (`gtk3-demo`, `xeyes`,
`xclock`, `xterm`, fvwm3 startup) stable.

**Spec:** [`docs/superpowers/specs/2026-04-30-phase3-3-wm-validation-followups-design.md`](../specs/2026-04-30-phase3-3-wm-validation-followups-design.md).

**Architecture:** This is a polish phase. Prefer targeted forwarding, exact
wire-shape fixes, and verification-driven changes over broad new protocol
surface. Keep local server state as the source of truth where that is already
the model, and only mirror or proxy to the host where the current host-backed
window/picture pipeline already makes that safe.

**Project conventions:**

```sh
cargo +nightly fmt --all -- --check
cargo clippy --workspace
cargo test --workspace
```

Manual validation is mandatory. Most success criteria in this phase are visual
or interaction-level behaviors that unit tests cannot prove alone.

---

## File Structure

| Path | Status | Purpose |
|---|---|---|
| `crates/yserver-core/src/host_x11.rs` | modify | Host SHAPE/XFIXES capability detection, SHAPE forwarding, cursor-by-name forwarding, host-side event selection, RENDER audit/fixes |
| `crates/yserver-core/src/nested.rs` | modify | SHAPE/XFIXES request handling, synthetic Expose logic, root resize event fanout, validation-oriented glue |
| `crates/yserver-core/src/server.rs` | modify | Audit summaries and shared server-side helpers where needed |
| `crates/yserver-core/src/randr.rs` | modify if needed | Keep root resize state expectations aligned with `handle_host_container_resize` |
| `crates/yserver-protocol/src/x11/shape.rs` | modify if needed | SHAPE request/rect tests or helpers |
| `crates/yserver-protocol/src/x11/xfixes.rs` | modify | `ChangeCursorByName` parser tests and helpers |
| `crates/yserver-protocol/src/x11/mod.rs` | modify if needed | Core event encoder coverage for `ConfigureNotify`/`Expose` assertions |
| `docs/status.md` | modify | Track Phase 3.3 progress, validation outcomes, RENDER findings, and Phase 3.4 follow-ups |
| `docs/` | add | Small root-listener probe for manual resize verification |

The work should land as compile-safe commits:

1. **Host SHAPE mirroring** — detect SHAPE on the host and mirror resolved top-level shape state.
2. **XFIXES cursor-by-name forwarding** — add minor 23 parsing and host pass-through.
3. **RENDER audit and confirmed fixes** — add audit counters, fix known glyph composite bug, reconcile status docs.
4. **Resize and Expose follow-ups** — root `ConfigureNotify`, optional synthetic Expose path if validation requires it, plus probe/docs.
5. **WM validation and status update** — run the matrix, document results, and file Phase 3.4 leftovers.

---

## Commit 1 - Host SHAPE Mirroring

### Task 1.1: Add host SHAPE capability discovery

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`

- [ ] **Step 1: Factor host `QueryExtension` probing into a reusable helper if that reduces duplication.**

`init_render` and `init_xkb` already hand-roll `QueryExtension`. Before adding
SHAPE and XFIXES detection, either:

- keep the existing style and add two more focused init helpers, or
- extract a small internal helper that returns `(present, major_opcode, first_event, first_error)`.

Do not broaden scope beyond the extension probes needed in this phase.

- [ ] **Step 2: Cache host SHAPE presence/opcode on `HostX11`.**

Add a field such as `host_shape_opcode: Option<u8>` and initialize it during
`HostX11::open_from_env()`. If absent, log once at INFO and keep local-only
behavior.

- [ ] **Step 3: Add focused tests for the no-extension path if practical.**

If `host_x11.rs` tests can construct a minimal `HostX11` safely, assert that
the forwarding helpers are no-ops when the opcode is absent. If direct
construction is awkward, cover the fallback through a smaller pure helper.

### Task 1.2: Mirror resolved top-level shape rectangles to the host

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-core/src/server.rs` if helper state needs to move

- [ ] **Step 1: Add a host helper for `ShapeRectangles(Set, Unsorted)`.**

Implement a helper like `set_shape_rectangles(host_xid, kind, rects)` that:

- sends SHAPE `ShapeRectangles`,
- always uses `op=Set`,
- always uses `ordering=Unsorted`,
- writes the resolved rectangle list exactly once per kind update.

Use the existing wire-writing style in `host_x11.rs`; do not add a heavy SHAPE
abstraction layer.

- [ ] **Step 2: Identify the real top-level host target from SHAPE mutations.**

`handle_shape_request` already updates `ServerState::shape_windows`; extend it
to detect when the destination window has a `host_xid` and only mirror in that
case. Subwindows without host IDs stay local-only.

- [ ] **Step 3: Mirror post-resolution state, not raw client opcodes.**

After each `RECTANGLES`, `MASK`, `COMBINE`, or `OFFSET` mutation, recompute the
resolved rect list already stored locally and forward that result to the host
for bounding and/or clip as appropriate.

Keep local `shape_windows` authoritative for:

- `QueryExtents`
- `GetRectangles`
- `InputSelected`

Input shape remains local-only in this phase.

- [ ] **Step 4: Avoid regressions in the current local shape semantics.**

Do not change the current region algebra unless a real bug is found. This
commit is about mirroring current resolved state to the host, not redesigning
shape math.

### Task 1.3: Add SHAPE tests and manual validation hooks

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Add encoder/forwarding tests for host SHAPE writes.**

Cover:

- bounding vs clip kind mapping,
- rect-list encoding,
- no-op behavior when the host SHAPE opcode is absent,
- the top-level coordinate invariant that rects are forwarded without per-rect translation.

- [ ] **Step 2: Add a resolved-rect regression test for `COMBINE`.**

Use a local-only source window and assert the forwarded host write carries the
merged resolved destination rectangles rather than trying to mirror an
unresolvable source XID.

- [ ] **Step 3: Record the manual e16/wmaker re-validation requirement in the final status update.**

This commit is not complete until themed WM chrome is checked by hand under
`ynest`.

---

## Commit 2 - XFIXES `ChangeCursorByName`

### Task 2.1: Add parser coverage for minor 23

**Files:**
- Modify: `crates/yserver-protocol/src/x11/xfixes.rs`

- [ ] **Step 1: Add or extend a request parser for `ChangeCursorByName`.**

Parse:

- cursor XID,
- `nbytes`,
- padded name bytes.

Preserve the raw UTF-8 name bytes; do not introduce a theme cache or a string
normalization layer.

- [ ] **Step 2: Write parser tests first.**

Cover:

- a normal name,
- 4-byte padding handling,
- truncated input returning `None` or equivalent parse failure.

### Task 2.2: Forward minor 23 to the host

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Cache host XFIXES presence/opcode on `HostX11`.**

Mirror the SHAPE detection pattern with `host_xfixes_opcode: Option<u8>`.

- [ ] **Step 2: Add a focused host forwarding helper.**

Implement `xfixes_change_cursor_by_name(host_cursor_xid, name_bytes)` using raw
wire forwarding through the cached host XFIXES opcode.

- [ ] **Step 3: Add a host-XFIXES-absent fallback test.**

Assert that the forwarding path becomes a safe no-op when the host XFIXES
opcode is unavailable.

- [ ] **Step 4: Add a new branch in `handle_xfixes_request`.**

For minor 23:

- parse the request,
- translate nested cursor XID to host cursor XID via existing cursor mappings,
- forward to the host when both the XID and host XFIXES opcode exist,
- otherwise log at DEBUG and treat it as a safe no-op.

This request has no reply; do not synthesize one.

- [ ] **Step 5: Preserve current behavior for all existing XFIXES minors.**

This commit should be narrowly scoped to minor 23 plus the shared extension
probe plumbing.

### Task 2.3: Validate cursor behavior

**Files:**
- Modify: `docs/status.md`

- [ ] **Step 1: Verify e16 cursor-theme interactions manually.**

Confirm hover/click regions change cursors and that the `XFIXES::unknown
minor=23` logging disappears.

- [ ] **Step 2: Update status notes to distinguish host-absent fallback from success.**

If the host lacks XFIXES, document the local no-op fallback explicitly.

---

## Commit 3 - RENDER Audit and Confirmed Fixes

### Task 3.1: Add audit counters with a narrow logging surface

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-core/src/server.rs` if shared state is cleaner there

- [ ] **Step 1: Define a debug-only audit path for RENDER minors.**

Gate the audit behind a dedicated log target such as
`ynest::render_audit=debug`. Count, at minimum:

- total requests seen per minor,
- forwarded requests per minor,
- dropped/no-op requests per minor,
- optional reason buckets for drops.

- [ ] **Step 2: Emit a summary on client disconnect.**

Keep the output compact and structured enough to compare across runs. Do not
turn this into a permanent verbose trace.

- [ ] **Step 3: Prefer small helper functions over repeated counter updates in giant match arms.**

`nested.rs` is already large. Keep the instrumentation readable.

### Task 3.2: Fix the confirmed multi-glyph composite bug

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`

- [ ] **Step 1: Extend glyph delta patching across all non-255 commands.**

`render_composite_glyphs` currently patches only the first eligible glyph
command. Fix the loop so later runs receive the same offset adjustment.

- [ ] **Step 2: Add a regression test for multi-run glyph payloads.**

Assert that every non-255 command in the encoded stream gets its delta adjusted
by `(x_off, y_off)`.

### Task 3.3: Reconcile RENDER docs and triage outputs

**Files:**
- Modify: `docs/status.md`

- [ ] **Step 1: Fix the `SetPictureClipRectangles` status mismatch.**

The code already forwards it; update the opcode table accordingly.

- [ ] **Step 2: Run the e16 audit scenario and capture findings.**

Use a fixed short session:

1. start e16,
2. open a couple of themed apps,
3. hover/click chrome,
4. close them.

Diff the observed behavior against the Phase 3.1 trace notes where helpful.

Cross-reference the run against the Phase 3.1 parity artifact and, where
practical, diff Xephyr/xtrace output or the forwarded ynest request stream
against the audit counts.

- [ ] **Step 3: Land all in-scope Phase 3.3 audit fixes without expanding opcode coverage.**

Allowed in this phase:

- wire-shape fixes found by the audit,
- one-line dispatch fixes,
- client-to-host ID translation fixes.

Do not expand the opcode set in this phase. Missing opcodes become Phase 3.4
follow-ups in `docs/status.md`.

---

## Commit 4 - Resize and Expose Follow-Ups

### Task 4.1: Root `ConfigureNotify` on host container resize

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-protocol/src/x11/mod.rs` if tests are needed there

- [ ] **Step 1: Fan out `ConfigureNotify` on `ROOT_WINDOW` during resize.**

Inside `handle_host_container_resize`, after updating `RandrState` and root
geometry, emit a core `ConfigureNotify` to clients selecting
`StructureNotifyMask` on root.

- [ ] **Step 2: Emit core event before RANDR notifications initially.**

Use the ordering chosen in the design doc unless manual validation shows a real
client regression.

- [ ] **Step 3: Add tests for the root resize pipeline.**

Cover:

- `RandrState` width/height update,
- stored `ROOT_WINDOW` geometry update,
- `GetGeometry(ROOT_WINDOW)` reply agreement,
- `StructureNotify`-only subscribers receiving `ConfigureNotify`.

### Task 4.2: Verify background re-tile and add a root-listener probe

**Files:**
- Add: `docs/...` probe file
- Modify: `docs/status.md`

- [ ] **Step 1: Add a tiny standalone probe that selects only `StructureNotifyMask` on root.**

Prefer a minimal script or small C helper that is easy to run during manual
validation. Document invocation and expected output.

- [ ] **Step 2: Verify `bg_pixel` behavior before changing code.**

Only add a host-side background reapply if manual validation shows the host is
not repainting newly exposed container area correctly.

- [ ] **Step 3: Document the manual resize checks.**

Explicitly call out:

- root `ConfigureNotify` probe output,
- panel/background reflow,
- app visibility after resize.

### Task 4.3: Synthetic Expose only if validation still fails

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-core/src/server.rs` only if a server-side helper is truly needed

- [ ] **Step 1: Run verification first and decide whether implementation is needed.**

Do not add synthetic Expose machinery if all of these already pass under the
validation WMs:

- off-screen drag and back,
- behind-sibling drag and back,
- sub-window cross-border drag and back,
- pure restack obscure/unobscure.

- [ ] **Step 2: If needed, select `VisibilityChangeMask` on host top-level subwindows.**

Extend `create_subwindow` so host top-levels request the visibility events
needed for the obscured-to-visible trigger.

- [ ] **Step 3: Implement the two explicit synthesis triggers only.**

If Phase B is required, synthesize full-window `Expose` for:

- off-screen recovery detected from old/new host geometry,
- `VisibilityFullyObscured -> {VisibilityUnobscured, VisibilityPartiallyObscured}` transitions.

Avoid broad heuristics tied to generic configure churn.

- [ ] **Step 4: Reuse existing Expose fanout paths.**

Route synthetic exposes through the same fanout used for real host Expose so
descendant subwindows receive their share consistently.

- [ ] **Step 5: Add suppression bookkeeping if and only if synthesis lands.**

Track recent real host Expose delivery per top-level in host-pump state keyed
by host XID and suppress duplicate synthetic repaint bursts within the chosen
short window.

- [ ] **Step 6: Add narrowly targeted tests for the state machines.**

Only if Phase B lands, cover:

- geometry transition detection,
- visibility transition gating,
- wire shape of the synthesized full-window `Expose`.

---

## Commit 5 - WM Validation and Status Update

### Task 5.1: Keep the regression set passing between each milestone

**Files:**
- Modify: `docs/status.md`

- [ ] **Step 1: Run the project gates after each commit-sized change.**

Use:

```sh
cargo +nightly fmt
cargo clippy --workspace
cargo test --workspace
```

- [ ] **Step 2: Re-check the regression set manually as Phase 3.3 changes land.**

Minimum manual apps:

- `gtk3-demo`
- `xeyes`
- `xclock`
- `xterm`
- fvwm3 startup

### Task 5.2: Run the WM validation matrix in the planned order

**Files:**
- Modify: `docs/status.md`

- [ ] **Step 1: Re-validate `wmaker`.**

Focus on:

- SHAPE silhouette correctness,
- drag/off-screen restoral,
- subwindow expose behavior,
- cursor sanity.

- [ ] **Step 2: Re-validate `e16`.**

Focus on:

- SHAPE silhouette correctness,
- cursor-by-name behavior,
- RENDER audit findings,
- drag/restack behavior.

- [ ] **Step 3: Re-validate `fvwm3`.**

Focus on root resize behavior, desktop background reflow, and `GetGeometry` /
`ConfigureNotify` agreement. Keep the known "apps disappear after host resize"
issue deferred unless a trivial fix falls out naturally.

- [ ] **Step 4: Bring up Openbox.**

Treat new non-trivial issues as Phase 3.4 follow-ups unless a one-line missing
dispatch or reply fix is the only blocker.

### Task 5.3: Close out status and follow-up tracking

**Files:**
- Modify: `docs/status.md`

- [ ] **Step 1: Mark which Phase 3.3 items are resolved by verification alone.**

If the backing-store mitigation from commit `93b988a` fully covers items #2/#4,
say so explicitly in `docs/status.md`.

- [ ] **Step 2: Record any remaining deferred problems as Phase 3.4 bullets.**

Keep each follow-up to one line with a one-line repro or symptom.

- [ ] **Step 3: Update the current project status summary if Phase 3.3 meaningfully changes it.**

The top-level status text should stay aligned with the real validation state.

---

## Validation Checklist

- [ ] `cargo +nightly fmt`
- [ ] `cargo clippy --workspace`
- [ ] `cargo test --workspace`
- [ ] `gtk3-demo` still works under `ynest`
- [ ] `xeyes` still works under `ynest`
- [ ] `xclock` still works under `ynest`
- [ ] `xterm` still works under `ynest`
- [ ] `fvwm3` still starts under `ynest`
- [ ] `wmaker` validation pass completed
- [ ] `e16` validation pass completed
- [ ] `fvwm3` resize validation pass completed
- [ ] `Openbox` first bring-up completed
## Notes for Implementation

- Keep SHAPE local state authoritative and mirror only the resolved top-level
  bounding/clip rectangles to the host.
- Do not advertise new extensions in this phase.
- Do not implement synthetic Expose machinery unless verification proves the
  backing-store fix is still insufficient.
- Keep RENDER audit output debug-only and easy to remove or ignore.
- Prefer adding small testable helpers over making `nested.rs` even more
  monolithic.
- Fluxbox is not installed in this environment, so Phase 3.3 validation stops at
  Openbox; do not treat missing Fluxbox validation as an implementation failure.
