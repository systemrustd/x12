# COW-authoritative scene + reparent redirect reconciliation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop v2's scene from inventing per-toplevel layers above COW (Phase 1), then fix the upstream bug where redirected children keep stale backings after being reparented out of a redirected subtree (Phase 2). Together these align yserver with Xorg's compositor contract.

**Architecture:**
Phase 1 (2a) gates the top-level scene walk on whether COW is registered. When a compositor has registered to paint COW, scene emit = `root + COW + cursor` only — Xorg's "compositor reads backings, presents into COW, X server doesn't itself compose" contract. Phase 2 mirrors `compUnredirectOneSubwindow` + `compRedirectOneSubwindow` from `/home/jos/Projects/xserver/composite/compwindow.c:453-454`: on `ReparentWindow`, reconcile the moved window's redirect status against the new parent's `RedirectSubwindows` state. Reuses the existing `release_redirected_backing` trait method (`crates/yserver-core/src/backend/trait_def.rs:439`) and the existing `teardown_redirect_for_window` helper (`crates/yserver-core/src/core_loop/process_disconnect.rs:298`), threading `origin` through.

**Tech Stack:** Rust, existing v2 store/scene/backend types.

**Reference context for executors:**
- `docs/wip-cow-shadow-hunt-2026-05-20.md` — original investigation; "Picking back up tomorrow" section enumerates option 1, which is Phase 1.
- Xorg source `/home/jos/Projects/xserver/composite/compwindow.c:453-454` — load-bearing reference for Phase 2.
- Existing protocol-level teardown helper: `crates/yserver-core/src/core_loop/process_disconnect.rs:298` (`teardown_redirect_for_window`).
- Existing backing-allocation helper: `crates/yserver-core/src/core_loop/process_request.rs:511` (`activate_redirect_backing_for`).
- Per AGENTS.md:15 — **work on a feature branch for phases.** Branch off `rendering-model-v2`; squash-merge on completion (ask before merging).
- AGENTS.md:11 — **regular clippy, not pedantic.**

---

## Branch setup

Before Task 1:

```bash
git checkout rendering-model-v2
git pull --ff-only 2>/dev/null || true
git checkout -b cow-authoritative-mode
```

The four pre-existing working-tree diffs (Justfile recipe, damage_fanout trace, AddTraps damage fix, scanout-bundle dump) are diagnostic/standalone fixes from the investigation; commit them as separate focused commits before Task 1 if they're still uncommitted, OR carry them as the branch's initial state — either way they don't conflict with anything below.

---

## File Structure

**Phase 1 (2a — scene-walk gating):**
- Modify: `crates/yserver/src/kms/v2/scene.rs` — gate the top-level walk in `build_scene` on `cow.is_some()`. Two existing tests must be updated: `build_scene_appends_cow_above_top_levels` (scene.rs:3906) and `build_scene_cow_is_below_cursor` (scene.rs:4025), both of which currently assert behavior that's intentionally obsolete under 2a.

**Phase 2 (reparent redirect reconciliation):**
- Modify: `crates/yserver-core/src/core_loop/process_disconnect.rs` — thread `origin: Option<OriginContext>` through `teardown_redirect_for_window` so the reparent path can pass a real origin (existing callers continue passing `None`).
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` — wire reconciliation into `handle_reparent_window` (line 7092 region), gated on `backend.supports_redirect_activation()`. Uses the threaded `teardown_redirect_for_window` for unredirect, the existing `activate_redirect_backing_for` for redirect, and the existing `flip_redirect_target_mode` for mode swaps.

**Phase 3 (verification only):**
- Run cargo test, regular clippy, rendercheck, hardware smoke.

---

## Task 1: Phase 1 — pre-Phase-1 cleanup of the two existing COW tests

**Why first:** the existing tests at scene.rs:3906 (`build_scene_appends_cow_above_top_levels`) and scene.rs:4025 (`build_scene_cow_is_below_cursor`) both assert "top-level + COW + cursor" draw shapes. Under 2a, COW being present means top-levels are stripped. These tests will fail Phase 1 implementation if not updated first, and the cleanest path is to repurpose each as a cow=None test (preserving the legacy assertion) and write the new cow=Some test as a separate Task 2 step.

**Files:**
- Modify: `crates/yserver/src/kms/v2/scene.rs:3906` — `build_scene_appends_cow_above_top_levels`.
- Modify: `crates/yserver/src/kms/v2/scene.rs:4025` — `build_scene_cow_is_below_cursor`.

- [ ] **Step 1.1: Read both existing tests + the `alloc_stub_window` helper**

```bash
sed -n '2980,3066p' crates/yserver/src/kms/v2/scene.rs   # alloc_stub_window
sed -n '3906,4120p' crates/yserver/src/kms/v2/scene.rs   # the two tests
```

Key facts confirmed:
- `alloc_stub_window` takes `(store, windows_v2, xid, x, y, w, h, parent, mapped)` and returns `()`. Look up the resulting DrawableId afterward via `store.lookup(xid)`.
- The root constant is `yserver_core::resources::ROOT_WINDOW: ResourceId = ResourceId(0x100)` (yserver-core/src/resources.rs:30). **Note the `yserver_core::` prefix** — these tests live in the `yserver` crate, not `yserver-core`; the existing v2 code uses `yserver_core::resources::COMPOSITE_OVERLAY_WINDOW` at backend.rs:2923.
- The canonical fixture setup pattern (lifted verbatim from the existing tests at scene.rs:3906) is:

```rust
let mut core = KmsCore::for_tests();
let mut store = DrawableStore::new();
let platform = PlatformBackend::for_tests();
let mut windows_v2 = super::super::backend::WindowsV2Map::new();
```

`PlatformBackend::for_tests()` returns 800×600 outputs. COW's xid is `yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0` — current value 0x103.

- [ ] **Step 1.2: Rename `build_scene_appends_cow_above_top_levels` → `build_scene_cow_none_emits_top_levels`, drop the COW from its fixture, assert top-levels only**

The test currently constructs a fixture with at least one top-level + COW and asserts COW comes after top-levels. Rewrite it so the fixture has no COW (`build_scene(..., cow=None, ...)`) and assert that the top-level draws are present and that no draw uses the COW xid as its source. This keeps the assertion of the cow=None behavior live (top-level walk still emits).

Edit shape:
- Remove the COW storage allocation and the `compositor.register_cow(...)` call.
- Change `build_scene(...)` call to pass `None` for the cow parameter.
- Assertion becomes: top-level draws are present (count + position match the fixture).

- [ ] **Step 1.3: Rename `build_scene_cow_is_below_cursor` → `build_scene_cow_none_cursor_at_top`, drop COW, keep cursor-on-top assertion**

The current test asserts `top-level + COW + cursor = 3 draws`. Under 2a that becomes `COW + cursor = 2 draws` when cow=Some, OR `top-level + cursor = 2 draws` when cow=None. Rewrite as cow=None: top-level + cursor = 2 draws (with cursor last). The COW-present cursor ordering assertion moves into the new Task 2 test.

- [ ] **Step 1.4: Run the renamed tests to confirm they still pass**

```bash
cargo test -p yserver --lib build_scene_cow_none 2>&1 | tail -10
```

Expected: PASS for both renamed tests. (They're now cow=None, which the current code still handles correctly — the rename + fixture trim doesn't change behavior.)

- [ ] **Step 1.5: Commit the rename + trim**

```bash
git add crates/yserver/src/kms/v2/scene.rs
git commit -m "test(scene): rename existing COW tests to cow=None form for 2a

