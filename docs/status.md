# Status — Rendering Re-architecture

Working doc for the rendering re-architecture program tracked in
`docs/superpowers/specs/2026-05-12-rendering-rearchitecture.md` and
phase-detail plans under `docs/superpowers/plans/`.

The previous status doc (covering phases 1–6 and the host-X11 era)
is archived at `status-archive-2026-05-13.md`. Re-read it for
historical context; new work tracks here.

Cross-cutting bugs and followups that don't fit a phase live in
[`known-issues.md`](known-issues.md).

---

## Phase progress

### Done

- [x] **Phase 3A — Infrastructure** (`4af9e01`)
  - `PaintBatch` state machine: `Idle → Recording → Closed → Submitted → Retired`, plus `Poisoned` terminal.
  - `BatchUploadArena` (chunked host-visible bump allocator, 1 MiB → 64 MiB).
  - `BatchDescriptorArena` (per-batch descriptor pool, chunk-grown).
  - `BatchFlushReason` enum with strict/best-effort semantics.
  - `KmsBackend::record_paint_batch_op` (wide API) + `record_paint_op` (shim).
  - `paint_resources()` borrow-split helper, gates on `renderer_failed`.
  - Layout-state policy + CPU-visible / sync-export audit.
  - Plan: `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase3.md`
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3a-results.md`

- [x] **Phase 3B — Fill + copy-distinct + copy-same** (`82558a5`)
  - Migrated `try_vk_solid_fill`, `try_vk_fill_with_function`, fill-mirror-solid; `copy_area_distinct` (4 sites) and `copy_area_same` (1 site).
  - `run_legacy_paint_op` wrapper for non-migrated recorders.
  - `renderer_failed` gate on all paint paths.
  - Drawable-destruction barriers (salvage after AMD VM_CONTEXT fault) at 5 sites: `DestroyWindow`, `configure_window` resize, `FreePixmap`, `RenderFreePicture`, `RenderCreateCursor` rescue path. Strict-flush failure preserves lifetime invariant via `mem::forget` / leave-in-place.
  - `feedback_kmsbackend_drop_order` + `feedback_paintbatch_destruction_barrier` memories.
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3b-results.md`

- [x] **Phase 3C — PutImage + cursor mirror** (`ac270d9`)
  - Migrated `try_vk_put_image` and `upload_bgra_to_mirror` to per-batch `BatchUploadArena` staging.
  - Gradient-create `flush_if_needed(ProtocolBarrier)` (conservative protocol boundary).
  - Outer-flag OOM-poison-avoidance pattern (both T1 + T2 fixes folded after codex review).
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3c-results.md`

- [x] **Phase 3D — copy-same-overlap** (`47f213f`)
  - Migrated `try_vk_copy_area` same-overlap arm to `record_paint_batch_op`.
  - `CopyScratch::needs_grow` + pre-resize `flush_if_needed(ProtocolBarrier)` to prevent the dangling-image hazard (`ensure_size`'s `queue_wait_idle` doesn't wait for un-submitted commands).
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3d-results.md`

- [x] **KMS teardown fix** (`a693255`) — inter-phase, between 3D and 3E
  - Codex-pinpointed P0: pre-fix shutdown left DRM framebuffers bound to CRTCs, breaking host Wayland sessions (`labwc`/`dms`) until reboot.
  - 6-step `disable_output`: stop composites → flush PaintBatch → vkDeviceWaitIdle → drain DRM pageflip-completes → atomic disable → force-reset (success) / disarm (failure).
  - `ScanoutBo::disarm` + `Buffer::disarm` for failed-output paths (RAII Drop becomes no-op; resources leak until DRM-fd close at process exit reaps).
  - Atomic `disable_output` itself still EINVALs (see followups); disarm makes it harmless.
  - Plan: `docs/superpowers/plans/2026-05-13-kms-teardown-fix.md`
  - Results: `docs/superpowers/plans/2026-05-13-kms-teardown-fix-results.md`

