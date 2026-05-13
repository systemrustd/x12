# Phase 3E — rendering re-architecture — text-run migration

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate the two text-run recorder call sites — `try_vk_text_run` (core PolyText8/16) and `try_vk_render_composite_glyphs` (RENDER CompositeGlyphs) — from `run_legacy_paint_op + run_one_shot_op` to `record_paint_batch_op`, eliminating the per-text-run `flush_if_needed(ProtocolBarrier)` that fires under every GTK widget re-render.

**Architecture:** Both call sites use the same recorder `text::record_text_run` which samples the global `GlyphAtlas` via the persistent `TextPipeline::descriptor_set`. No `BatchDescriptorArena` work is required — text reads from a long-lived sampler binding. `GlyphAtlas::intern` (the glyph-upload step) stays as a self-contained one-shot for now; its per-glyph `queue_wait_idle` is a separate scope (phase 5 or a batch-aware atlas-upload pass). 3E just unwraps the recorder side so text-runs land in the open `PaintBatch` alongside fill / copy / PutImage.

**Tech Stack:** Rust, ash (Vulkan), the existing 3A–3D infrastructure (`PaintBatch`, `record_paint_batch_op`, `paint_resources()`, `renderer_failed` gate, drawable-destruction barriers, audit catalogue).

---

## Prerequisite — confirm post-3D + post-Composite-fix baseline

Before T1, verify the tree state:

```bash
cd /home/jos/Projects/yserver
git log --oneline graphics-followups | head -10
rg -n 'record_paint_batch_op' crates/yserver/src/kms/backend.rs | head
```

Expected:
- The composite mode-constants fix (`92a2a83`) is in the tree (xfwm4 / xfce4 starts past the Composite handshake).
- KMS teardown fix (T1–T3 + results, ending around `a693255` / `b41ac38`) is landed — `disable_output` follows the 6-step sequence and disarms on failure.
- ≥ 3 `record_paint_batch_op` call sites in `backend.rs`: `try_vk_put_image`, `upload_bgra_to_mirror`, `try_vk_copy_area` (same-overlap).

If any of those are missing, STOP — the prerequisite chain didn't fully land.

## Phase context

Read `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3d-results.md` first. 3D narrowed the COPY family to one recorder (same-overlap); 3E narrows the TEXT family to two recorders (text-run × 2 call sites). Reasoning matches 3D's: keep each phase small enough to land cleanly, defer mask-scratch / dst_readback / render-composite to **3F** because those need their own infrastructure (per-batch arena staging for MaskScratch, plus dst_readback layout-state work).

Key invariants 3E inherits from 3A/3B/3C/3D:

1. **Drop-order**: `KmsBackend.scheduler` before `KmsBackend.ops_command_pool`. Don't touch.
2. **Drawable-destruction barriers**: 3B's 5 sites cover text-run targets (windows + pixmaps). No new barriers.
3. **`renderer_failed` gate**: via `paint_resources()` on every paint entry.
4. **`record_paint_batch_op`** is the load-bearing API for recorders. `record_paint_op` (shim) is fine for recorders that don't need `&mut PaintBatch`. Text-run **doesn't** need the wide API (atlas + pipeline are shared, not per-batch), so either works. **Use `record_paint_op` (the shim)** to keep the call site simple — matches the 3B fill/copy template more directly.
5. **`shutting_down` gate**: already wired in 3B's `composite_and_flip` and 3C's KMS-teardown work. Text-run doesn't add new entry points so no new gate.

## Out of scope — deferred to phase 3F

- `try_vk_render_traps_or_tris` (RENDER Trapezoids / Triangles) — uses **MaskScratch** to rasterize trapezoid coverage into an alpha mask, then composites. Needs MaskScratch's `upload_r8` migrated to arena-staging + a needs_grow pre-flush (3D's CopyScratch pattern). Sized as 3F-1.
- `try_vk_render_composite` (RENDER Composite) — uses **dst_readback** for non-Over operators (reads existing dst pixels into a scratch image, blends, writes). dst_readback's grow path has the same dangerous queue_wait_idle-then-destroy shape as CopyScratch. Sized as 3F-2.

3E lands first so 3F can build on the same audit-catalogue / verification scaffolding.