The existing build_scene_appends_cow_above_top_levels and
build_scene_cow_is_below_cursor both assert 'top-levels emitted
alongside COW' which is intentionally obsolete under Phase 1
(COW-authoritative). Renamed + trimmed each to its cow=None
behavior to keep that path under test; the new cow=Some shape
gets a dedicated test in the next commit."
```

## Task 2: Phase 1 — write the cow=Some COW-authoritative failing test

**Files:**
- Modify: `crates/yserver/src/kms/v2/scene.rs` — append a new test in the same `#[cfg(test)] mod tests` block as the two renamed ones.

- [ ] **Step 2.1: Append the new failing test**

```rust
#[test]
fn build_scene_cow_some_strips_top_levels_and_keeps_cursor_at_top() {
    // Phase 1 (2a): when a compositor has registered COW, the scene
    // strips per-top-level entries — scanout becomes root + COW
    // (+ cursor) only. Mirrors Xorg's compositor contract: the X
    // server doesn't compose redirected toplevels itself; the
    // compositor reads backings, composes, and Presents into COW.
    // Cursor stays at top-of-z (last in draws).

    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows_v2 = super::super::backend::WindowsV2Map::new();

    // Two mapped top-levels with live storage (non-null sample
    // views via alloc_stub_window's sentinel image_view) — under
    // cow=None they would appear in draws. Under 2a they must NOT.
    alloc_stub_window(&mut store, &mut windows_v2, 0x4000, 100, 100, 200, 150, None, true);
    alloc_stub_window(&mut store, &mut windows_v2, 0x4001, 50, 50, 50, 50, None, true);
    core.top_level_order.push(0x4000);
    core.top_level_order.push(0x4001);
    let top_a_id = store.lookup(0x4000).expect("top-level A allocated");
    let top_b_id = store.lookup(0x4001).expect("top-level B allocated");

    // Register COW: allocate stub storage with a sentinel
    // image_view (so build_scene doesn't filter on the null-view
    // gate) and call store.allocate directly — same pattern as
    // the existing build_scene_appends_cow_above_top_levels test.
    let mut cow_storage = super::super::store::Storage::for_tests_null(
        extent(800, 600), // matches PlatformBackend::for_tests() output
        vk::Format::B8G8R8A8_UNORM,
    );
    let cow_sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(0xC0_C0_C0_C0);
    cow_storage.image_view = cow_sentinel;
    cow_storage.sample_view = cow_sentinel;
    let cow_id = store
        .allocate(
            yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0,
            DrawableKind::Window,
            24,
            true, // scene_participating
            cow_storage,
        )
        .expect("alloc COW stub");

    // Cursor.
    let mut cursor_storage = super::super::store::Storage::for_tests_null(
        extent(16, 16),
        vk::Format::B8G8R8A8_UNORM,
    );
    let cursor_sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(0xCAFE_BABE);
    cursor_storage.image_view = cursor_sentinel;
    cursor_storage.sample_view = cursor_sentinel;
    let cursor_id = store
        .allocate(0xCAFE_0003, DrawableKind::Pixmap, 32, false, cursor_storage)
        .expect("alloc cursor stub");
    core.cursor_x = 50.0;
    core.cursor_y = 60.0;
    let cursor = CursorEntry {
        id: cursor_id,
        extent: extent(16, 16),
        hot_x: 0,
        hot_y: 0,
        record_version: 0,
        bgra_bytes: None,
    };

    let built = build_scene(
        &core,
        &mut store,
        &windows_v2,
        0,
        &platform,
        Some(cursor),
        None,
        Some(cow_id),
        false,
    );

    // Expect: COW + cursor only (root storage isn't allocated
    // in this fixture so it doesn't contribute a draw). Pin all
    // three invariants explicitly so the test catches each
    // possible breakage shape:
    //
    //   (a) no top-level layer sneaks through;
    //   (b) the COW layer IS emitted (a regression that emits
    //       nothing but the cursor must fail this);
    //   (c) the COW layer has the right shape (source view,
    //       full-extent dst, alpha-passthrough on);
    //   (d) cursor is last (i.e. immediately above COW).
    let top_a_view = store.get(top_a_id).expect("top_a present").storage.sample_view;
    let top_b_view = store.get(top_b_id).expect("top_b present").storage.sample_view;
    let cow_view = store.get(cow_id).expect("cow present").storage.sample_view;
    let cursor_view = store.get(cursor_id).expect("cursor present").storage.sample_view;

    // (a)
    let top_emit_count = built
        .scene
        .draws
        .iter()
        .filter(|d| d.image_view == top_a_view || d.image_view == top_b_view)
        .count();
    assert_eq!(
        top_emit_count, 0,
        "cow-authoritative mode must strip top-level draws, got {} of them in {:?}",
        top_emit_count, built.scene.draws,
    );

    // (b) + (c): exactly one COW draw, full-extent, alpha-
    // passthrough, sampling cow_sentinel.
    let cow_draws: Vec<_> = built
        .scene
        .draws
        .iter()
        .filter(|d| d.image_view == cow_view)
        .collect();
    assert_eq!(
        cow_draws.len(),
        1,
        "exactly one COW draw expected, got {} in {:?}",
        cow_draws.len(),
        built.scene.draws,
    );
    let cow_draw = cow_draws[0];
    assert_eq!(cow_draw.image_view, cow_sentinel, "COW draw must sample cow_sentinel");
    assert_eq!(cow_draw.dst_origin, [0.0, 0.0], "COW dst_origin must be (0, 0)");
    assert_eq!(cow_draw.dst_size, [800.0, 600.0], "COW dst_size must be full output extent (800x600)");
    assert!(cow_draw.alpha_passthrough, "COW must blend with alpha_passthrough=true");

    // (d): cursor is the last draw, COW immediately precedes it.
    assert!(built.scene.draws.len() >= 2, "expected at least COW + cursor");
    let last = built.scene.draws.last().expect("at least one draw");
    let penultimate = &built.scene.draws[built.scene.draws.len() - 2];
    assert_eq!(
        last.image_view, cursor_view,
        "cursor must be the top (last) scene draw"
    );
    assert_eq!(
        penultimate.image_view, cow_view,
        "COW must be immediately below cursor",
    );
}
```

The fixture is the canonical setup pattern in this file (verbatim from existing tests at scene.rs:3906+).

- [ ] **Step 2.2: Run the new test to verify it fails**

```bash
cargo test -p yserver --lib build_scene_cow_some_strips_top_levels 2>&1 | tail -15
```

Expected: FAIL — top_emit_count is 2, not 0, because the current `build_scene` always emits top-levels regardless of `cow`.

