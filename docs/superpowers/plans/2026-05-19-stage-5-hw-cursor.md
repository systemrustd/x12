# Historical Stage 5 strategy plug-in — HW cursor reintroduction (v2)

Status 2026-05-20: HW cursor reintroduction is no longer the active
Stage 5 scope. It has moved to implemented prerequisite work for the
new Stage 5 performance-closure plan:
`docs/superpowers/plans/2026-05-20-stage-5-make-v2-fast.md`.

Pre-implementation design. Drafted 2026-05-19, revised seven
times same day after successive codex review passes
(v1 → v2 → v3 → v4 → v5 → v6 → v7 → v8 — see "Codex review
revisions" sections at the bottom for the per-pass deltas).
Land order assumes codex's Cinnamon work settles first.

## Goal

Bring back the DRM hardware cursor plane on v2 as a
`SceneCompositor` **strategy** over the existing scene cursor
entry. The cursor stays a scene entry; the strategy chooses the
render target for that one entry (plane vs SW blit).

At drafting time, per spec
`docs/superpowers/specs/2026-05-15-rendering-model-v2.md` §I7,
HW cursor returned under I6's retirement model as a
`SceneCompositor`-owned strategy choice, never as a side path that
bypasses scene ownership.

Original v1 motivation (`crates/yserver/src/kms/cursor_plane.rs:1-15`):
the cursor quad was tied to compositor cadence — every position
change waited for the next `composite_and_flip`, stalled by
per-op `vkQueueWaitIdle` in the paint pipeline. Observed as
severe pointer lag in mate-control-center hover. `drmModeMoveCursor`
is one ioctl, microseconds, doesn't touch the GPU.

## Architectural shape

The strategy split has three layers (codex review insight — the
"all in build_scene" or "trait object on SceneCompositor" framings
were both wrong shape):

1. **`build_scene` computes a pure decision.** Returns
   `CursorAssignment::Hw { upload_needed, position }` or
   `CursorAssignment::Sw { draw, damage_rects }`. No DRM calls,
   no side effects, no plane visibility flips — `build_scene`
   currently produces draw lists / damage / snapshots / sampled
   ids, and the cursor decision joins that pure-data set.

2. **`SceneCompositor` owns transition state, per output, gated
   on successful flip submission.** v2 builds + retires frames
   per output (`tick_one_output`), so transition state cannot be
   scene-global: if output A retires first and we call
   `cursor_plane_show()` while output B is still scanning a BO
   that contains SW cursor pixels, the double-cursor hazard
   reappears on output B (codex v2-pass insight). Each output's
   state lives in:
   - Per-output `last_frame_cursor_mode: { Sw(prev_pos), Hw, Hidden }`.
   - **A new `CursorTransition` field on `PendingAck`** populated
     only after the compose / atomic commit succeeds. If the
     commit fails, the transition simply isn't queued — no
     orphaned `pending_show` flag survives a failed submit.
   - `handle_page_flip_complete` for each output consumes its
     own `PendingAck.cursor_transition` and applies plane
     show/hide via `cursor_plane_show_on_crtc(idx)` /
     `cursor_plane_hide_on_crtc(idx)` — per-CRTC, not global.
   - Steady-state HW mode is reached only when ALL active
     outputs have retired their `Sw → Hw` transition flip; the
     `cursor_mode()` query (point 4 below) returns `Hw` only in
     that fully-quiesced state. Mixed-mode (some outputs HW,
     some still pending) keeps `cursor_mode() = Mixed`, which
     the pointer fast path treats as not-yet-HW.

3. **`PlatformBackend` performs DRM side effects only when the
   compositor tells it to, per CRTC.** New hooks (codex v6-pass
   — `set_cursor2` IS the show operation in legacy DRM, so
   "upload" must NOT rebind, or it'd reintroduce the multi-output
   premature-show hazard):
   - `cursor_plane_upload_image(version, width, height, bytes)`
     — write bytes into the shared dumb buffer + advance
     `uploaded_version` to `version`. **No `set_cursor2` rebind.**
     Idempotent if the requested `version` already matches.
   - `cursor_plane_show_on_crtc(idx, hot_x, hot_y, x, y)` —
     `set_cursor2(crtc, Some(dumb), (hot_x, hot_y))` for the
     retiring CRTC + `move_cursor(crtc, x, y)` to position it
     at the last known coords. The atomic show-bind path.
   - `cursor_plane_rebind_visible_crtcs(hot_x, hot_y, x, y)` —
     for steady-state sprite change (every CRTC currently in HW
     mode): re-issue `set_cursor2` against the just-uploaded
     dumb buffer ONLY for CRTCs whose `cursor_plane` state is
     already `visible`, **then immediately follow each rebind
     with `move_cursor(crtc, x, y)`** — `set_cursor2` may reset
     the kernel-side position to (0, 0), matching v1's pattern
     at `backend.rs:2173`. Hidden / pending CRTCs are skipped.
   - `cursor_plane_move(x, y)` — `drmModeMoveCursor` per
     visible CRTC (skip hidden). The kernel handles "off this
     CRTC" naturally; we don't pre-filter on geometry.
   - `cursor_plane_hide_on_crtc(idx)` — `set_cursor2(crtc, None, …)`.
     Per-output recovery uses this.
   - `cursor_plane_hide_all()` — global hide fallback for
     drain_all / shutdown / VT-leave / DRM-master loss recovery
     paths only.

4. **Pointer motion bypasses build_scene cadence in steady-state
   HW** (codex's critical addition — otherwise HW cursor stays
   compositor-cadence-bound and we lose the entire latency
   benefit). `process_pointer_absolute` on the **core thread**
   (NOT the libinput sender thread — codex v2-pass constraint;
   v2's single-threaded core owns DRM state) checks
   `SceneCompositor::cursor_mode() == Hw` and, if so, calls
   `platform.cursor_plane_move(x, y)` directly. No
   `wake_for_damage()`, no scene build. Mirrors v1's
   `hw_cursor_move()` call site at `backend.rs:6573`.

   While a `Sw → Hw` transition is pending on any output, the
   fast path is suppressed — `cursor_mode()` returns `Mixed`
   and `process_pointer_absolute` falls back to scene-wake. The
   first frame after all outputs retire the transition flips to
   `cursor_mode() = Hw` and the fast path takes over.

   `SceneCompositor` exposes a thin `cursor_mode()` query for
   this; ownership of the cursor entry stays with the scene.

## Current state pinned to source

### v1 — works today

- `crates/yserver/src/kms/cursor_plane.rs` — `CursorPlane` owns a
  shared 64×64 BGRA DRM dumb buffer bound to every CRTC via
  `set_cursor2`. Per-CRTC visibility tracked by the kernel.
  `CursorPlane::visible` flag at `cursor_plane.rs:43` is
  **global, not per-CRTC** — a known gap for multi-output
  hotplug (see Phase B).
- `crates/yserver/src/kms/backend.rs:792-803,1996-2230`:
  - Backend fields: `cursor_plane: Option<CursorPlane>`,
    `hw_cursor_xid: Option<u32>` (which cursor is currently
    uploaded), `hw_cursor_hotspot: (u16, u16)`.
  - Init at bootstrap: `CursorPlane::new(device)`; silent SW
    fallback on failure.
  - Three ops: `hw_cursor_refresh()` (upload sprite +
    `set_cursor2`), `hw_cursor_move()` (one `drmModeMoveCursor`
    ioctl per CRTC), `hw_cursor_hide_all()`.
  - Call sites: pointer-motion (`backend.rs:6571-6573`),
    DefineCursor / XFixesChangeCursorByName, boot.
- Compositor cursor quad is gated off when `hw_cursor_active()`
  is true (`backend.rs:7662`) to prevent double-draw.

### v2 — SW cursor only, parked HW

- `crates/yserver/src/kms/v2/scene.rs:232,254-259,1149-1207` —
  `CursorEntry { id: DrawableId, extent, hot_x, hot_y }` is
  registered once at backend init (a 16×16 default-arrow BGRA
  `Pixmap` DrawableKind). `build_scene` emits a `CompositeDraw`
  at top-of-z with `alpha_passthrough=true`, damages prev + new
  cursor rect (`cursor_prev_pos` carried frame-to-frame to avoid
  trails under buffer-age LOAD).
- `crates/yserver/src/kms/v2/backend.rs:399-430` —
  `init_cursor_sprite` builds the default-arrow drawable + calls
  `scene.register_cursor`.
- `crates/yserver/src/kms/v2/backend.rs:4162-4209` —
  `create_cursor` / `create_glyph_cursor` / `define_cursor` mint
  valid xids but **don't rasterise per-cursor sprites or swap
  the scene cursor** ("Stage 4 cursor scene-layer work"
  documented inline).
- `crates/yserver/src/kms/v2/backend.rs:1075-1078` — HW cursor
  trait methods are explicit no-ops per spec §I7.
- `crates/yserver/src/kms/v2/scene.rs:670` — current scene
  ordering: `build_scene` → acquire BO → record/submit compose
  → atomic flip → pageflip retirement. Plane visibility flips
  must hook into the pageflip-complete event, not `build_scene`.

## Phases

### Phase A (prereq, independent) — `define_cursor` + sprite rasterisation

Implement `create_cursor` / `create_glyph_cursor` / `define_cursor`
/ `xfixes_change_cursor_by_name` in v2 backend. For each cursor
xid the client creates:

1. Rasterise into a v2 `Pixmap` DrawableKind, store hot_x/hot_y
   on a per-cursor record.
2. **Keep a canonical CPU BGRA byte vector alongside the v2
   pixmap** (codex review insight). SW path samples the pixmap;
   HW path uploads from the CPU bytes. Avoids needing fenced
   GPU readback at HW upload time.

   **Pixel-format invariant** (codex v2-pass insight): the CPU
   bytes are **little-endian BGRA** matching DRM `ARGB8888`,
   with the same alpha semantics (premultiplied vs straight) as
   the SW path's sample. If the Vulkan cursor sprite uses
   premultiplied alpha and the dumb buffer expects straight,
   HW and SW cursors will render subtly differently and the
   transition will flash. Pin the convention at rasterisation
   time and assert in test that the byte vector and the
   pixmap's sample data agree on a fixed test sprite.

3. **Each cursor record is an immutable byte blob with a stable
   version key** (codex v2-pass insight). Theme reload / XFixes
   replacement / RenderCreateCursor of a new image does NOT
   mutate the existing record — it allocates a fresh record
   with a fresh version. Pointer-grab paths that captured an
   "effective cursor" reference observe the version they
   captured. Concretely: the effective-cursor lookup returns
   `Arc<CursorRecord>` so the bytes live as long as anything
   that resolved them, even if a newer record has superseded
   the canonical xid mapping.

   **Ordering invariants** (codex v3-pass — Arc lifetime alone
   isn't enough):
   - Version numbers are **monotonically increasing** server-
     wide (single `AtomicU64` on `KmsCore`). Comparison is
     by **value**, never by `Arc` pointer identity — two
     different `Arc<CursorRecord>` allocations can hold the
     same bytes (e.g. theme reload with no real change) and
     should NOT be treated as the same record by accident.
   - Mutation of the xid → record map AND reads of the effective
     cursor for scene tick happen on the **core thread** (v2's
     single-threaded ownership of DRM + scene state). No
     cross-thread shared mutability needed; no locks.
   - Old records need no special cleanup beyond `Arc` drop. Test
     coverage asserts: replacing a record never mutates the old
     bytes (memcmp the `Arc<CursorRecord>.bytes` slice held by a
     captured reference before and after the replacement).

4. Track effective cursor per pointer-window (DefineCursor on a
   window changes effective cursor when pointer crosses in).
5. When the effective cursor changes, swap
   `SceneCompositorInner.cursor` to the new `CursorEntry` and
   queue a HW upload (if HW mode is currently active on any
   output). The upload version is tracked per CRTC's bound
   record so a mid-frame replacement during pending transitions
   is ordered correctly.

**Independent of HW cursor.** Without it the v2 pointer always
shows the default 16×16 arrow regardless of client theme; with
it the SW cursor path becomes theme-correct. HW cursor work
needs this as a prereq (the plane uploads from the current
cursor's pixels — without Phase A, the plane always uploads the
default arrow).

May already be planned by other in-flight work — check before
duplicating.

### Phase B — `CursorPlane` on v2's `PlatformBackend`

Reuse `crate::kms::cursor_plane::CursorPlane` (already DRM-only,
doesn't know v1 from v2):

- Promote module visibility if currently scoped to v1 only.
- Add `Option<CursorPlane>` field to v2's `PlatformBackend`.
- Init in v2's bootstrap symmetric to v1 (`backend.rs:1996`):
  `CursorPlane::new(device.clone())` once; silent SW fallback on
  failure.
- Don't share state with v1. Each backend is instantiated for
  its own DRM device, but only one is alive at a time per
  process — each owns its own `CursorPlane`.
- **Convert `CursorPlane::visible` from global to per-CRTC**
  (codex finding — `cursor_plane.rs:43`). Multi-output state
  needs each CRTC's visibility tracked independently for hotplug
  + output-disable to do the right thing. v1 inherits the same
  fix; refactor lands once and benefits both backends.
- New hooks on `PlatformBackend` (codex v4-pass — NO global
  show hook; codex v6-pass — upload and show are SEPARATE so
  buffer mutation never doubles as a bind-and-show):
  - `cursor_plane_upload_image(version: u64, width: u16, height: u16, bytes: &[u8]) -> Result<()>` —
    write bytes into the shared dumb buffer
    (`width`×`height` BGRA8, dimensions ≤ 64×64) and advance
    `uploaded_version` to `version`. **No `set_cursor2`, no
    rebind, no visibility change.** Idempotent if `version`
    already matches `uploaded_version` (skip the memcpy). The
    `version` parameter is the `Arc<CursorRecord>.version` so
    the upload-dedup comparison is by value, not pointer
    identity.
  - `cursor_plane_show_on_crtc(idx: usize, hot_x: u16, hot_y: u16, x: i32, y: i32) -> Result<()>` —
    `set_cursor2(crtc, Some(dumb), (hot_x, hot_y))` for the
    retiring CRTC + `move_cursor(crtc, x, y)` to position the
    just-shown plane at the last known coords. This is the
    only path that calls `set_cursor2(..., Some(..))`. Called
    per-output from `handle_page_flip_complete` when that
    output's `PendingAck.cursor_transition == ShowOnRetire { .. }`.
  - `cursor_plane_rebind_visible_crtcs(hot_x: u16, hot_y: u16, x: i32, y: i32) -> Result<()>` —
    for steady-state sprite change: re-issue `set_cursor2` only
    on CRTCs whose plane state is already `visible`, then
    immediately follow each rebind with `move_cursor(crtc, x, y)`.
    Hidden / pending CRTCs are NOT touched. Hotspot can change
    when the new sprite has a different anchor. The explicit
    move is required because `set_cursor2` may reset the kernel-
    side position to (0, 0) — codex v7-pass + v1 pattern at
    `backend.rs:2173`.
  - `cursor_plane_move(x: i32, y: i32) -> Result<()>` —
    `drmModeMoveCursor` per visible CRTC (skip hidden CRTCs).
  - `cursor_plane_hide_on_crtc(idx: usize) -> Result<()>` —
    `set_cursor2(crtc, None, …)` on one CRTC. Called per-output
    for `HideOnRetire` retirements and for output-local
    recovery.
  - `cursor_plane_hide_all() -> Result<()>` — global hide
    fallback for global recovery paths only (drain_all /
    shutdown / VT-leave / DRM-master-loss). Never the
    steady-state path.
- Expose `cursor_plane_available() -> bool` so the scene
  strategy decision can gate on it without a `&PlatformBackend`
  borrow during pure scene build.

### Phase C — Strategy decision in `build_scene` (PURE)

`build_scene` returns a `CursorAssignment` enum as part of
`SceneBuild`. **No side effects**. At `scene.rs:1186-1207`
(cursor entry emission), the decision replaces the unconditional
`draws.push(...)`:

```
enum CursorAssignment {
    Hw {
        x: i32,
        y: i32,
        /// The effective cursor's `Arc<CursorRecord>.version`.
        /// SceneCompositor compares this against
        /// `CursorPlane.uploaded_version` to decide whether the
        /// retire should also trigger an upload. Compared by
        /// VALUE, never by pointer identity (codex v3-pass).
        record_version: u64,
        hot_x: u16,
        hot_y: u16,
    },
    Sw { /* draw included in scene.draws; damage in projected */ },
    Hidden { /* cursor off-output or fully clipped */ },
}
```

Strategy decision gates (pure function of scene-tick inputs):
- Cursor `extent.width ≤ 64 && extent.height ≤ 64`
- `cursor_strategy_hint.hw_available == true` (set from
  `platform.cursor_plane_available()` at scene-build entry)
- No transform / format conversion required (sprite is BGRA8)
- Cursor visible within at least one output's box

Any gate fails → `Sw` (existing path: push CompositeDraw, add
prev+new damage). The `SceneCompositor` outer caller (already
sequencing `build_scene` → BO acquire → compose → flip → pageflip
retire at `scene.rs:670`) consumes the `CursorAssignment` and
issues platform-side calls at the right point in that sequence
(see Phase D for ordering).

### Phase D — HW/SW transitions, per-output, gated on successful flip

State lives per output, not on scene-globals (codex v2-pass —
v2's `tick_one_output` retires frames per output, so global
flags would let output A's `cursor_plane_show()` fire while
output B is still scanning the SW-cursor BO). Each output's
`OutputSceneState` (sibling to today's existing fields) carries:

```
last_frame_cursor_mode: CursorMode { Sw(prev_pos) | Hw | Hidden },
```

And a new `cursor_transition: Option<CursorTransition>` field
on **each `PendingAck`** (the per-output structure already
holding compose generation + scanout BO refs):

```
enum CursorTransition {
    ShowOnRetire {
        upload_version: u64,  // compared by value vs CursorPlane.uploaded_version
        hot_x: u16,
        hot_y: u16,
        x: i32,               // cursor position at scene-build time
        y: i32,               // (root-space; show_on_crtc translates per-CRTC)
    },
    HideOnRetire,
    // No `UploadOnly` (codex v4-pass): sprite changes during
    // steady-state HW upload synchronously (see "Sprite change
    // in steady-state HW" below) because v2's empty-damage fast
    // path would starve a PendingAck-driven upload.
    // Move-only transitions are also NOT queued here — they're
    // synchronous via the pointer fast path.
}
```

**Population is gated on successful submit**: the
`build_scene` step computes the desired `CursorAssignment` and
the would-be transition, but `cursor_transition` is only
written into `PendingAck` AFTER the compose + atomic commit
succeeds for that output. If commit fails, the transition
isn't queued at all — no orphaned pending flag, the next
frame re-decides from scratch. (Codex v2-pass insight: critical
to avoid `pending_show` lingering past a commit failure.)

**Application is per-CRTC at pageflip retirement**:
`handle_page_flip_complete(output_idx)` reads the retired
`PendingAck.cursor_transition`. For `ShowOnRetire { upload_version, hot_x, hot_y, x, y }`:

1. If `CursorPlane.uploaded_version != upload_version`, call
   `cursor_plane_upload_image(upload_version, w, h, bytes)`
   first. This writes the dumb buffer but does NOT call
   `set_cursor2` — critical: an upload here must NOT make
   the new bytes visible on other CRTCs that haven't retired
   yet. Idempotent for an already-current version.
2. Then call `cursor_plane_show_on_crtc(output_idx, hot_x,
   hot_y, x, y)`. This is the single `set_cursor2(..., Some(...))`
   site, exclusively for the retiring CRTC.

For `HideOnRetire`: call `cursor_plane_hide_on_crtc(output_idx)`.

**Sprite change in steady-state HW** (codex v4-pass — the
`Hw → Hw (sprite changed)` row above can't ride a `PendingAck`
because there's no scene change to drive a compose tick).
v2's empty-damage fast path at `scene.rs:695` skips submit when
nothing else damaged; queueing an `UploadOnly` transition would
starve forever. Resolution: when the effective cursor changes
and ALL active outputs are currently `Hw` mode (no pending
transitions), `define_cursor` / `xfixes_change_cursor_by_name`
fires:

1. `cursor_plane_upload_image(new_version, w, h, bytes)` —
   writes the shared dumb buffer + advances
   `uploaded_version`.
2. `cursor_plane_rebind_visible_crtcs(new_hot_x, new_hot_y, cursor_x, cursor_y)` —
   re-issues `set_cursor2` ONLY on already-visible CRTCs to
   pick up the new bytes + new hotspot, then `move_cursor` to
   the current cursor position on each (set_cursor2 may reset
   position to (0, 0)). Hidden / pending CRTCs are skipped
   (codex v6-pass — without this split, the rebind would
   prematurely show the cursor on not-yet-retired outputs).
   The cursor position is read from the current
   `core.cursor_x` / `core.cursor_y` on the same core thread
   as the protocol handler.

Both steps run synchronously from the protocol handler on the
core thread. The `CursorPlane`'s tracked `uploaded_version`
advances at step 1.

If ANY output is `Mixed` (transition pending), the upload is
queued via the deferred-upload buffer (see "Shared dumb-buffer
upload ordering" below) and consumed when the Mixed window
drains.

**Per-output SW cursor history** (codex v3-pass insight, with
codex v4-pass transactional refinement). v2
currently has `SceneCompositorInner.cursor_prev_pos` as a
**scene-global** field at `scene.rs:240`. This must move
per-output (into each `OutputSceneState` alongside
`last_frame_cursor_mode`) — otherwise multi-output SW damage is
still wrong even with per-output plane visibility correct, because
the prev-rect damage projection on output A could use output B's
cursor position. Implementation note: retire the global field as
part of Phase C/D's scene refactor, not as an afterthought.

**Transactional commit timing** (codex v4-pass): the per-output
`prev_pos` must NOT advance eagerly at scene-build time. A
build that later skips BO acquisition or fails to submit would
lose the previous SW rect that the NEXT frame needs to damage
to clear trailing pixels. Either:

- **(preferred)** carry the new prev_pos as a field on the
  output's `PendingAck` (alongside `cursor_transition`); apply
  it to `OutputSceneState.prev_pos` only when that ack retires
  successfully, OR
- apply it inside the submit path right after the atomic
  commit succeeds, before the function returns.

Failed submit → prev_pos for that output is NOT advanced, and
the next frame still damages the OLD prev rect to clear the
trail. Same discipline as `cursor_transition` itself — both
consumers are "what's actually on the screen for this output".

This rule extends naturally to the cursor-crossing-between-
outputs case: when the cursor moves from output 0 to output 1,
output 0's scene tick the following frame sees `cursor` not
visible on its layout but its own `prev_pos` is still the old
value, so it correctly damages the trailing rect to clear it.
Once that frame retires, output 0's `prev_pos` advances to
`None` (or wherever the scene decided) via the same
PendingAck-applied path.

**Shared dumb-buffer upload ordering** (codex v3-pass — the
plane mutates one shared dumb buffer, so multi-output uploads
need a sequencing rule). The chosen rule:

- **One upload per version, fired at the LAST moment before the
  first show that requires it.** When multiple outputs queue
  `ShowOnRetire { upload_version: V }` for a fresh V,
  the upload is issued once when the first output's PendingAck
  retires; each subsequent output's retire just calls
  `set_cursor2` for its own CRTC (no re-upload). Idempotency
  via comparison of `CursorPlane`'s tracked uploaded version
  against the transition's requested version.
- **Sprite swap during `Mixed`** (sprite changes while at
  least one output still has an in-flight transition): defer
  the upload of the new bytes until **all currently-active HW
  outputs have retired their previous-version showings**.
  Accept that the new sprite is invisible for that bounded
  window (≤ one pageflip per pending output). Reason: the
  shared dumb buffer can't show two different sprites
  simultaneously; uploading immediately would cause already-HW
  outputs to flicker to the new sprite a frame before their
  scene tick decides whether they want it. The deferred-upload
  buffer is small (one pending `Arc<CursorRecord>` slot).

  **Liveness** (codex v4-pass — without this the pending Arc
  could be stranded forever if the wait set drains via
  non-show retirements). Track a per-pending-upload `wait_set`
  = the set of outputs whose retirement of the previous version
  the new upload is waiting on. Each event removes the relevant
  output from the wait set:
  - `ShowOnRetire` retires (previous version observed) → remove.
  - `HideOnRetire` retires (output transitioned away from HW) →
    remove.
  - Output-local recovery (output disable, future per-output
    timeout) → remove.
  - Output removal (hotplug-out) → remove.

  When the wait set becomes empty, fire the deferred upload
  immediately (regardless of why it drained — it's the same
  steady-state-HW path as Phase D "Sprite change in steady-
  state HW" above). If the wait set drains because every
  output went to SW/Hidden (no HW consumers left), the upload
  is still performed so `CursorPlane.uploaded_version` matches
  the latest record; the next `Hidden → Hw` or `Sw → Hw`
  transition then has no stale version to chase.

  Edge case: if a new sprite swap arrives while a previous
  deferred upload is still pending, the second swap REPLACES
  the deferred slot (the wait set is updated to whatever set
  the latest deferred upload is now waiting on). Only the
  latest version is ever uploaded — intermediate versions are
  dropped on the floor with respect to the dumb buffer; their
  `Arc<CursorRecord>` still live for any holder via Phase A's
  refcount discipline.

**Flapping `Sw → Hw → Sw` while other outputs are pending**
(codex v3-pass — needs an explicit policy sentence; codex
v4-pass refined the bound for heterogeneous refresh rates):

Pending retire is allowed to complete; the next frame repairs
to the latest desired mode. Worst-case visible transient: **at
most one stale retired HW interval per output, plus one repair
flip per affected output**. With a 60 Hz + 144 Hz mixed setup
the wall-clock transient can span multiple high-refresh frames
while waiting for the slow output's retire cadence — that's
acceptable; the alternative (cancelling an in-flight commit) is
worse. Concretely: each output has at most one in-flight `PendingAck` so the same output
cannot enqueue a second `Sw → Hw` until the first retires; if
the scene tick AFTER the retirement decides `Hw → Sw`, the next
frame queues a `HideOnRetire` and the plane hides one flip
later. The compositor logs a debug message on each flap so
flapping-under-load shows up in traces, but no special handling.

Transition matrix (read PER OUTPUT — every output goes through
its own row independently):

| Prev → Curr (this output) | Scene-build action for this output | Cursor transition queued post-submit | Post-pageflip action on this CRTC |
|---|---|---|---|
| Hidden → Hw | Omit SW draw | `ShowOnRetire { upload_version, hot_x, hot_y, x, y }` | retire: maybe `cursor_plane_upload_image` (if version != uploaded) + `cursor_plane_show_on_crtc(idx, hot_x, hot_y, x, y)` |
| Sw → Hw | Omit SW draw; damage prev SW pos (BO repair) | `ShowOnRetire { upload_version, hot_x, hot_y, x, y }` | retire: maybe `cursor_plane_upload_image` (if version != uploaded) + `cursor_plane_show_on_crtc(idx, hot_x, hot_y, x, y)` |
| Hw → Hw (move only, steady-state) | Scene tick suppressed; pointer fast path called `cursor_plane_move` directly | — | — |
| Hw → Hw (sprite changed) | No scene change | **Not via PendingAck — see "Sprite change in steady-state HW" below** | (handled synchronously, not at retire) |
| Hw → Sw | Push SW draw + new-rect damage | `HideOnRetire` | `cursor_plane_hide_on_crtc(idx)` |
| Sw → Sw | Today's prev + new damage in BO | — | — |
| Hw → Hidden | Omit SW draw | `HideOnRetire` | `cursor_plane_hide_on_crtc(idx)` |
| Sw → Hidden | Damage prev SW pos so BO clears | — | — |

Mental model: each output independently transitions through
its own state machine. Steady-state HW (`cursor_mode() == Hw`,
fast path active) is reached only when EVERY active output has
retired its `Sw → Hw` show. During the transition window
`cursor_mode()` returns `Mixed`, the pointer fast path is
suppressed, and pointer motion goes through normal scene wake.

**Pointer-motion fast path** (steady-state HW only, core
thread, NOT libinput sender thread — codex v2-pass constraint
on concurrency):
```
// In process_pointer_absolute on the core thread:
if scene.cursor_mode() == CursorMode::Hw {
    platform.cursor_plane_move(x, y);
    // Do NOT wake_for_damage — scene tick is unnecessary.
} else {
    // CursorMode::Mixed or CursorMode::Sw(_) — scene tick
    // owns the cursor this frame. Mixed means at least one
    // output still has a pending Sw → Hw transition flip,
    // and we must not move the plane before it shows.
    self.scene.wake_for_damage();
}
self.dispatch_motion_event(server_state);
```
This is the latency win — single ioctl per pointer event, no
GPU work, no scene cadence dependency. Matches v1's
`backend.rs:6573` call site.

#### Phase D' — Failure recovery & terminal-state policy

When a pageflip never lands, gets dropped, or the output goes
away, we cannot leave the cursor plane half-configured. v2
already stalls `pending_acks` if a pageflip event is lost;
cursor transitions amplify the visible failure (codex v2-pass).
Explicit recovery hooks:

- **`drain_all`** (any path that flushes pending compose state
  without retiring through pageflips): call
  `cursor_plane_hide_all()`, clear all per-output
  `cursor_transition` slots, reset every output's
  `last_frame_cursor_mode` to `Hidden`. Next successful frame
  re-decides from scratch.
- **Output disable** (`drm::Device::disable_output`): hide the
  plane on THAT CRTC via `cursor_plane_hide_on_crtc(idx)`
  before issuing the disable. Drop the output's pending
  cursor_transition. Other outputs unaffected.
- **Shutdown** (`shutting_down` flag set on PlatformBackend):
  `cursor_plane_hide_all()`; do not enqueue further transitions.
- **DRM master loss / VT switch out**: hide plane on all CRTCs;
  invalidate `CursorPlane`'s uploaded-xid version so VT-switch-
  back re-uploads. See Phase F for the full lifecycle hooks.
- **Pageflip timeout (future)**: when v2 grows a timeout
  recovery for stalled `pending_acks`, the timeout handler
  invokes `drain_all`'s cursor-reset path for the stalled
  output, then lets the normal recovery flow proceed.

**Recovery scope** (codex v3-pass tightening — partial vs total
recovery differ; the v3 wording "every CRTC" was too strong for
the output-disable case and could have hidden healthy outputs):

- **Output-local recovery** (output disable, future output-
  specific pageflip-timeout): resets only THAT output's
  `last_frame_cursor_mode` to `Hidden`, hides only THAT CRTC's
  plane, drops only THAT output's pending `cursor_transition`,
  removes that output from any deferred-upload wait set (per
  Phase D's wait-set drain rule — if the wait set empties, the
  deferred upload fires on the remaining HW outputs). Other
  outputs continue undisturbed.
- **Global recovery** (`drain_all`, shutdown, VT-leave,
  DRM-master loss): resets every output's mode + hides every
  CRTC via `cursor_plane_hide_all()` + clears every pending
  `cursor_transition` slot + **drops the deferred-upload slot
  + invalidates `CursorPlane.uploaded_version`** (codex
  v5-pass — without this rule, VT-leave with a deferred upload
  pending would either strand the Arc or attempt to ioctl on
  a kernel that's already taken the device away).

  Critical: global recovery does NOT fire the deferred upload
  on its way out, even though the wait set is technically
  "draining via output removal". The drain-fires-upload rule
  in Phase D applies only when individual outputs leave the
  wait set during normal operation; mass clear under global
  recovery skips the upload entirely. The next successful
  acquire/modeset (VT-acquire path) re-uploads from the
  latest `Arc<CursorRecord>` via the normal Phase-D rule.

Invariant after either: every output cleared by this recovery
sees an empty `cursor_transition` slot + `Hidden` mode +
plane-hidden state. Its next successful scene tick starts from
a clean position and issues a fresh `Hidden → Sw` or
`Hidden → Hw` transition based on its own `CursorAssignment`
decision. Outputs NOT touched by partial recovery keep their
existing mode + history. After global recovery,
`CursorPlane.uploaded_version` is invalid until the next
upload — the next `Sw → Hw` or `Hidden → Hw` retire issues a
fresh upload via the Phase-D rule.

### Phase E — I6 lifetime / retirement

Codex review insight: the "no DrawableId reference" claim is
correct **only after** the cursor bytes are in the dumb buffer.
Getting them there is the part that needs discipline.

Two options, listed in order of preference:

1. **CPU-bytes-alongside-pixmap (preferred — Phase A change).**
   Phase A keeps a canonical `Vec<u8>` of BGRA cursor bytes
   alongside the v2 sprite Pixmap. The HW upload path is then a
   plain memcpy (`cursor_bytes` → dumb buffer's `mmap`). No GPU
   readback, no fence/ticket. The pixmap is for SW composition;
   the CPU bytes are for plane upload. Two pixel storages for
   one cursor — acceptable: cursors are ≤ 64×64 = 16 KB BGRA.

2. **GPU readback path (only if option 1 turns out structurally
   bad).** If the sprite lives only as a GPU pixmap, upload-time
   readback must obey the existing render-fence discipline: wait
   for the sprite's last render ticket or route through
   `RenderEngine::get_image` (`engine.rs:1748`) which already
   does a fenced readback. CPU-side staging + sync wait.
   Strictly worse than option 1 but stays I6-compliant.

After upload, no persistent reference from the plane to a
`DrawableId`. The sprite Pixmap is freed via the normal
`DrawableStore` decref path; doesn't matter whether the plane is
currently using its pixels because the dumb buffer is the
authoritative source for the kernel.

### Phase F — Modeset / hotplug / VT-switch / suspend lifecycle hooks

Codex review insight — these don't fall out of the existing
`CursorPlane` shape and need explicit handling:

- **Output disable** (`drm::Device::disable_output`): hide the
  cursor plane on that CRTC **before** issuing the disable
  (`cursor_plane_hide_on_crtc(idx)`). Otherwise the kernel may
  hold a stale plane→fb binding through the disable. Drop the
  output's pending `cursor_transition` if any (Phase D' overlap).
- **Output enable / hotplug**: on a newly active CRTC,
  **validate cursor-plane size + format capability** for that
  CRTC (per-CRTC plane capabilities can differ on multi-IP-
  block systems — Intel iGPU + dGPU mix, multi-zone Mali) and
  cache that capability in the per-CRTC plane state. **Do NOT
  rebind / `set_cursor2(..., Some(...))` immediately on the new
  CRTC** (codex v7-pass — that would be an out-of-band show
  bypassing the per-output retirement gate). If HW mode is
  active on the existing outputs, queue a `ShowOnRetire`
  transition on the new CRTC's first compose tick; its
  visibility flip retires through the same per-CRTC PendingAck
  path as every other show, which is the sole call site for
  `set_cursor2(..., Some(...))`.
- **Full modeset**: invalidate
  `CursorPlane.uploaded_version` (set to a sentinel that
  cannot match any `Arc<CursorRecord>.version`) so the next
  Phase-D retire upload-compares non-equal and re-uploads.
  Force a re-show on every active CRTC via fresh `ShowOnRetire`
  transitions. The cursor record itself (CPU bytes + version)
  is NOT mutated — those are immutable per Phase A.
- **VT switch (codex v2-pass spell-out — common KMS path,
  subsumes generic suspend/resume on most desktop sessions)**:
  - **VT leave** (`DRM_IOCTL_DROP_MASTER` from `logind` /
    seat handover): hide plane on all CRTCs immediately
    (synchronous, not pageflip-retired — the kernel is taking
    the device away); clear all pending cursor_transition slots;
    keep the `CursorPlane` struct + dumb buffer allocation but
    treat the kernel-side binding as stale.
  - **VT acquire** (`SET_MASTER` regained): treat as a full
    modeset — invalidate `CursorPlane`'s tracked upload version,
    queue `ShowOnRetire` transitions on every active output's
    next compose. The upload from the latest cursor record's
    versioned blob fires through the normal Phase-D ordering rule
    (one upload at the first retire that requires the new
    version). Let pageflip retirement bring the cursor back.

    **Current-record read at compose time** (codex v6-pass):
    the post-acquire compose reads the **current effective
    `Arc<CursorRecord>`** at that moment, not any Arc that may
    have been captured before recovery. If a fresh
    `RenderCreateCursor` / `DefineCursor` /
    `XFixesChangeCursorByName` lands between VT-leave (which
    dropped the deferred slot) and the next compose tick, the
    new record's version is what gets uploaded. No transient
    record can be "lost" by the dropped deferred slot — the
    upload source is always whatever the effective-cursor
    lookup returns at the moment of upload.

    **Acquire-failure path** (codex v3-pass): if the first
    post-acquire compose fails on any output, Phase D's
    "populate `cursor_transition` only after successful commit"
    rule prevents a stale `ShowOnRetire` from queueing for that
    output. If the upload itself fired before commit (the
    deferred-upload path normally fires AT retire, so this
    shouldn't happen, but a future code path that pre-uploads
    speculatively would risk it), Phase D' global recovery
    applies: `cursor_plane_hide_all()` + invalidate uploaded
    version. The next successful compose on any output
    re-triggers the upload via the normal Phase-D rule.
- **Suspend/resume**: dumb buffer typically survives the
  kernel side, but the user-mapped `mmap` does not always
  reliably persist across deep ACPI S3. Safest: drop the
  `CursorPlane` on suspend, recreate on resume, force re-upload
  via the same VT-acquire path.
- **DRM master loss without VT switch** (rare — usually only
  on `seatd` bugs / udev races): same as VT leave.
- **GPU reset / hang recovery (future)**: cursor plane is
  decoupled from the GPU (dumb buffer, not GEM-shared with the
  render path), so a render-side reset doesn't invalidate plane
  state. Still: when v2 grows GPU-reset handling, the cursor
  plane's `hw_cursor_xid` should NOT be cleared by the reset
  path. Document this when the reset path lands.
- **Per-CRTC visibility tracking** (consumes the Phase B
  refactor): `cursor_plane_show_on_crtc(idx)` /
  `cursor_plane_hide_on_crtc(idx)` flip per-CRTC state. The
  `cursor_plane_hide_all()` helper exists for the failure
  paths (Phase D') and shutdown.

v2 has no dynamic hotplug or VT-switch path today, but naming
these hooks keeps the strategy from silently depending on
boot-only topology. Implementation lands when the corresponding
PlatformBackend lifecycle hook lands (won't all be one PR);
the cursor-plane integration tracks each as it arrives.

### Phase G — Tests

Pure-function tests for the strategy decision (no Vk, no DRM):
- `cursor_assignment_picks_hw_for_small_sprite_when_plane_available`
- `cursor_assignment_picks_sw_when_extent_exceeds_64`
- `cursor_assignment_picks_sw_when_plane_unavailable`
- `cursor_assignment_hidden_when_cursor_off_output`

`build_scene` tests:
- `build_scene_returns_hw_assignment_with_no_cursor_draw`
- `build_scene_returns_sw_assignment_with_cursor_draw_and_damage`

`SceneCompositor` transition tests (ordering is the regression
gate per codex's review):
- `transition_sw_to_hw_damages_previous_sw_pos_in_scene_build`
- `transition_sw_to_hw_queues_plane_show_for_next_pageflip` —
  pin that show is NOT called during scene build, only at
  pageflip retirement.
- `transition_hw_to_sw_pushes_sw_draw_and_queues_plane_hide`
- `transition_hw_to_hw_sprite_change_re_uploads_without_repaint`
- `pointer_motion_in_hw_mode_does_not_wake_scene_damage`

Multi-output transition tests (codex v2-pass regression gates —
prove the per-output state machine, not just the single-output
happy path):
- `two_output_sw_to_hw_retiring_output_0_does_not_show_cursor_on_output_1` —
  the load-bearing test for the per-output PendingAck design.
  Output 0 retires its show; output 1's transition is still
  queued; cursor plane must NOT be visible on output 1 yet.
- `commit_failure_after_sw_to_hw_decision_leaves_no_pending_show` —
  inject a compose/commit failure on a frame that decided
  `Sw → Hw`; assert no `cursor_transition` is left in any
  PendingAck slot.
- `drain_all_with_pending_transition_hides_plane_and_clears_state` —
  call `drain_all` with a `Sw → Hw` transition queued; assert
  all CRTCs hidden + every output's cursor mode reset to
  `Hidden` + every cursor_transition slot empty.
- `fast_path_move_during_pending_sw_to_hw_does_not_move_plane` —
  cursor_mode() returns `Mixed` while a transition is pending;
  pointer fast path falls back to scene wake; no
  `cursor_plane_move` ioctl fires.
- `sprite_replaced_while_hw_active_upload_version_orders_correctly` —
  swap the effective cursor while in steady-state HW (all
  outputs already Hw mode, no pending transitions); assert
  the older `Arc<CursorRecord>` is still valid for any captured
  reference, and the synchronous `cursor_plane_upload_image` +
  `cursor_plane_rebind_visible_crtcs` pair advances
  `CursorPlane.uploaded_version` and rebinds only visible CRTCs
  (codex v6-pass — hidden CRTCs must not be touched).
  Companion test
  `sprite_replaced_while_mixed_defers_upload_until_wait_set_empty`
  pins the deferred-upload path: a swap during Mixed populates
  the deferred slot, the wait set drains as transitions retire,
  and the upload fires when the set empties.
- `upload_image_does_not_show_on_unretired_crtcs` (codex v6-pass
  regression gate) — call `cursor_plane_upload_image` while one
  CRTC is HW and another is still SW; assert no `set_cursor2`
  is issued on the SW CRTC. This pins the load-bearing
  upload/show split.

`PlatformBackend` lifecycle tests:
- `output_disable_hides_cursor_plane_on_that_crtc`
- `cursor_plane_per_crtc_visibility_independent`
- `vt_leave_hides_all_crtcs_synchronously`
- `vt_acquire_queues_show_on_every_active_output_next_compose`

Hardware smoke (post-merge, gating default-on flip):
- Pointer hover over gtk3-demo gradient widgets — pre-fix v1
  showed severe lag; v2 + HW cursor should match v1.
- mate-control-center hover (the original v1 motivator).
- Multi-output smoke if available (this machine has two
  monitors as of the multi-output clamp fix `15e8300`):
  cursor crossing between monitors must keep working under HW
  cursor mode.

### Phase H — Rollout

`YSERVER_V2_HW_CURSOR=1` env-gated initially, default OFF.

v2 is already the boot default and this change touches
presentation ordering, cursor damage, and DRM side effects.
Default-off with loud-but-nonfatal SW fallback on `CursorPlane`
init failure is the right first landing shape — stricter than
v1's pattern because v1 was the *only* backend at the time,
while v2 default-on plus broken HW cursor would degrade every
user's experience.

Gating criteria for flipping default-on:
- All Phase G unit tests passing.
- One clean MATE-with-compositor smoke.
- One clean XFCE-with-compositor smoke.
- One clean Cinnamon smoke (post codex's Cinnamon work).
- Multi-output smoke: cursor crosses between monitors under
  HW mode without artefacts.
- One regression run of the full v2 lib test suite.

## Minimum viable patch

**A + B + C + D + E + G (subset).** Phase F's lifecycle hooks
and the full Phase G test list can land incrementally; the
critical tests for the first landing are the SW↔HW transition
ordering tests + the pointer-motion-no-wake test (those gate
correctness, not perf).

A is a separate prereq with independent value (theme-correct
cursors). The bare HW cursor latency win is real even with the
default 16×16 arrow, but pairing the two lands cleanly.

LOC estimate revised: ~250-300 (was ~110 in v1 of this plan
before the layering split + lifecycle hooks). Larger because
the strategy boundary now spans build_scene / SceneCompositor /
PlatformBackend, and the per-CRTC visibility + lifecycle hooks
add real surface.

## What NOT to do

- **No bypass side path.** Cursor stays a scene entry; HW plane
  is the render target choice for that entry. The pointer-motion
  fast path is permitted because it operates on an *already-
  scene-assigned* HW cursor — it just skips the build_scene
  cadence for movement-only updates. Mode transitions still flow
  through the scene.
- **No `CursorPlane` sharing between v1 and v2.** Each backend
  owns its own.
- **No atomic-plane API upgrade yet.** `set_cursor2` + legacy
  `move_cursor` work on every mainstream GPU. Atomic-plane is a
  Stage 5 nice-to-have, not blocking.
- **Don't pre-emptively support sizes > 64×64.** Universal HW
  minimum is 64×64 on Intel/AMD/Mali iGPUs since ~2010. X11
  cursor themes are ≤ 32×32 in practice. The SW fallback for
  larger cursors stays as today's full SceneCompositor path.
- **Don't make `build_scene` perform DRM side effects.** The
  function currently produces pure scene/damage/snapshot data;
  any DRM call from inside it breaks the testability of the
  strategy decision (codex review insight).
- **Don't show the plane before the SW cursor's pixels are off
  the displayed scanout BO.** Gate visibility flips on pageflip
  retirement, not scene build (codex review insight).
- **Don't bypass the render-fence discipline for cursor uploads
  from a GPU pixmap.** Either the Phase A CPU-bytes path or a
  fenced readback via `RenderEngine::get_image` — never a raw
  memcpy of GPU-managed memory.

## Codex review revisions

### v1 → v2 (2026-05-19, first codex pass)

1. **Strategy boundary**: was "option 1 (`platform.try_assign_cursor_plane` from build_scene)" vs "option 2 (trait object on SceneCompositor)". Codex flagged both as wrong shape: option 1 puts side effects in pure scene-build, option 2 is heavyweight up front. Resolved by the three-layer split in "Architectural shape" above. Plus the **pointer-motion fast path** that codex flagged as critical — without it, HW cursor stays compositor-cadence-bound and we lose the latency benefit entirely.

2. **Transition ordering**: was "show plane in scene build, damage previous SW pos same-frame". Codex flagged the one-frame double-cursor hazard: the previously-presented scanout BO still contains the SW cursor pixels until the next flip lands. Resolved by gating plane visibility flips on pageflip retirement events, not scene build (Phase D revised matrix).

3. **I6 lifetime**: was "simple memcpy from sprite Pixmap to dumb buffer". Codex flagged that if the sprite lives only as a GPU `Pixmap`, the readback must obey the existing render-fence discipline. Resolved by Phase A capturing CPU BGRA bytes alongside the v2 pixmap, so HW upload is plain CPU→dumb-buffer with no GPU involvement.

4. **Lifecycle gaps**: original plan didn't address output disable/enable, modeset, suspend/resume, per-CRTC visibility. Codex flagged that `CursorPlane::visible` is global at `cursor_plane.rs:43`, which is weak once outputs can appear/disappear. Resolved by Phase F + the Phase B per-CRTC refactor.

5. **Rollout**: env-gated off-by-default confirmed correct, with tighter gating criteria for flipping default-on (Phase H).

### v2 → v3 (2026-05-19, second codex pass)

Codex re-reviewed v2 and flagged that the per-CRTC visibility
refactor from v1 → v2 was correct in principle but the
transition state machine itself was still global. Material
changes:

1. **Per-output transition state machine** (the load-bearing
   v3 change). v2's `tick_one_output` retires frames per output;
   global `pending_show_at_next_flip` would let output A's
   show fire while output B's BO still contained SW cursor
   pixels — same double-cursor hazard returns on output B.
   Resolved: every output carries its own
   `last_frame_cursor_mode`; transitions are queued by attaching
   a new `cursor_transition: Option<CursorTransition>` field to
   each output's `PendingAck`, consumed per-CRTC at that
   output's pageflip retirement. `cursor_mode()` returns `Mixed`
   while any output's transition is in flight, suppressing the
   pointer fast path until steady-state.

2. **Transition payload gated on successful submit**. Codex
   flagged that if compose/atomic commit fails after the scene
   tick decided `Sw → Hw`, a `pending_show` flag would linger
   with no pageflip to consume it. Resolved by populating
   `PendingAck.cursor_transition` only after the per-output
   commit succeeds — failed commits drop the transition and the
   next frame re-decides.

3. **Failure-recovery policy (new Phase D')**: `drain_all`,
   output disable, shutdown, VT switch, DRM master loss, future
   pageflip-timeout recovery all need explicit cursor-plane
   reset. Codex flagged that without this, `Sw → Hw` can leave
   a BO without SW pixels while the plane never shows. Resolved
   by enumerating the recovery hooks and the invariant they
   leave behind (plane hidden + every output's mode `Hidden`).

4. **Pixel-format invariant** (Phase A). Codex asked the plan
   to pin canonical little-endian BGRA for DRM `ARGB8888` with
   alpha semantics matching the SW path, so HW and SW cursors
   render identically. Resolved by adding the invariant
   explicitly + an assertion-style test on a fixed sprite.

5. **Versioned immutable cursor records** (Phase A). Codex
   flagged that theme reload / XFixes replacement mutating the
   bytes in place would race with pointer grabs holding an
   effective-cursor reference. Resolved by making each record
   immutable + reference-counted (`Arc<CursorRecord>`); fresh
   bytes allocate a fresh record with a fresh version.

6. **VT switch terminology** (Phase F). Codex asked for
   explicit VT leave/acquire hooks (the common KMS handover
   path; suspend/resume is a strict subset). Resolved by
   spelling out the VT leave (synchronous hide-all + clear
   pending) + VT acquire (treat as full modeset, queue
   `ShowOnRetire` on every active output's next compose) flow.

7. **Pointer fast path thread constraint**. Codex confirmed
   the core-thread pattern works (v2's single-threaded core
   owns DRM state) but warned not to move the DRM cursor
   ioctls into the libinput sender thread. Resolved by noting
   the thread constraint in the fast-path code block.

8. **New regression tests** (Phase G). Five new tests cover
   the multi-output state machine: two-output Sw→Hw retiring
   ordering, commit-failure-no-pending-show, drain_all-with-
   pending-transition, fast-path-suppressed-while-mixed, and
   sprite-replacement-version-ordering. Plus two new lifecycle
   tests for VT switch.

Codex's net verdict on v2 of the plan: "directionally sound and
fixes the major architectural problems. The main remaining risk
is global cursor transition state leaking across independently-
retired outputs." That risk is what v3 above resolves.

### v3 → v4 (2026-05-19, third codex pass)

Codex re-reviewed v3 and acknowledged that the per-output
PendingAck design fixes the load-bearing leak. Seven tightenings
on top of that:

1. **Shared dumb-buffer upload ordering rule** (Phase D
   addition). v3 left it ambiguous when uploads fire during
   `Mixed`. Resolved: one upload per version, at the first
   retire that requires it; sprite swap during Mixed defers the
   new upload until all currently-active HW outputs have retired
   their previous-version showings (≤ one pageflip per pending
   output, acceptable bounded window).

2. **Retire global `cursor_prev_pos`** (Phase D addition). v2's
   `SceneCompositorInner.cursor_prev_pos` is scene-global at
   `scene.rs:240`; v3 made `last_frame_cursor_mode` per-output
   but didn't call out that `cursor_prev_pos` is part of the
   same migration. Resolved by an explicit note in Phase D
   that the field moves into each `OutputSceneState`.

3. **Flapping `Sw → Hw → Sw` policy** (Phase D addition).
   Codex asked for one sentence specifying behaviour when an
   output decides to revert mid-transition. Resolved: pending
   retire is allowed to complete; the next frame repairs to
   the latest desired mode. One bounded transient frame
   accepted, debug-log on each flap.

4. **D' partial vs total recovery scope** (Phase D'
   tightening). v3 wording said "every CRTC" as the post-
   recovery invariant, which is wrong for output-disable
   (would have hidden healthy outputs). Resolved by splitting
   D' into output-local recovery (output disable, future
   per-output pageflip-timeout) vs global recovery (drain_all,
   shutdown, VT-leave, master loss).

5. **`Arc<CursorRecord>` ordering invariants** (Phase A
   addition). Codex flagged that immutability + refcount
   isn't enough without spelling out: (a) monotonic versions
   compared by value, not pointer identity; (b) xid-map
   mutation and scene effective-cursor reads on the same
   thread (core); (c) test that replacement never mutates old
   bytes. Resolved with explicit invariants under Phase A §3.

6. **VT-acquire failure path** (Phase F addition). v3 didn't
   spell out what happens if the first post-acquire compose
   fails after queuing intended `ShowOnRetire`. Resolved:
   Phase D's "populate only after successful commit" rule
   prevents stale show queuing for any failed-commit output;
   if a future code path pre-uploads speculatively, D' global
   recovery applies and the next successful compose re-uploads
   via the normal Phase-D rule.

7. **(Confirmation, no plan change)** `Mixed` fast-path
   suppression has no race on the single-threaded core; the
   semantic invariant ("`cursor_mode() == Hw` only when every
   active output is retired HW with no pending transition")
   matches v3's wording. No edit needed.

Codex's net verdict on v3 of the plan: "Moving `cursor_transition`
onto each output's `PendingAck`, populated only after that
output's commit succeeds and consumed only on that output's
pageflip retirement, is the right design for the independent-
retirement hazard." All other findings are tightenings rather
than structural changes — v4 absorbs them.

### v4 → v5 (2026-05-19, fourth codex pass)

Codex re-reviewed v4 and acknowledged the v3→v4 tightenings
were mostly captured correctly, but flagged four load-bearing
gaps + one wording fix that prevented v4 from being landable
as an implementation plan. v5 absorbs:

1. **`Hw → Hw (sprite changed)` was tied to `UploadOnly` via
   `PendingAck`, which starves under v2's empty-damage skip**
   at `scene.rs:695` — no scene change means no flip, no
   retirement, no upload. Resolved: in steady-state HW with
   no pending transitions, sprite changes upload **synchronously**
   from `define_cursor` / `xfixes_change_cursor_by_name`; the
   shared dumb buffer covers every CRTC's existing binding. The
   `UploadOnly` PendingAck variant is removed; the Phase D
   matrix row redirects to the new "Sprite change in steady-
   state HW" subsection.

2. **Deferred-upload liveness**: v4's "defer until all HW
   outputs retire previous version" rule could strand a pending
   `Arc<CursorRecord>` forever if the wait set drained via
   non-show events. Resolved by enumerating the wait-set
   shrinkage events (ShowOnRetire / HideOnRetire / output-local
   recovery / output removal) and firing the deferred upload
   when the set becomes empty regardless of why it drained.
   Edge case for stacked sprite swaps (second swap arrives
   while first still deferred) added: the deferred slot is
   replaced with the latest, intermediate versions skip the
   dumb buffer but Arcs remain valid.

3. **`cursor_prev_pos` transactional commit timing**: v4
   moved the field per-output but didn't specify when it
   advances. Eager-at-build-time would lose the prev rect on
   failed submit. Resolved: prev_pos becomes a `PendingAck`
   field, applied to `OutputSceneState.prev_pos` only on
   successful retirement. Same discipline as `cursor_transition`.
   The cursor-crossing-between-outputs case (output 0 keeps
   its trailing prev rect for one more frame) falls out
   naturally from this rule.

4. **Phase B API listing still had `cursor_plane_show()` /
   `cursor_plane_hide()` (global)**: a global show is exactly
   the failure mode the per-output state machine eliminates.
   Resolved by deleting those two from the hook list and
   keeping only `cursor_plane_show_on_crtc(idx)` /
   `_hide_on_crtc(idx)` + `cursor_plane_hide_all()` for global
   recovery. No global show hook anywhere.

5. **"One bounded transient frame" wording too strong** for
   heterogeneous refresh rates (60 Hz + 144 Hz outputs retire
   at different rates). Reworded as "at most one stale retired
   HW interval per output, plus one repair flip per affected
   output" with explicit acknowledgement that wall-clock
   transient can span multiple high-refresh frames.

Codex's net verdict on v4: "close, but not quite landable as an
implementation plan. I'd make the four edits above first,
especially `UploadOnly` progress and deferred-upload liveness,
because those are load-bearing rather than cosmetic." v5 is the
result.

### v5 → v6 (2026-05-19, fifth codex pass)

Codex re-reviewed v5 and called it "nearly landable" with three
remaining fixes — one medium (global-recovery vs deferred-
upload semantics), one low/medium (API signature precision),
one low (stale `UploadOnly` text). v6 absorbs:

1. **Global recovery vs deferred-upload slot** (Phase D'
   addition). v5's wait-set drain rule said "drain → fire
   upload" but didn't say what `drain_all` / shutdown / VT-leave
   / DRM-master loss do to the deferred slot. Critical: VT-leave
   must NOT fire the deferred upload (DRM master is gone, ioctl
   will fail). Resolved by an explicit rule: global recovery
   drops the deferred slot + invalidates `uploaded_version` +
   does NOT fire the upload on its way out. The drain-fires-
   upload rule only applies to individual outputs leaving the
   wait set during normal operation. Next acquire/modeset
   re-uploads from the latest `Arc<CursorRecord>` via the
   normal Phase-D rule.

2. **`cursor_plane_upload` API signature widened** (Phase B
   correction). v5's `(bytes, hot_x, hot_y)` was too skinny —
   the implementation needs `width` and `height` for
   `CursorPlane::load_image` and `version` for the upload-
   deduplication comparison. Resolved by widening to
   `(version, width, height, bytes, hot_x, hot_y) -> Result<()>`
   and documenting that the platform side follows up with
   `cursor_plane_move(last_x, last_y)` per CRTC because
   `set_cursor2` may reset cursor position (v1 does this at
   `backend.rs:2173` after `hw_cursor_refresh`).

3. **Stale `UploadOnly` text removed** (Phase D enum stub +
   Phase G test). v5 removed the variant from the matrix and
   revision notes but left it in the inline enum example
   (~line 305) and one test description (~line 702). Pure
   code-spec drift. Resolved by deleting the variant from the
   enum (with a comment explaining the alternative path) and
   rewording the test to describe the synchronous upload +
   the deferred-upload companion test.

Codex's net verdict on v5: "nearly landable, but I would fix
the global-recovery/deferred-upload rule before implementation.
The rest is mostly API precision and stale wording." v6 is the
result.

### v6 → v7 (2026-05-19, sixth codex pass)

Codex re-reviewed v6 and flagged one load-bearing finding +
two stale naming fixes + one missing sentence:

1. **`cursor_plane_upload` was specified to rebind every active
   CRTC via `set_cursor2`** — but in legacy DRM `set_cursor2(crtc,
   Some(...))` IS the show/bind operation. Rebinding all CRTCs
   from inside an upload would prematurely show the HW cursor on
   outputs whose `Sw → Hw` transition hasn't retired yet, exactly
   the multi-output double-cursor hazard the entire per-output
   `PendingAck` design closed. Resolved by splitting the API:
   - `cursor_plane_upload_image(version, w, h, bytes)` — buffer
     + version only. NO `set_cursor2`.
   - `cursor_plane_show_on_crtc(idx, hot_x, hot_y, x, y)` — the
     sole `set_cursor2(..., Some(...))` site; called per-output
     from pageflip retirement.
   - `cursor_plane_rebind_visible_crtcs(hot_x, hot_y)` — for
     steady-state sprite change, rebinds only already-visible
     CRTCs; hidden / pending CRTCs untouched.
   New Phase G test `upload_image_does_not_show_on_unretired_crtcs`
   pins this as a regression gate.

2. **`CursorAssignment::Hw` field naming** (Phase C). v6 still
   had `upload_xid_changed: Option<u32>`, which contradicts the
   Phase A version-keyed `Arc<CursorRecord>` design. Resolved
   by renaming to `record_version: u64` with explicit "compared
   by value, not pointer identity" callout. Added `hot_x`,
   `hot_y` so the strategy decision feeds the retire path
   everything it needs.

3. **Phase F modeset wording** ("`hw_cursor_xid = None` + bump
   `upload_bytes_version`") was v1-backend speak that contradicts
   v2's invariant (cursor records are immutable per Phase A;
   versions are monotonic, never "bumped" on the record).
   Resolved by rewording to "invalidate
   `CursorPlane.uploaded_version`" — the record itself is
   untouched; only the plane's tracking is reset.

4. **VT-acquire reads the current effective `Arc<CursorRecord>`
   at compose time** (codex v6-pass missing sentence). Added an
   explicit clause to Phase F's VT-acquire section: a fresh
   cursor change landing between recovery and the next compose
   is picked up because the upload source is whatever the
   effective-cursor lookup returns AT upload time, not a
   captured Arc.

Plus a standardize-on-`upload_version` rename across the matrix
and the `CursorTransition` enum (was inconsistent between
`upload_bytes_version` and `upload_version`).

Codex's net verdict on v6: "Not fully landable yet. v6 closes
the three named v5 deltas in wording, but it exposes one
load-bearing gap... Fix the upload-vs-show split first."

Implementation order (codex v6-pass recommendation, kept for
v7): ship Phase A as a standalone PR first (independent user
value — theme-correct cursors); then a small Phase B PR
landing the `CursorPlane` per-CRTC API + the upload/show split
ahead of the SceneCompositor wiring; then Phases C/D/D'/F on
top.

### v7 → v8 (2026-05-19, seventh codex pass)

Codex re-reviewed v7 and ack'd the upload/show split as correct
and complete. Three small fixes flagged before "implementation-
ready":

1. **`cursor_plane_rebind_visible_crtcs` needs to reposition
   after the rebind.** v7 said prior per-CRTC coords were
   "already in plane state", but that's kernel state and
   `set_cursor2` may reset position to (0, 0). v1 follows
   `hw_cursor_refresh` with `hw_cursor_move()` at
   `backend.rs:2173` for this reason. Resolved by widening the
   signature to `cursor_plane_rebind_visible_crtcs(hot_x,
   hot_y, x, y)` — each per-CRTC `set_cursor2` is immediately
   followed by `move_cursor(crtc, x, y)`.

2. **Phase F output-enable wording sounded like immediate
   show.** v7 said "rebind the currently-uploaded cursor image
   to that CRTC" before also saying "queue ShowOnRetire" — the
   word "rebind" implied an immediate `set_cursor2(..., Some)`
   that would bypass the per-output retirement gate. Resolved
   by rewording to "validate cursor-plane size + format
   capability for that CRTC and cache it; **do NOT rebind
   immediately**; show solely via the first retired
   `ShowOnRetire`."

3. **CursorTransition enum stub drift.** Top-of-Phase-D enum
   sketch still showed `ShowOnRetire { upload_version: u64 }`
   while the matrix and retire-handler text correctly used
   `ShowOnRetire { upload_version, hot_x, hot_y, x, y }`.
   Resolved by widening the enum stub to match — fields
   spelled out with one-line comments per field.

Codex's net verdict on v7: "Not quite landable as written: fix
the rebind reposition requirement and the Phase F output-enable
wording. After that, I'd call it implementation-ready." v8 is
the result; the three fixes are pure mechanical edits without
new design content.

## Related

- Spec `docs/superpowers/specs/2026-05-15-rendering-model-v2.md`
  §I7, §820-841, §887-890.
- v1 implementation: `crates/yserver/src/kms/cursor_plane.rs`,
  `crates/yserver/src/kms/backend.rs:792-803,1996-2230,6571,7662`.
- v2 SW cursor path: `crates/yserver/src/kms/v2/scene.rs:232,254-259,1149-1207,670`.
- v2 stubs to fill in Phase A: `crates/yserver/src/kms/v2/backend.rs:4162-4209`.
- Multi-output pointer clamp fix (related, recently shipped):
  `15e8300` — makes multi-output smoke meaningful for Phase G.
- Codex reviews of plan v1 + v2 + v3 + v4 + v5 + v6 + v7: this conversation, 2026-05-19.