## File structure

| File | Role | Touched in |
|---|---|---|
| `crates/yserver/src/kms/backend.rs` | Migrate `try_vk_text_run` (~line 4430) + `try_vk_render_composite_glyphs` (~line 5181); update `run_legacy_paint_op` audit catalogue (~line 1697 doc block) | T1, T2, T3 |
| `crates/yserver/src/kms/vk/ops/text.rs` | `record_text_run` signature unchanged — it's already arena-free (atlas + pipeline are pre-existing, mirror is `&mut DrawableImage`). | (read only) |
| `crates/yserver/src/kms/vk/glyph.rs` | `GlyphAtlas::intern` keeps its self-contained one-shot upload + `queue_wait_idle`. No change in 3E. | (read only) |
| `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3e-results.md` | Results doc | T3 |

## Pre-task notes (read before starting)

1. **`GlyphAtlas::intern` stays as-is.** It runs its own one-shot CB to copy staging → atlas, then `queue_wait_idle`. The waitidle is wasteful (every new glyph blocks) but it's a self-contained synchronization point — the atlas is `SHADER_READ_ONLY_OPTIMAL` when intern returns. Subsequent batch-recorded reads of the atlas observe the new glyph correctly because (a) intern's submit completes before our subsequent batch submit, and (b) the atlas image's layout state is updated CPU-side. Out of scope for 3E. Phase 5 or a batch-aware `intern_into_batch` API can revisit.

2. **`record_text_run` reads atlas via pipeline's persistent descriptor.** `TextPipeline::descriptor_set` is allocated once at backend init and binds the atlas's `VkImage` + sampler. The recorder calls `cmd_bind_descriptor_sets(pipeline.descriptor_set)` — no per-batch descriptor allocation needed. This is **why 3E doesn't need BatchDescriptorArena**: text-run is structurally simpler than render-composite / traps will be.

3. **Borrow split**: 4 disjoint field borrows must coexist in T1 and T2's migrated code:
   - `&mut self.scheduler` (via the `record_paint_op` call).
   - `&mut self.windows[xid].vk_mirror` OR `&mut self.pixmaps[xid].vk_mirror` (the `mirror` argument).
   - `&self.glyph_atlas` (read-only).
   - `&self.text_pipeline` (read-only).
   Rust handles this via direct field paths. `intern` is `&mut self.glyph_atlas` and runs BEFORE the recorder, so the mut borrow drops before the shared borrow needed by the closure.

4. **No OOM-poison concern in 3E**: text-run allocates nothing per-batch (no arena, no descriptor pool — pipeline.descriptor_set is persistent). The recorder records draws into the batch CB; the closure returns Ok or Err on `cmd_*` failure. No special handling needed beyond what `record_paint_op` already does.

5. **Atlas state coherence under batching**: the atlas image's `current_layout` field is mutated CPU-side by `intern`'s barriers. The recorder reads `atlas.extent()` (CPU-side, no state change) and the pipeline does the sample. There's no risk of the recorder seeing a stale atlas state because intern completes (including waitidle) before the recorder runs. The waitidle ALSO drains any prior batch submits, which is wasteful but safe.

6. **No new destruction barriers needed.** Text-run targets are windows/pixmaps, both covered by 3B's destruction barriers (`destroy_window`, `free_pixmap`, etc.). The atlas image itself is owned by `GlyphAtlas` on `KmsBackend` and only drops at backend teardown (where the KMS teardown fix already handles disarm). The text pipeline similarly persists across backend lifetime.

7. **Test coverage**: neither function has a direct unit test (coverage is xts5 + rendercheck + hardware smoke). Don't add a test. Hardware smoke (T3) is the gate.

8. **clippy**: project preference is plain `cargo clippy`. 5 pre-existing `doc_lazy_continuation` warnings; no new ones.

---

## Task 1: Migrate `try_vk_text_run` to `record_paint_op`

**Goal:** Replace the `flush_if_needed(ProtocolBarrier) + run_one_shot_op` shape with `paint_resources()` + `self.scheduler.record_paint_op(...)`. The intern loop above stays unchanged.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (`try_vk_text_run`, lines ~4430–4551)

### Step 1: Read the existing function