- [ ] **Step 2.3: Commit the failing test**

```bash
git add crates/yserver/src/kms/v2/scene.rs
git commit -m "test(scene): cow=Some must strip top-level draws (Phase 1, 2a)

Pre-implementation failing test pinning the COW-authoritative
behavior: when build_scene is called with cow=Some(_), no top-
level layer must contribute to draws, and the cursor stays at
the top of z. Fails on the current build_scene which always
emits top-levels regardless of cow."
```

## Task 3: Phase 1 — implement the cow=Some top-level walk skip

**Files:**
- Modify: `crates/yserver/src/kms/v2/scene.rs` — at the `for &top_xid in &core.top_level_order` loop currently at scene.rs:1576-1596.

- [ ] **Step 3.1: Apply the gate**

Replace the existing trace + loop at scene.rs:1571-1602 with:

```rust
    // Stage 4d — COW-authoritative mode (option 2a from the
    // shadow-hunt). Once a compositor has registered COW (i.e.
    // `cow.is_some()` — set by `register_cow` on the first paint
    // that lands in COW), the scene strips per-top-level entries.
    // Scanout becomes `root + COW + cursor` only, mirroring Xorg's
    // compositor contract: the X server doesn't itself compose
    // redirected toplevels; the compositor reads the redirected
    // backings, composes, and Presents the result into COW.
    //
    // Emitting top-levels alongside COW in v2 invented a layered-
    // damage model that turned every gap (missed projection, off-
    // by-one ancestor walk, ack timing, scene_participating gate)
    // into a visible artefact. Skipping the walk removes that
    // whole class of failure.
    //
    // `cow.is_none()` covers two cases that must still emit top-
    // levels: (1) no compositor active, scene must drive the
    // display itself; (2) compositor active but hasn't painted
    // into COW yet, so the initial frame isn't a blank screen.
    if cow.is_some() {
        log::trace!(
            "v2 scene_walk begin output={output_idx} cow_authoritative=true \
             top_levels_skipped={n} \
             layout=({layout_x0},{layout_y0} {layout_w}x{layout_h})",
            n = core.top_level_order.len(),
        );
    } else {
        log::trace!(
            "v2 scene_walk begin output={output_idx} cow_authoritative=false \
             top_levels={n} \
             layout=({layout_x0},{layout_y0} {layout_w}x{layout_h})",
            n = core.top_level_order.len(),
        );
        for &top_xid in &core.top_level_order {
            emit_window_subtree(
                top_xid,
                0,
                0,
                store,
                windows_v2,
                layout_x0,
                layout_y0,
                layout_w,
                layout_h,
                &mut draws,
                &mut snapshots,
                &mut sampled_ids,
                &mut projected,
                false,
            );
        }
    }
    log::trace!(
        "v2 scene_walk end output={output_idx} draws={n_draws} \
         sampled={n_sampled}",
        n_draws = draws.len(),
        n_sampled = sampled_ids.len(),
    );
```

- [ ] **Step 3.2: Run the new failing test to verify it now passes**

```bash
cargo test -p yserver --lib build_scene_cow_some_strips_top_levels 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 3.3: Run the full scene test module to catch regressions in adjacent tests**

```bash
cargo test -p yserver --lib build_scene 2>&1 | tail -30
```

Expected: ALL PASS. If any other scene test fails it's depending on the old top-levels-alongside-COW shape; adjust to use cow=None or update its assertion to the new (correct) behavior.

- [ ] **Step 3.4: Commit Phase 1 implementation**

```bash
git add crates/yserver/src/kms/v2/scene.rs
git commit -m "feat(scene): cow-authoritative mode strips top-level draws (Phase 1, 2a)

When a compositor has registered COW (cow.is_some() in build_scene),
the top-level scene walk is skipped. Scanout becomes root + COW
(+ cursor) only — Xorg's compositor contract: the X server doesn't
compose redirected toplevels itself; the compositor reads backings,
composes, and Presents the composited image into COW. Emitting top-
levels alongside COW grew an invented layered-damage model that
surfaced as missing-tray-applets, shadow gaps, and missing post-
interaction repaints.

Phase 1 alone may not eliminate the missing-tray-applets symptom
because mate-panel's pixmap (which the compositor reads to build
COW) still doesn't contain reparented descendants' paints — Phase 2
(Xorg-style reparent redirect reconciliation) fixes that upstream
bug."
```

## Task 4: Phase 1 — informational hardware smoke

**Goal:** Confirm Phase 1 didn't break anything that was working. The tray-applets symptom may still appear (Phase 2 fixes that), but everything else should still render.

- [ ] **Step 4.1: Run the hardware smoke recipe**

```bash
just yserver-mate-hw-trace
```

Reproduce the steady-state desktop on bee, wait for the panel to settle.

- [ ] **Step 4.2: Capture a dump (Ctrl+Alt+F12) then end the run**

The bundle lands in `./yserver-v2-drawable-0-*.ppm` + `./yserver-v2-scanout-0-out0.ppm`. Log at `./yserver-hw-mate.log`.

- [ ] **Step 4.3: Convert the scanout for visual review**

```bash
mkdir -p target/diag
magick yserver-v2-scanout-0-out0.ppm -resize 1280 target/diag/phase1-scanout.png
```

- [ ] **Step 4.4: Assess**

Expected:
- Panels visible.
- CC visible (if open) with shadow.
- Windows render.
- **Tray applets may still be missing** — this is the Phase 2 bug, not a Phase 1 regression.

If anything ELSE regressed (panels gone, CC gone, shadows wrong) — stop and triage before Phase 2.

If smoke is clean modulo the known applet issue → proceed.

## Task 5: Phase 2 — thread `origin` through `teardown_redirect_for_window`

**Why:** the existing helper (`crates/yserver-core/src/core_loop/process_disconnect.rs:298`) hardcodes `None` for the backend calls' origin. The reparent path has a real `origin` we should pass through. Threading a parameter is a minimal change; both existing callers stay correct.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_disconnect.rs:298` — add `origin: Option<OriginContext>` parameter, replace `None` in the two backend calls with it.
- Modify: `crates/yserver-core/src/core_loop/process_disconnect.rs:221,224` — existing callers pass `None`.

- [ ] **Step 5.1: Read the existing function**

```bash
sed -n '290,341p' crates/yserver-core/src/core_loop/process_disconnect.rs
```

- [ ] **Step 5.2: Add the `origin` parameter and thread it through**

Edit the function signature to:

```rust
pub(crate) fn teardown_redirect_for_window(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<crate::backend::OriginContext>,
    window: ResourceId,
) {
```

Replace the two `None` arguments to backend calls (currently at lines 312 and 335) with `origin`:

```rust
    if let Err(err) = backend.release_redirected_backing(origin, backing.host_pixmap) {
```

```rust
    if let Err(err) = backend.set_window_scene_participation(origin, host_window, true) {
```

- [ ] **Step 5.3: Update the two existing call sites**

At lines 221 and 224 in the same file, the two existing callers (both in the disconnect path) need a `None` origin added:

```rust
                teardown_redirect_for_window(state, backend, None, child);
```

```rust
            teardown_redirect_for_window(state, backend, None, *window);
```

- [ ] **Step 5.4: Build to confirm signature change compiles cleanly**

```bash
cargo build -p yserver-core 2>&1 | tail -5
```

Expected: clean. If any other caller exists that the grep didn't catch, the build error will name it; add `None` there too.

- [ ] **Step 5.5: Run the disconnect-path tests to confirm no regression**

```bash
cargo test -p yserver-core --lib teardown 2>&1 | tail -10
cargo test -p yserver-core --lib disconnect 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 5.6: Commit**

```bash
git add crates/yserver-core/src/core_loop/process_disconnect.rs
git commit -m "refactor(composite): thread origin through teardown_redirect_for_window

Existing helper hardcoded None for the backend.release_redirected_backing
and set_window_scene_participation origin. The reparent-redirect-
reconciliation path (Phase 2 of cow-authoritative-mode) has a real
origin to pass; threading the parameter avoids forking a near-
duplicate helper. Both existing call sites (process_disconnect) keep
passing None — same behavior."
```

## Task 6: Phase 2 — make `RecordingBackend` configurable for `supports_redirect_activation`

**Why:** `RecordingBackend` inherits the default `supports_redirect_activation() = false` from the trait (trait_def.rs:459). The Phase 2 reconciliation block in `handle_reparent_window` is gated on that returning `true`. Tests need a recording backend that claims `true` so the production path actually runs.

**Files:**
- Modify: `crates/yserver-core/src/backend/recording.rs:120` — add a per-instance flag + builder method + trait override.

- [ ] **Step 6.1: Add the flag + builder + trait override**

In the `RecordingBackend` struct (currently around line 120), add a field next to the existing `cow_next_release_is_final`:

```rust
    /// Phase 2 (reparent reconciliation): lets tests opt in to
    /// claiming `supports_redirect_activation = true` so the
    /// production reconciliation block in `handle_reparent_window`
    /// (gated on the trait method) actually runs. Default `false`
    /// matches the trait default — v1 / host-X11 semantics.
    pub redirect_activation_supported: bool,
```

In `RecordingBackend::new()` (line 151), initialize it to `false`:

```rust
            redirect_activation_supported: false,
```

Just before the closing `}` of `impl RecordingBackend { ... }` (line ~190 — same impl block as `new`), add a builder method:

```rust
    /// Phase 2: opt in to claiming
    /// `supports_redirect_activation = true`. Used by tests that
    /// exercise the reparent-redirect-reconciliation path
    /// (`handle_reparent_window` gates its reconciliation block
    /// on `backend.supports_redirect_activation()`).
    #[must_use]
    pub fn with_redirect_activation(mut self) -> Self {
        self.redirect_activation_supported = true;
        self
    }
```

In the `impl Backend for RecordingBackend { ... }` block (search for `impl Backend for RecordingBackend`), add the trait override near other simple boolean overrides:

```rust
    fn supports_redirect_activation(&self) -> bool {
        self.redirect_activation_supported
    }
```

- [ ] **Step 6.2: Build**

```bash
cargo build -p yserver-core 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 6.3: Sanity-check existing RecordingBackend tests still pass**

```bash
cargo test -p yserver-core --lib recording 2>&1 | tail -10
```

Expected: PASS — default value is unchanged (`false`), only the opt-in builder is new.

- [ ] **Step 6.4: Commit**

```bash
git add crates/yserver-core/src/backend/recording.rs
git commit -m "feat(recording): opt-in supports_redirect_activation flag

Phase 2 (reparent reconciliation) gates its block in
handle_reparent_window on Backend::supports_redirect_activation().
The default trait impl returns false; RecordingBackend kept that
default. Adding a per-instance flag + a with_redirect_activation()
builder lets tests opt in, so the production code path runs in
unit tests. Default behaviour unchanged."
```

## Task 7: Phase 2 — write the reparent reconciliation regression tests (TDD)

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` — append the backing-state tests in the existing `#[cfg(test)] mod tests` block, modeled on `dispatch_damage_subtract` at line 14127.
- Modify: `crates/yserver/src/kms/v2/backend.rs` — append the end-to-end resolver test in the v2 backend's `#[cfg(test)] mod tests` block (this one needs `KmsBackendV2` + access to `pub(crate) resolve_paint_target`, so it lives in the yserver crate).

- [ ] **Step 7.1: Read existing reparent + redirect test fixtures**

```bash
sed -n '14127,14310p' crates/yserver-core/src/core_loop/process_request.rs
```

Map out: how `ServerState` is constructed in tests, how `composite_redirects` is seeded, whether there's an existing `dispatch_reparent_window` (probably not — write one as a thin helper).

- [ ] **Step 7.2: Write the failing test for inherited-redirect revocation**

```rust
#[test]
fn reparent_out_of_redirected_subtree_revokes_inherited_redirect() {
    // Phase 2: mirrors compUnredirectOneSubwindow in
    // /home/jos/Projects/xserver/composite/compwindow.c:453.
    // A window that inherited redirect from its parent's
    // RedirectSubwindows must lose its backing when reparented
    // to a parent without RedirectSubwindows.
    //
    // The actual regression: nm-applet (created as a direct
    // child of root with RedirectSubwindows(root, Manual)
    // active, then reparented into mate-panel's notification
    // socket) keeps a stale backing. Paints land there instead
    // of in mate-panel's pixmap; the compositor reads mate-
    // panel's pixmap and sees an empty tray area.
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new()
        .with_redirect_activation();

    let root_xid = crate::resources::ROOT_WINDOW;
    let mate_panel_xid = ResourceId(0x110_0003);
    let socket_xid = ResourceId(0x210_0013);
    let nm_applet_xid = ResourceId(0x180_000b);

    // root has RedirectSubwindows(Manual); socket does not.
    state.composite_redirects.insert(
        (root_xid, true),
        crate::server::RedirectRecord {
            mode: crate::server::CompositeRedirectMode::Manual,
            owner: ClientId(14),
        },
    );

    seed_window(&mut state, mate_panel_xid, root_xid, 2560, 28);
    seed_redirected_window(&mut state, &mut backend, mate_panel_xid);
    seed_window(&mut state, socket_xid, mate_panel_xid, 26, 27);
    seed_window(&mut state, nm_applet_xid, root_xid, 26, 27);
    seed_redirected_window(&mut state, &mut backend, nm_applet_xid);

    assert!(
        state.resources.window(nm_applet_xid).unwrap().redirected_backing.is_some(),
        "pre-condition: nm-applet has an inherited-redirect backing"
    );

    dispatch_reparent_window(&mut state, &mut backend, nm_applet_xid, socket_xid, 0, 0);

    assert_eq!(
        state.resources.window(nm_applet_xid).unwrap().parent,
        socket_xid,
    );
    assert!(
        state.resources.window(nm_applet_xid).unwrap().redirected_backing.is_none(),
        "post-condition: nm-applet's inherited-redirect backing is freed after reparenting out of the redirected subtree"
    );
}
```