- [x] **Phase 3E — Text-run** (`492b4bc`)
  - Migrated `try_vk_text_run` and `try_vk_render_composite_glyphs` to `record_paint_op`.
  - Single `paint_resources()` call before the intern loop (gates atlas upload on `renderer_failed`).
  - `GlyphAtlas::intern` intentionally unchanged (its per-glyph `queue_wait_idle` is phase-5 sync-rework scope).
  - Hardware smoke: MATE renders, gedit fast text-scroll observed.
  - Interleaved fixes during 3E smoke: composite-pool-release per-frame (`cb44c1d`), Composite mode-constant attempt + revert (`92a2a83` → `3751c11`, filed).
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3e-results.md`

### Inter-phase chores landed alongside

- [x] **Composite defer log summary** (`4c4741b`) — turn per-frame `pool_ring_exhausted` warn-spam into a periodic 5s `info!` summary.
- [x] **Scroll-wheel support** (`b7d17a1`) — `InputEvent::PointerScroll` + libinput axis translation + synthetic-button-code mapping to X11 buttons 4/5/6/7. `has_axis` fix (`56f93d9`) closes a libinput "client bug" log flood.
- [x] **Composite pool-release per-frame** (`cb44c1d`) — fixed a pre-existing FIFO-drain bug where one lagging output held pool slots hostage for already-retired frames on the other output. Caught by codex during 3E smoke.

### Remaining — in priority order

- [ ] **Phase 3F — Render-composite + traps + scratch infrastructure**
  - Recorders: `try_vk_render_composite` (RENDER Composite), `try_vk_render_traps_or_tris` (RENDER Trapezoids/Triangles).
  - Per-batch `MaskScratch::upload_r8` staging (currently shared host buffer, aliases between in-batch ops).
  - `dst_readback` strategy for non-Over RENDER operators (current `ensure` grow has the same `queue_wait_idle + destroy` hazard as 3D's `CopyScratch`).
  - Sized roughly like 3D × 2; may split into 3F-1 (render-composite + dst_readback) and 3F-2 (traps + MaskScratch) if scope rules.
- [ ] **Phase 4 — Sync rework**
  - Retire `vkQueueWaitIdle` from `run_one_shot_op` (the hot-path drain).
  - Retire the close-time wait in `PaintBatch::submit_and_wait`.
  - Real `VkFence` polled via `vkGetFenceStatus`, or timeline semaphore. Many readers + writers; non-blocking status checks at composite poll.
- [ ] **Phase 5 — Targeted `VkFence` for record_get_image + atlas grow**
  - `record_get_image` still on `run_one_shot_op + queue_wait_idle` (4 readback handlers).
  - `GlyphAtlas::intern`'s per-glyph one-shot upload + waitidle (phase-3E deliberate defer).
  - `MaskScratch` / `CopyScratch` / `dst_readback` `ensure_size` grow paths (after 3F migrates their consumers, the grow can defer through the batch retire queue instead of waitidle).
- [ ] **Phase 6 — Resource lifetime: batch-owned refcounted handles**
  - Codex's long-term recommendation from 3B salvage: instead of relying on protocol destruction barriers + `queue_wait_idle`, adopt destroyed VkImages into the open `PaintBatch` via `BatchResource` so destruction defers automatically.
  - Subsumes the 3D needs_grow + pre-resize-flush pattern for `CopyScratch`, the analogous patterns 3F will introduce for `MaskScratch` + `dst_readback`, and the 3B destruction-barrier collection.

---

## Followups not on the rework critical path

See `known-issues.md` for the full ticklist with detail. Highlights tracked here for awareness during rework planning:

- [ ] **`disable_output` atomic EINVAL** — recurring shutdown warn; disarm path mitigates but per-property split is the real fix.
- [ ] **Composite Manual-mode regression** — `92a2a83` reverted; needs decoupling of redirect-record from `redirected_backing` allocation.
- [ ] **Caja right-click popup offset** — coordinate-translation bug, surfaced 2026-05-13.
- [ ] **Caja wheel needs view-switch** — yserver event-delivery regression; 3 bisect candidates filed.
- [ ] **Color R↔B swap on JPEG backgrounds** — likely PutImage byte-permutation vs visual-byte-order mismatch.
- [ ] **`minor_code = 0` hardcoded in extension error encoder** — debug-clarity bug; threading the minor through `emit_x11_error` (~60-80 call sites).
- [ ] **Listener starvation under chatty clients** — single-threaded core loop's per-iteration read budget is unbounded; xfce4-panel couldn't complete X11 setup handshake while xfdesktop flooded QueryPointer.
- [ ] **xfce4 text rendering broken** — may or may not be fixed by 3E; needs revalidation.
- [ ] **XInput2 valuator scroll** — GTK apps that depend on XI2 axis events don't see the wheel until they fall back to core button-4/5.
- [ ] **Per-glyph queue_wait_idle in `GlyphAtlas::intern`** — phase-5 scope but called out so 3E results aren't read as "text path is fully batched."

---

## Source-of-truth pointers

- HLD: `docs/superpowers/specs/2026-05-12-rendering-rearchitecture.md`
- Phase plans: `docs/superpowers/plans/2026-05-1[23]-rendering-rearchitecture-phase3{a,b,c,d,e,f}.md`
- Phase results: same directory, `*-results.md` suffix.
- Cross-cutting bugs: `known-issues.md`
- Pre-rework history: `status-archive-2026-05-13.md`
- Per-skill memory: `~/.claude/projects/-home-jos-Projects-yserver/memory/MEMORY.md`