- [ ] **Step 1: Read backend.rs lines 4430–4551**

Note the structure:
1. Lines 4430–4445: parameter check, early returns on empty input. **Keeps.**
2. Lines 4446–4451: **OLD** raw `self.vk.as_ref().cloned()` + `self.ops_command_pool.as_ref().map(|p| p.handle())` bindings. Used today by the intern loop's `pool_handle` arg AND by `run_one_shot_op` below. **REPLACED** with a single `paint_resources()` call that gates on `renderer_failed`.
3. Lines 4452–4470: atlas / pipeline presence check + mirror format check. **Keeps.**
4. Lines 4472–4494: intern loop. **Keeps as-is** (self-contained one-shot per glyph, uses `pool_handle` from step 2 — now sourced from `paint_resources()`).
5. Lines 4496–4501: `foreground_rgba` build. **Keeps.**
6. Lines 4503–4513: inline `flush_if_needed(BatchFlushReason::ProtocolBarrier)`. **REMOVED.**
7. Lines 4515–4529: mirror + atlas + pipeline borrows. **Keeps** (these are the closure's captures).
8. Lines 4531–4550: `run_one_shot_op(...) { record_text_run(...) }`. **Replaced by `self.scheduler.record_paint_op(...)`**, reusing `vk_arc` / `pool_handle` from step 2.

### Step 2: Apply the migration

The key structural change codex flagged in plan-v1 review: replace BOTH the early `vk_arc`/`pool_handle` raw binding AND the later one-shot recorder with a **single** `paint_resources()` call placed BEFORE the intern loop. This (a) gates the glyph upload on `renderer_failed`, (b) removes the duplicate binding that becomes unused after the migration, (c) keeps the borrow split clean.

- [ ] **Step 2a: Replace the early raw binding (lines ~4446–4451)**

Find:

```rust
        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return false;
        };
```

Replace with:

```rust
        // 3E: acquire batch resources up-front (gated by
        // renderer_failed). Atlas upload (intern) and the recorder
        // BOTH route through this — single source for vk_arc /
        // pool_handle removes the duplicate binding that pre-3E
        // had (one for intern's pool, one for run_one_shot_op's
        // submit).
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            return false;
        };
```

- [ ] **Step 2b: Keep the intern loop unchanged**

The intern loop body uses `pool_handle` (from step 2a) — no edit needed inside the loop.

- [ ] **Step 2c: Replace the flush + run_one_shot_op block (lines ~4503–4550)**

Replace the entire block from the inline `use crate::kms::scheduler::paint_batch::BatchFlushReason;` flush down through the `match run_one_shot_op(...) { ... }` (and its closing brace) with:

```rust
        // Re-borrow the mirror mutably for the recording. The
        // closure also captures atlas + pipeline (read-only) from
        // disjoint fields. self.scheduler.record_paint_op is the
        // remaining mutable borrow; field-disjoint, so the borrow
        // checker accepts.
        let mirror = if let Some(w) = self.windows.get_mut(&host_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&host_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(mirror) = mirror else {
            return false;
        };
        let atlas = self.glyph_atlas.as_ref().expect("checked above");
        let pipeline = self.text_pipeline.as_ref().expect("checked above");

        match self
            .scheduler
            .record_paint_op(vk_arc, pool_handle, |vk, cb| {
                vk_text::record_text_run(
                    vk,
                    cb,
                    mirror,
                    atlas,
                    pipeline,
                    &glyphs_to_draw,
                    foreground_rgba,
                )
            }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk text_run: record failed on xid {host_xid:#x}: {e:?} — falling back to \
                     pixman"
                );
                false
            }
        }
    }
```

- [ ] **Step 2d: Delete the unused `run_one_shot_op` import**

Find the `use` statement at the top of the function:

```rust
        use crate::kms::vk::{
            glyph::GlyphKey,
            ops::{run_one_shot_op, text as vk_text},
        };
```

Change to:

```rust
        use crate::kms::vk::{glyph::GlyphKey, ops::text as vk_text};
```

### Step 3: Build

- [ ] **Step 3: `cargo check -p yserver`**

Expected: clean.

If you see `cannot borrow self.scheduler as mutable because self.* is already borrowed`: confirm the order is (a) `paint_resources()` (shared), (b) mirror re-borrow via direct `self.windows.get_mut`/`self.pixmaps.get_mut`, (c) atlas/pipeline via direct `self.glyph_atlas.as_ref()` / `self.text_pipeline.as_ref()`, (d) `self.scheduler.record_paint_op(...)`. Each binding is a `&mut` or `&` to a distinct field of `self`.

If `vk_arc`/`pool_handle` is reported unused after this edit, that's a sign the closure didn't capture them — make sure the call signature matches `record_paint_op(vk_arc, pool_handle, |vk, cb| { ... })`.

### Step 4: Run tests

- [ ] **Step 4: `cargo test -p yserver --lib`**

Expected: 138 passed, 0 failed, 3 ignored (same as post-teardown baseline).

### Step 5: fmt + clippy

- [ ] **Step 5: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 6: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing `doc_lazy_continuation` warnings; no new ones.

### Step 6: Commit T1

- [ ] **Step 7: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "$(cat <<'EOF'
refactor(kms): migrate try_vk_text_run to record_paint_op

Replaces the unconditional `flush_if_needed(ProtocolBarrier) +
run_one_shot_op` shape in try_vk_text_run with paint_resources() +
self.scheduler.record_paint_op(...). Core PolyText8/16 text runs
now append into the open PaintBatch alongside fill / copy / PutImage
recorded in the same protocol cycle — no per-text-run flush.

GlyphAtlas::intern is unchanged: it stays a self-contained one-shot
upload with internal queue_wait_idle. Per-glyph wait is a separate
scope (phase 5 sync rework or a batch-aware atlas-upload pass).
The recorder side reads atlas via TextPipeline::descriptor_set,
which is a persistent binding — no per-batch descriptor work is
required for the text-run family.

Borrow split: scheduler / mirror / atlas / pipeline are all
disjoint fields of KmsBackend; the closure captures them via
direct field paths.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Migrate `try_vk_render_composite_glyphs` to `record_paint_op`

**Goal:** Same migration shape as T1, applied to the RENDER `CompositeGlyphs` path. Mechanically nearly identical body.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (`try_vk_render_composite_glyphs`, lines ~5181–5473)

### Step 1: Read the existing function

- [ ] **Step 1: Read backend.rs lines 5181–5473**

Most of the function (5181–5421) is RENDER-protocol parsing — walking the `items` byte buffer to build `Vec<TextGlyph>`. All of that is unchanged. The migration touches the early raw `vk_arc`/`pool_handle` binding (~lines 5244–5258) AND the tail (`flush_if_needed` + `run_one_shot_op` ~lines 5427–5472).

Same shape as T1. Codex's plan-v1 review caught that both functions have a duplicate binding once the run_one_shot_op call is replaced — fold the change into a single `paint_resources()` call before the intern loop.

### Step 2: Apply the migration

- [ ] **Step 2a: Replace the early raw binding (lines ~5244–5258)**

Find:

```rust
        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            log::debug!("vk text bail: no Vk context");
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            log::debug!("vk text bail: no ops_command_pool");
            return false;
        };
```

Replace with:

```rust
        // 3E: acquire batch resources up-front (gated by
        // renderer_failed). Atlas upload (intern) and the recorder
        // BOTH route through this — single source for vk_arc /
        // pool_handle.
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            log::debug!("vk text bail: paint_resources unavailable (renderer_failed or vk/pool absent)");
            return false;
        };
```

(Folding the two prior log-debug lines into one is acceptable — they were two separate bail reasons before but `paint_resources()` returns `None` for any of the three conditions, so a single log line is fine.)

- [ ] **Step 2b: Keep the intern loop unchanged**

The intern loop's `atlas.intern(..., pool_handle)` call already references `pool_handle` from step 2a. No edit inside the loop.

- [ ] **Step 2c: Replace the flush + run_one_shot_op block (lines ~5427–5472)**

Replace the entire `use crate::kms::scheduler::paint_batch::BatchFlushReason; if let Err(e) = self.flush_if_needed(...)` block AND the subsequent `match run_one_shot_op(...) { ... }` call with:

```rust
        // Re-borrow the mirror mutably for the recording. The
        // closure also captures atlas + pipeline (read-only) from
        // disjoint fields. self.scheduler.record_paint_op is the
        // remaining mutable borrow; field-disjoint, so the borrow
        // checker accepts.
        let mirror = if let Some(w) = self.windows.get_mut(&dst_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(mirror) = mirror else {
            return false;
        };
        let atlas = self.glyph_atlas.as_ref().expect("checked above");
        let pipeline = self.text_pipeline.as_ref().expect("checked above");

        match self
            .scheduler
            .record_paint_op(vk_arc, pool_handle, |vk, cb| {
                vk_text::record_text_run(
                    vk,
                    cb,
                    mirror,
                    atlas,
                    pipeline,
                    &glyphs_to_draw,
                    foreground_premul,
                )
            }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk render_composite_glyphs: record failed on dst xid {dst_xid:#x}: {e:?} \
                     — falling back to pixman"
                );
                false
            }
        }
    }
```

- [ ] **Step 2d: Delete the unused `run_one_shot_op` import**

Find the `use` statement near line 5194-5197:

```rust
        use crate::kms::vk::{
            glyph::GlyphKey,
            ops::{run_one_shot_op, text as vk_text},
        };
```

Change to:

```rust
        use crate::kms::vk::{glyph::GlyphKey, ops::text as vk_text};
```

### Step 3: Build

- [ ] **Step 3: `cargo check -p yserver`**

Expected: clean.

### Step 4: Run tests

- [ ] **Step 4: `cargo test -p yserver --lib`**

Expected: 138 passed.

### Step 5: fmt + clippy

- [ ] **Step 5: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 6: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing warnings; no new ones.

### Step 6: Commit T2

- [ ] **Step 7: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "$(cat <<'EOF'
refactor(kms): migrate try_vk_render_composite_glyphs to record_paint_op

Same migration shape as T1 (try_vk_text_run): drop the
unconditional ProtocolBarrier flush + run_one_shot_op, replace
with paint_resources() + self.scheduler.record_paint_op(...).
RENDER CompositeGlyphs runs now coexist in the open PaintBatch.

The intern loop is unchanged. The recorder body samples atlas via
TextPipeline::descriptor_set (persistent binding). Borrow split
matches T1.

Closes the text-run family migration.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Update audit catalogue + validation + results doc

**Goal:** Cleanup + end-to-end verification + results doc following the 3A/3B/3C/3D template.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (the `run_legacy_paint_op` doc catalogue near line 1697 + the two inline "3D-deferred: text-run needs..." comments inside the migrated functions)
- Create: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3e-results.md`

### Step 1: Update the audit catalogue

- [ ] **Step 1: Find the catalogue**

Run: `grep -n "Phase-3B T0 catalogue\|run_legacy_paint_op catalogue\|try_vk_text_run\|try_vk_render_composite_glyphs" crates/yserver/src/kms/backend.rs`

The doc-comment block in `run_legacy_paint_op` (~line 1697) lists every paint-side `run_one_shot_op` site and its migration status. The text-run entries currently read like:

```
///   try_vk_text_run:                  text::record_text_run               — borrow-conflict fallback
///   try_vk_render_composite_glyphs:   text::record_text_run               — borrow-conflict fallback
```

(Exact text may vary — the labels updated through 3B/3C/3D iterations. Look at how `try_vk_put_image` is labeled — `migrated 3C T1 (record_paint_batch_op + arena)` — and mirror that style.)

- [ ] **Step 2: Update both entries**

Change them to:

```
///   try_vk_text_run:                  text::record_text_run               — migrated 3E T1 (record_paint_op)
///   try_vk_render_composite_glyphs:   text::record_text_run               — migrated 3E T2 (record_paint_op)
```

### Step 2: Remove stale "3D-deferred" inline comments inside the two migrated functions

- [ ] **Step 3: Find them**

Run: `grep -n "3D-deferred: text-run" crates/yserver/src/kms/backend.rs`

There should be ZERO hits after T1 and T2 (those comments were inside the removed `flush_if_needed` blocks). Confirm with the grep. If any survive, they're stale — delete the orphan comment block.

### Step 3: Static verification

- [ ] **Step 4: Cutover greps**

```bash
cd /home/jos/Projects/yserver
rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
```
Expected sites (semantic, not numeric): `run_legacy_paint_op` body, 3 readback handlers (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`), **3F-deferred** borrow-conflict fallbacks (render-composite × 2, traps × 1, copy-same-overlap is migrated as of 3D). ZERO hits inside `try_vk_text_run` or `try_vk_render_composite_glyphs`.