The helpers `make_test_state`, `seed_window`, `seed_redirected_window`, `dispatch_reparent_window` are local to this test module. If they don't already exist with these names, model them on `dispatch_damage_subtract`'s pattern (Step 7.1) — each should be ≤ 25 lines. `dispatch_reparent_window` should call `handle_reparent_window` directly (it's `fn`-private in this module, so the test can call it without changing visibility). The backend is `RecordingBackend::new().with_redirect_activation()` per Task 6 — that's the supports_redirect_activation = true case that gates the production reconciliation block.

- [ ] **Step 7.3: Write the failing test for inherited-redirect grant on reparent-in**

```rust
#[test]
fn reparent_into_redirected_subtree_grants_inherited_redirect() {
    // Phase 2: mirrors compRedirectOneSubwindow at
    // /home/jos/Projects/xserver/composite/compwindow.c:454. A
    // window with no own redirect, no parent RedirectSubwindows,
    // gains a backing when reparented under a parent with active
    // RedirectSubwindows.
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new()
        .with_redirect_activation();

    let root_xid = crate::resources::ROOT_WINDOW;
    let mate_panel_xid = ResourceId(0x110_0003);
    let unredirected_parent_xid = ResourceId(0x300_0001);
    let target_xid = ResourceId(0x300_0010);

    state.composite_redirects.insert(
        (mate_panel_xid, true),
        crate::server::RedirectRecord {
            mode: crate::server::CompositeRedirectMode::Automatic,
            owner: ClientId(14),
        },
    );

    seed_window(&mut state, mate_panel_xid, root_xid, 2560, 28);
    seed_window(&mut state, unredirected_parent_xid, root_xid, 100, 100);
    seed_window(&mut state, target_xid, unredirected_parent_xid, 50, 50);

    assert!(
        state.resources.window(target_xid).unwrap().redirected_backing.is_none(),
        "pre-condition: target has no backing"
    );

    dispatch_reparent_window(&mut state, &mut backend, target_xid, mate_panel_xid, 0, 0);

    assert_eq!(
        state.resources.window(target_xid).unwrap().parent,
        mate_panel_xid,
    );
    assert!(
        state.resources.window(target_xid).unwrap().redirected_backing.is_some(),
        "post-condition: target gained an inherited-redirect backing"
    );
}
```

- [ ] **Step 7.4: Write the failing test for direct-redirect survival across reparent**

```rust
#[test]
fn reparent_with_direct_redirect_keeps_backing() {
    // Phase 2 invariant: RedirectWindow(W) is a per-window
    // redirect independent of W's parent. Reparenting W must
    // NOT touch its backing.
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new()
        .with_redirect_activation();

    let root_xid = crate::resources::ROOT_WINDOW;
    let mate_panel_xid = ResourceId(0x110_0003);
    let socket_xid = ResourceId(0x210_0013);
    let directly_redirected_xid = ResourceId(0x400_0001);

    state.composite_redirects.insert(
        (directly_redirected_xid, false),
        crate::server::RedirectRecord {
            mode: crate::server::CompositeRedirectMode::Manual,
            owner: ClientId(14),
        },
    );

    seed_window(&mut state, mate_panel_xid, root_xid, 2560, 28);
    seed_window(&mut state, socket_xid, mate_panel_xid, 26, 27);
    seed_window(&mut state, directly_redirected_xid, root_xid, 50, 50);
    seed_redirected_window(&mut state, &mut backend, directly_redirected_xid);

    let backing_before = state
        .resources
        .window(directly_redirected_xid)
        .unwrap()
        .redirected_backing
        .as_ref()
        .map(|b| b.host_pixmap);
    assert!(backing_before.is_some());

    dispatch_reparent_window(&mut state, &mut backend, directly_redirected_xid, socket_xid, 0, 0);

    let backing_after = state
        .resources
        .window(directly_redirected_xid)
        .unwrap()
        .redirected_backing
        .as_ref()
        .map(|b| b.host_pixmap);
    assert_eq!(
        backing_before, backing_after,
        "RedirectWindow(W) survives reparent"
    );
}
```

- [ ] **Step 7.4b: Write the failing test for cross-redirect-mode reparent (Manual ↔ Automatic)**

This pins the production code's `old_parent_redirects_subwindows && new_parent_redirects_subwindows` arm (the third branch of the reconciliation block in Task 8.2's snippet). Without it, that branch can silently misbehave while the other three tests still pass.

```rust
#[test]
fn reparent_between_redirected_parents_with_different_modes_flips_mode() {
    // Phase 2: when both old_parent and new_parent have
    // RedirectSubwindows but with different modes (Manual ↔
    // Automatic), the production reconciliation calls
    // flip_redirect_target_mode. That helper preserves the
    // backing handle (X Composite spec: old named pixmaps remain
    // allocated until FreePixmap; only redirectDraw flips) and
    // updates the window's own scene_participating flag per the
    // new mode (Manual ⇒ false, Automatic ⇒ true).
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new()
        .with_redirect_activation();

    let root_xid = crate::resources::ROOT_WINDOW;
    let parent_manual_xid = ResourceId(0x500_0001);
    let parent_automatic_xid = ResourceId(0x500_0002);
    let target_xid = ResourceId(0x500_0010);

    state.composite_redirects.insert(
        (parent_manual_xid, true),
        crate::server::RedirectRecord {
            mode: crate::server::CompositeRedirectMode::Manual,
            owner: ClientId(14),
        },
    );
    state.composite_redirects.insert(
        (parent_automatic_xid, true),
        crate::server::RedirectRecord {
            mode: crate::server::CompositeRedirectMode::Automatic,
            owner: ClientId(14),
        },
    );

    seed_window(&mut state, parent_manual_xid, root_xid, 100, 100);
    seed_window(&mut state, parent_automatic_xid, root_xid, 100, 100);
    // target is initially a child of parent_manual_xid → inherits
    // Manual redirect → has a backing allocated, and its own
    // scene_participating is false (Manual semantics).
    seed_window(&mut state, target_xid, parent_manual_xid, 50, 50);
    seed_redirected_window(&mut state, &mut backend, target_xid);

    let backing_before = state
        .resources
        .window(target_xid)
        .unwrap()
        .redirected_backing
        .as_ref()
        .map(|b| b.host_pixmap);
    assert!(backing_before.is_some(), "pre: target has Manual-inherited backing");

    // Reparent to the Automatic parent.
    dispatch_reparent_window(&mut state, &mut backend, target_xid, parent_automatic_xid, 0, 0);

    let backing_after = state
        .resources
        .window(target_xid)
        .unwrap()
        .redirected_backing
        .as_ref()
        .map(|b| b.host_pixmap);
    assert_eq!(
        backing_before, backing_after,
        "mode-flip across redirected parents preserves the backing handle (X Composite spec)"
    );

    // Mode flip side effect: scene_participating flag on the
    // window itself follows the new parent's mode. The flip is
    // visible in the RecordingBackend's call log — look for a
    // SetWindowSceneParticipation(_, true) for target's host xid
    // (Automatic ⇒ participating=true; pre-reparent it was
    // false under Manual).
    let calls = backend.calls.lock().expect("calls poisoned");
    let saw_participation_true = calls.iter().any(|c| matches!(
        c,
        crate::backend::recording::RecordedCall::SetWindowSceneParticipation { participating: true, .. }
    ));
    assert!(
        saw_participation_true,
        "expected SetWindowSceneParticipation(_, true) after Manual → Automatic flip; got {:?}",
        *calls,
    );
}
```

**Note on the call-log assertion:** the exact `RecordedCall` variant name (`SetWindowSceneParticipation`) is a placeholder for whatever the RecordingBackend already records for the `set_window_scene_participation` trait method. Search `crates/yserver-core/src/backend/recording.rs` for the matching variant near the existing call-recording impls; adjust the variant name + field destructuring to match. If the variant doesn't yet exist (the recorder is opt-in per-method per the file's "implement on demand" convention), add it as a small extension along with the test.

- [ ] **Step 7.5: Write the failing end-to-end resolver test in the yserver crate**

This is the **root-cause assertion** — the other three tests pin backing-existence state in yserver-core, but the actual user-visible behavior lives in v2's `resolve_paint_target` ancestor walk. To exercise the production path, the test must:

1. Use `KmsBackendV2::for_tests()` so `supports_redirect_activation()` returns `true` (KmsBackendV2 overrides to true; trait_def.rs:461 comment).
2. Drive the ReparentWindow through the top-level `yserver_core::core_loop::process_request::process_request(...)` dispatcher (process_request.rs:73) — `handle_reparent_window` is `fn`-private to its module so we can't call it directly from another crate.
3. Assert against `backend.resolve_paint_target(nm_applet_host_xid)` (accessible because the test is in the same crate's module, where `pub(crate)` is in scope).

**Heads up:** there are currently NO existing tests in `crates/yserver/src/` that dispatch through `process_request` (a `grep -rn 'process_request(' crates/yserver/src/` returns nothing). This test breaks new ground; the snippet below is the canonical recipe. Expect a few compile-iterate cycles to nail down the fixture details (window registration order, exact field setters on `ServerState.resources`, `WindowsV2Map` entry shape).

`process_request`'s signature (from process_request.rs:73):

```rust
pub fn process_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,           // { opcode: u8, data: u8, length_units: u32 }
    body: &[u8],
    attached_fd: Option<OwnedFd>,
) -> io::Result<RequestOutcome>
```

`RequestHeader` is `pub struct RequestHeader { opcode: u8, data: u8, length_units: u32 }` (yserver-protocol/src/x11/mod.rs:109). For ReparentWindow that's `{ opcode: 7, data: 0, length_units: 4 }` (X11 length_units is total request size including the 4-byte header, in 4-byte units → 4*4 = 16 bytes total). The `body` slice excludes the 4-byte request header — for ReparentWindow that leaves 12 bytes: `window(4 LE) + parent(4 LE) + x(2 LE i16) + y(2 LE i16)`.

```rust
#[test]
fn resolve_paint_target_after_reparent_out_routes_to_new_redirected_ancestor() {
    use yserver_core::backend::Backend;
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_core::server::{CompositeRedirectMode, RedirectRecord, ServerState};
    use yserver_protocol::x11::{ClientId, ResourceId};

    // Phase 2 root-cause pin: build a tree mirroring the live
    // mate-panel case (root → mate-panel, root → nm-applet),
    // dispatch a ReparentWindow that moves nm-applet under
    // mate-panel's socket, then assert resolve_paint_target
    // returns mate-panel's backing with the right offset.
    //
    // Pre-fix: nm-applet's stale Manual-redirect backing wins,
    // resolve_paint_target returns it with offset (0, 0).
    // Post-fix (handle_reparent_window's reconciliation): the
    // backing is freed, resolve_paint_target walks up the
    // ancestor chain to mate-panel's backing with the offset
    // = nm-applet's screen-coord position within mate-panel.

    // Construct: pick up the canonical v2 test fixture pattern
    // from the closest existing test in this file (Step 7.1's
    // search). What follows is the test's logical shape; adapt
    // the fixture-construction lines to match.

    let mut state = ServerState::new();
    let mut backend = KmsBackendV2::for_tests();

    let root_xid = ROOT_WINDOW;
    let mate_panel_xid = ResourceId(0x110_0003);
    let socket_xid = ResourceId(0x210_0013);
    let nm_applet_xid = ResourceId(0x180_000b);

    // Pre-state: root has RedirectSubwindows(Manual). mate-panel
    // is a redirected direct child. socket is a child of mate-
    // panel (not directly redirected). nm-applet is currently
    // a direct child of root (and therefore inherits redirect).
    state.composite_redirects.insert(
        (root_xid, true),
        RedirectRecord {
            mode: CompositeRedirectMode::Manual,
            owner: ClientId(14),
        },
    );

    // Use whatever the local fixture pattern is to seed:
    //   mate-panel @ (0,0), 2560x28, redirected.
    //   socket @ (2387,0) inside mate-panel, 26x27, not redirected.
    //   nm-applet @ (0,0) under root, 26x27, redirected via
    //     inheritance from root's RedirectSubwindows.
    seed_v2_window(&mut state, &mut backend, mate_panel_xid, root_xid, 0, 0, 2560, 28);
    seed_v2_redirected_backing(&mut state, &mut backend, mate_panel_xid);
    let mate_panel_backing_id = backing_drawable_id(&backend, mate_panel_xid)
        .expect("mate-panel backing drawable id");
    seed_v2_window(&mut state, &mut backend, socket_xid, mate_panel_xid, 2387, 0, 26, 27);
    seed_v2_window(&mut state, &mut backend, nm_applet_xid, root_xid, 0, 0, 26, 27);
    seed_v2_redirected_backing(&mut state, &mut backend, nm_applet_xid);

    // Drive a ReparentWindow request through the top-level
    // dispatcher so the production handle_reparent_window code
    // path (with the Phase 2 reconciliation block) runs.
    dispatch_reparent_window_v2(
        &mut state,
        &mut backend,
        nm_applet_xid,
        socket_xid,
        /* x */ 0,
        /* y */ 0,
    );

    let nm_applet_host_xid = host_xid_for(&backend, nm_applet_xid)
        .expect("nm-applet host xid present");

    let resolved = backend
        .resolve_paint_target(nm_applet_host_xid)
        .expect("resolve must succeed");

    assert_eq!(
        resolved.id, mate_panel_backing_id,
        "paints into nm-applet must route to mate-panel's redirected backing post-reparent"
    );
    assert_eq!(
        resolved.offset,
        (2387, 0),
        "offset must place the paint at nm-applet's screen-coord position within mate-panel's backing"
    );
}
```

The helpers `seed_v2_window`, `seed_v2_redirected_backing`, `backing_drawable_id`, `host_xid_for`, `dispatch_reparent_window_v2` are local to this test module — write them next to the test.

`dispatch_reparent_window_v2` is the shape that drives the production path; here's its skeleton (the executor pastes it as-is, then iterates `state` + `backend` setup until the dispatch succeeds):

```rust
fn dispatch_reparent_window_v2(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackendV2,
    window: ResourceId,
    parent: ResourceId,
    x: i16,
    y: i16,
) {
    use yserver_core::backend::Backend;
    use yserver_core::core_loop::process_request;
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&parent.0.to_le_bytes());
    body.extend_from_slice(&x.to_le_bytes());
    body.extend_from_slice(&y.to_le_bytes());

    process_request::process_request(
        state,
        backend as &mut dyn Backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 7,        // ReparentWindow
            data: 0,
            length_units: 4,  // 16 bytes total / 4-byte unit
        },
        &body,
        None,
    )
    .expect("process_request must succeed");
}
```

If `process_request` returns `Err` or `RequestOutcome::Handled` doesn't match, the test fails the unwrap — that's intentional, the failure mode itself is diagnostic for "fixture state mismatch."

- [ ] **Step 7.6: Run the five new tests to verify their pre-Phase-2 failure shape**

```bash
cargo test -p yserver-core --lib reparent_out_of_redirected_subtree 2>&1 | tail -5
cargo test -p yserver-core --lib reparent_into_redirected_subtree 2>&1 | tail -5
cargo test -p yserver-core --lib reparent_with_direct_redirect 2>&1 | tail -5
cargo test -p yserver-core --lib reparent_between_redirected_parents_with_different_modes 2>&1 | tail -5
cargo test -p yserver --lib resolve_paint_target_after_reparent 2>&1 | tail -5
```

Expected pre-Phase-2 shape:
- Test 1 (revoke) FAILS on backing-still-present.
- Test 2 (grant) FAILS on backing-still-absent.
- Test 3 (direct-redirect survival) may already PASS by coincidence — the current handler doesn't touch backings, so a direct-redirect window's backing trivially survives. It's a regression guard, not a driver.
- Test 4 (mode-flip) FAILS because the current handler doesn't fire `flip_redirect_target_mode`, so no `SetWindowSceneParticipation(_, true)` call is recorded.
- Test 5 (resolver) FAILS because `resolve_paint_target` finds nm-applet's stale own backing instead of walking up to mate-panel.

- [ ] **Step 7.7: Commit the failing tests**

```bash
git add crates/yserver-core/src/core_loop/process_request.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "test(reparent): pin Xorg-style redirect reconciliation invariants

Five failing tests pinning Phase 2 semantics:

1. reparent_out_of_redirected_subtree_revokes_inherited_redirect —
   inherited-redirect backing freed when leaving the redirected
   subtree. Mirrors compUnredirectOneSubwindow.
2. reparent_into_redirected_subtree_grants_inherited_redirect —
   inverse: backing allocated when entering. Mirrors
   compRedirectOneSubwindow.
3. reparent_with_direct_redirect_keeps_backing — RedirectWindow(W)
   is per-window, survives reparent.
4. reparent_between_redirected_parents_with_different_modes_flips_mode —
   Manual ↔ Automatic across redirected parents preserves the
   backing handle (X Composite spec) but flips scene
   participation per the new mode.
5. resolve_paint_target_after_reparent_out_routes_to_new_redirected_ancestor —
   the root-cause assertion: paints into the reparented window
   resolve to the new chain's nearest redirected ancestor (which
   for nm-applet means mate-panel's backing, with the right
   screen-coord offset)."
```

## Task 8: Phase 2 — wire reconciliation into `handle_reparent_window`

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:7092` — `handle_reparent_window`.

- [ ] **Step 8.1: Read the existing handler**

```bash
sed -n '7092,7200p' crates/yserver-core/src/core_loop/process_request.rs
```

Identify where `old_parent` is captured and where the resource-table parent is updated. The reconciliation block goes between those two events (so we can read the pre-state) — or strictly after the parent update, with the pre-state captured into locals first.

- [ ] **Step 8.2: Insert reconciliation gated on `supports_redirect_activation()`**

The block — placed AFTER the parent update so the new-parent lookup is correct — looks like:

```rust
    // Phase 2: Xorg-style redirect reconciliation on reparent.
    // Mirrors compUnredirectOneSubwindow + compRedirectOneSubwindow
    // at /home/jos/Projects/xserver/composite/compwindow.c:453-454.
    // The window's redirect state under RedirectSubwindows is
    // inherited from its parent — when the parent changes, the
    // inheritance changes too. RedirectWindow(W) is per-window
    // and not affected.
    //
    // Without this, a window created under
    // RedirectSubwindows(root, Manual) and reparented into a
    // non-redirected ancestor (e.g. an XEMBED tray client like
    // nm-applet reparenting into mate-panel's notification
    // socket) keeps a stale backing. Paints land in that stale
    // backing; the compositor reads the new ancestor's pixmap
    // (which never received them) when building COW.
    //
    // Gated on `supports_redirect_activation()` for the same
    // reason as `activate_redirect_backing_for` at
    // process_request.rs:3289 — backends that don't opt in
    // (v1, host-X11, and RecordingBackend in its default
    // configuration) don't manage redirect state, so the
    // redirect helpers would panic / silently misbehave there.
    // Phase 2 tests opt in via
    // `RecordingBackend::with_redirect_activation()` (see
    // Task 6) and `KmsBackendV2::for_tests()` (which already
    // returns `true`).
    if backend.supports_redirect_activation() {
        let old_parent_redirects_subwindows = state
            .composite_redirects
            .contains_key(&(old_parent, true));
        let new_parent_redirects_subwindows = state
            .composite_redirects
            .contains_key(&(new_parent, true));
        let directly_redirected = state
            .composite_redirects
            .contains_key(&(window, false));
        let had_backing = state
            .resources
            .window(window)
            .is_some_and(|w| w.redirected_backing.is_some());

        if !directly_redirected {
            if old_parent_redirects_subwindows
                && !new_parent_redirects_subwindows
                && had_backing
            {
                crate::core_loop::process_disconnect::teardown_redirect_for_window(
                    state, backend, origin, window,
                );
            } else if !old_parent_redirects_subwindows
                && new_parent_redirects_subwindows
                && !had_backing
            {
                let new_mode = state
                    .composite_redirects
                    .get(&(new_parent, true))
                    .expect("just checked contains_key")
                    .mode;
                activate_redirect_backing_for(state, backend, origin, window, new_mode);
            } else if old_parent_redirects_subwindows && new_parent_redirects_subwindows {
                let old_mode = state
                    .composite_redirects
                    .get(&(old_parent, true))
                    .map(|r| r.mode);
                let new_mode = state
                    .composite_redirects
                    .get(&(new_parent, true))
                    .map(|r| r.mode);
                if old_mode != new_mode
                    && let Some(new_mode) = new_mode
                {
                    flip_redirect_target_mode(state, backend, origin, window, new_mode);
                }
            }
        }
    }
```

Bindings `old_parent`, `new_parent`, and `origin` are already in scope of `handle_reparent_window` — adapt the binding names to match what the handler already uses. (If `old_parent` is captured under a different name, search the function for `parent` assignments.)

- [ ] **Step 8.3: Build**

```bash
cargo build -p yserver-core 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 8.4: Run the five failing tests**

```bash
cargo test -p yserver-core --lib reparent_out_of_redirected_subtree 2>&1 | tail -5
cargo test -p yserver-core --lib reparent_into_redirected_subtree 2>&1 | tail -5
cargo test -p yserver-core --lib reparent_with_direct_redirect 2>&1 | tail -5
cargo test -p yserver-core --lib reparent_between_redirected_parents_with_different_modes 2>&1 | tail -5
cargo test -p yserver --lib resolve_paint_target_after_reparent 2>&1 | tail -5
```

Expected: all PASS.

- [ ] **Step 8.5: Full workspace tests**

```bash
cargo test --workspace 2>&1 | tail -10
```

Expected: all pass; pre-existing failures unchanged. Triage any new failures — most likely candidates are reparent-related tests that implicitly relied on the no-reconcile behavior.

- [ ] **Step 8.6: Commit Phase 2**

```bash
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(composite): reconcile redirect on reparent (Phase 2)

handle_reparent_window now mirrors compUnredirectOneSubwindow +
compRedirectOneSubwindow at compwindow.c:453-454. When a window's
parent changes:

- old_parent had RedirectSubwindows + window inherited redirect:
  call teardown_redirect_for_window (frees backing, restores W's
  scene participation).
- new_parent has RedirectSubwindows + window has no backing yet:
  call activate_redirect_backing_for under the new parent's mode.
- both parents redirect: flip_redirect_target_mode if mode differs.
- RedirectWindow(W) is per-window: skipped (the !directly_redirected
  guard).

Gated on Backend::supports_redirect_activation() so backends that
don't opt in (v1, host-X11, default RecordingBackend) skip the
reconciliation. Same gate as the existing activate path at
process_request.rs:3289. Tests opt in via the new
RecordingBackend::with_redirect_activation() (Task 6) or
KmsBackendV2::for_tests() (already returns true).

Fixes the missing-tray-applets symptom: nm-applet and other XEMBED
tray clients reparent into mate-panel's notification socket after
creation. Pre-fix they kept their stale Manual-redirect backings
from the brief moment they were direct children of root; paints
went to the stale backing, the compositor read mate-panel's
pixmap (which never received those paints), COW ended up empty
in the tray area. Post-fix the inherited redirect is revoked on
reparent, paints flow up to mate-panel's redirected backing, the
compositor sees them, COW renders correctly."
```

## Task 9: Phase 3 — verification

- [ ] **Step 9.1: Regular clippy + nightly fmt**

```bash
cargo +nightly fmt
cargo clippy --all-targets
```

Expected: exit 0 (clippy itself fails on errors; the pass/fail signal is the cargo exit code, not a grep of the output). Pre-existing warnings unchanged; if a new warning appears, fix it. (No pedantic — AGENTS.md:11 requires regular clippy only.)

- [ ] **Step 9.2: rendercheck baseline**

```bash
just rendercheck-yserver 2>&1 | tail -20
```

Expected: no new failures vs the most recent baseline. Look at `target/rc-logs/` if any category regresses.

- [ ] **Step 9.3: Hardware smoke — the actual test**

```bash
just yserver-mate-hw-trace
```

Wait for steady-state. Visually verify:
- Top panel: applications/places/system on left, **system tray applets visible in the middle (nm-applet, pamac-tray, etc.)**, clock on right.
- Bottom panel: window list intact.
- Open Control Center: window visible with shadow.
- Drag Control Center: content remains visible during drag, shadow follows.
- Click panel "Applications" menu: popup appears with menu items.
- Click nm-applet's icon: wifi network list popup appears.

If all green → Phase 1+2 done. Capture a Ctrl+Alt+F12 bundle for the record under `target/diag/2026-05-20-cow-authoritative-pass.ppm` (or similar).

- [ ] **Step 9.4: Triage any new symptoms before declaring done**

Possible remaining failure modes (none are required to land 1+2; document them for follow-up):
- **ARGB COW**: depth-32 COW for compositors that depend on alpha. Symptom: black where transparency was expected, especially at shadow edges.
- **audits #12 / #13**: damage-delivery edge cases. Symptom: widgets that update infrequently don't repaint after their initial render.
- **Cursor visibility**: cursor disappears (scene.rs cursor-append logic needs auditing).

If any of these appear, file them as follow-up plans rather than expanding this one.

- [ ] **Step 9.5: Optional post-smoke fix commit**

If Steps 9.1-9.3 surfaced fixable issues that block declaring success, fix them with focused commits — one logical fix per commit.

- [ ] **Step 9.6: Merge prep — ask user before squashing to `rendering-model-v2`**

```bash
git log --oneline rendering-model-v2..HEAD
```

Confirm the commit list with the user; squash on their say-so per AGENTS.md:16.

---

## Self-review checklist

- [x] **Spec coverage**: Phase 1 (2a) + Phase 2 (Xorg-style reparent reconciliation) + Phase 3 (verify) all present. Out-of-scope items (ARGB COW, audits #12/#13) explicitly flagged as follow-ups, not in this plan.
- [x] **No placeholders for executable code**: the test snippets reference real helpers (`alloc_stub_window`, `ROOT_WINDOW`, `COMPOSITE_OVERLAY_WINDOW`, `dispatch_damage_subtract`-style test helpers); the comments next to fixture lines say "copy from the renamed test above" rather than "fill in details" — that's a concrete instruction to look at a specific cited block.
- [x] **Type consistency**: `RedirectRecord`, `CompositeRedirectMode`, `ResourceId`, `ClientId` match server.rs:163-179. `teardown_redirect_for_window` signature change is explicit. `Backend::supports_redirect_activation()` gates Phase 2 the same way it gates the existing redirect-activation path.
- [x] **TDD discipline**: every code task is preceded by a failing test, then minimal impl, then verify pass. Phase 2 has 5 tests (revoke, grant, direct-redirect survival, cross-mode flip, root-cause resolver) before any production code change. Each test pins one branch of the Task 8.2 reconciliation block so a partial implementation can't pass with green tests.
- [x] **Commit cadence**: 8 commits — rename existing tests (Task 1), add cow=Some failing test (Task 2), Phase 1 impl (Task 3), thread origin through teardown helper (Task 5), make RecordingBackend opt-in for redirect activation (Task 6), add Phase 2 failing tests (Task 7), Phase 2 wiring impl (Task 8), optional Phase 3 fixes (Task 9). Branch is feature-branch per AGENTS.md:15; squash-merge at the end per AGENTS.md:16.
- [x] **No invented APIs**: Phase 2 reuses `release_redirected_backing` (trait_def.rs:439) and `teardown_redirect_for_window` (process_disconnect.rs:298); no new `free_redirected_backing` is added.
- [x] **Backend gating**: Phase 2 wraps reconciliation in `if backend.supports_redirect_activation()` matching the existing pattern at process_request.rs:3289.