```bash
rg -n 'record_paint_op\|record_paint_batch_op' crates/yserver/src/kms/backend.rs
```
Expected: at least 5 call sites total (3B fill × 4 + 3B copy × 2 + 3C put_image + 3C upload_bgra + 3D same-overlap + 3E text-run × 2). Should be ≥ 9.

```bash
rg -n '3D-deferred: text-run' crates/yserver/src/kms/backend.rs
```
Expected: ZERO hits.

- [ ] **Step 5: Tree green**

```bash
cargo +nightly fmt --check
cargo clippy -p yserver 2>&1 | tail -10
cargo test --workspace 2>&1 | tail -15
```

Expected:
- fmt: no diff.
- clippy: 5 pre-existing `doc_lazy_continuation` warnings; no new ones.
- tests: yserver lib 138 passed; workspace green.

### Step 4: Hardware smoke (REQUIRED — text-run is the next thing real GTK clients exercise heavily)

- [ ] **Step 6: Run from a separate TTY (per phase-3D-results.md teardown workflow)**

The KMS teardown fix is landed, so yserver exits cleanly without breaking the host Wayland session. Still test from F3 to F1 toggle as a precaution.

```bash
just yserver-mate-hw-release
# OR `just yserver-xfce-hw` if the user wants to revalidate the xfce4 path
# (xfwm4 should now start since the Composite mode-constant fix in 92a2a83)
```

Exercise text-heavy workloads:
- xterm with `seq 1 5000` (lots of glyph-atlas hits).
- mate-text-editor / pluma — type and watch text rendering.
- mate-control-center hover — text labels everywhere.
- File-manager view with many filenames.

**Pass criteria**:
- No `vk text_run: record failed` or `vk render_composite_glyphs: record failed` warns in `yserver-hw.log` under normal use.
- No `paint batch submit failed` / `renderer_failed` / `DEVICE_LOST`.
- Subjectively smoother text rendering than pre-3E (text-run pre-flush is gone, so text-heavy frames pack into one batch instead of many).
- No kernel GPU fault.

The text-rendering issue surfaced in the xfce4 broaden-test-surface session (`docs/known-issues.md`'s "Text rendering broken under xfce4 / GTK heavy workloads" entry) **may or may not** be fixed by 3E. Worth checking specifically: re-run `just yserver-xfce-hw` and see if the dialog text is now readable. If it's still illegible, that's a separate rendering bug not solved by removing the flush.

### Step 5: Write results doc

- [ ] **Step 7: Create `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3e-results.md`**

Follow the 3A/3B/3C/3D template. Sections:

1. **Header**: title, date, plan ref, branch `graphics-followups`, predecessor (KMS teardown fix results doc `a693255`).
2. **Scope landed**: paragraph + bullets describing T1 + T2 + T3. Note that `GlyphAtlas::intern` is intentionally unchanged.
3. **Preflight checks**: real fmt / clippy / test counts.
4. **Cutover greps**: actual `rg` output captured semantically.
5. **Done conditions** matching the section below.
6. **Hardware smoke results**: report the actual run.
7. **Plan bugs caught (folded back into plan)**: any recipe issues hit during T1/T2.
8. **Commit summary** table: Plan, T1, T2, T3.
9. **Known deferred items**: 3F = render-composite + traps + MaskScratch arena-staging + dst_readback strategy. Also: `GlyphAtlas::intern`'s per-glyph `queue_wait_idle` is phase 5 territory.
10. **What's next**: phase 3F planning.

### Step 6: Commit T3

- [ ] **Step 8: Commit**

```bash
git add crates/yserver/src/kms/backend.rs docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3e-results.md
git commit -m "$(cat <<'EOF'
docs(plans): phase-3E validation results

T1 + T2 migrated try_vk_text_run and try_vk_render_composite_glyphs
to record_paint_op. The unconditional ProtocolBarrier flush before
each text-run is gone; text-heavy frames now pack into the open
PaintBatch alongside fill / copy / PutImage.

GlyphAtlas::intern is intentionally unchanged; its per-glyph
queue_wait_idle is a separate scope (phase 5 sync rework).

Hardware smoke on <host>: <result>.

Phase 3F (render-composite + traps + MaskScratch + dst_readback)
is the next piece.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Done conditions

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` produces 5 pre-existing `doc_lazy_continuation` warnings; no new warnings.
3. `cargo test --workspace` green; yserver lib 138 passed.
4. `try_vk_text_run` uses `record_paint_op` with 4 disjoint field borrows (scheduler + mirror + atlas + pipeline).
5. `try_vk_render_composite_glyphs` similarly migrated.
6. The OLD inline `flush_if_needed(BatchFlushReason::ProtocolBarrier)` blocks at the tail of both functions are GONE.
7. The `run_legacy_paint_op` audit catalogue (~backend.rs:1697) reflects T1 + T2 migrations.
8. `GlyphAtlas::intern` is unchanged (verified by `git diff` on `crates/yserver/src/kms/vk/glyph.rs` — expect ZERO changes).
9. Hardware smoke green per T3 step 6 — no `vk text_run: record failed`, no `paint batch submit failed`, no kernel GPU fault under text-heavy workloads.

## Cutover greps (post-3E — semantic, not numeric)

```
$ rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
# SITES expected: run_legacy_paint_op body, 3 readback handlers
# (try_vk_get_image_pixels, hw_cursor_refresh, read_mirror_pixels),
# 3F-deferred fallbacks (render-composite × 2, traps × 1),
# open_with_commit, dump_scanout_one. ZERO hits inside
# try_vk_text_run or try_vk_render_composite_glyphs.

$ rg -n 'record_paint_op\|record_paint_batch_op' crates/yserver/src/kms/backend.rs
# Expected: ≥ 9 call sites (3B fill × 4 + 3B copy × 2 + 3C × 2 +
# 3D × 1 + 3E × 2).

$ rg -n '3D-deferred: text-run' crates/yserver/src/kms/backend.rs
# Expected: ZERO hits (those comments were inside the removed
# pre-flush blocks).
```

## Out-of-scope deferred to phase 3F

- `try_vk_render_traps_or_tris` (RENDER Trapezoids / Triangles) — uses **MaskScratch**.
- `try_vk_render_composite` (RENDER Composite) — uses **dst_readback** for non-Over operators.
- `MaskScratch::upload_r8` migration: convert from one-shot upload to arena-staged batch upload. Same shape as 3D's CopyScratch `needs_grow` + pre-resize flush pattern. The mask scratch image itself stays shared (single backend-wide R8 image).
- `dst_readback::ensure` grow path: same dangerous queue_wait_idle-then-destroy as CopyScratch. Needs a `needs_grow` accessor.

## Notes for the implementer

- **The borrow split is the only structural risk.** The 4-disjoint-field pattern is well-established by 3B/3C/3D; if cargo check complains, you forgot direct field paths. The pattern: `paint_resources()` first, then `mirror` via `self.windows.get_mut`/`self.pixmaps.get_mut`, then atlas + pipeline via `self.glyph_atlas.as_ref()` + `self.text_pipeline.as_ref()`, then `self.scheduler.record_paint_op(...)`.
- **`mirror: &mut DrawableImage` and the read-only atlas / pipeline references are moved into the FnOnce closure.** Don't reuse them after the call returns.
- **`record_text_run` is unchanged.** The recorder itself doesn't care whether it's inside a one-shot CB or a batched CB — its barriers + draws record the same way.
- **No tests to write.** Coverage comes from hardware smoke under text-heavy workloads.
- **Watch for the xfce4 text-rendering followup**: if T3's smoke reproduces the "unreadable dialog text" reported in the xfce4 broaden-test-surface session, file it more precisely. Most likely culprit if it persists: the `CompositeGlyphs xSrc/ySrc ≠ pen` invariant (already in feedback memory) interacting with GTK's font fallback chain. Separate from 3E scope.
