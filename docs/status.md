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

- [x] **Phase 3F-1 — Render-composite migration** (`fade626` + fix-ups `c4a4965`)
  - Migrated `try_vk_render_composite` to `record_paint_batch_op`. Descriptor set allocated per-batch via `RenderPipelineCache::allocate_descriptor_for_views_into` + `BatchDescriptorArena` (T2, `3fe108b`). `DstReadback::needs_grow` accessor (T1, `afc18f6`) + pre-resize flush gate prevents the same dangling-image hazard 3D fixed for `CopyScratch`.
  - Unconditional pre-record `ProtocolBarrier` flush before each RENDER Composite is gone; composite-heavy frames now pack into the open `PaintBatch` alongside fill / copy / put_image / text.
  - `try_vk_render_traps_or_tris` and the legacy shared-pool allocator + `reset_descriptors` deliberately retained for 3F-2.
  - Hardware smoke: TBD (user-owned).
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-1-results.md`

- [x] **Phase 3F-2 — Render-traps/triangles migration + MaskScratch arena** (`f242945` + cleanup `df5dbba`)
  - Migrated `try_vk_render_traps_or_tris` (the last paint-side recorder on legacy `run_one_shot_op + ProtocolBarrier flush`) to `record_paint_batch_op`. Mask coverage staging moved from `MaskScratch`'s private buffer to per-batch `BatchUploadArena` via the new `MaskScratch::record_upload_r8` + `needs_image_grow` + pub `ensure_image_size` trio.
  - Removed the legacy `RenderPipelineCache::reset_descriptors` / `allocate_descriptor_for_views` / `descriptor_pool` field (T4). All RENDER paths now allocate descriptors per-batch via `BatchDescriptorArena`.
  - Audit catalogue: traps row label corrected from "try_vk_render_traps (composite)" to "try_vk_render_traps_or_tris".
  - Hardware smoke: TBD (user-owned).
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-2-results.md`

### Inter-phase chores landed alongside

- [x] **Composite defer log summary** (`4c4741b`) — turn per-frame `pool_ring_exhausted` warn-spam into a periodic 5s `info!` summary.
- [x] **Scroll-wheel support** (`b7d17a1`) — `InputEvent::PointerScroll` + libinput axis translation + synthetic-button-code mapping to X11 buttons 4/5/6/7. `has_axis` fix (`56f93d9`) closes a libinput "client bug" log flood.
- [x] **Composite pool-release per-frame** (`cb44c1d`) — fixed a pre-existing FIFO-drain bug where one lagging output held pool slots hostage for already-retired frames on the other output. Caught by codex during 3E smoke.

- [x] **Phase 4 — Sync rework (close-time wait)** (`2135a16` + `642d544` + `6fe4a71` + `49ff484` + `f68d8c2`)
  - T1: replaced `vkQueueWaitIdle` in `PaintBatch::submit_and_wait` with `wait_for_fences` on a per-batch `VkFence`. Narrower wait scope.
  - T2: added `submit_async` / `try_retire_if_signaled` / `wait_for_completion` async-retirement building blocks.
  - T3: `RenderScheduler` gained `submitted_paint_batches` queue + `close_and_submit_async` + `poll_retired_paint_batches`. `flush_if_needed` branches strict (blocking) vs best-effort (async). Poll wired into composite tick.
  - T4: `MAX_IN_FLIGHT_PAINT_BATCHES = 4` backpressure cap on the queue.
  - T5: `drain_submitted_paint_batches` called after `vkDeviceWaitIdle()` in shutdown.
  - **Hardware smoke: confirmed on fuji (2026-05-14)** — heavy GTK use (GIMP drag, steady-state mate session) is now low-CPU and lag-free; "snappy as fuck" per user. Adapta + mate-cc burst case unchanged (separate workload, separate phase below).
  - Results: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase4-results.md`

- [x] **Phase 5 — Targeted `VkFence` for run_one_shot_op + scratch grow defer-release** (`604f009` + `c6dfecc` + `eea0316` + `067b6c3` + `43dd62c` + `11321b6` + this T7 commit)
  - T1: `run_one_shot_op` swapped `queue_wait_idle(graphics_queue)` for per-op `VkFence` + `wait_for_fences`. 5-path failure taxonomy (extends Phase 4's 4-path model with pre-submit failure window of `begin` / `record` / `end`). `cb_safe_to_free` flag gates outer CB free. 5 in-scope callers (`hw_cursor_refresh`, `read_mirror_pixels`, `try_vk_get_image_pixels`, `dump_scanout_one`, `run_legacy_paint_op`) latch `renderer_failed` on Err.
  - T2: `RenderScheduler::defer_resource_release` adopts a `BatchResource` into the open paint batch (lazy-Idle-open) when any live batch might reference it; synchronous release otherwise. `Poisoned` current batch is discarded before deciding (Drop on Poisoned is no-op → can't host adoptions). Companion `defer_resource_release_decision_for` pure helper + 10-case test matrix.
  - T3 / T4 / T5: `CopyScratch::ensure_size_returning_old` / `DstReadback::ensure_returning_old` / `MaskScratch::ensure_image_size_returning_old` return the OLD image wrapped as `Retired*Image: BatchResource`; callers defer-release through the scheduler. The three pre-flush gates (3D `CopyScratch`, 3F-1 `DstReadback`, 3F-2 `MaskScratch+DstReadback`) are entirely gone.
  - T6: redundant `queue_wait_idle`s deleted from `OpsStaging::ensure` and `GlyphAtlas::grow_staging`. Post-T1, every caller of these grow paths goes through `run_one_shot_op` whose per-op fence already retired the OLD buffer's last referencing CB. Audit comments left at both sites.
  - Hardware smoke: TBD (user-owned).
  - Results: `docs/superpowers/plans/2026-05-14-rendering-rearchitecture-phase5-results.md`

- [x] **Pixmap-allocation pool — burst-absorbing `VkImage` recycling** (`850bb9c` + `9443a2e` + `8b3f243` + `2966407` + `a7c2384` + this T6 commit)
  - Burst-absorbing recycling of server-owned pixmap `VkImage` + `VkImageView` + `VkDeviceMemory` triples, keyed by `(width, height, format)`. Closes the kernel-allocator burst hot path under adapta-nokto + mate-cc cross-vendor reproducer (catastrophic pre-pool on bee/RDNA2 + Arch and fuji/Intel + Arch).
  - T1 (`850bb9c`): `PixmapPool` infrastructure — new `crates/yserver/src/kms/vk/pixmap_pool.rs` with `PixmapPool` + key/entry/`PooledPixmapReturn` BatchResource + `try_take` / `try_return` / `stats` / `drain`. `Arc<Mutex<HashMap<…>>>` shape per the codex Round-1 P0 fold (`Arc<RefCell<…>>` doesn't satisfy `BatchResource: Send`). `DrawableImage::new_from_pool` constructor.
  - T2 (`9443a2e`): `free_pixmap` synchronous-flush gone from the common path; mirrors adopt as `PooledPixmapReturn` BatchResources into the currently-open paint batch via Phase 5 T2's `defer_resource_release`. Eligibility + bucket-cap decided at retire time (codex Round-3 P0 — uniform defer-release for all Vulkan-up server-owned mirrors). DRI3-imported variant routes to flush+drop fallback (T2 reviewer agent caught `into_pool_entry` panics for `ImageBacking::Imported`).
  - T3 (`8b3f243`): `allocate_pixmap_mirror` consults `self.pixmap_pool.as_ref().and_then(|p| p.try_take(key))` before falling through to `new_server_owned_pixmap`. Pool hit skips `vkCreateImage` + `vkAllocateMemory` entirely.
  - T4 (`2966407`): shutdown drain — `pixmap_pool.drain()` called after `scheduler.drain_submitted_paint_batches()`; defensive `Arc::strong_count > 1` warning catches Phase-4-T5 ordering bugs.
  - T5 (`a7c2384`): synthetic burst test (100 pixmaps × 2 bursts → `total_hits == 32 == PIXMAP_POOL_BUCKET_CAP`); `pixmap_pool_stats` + `force_retire_in_flight_for_test` accessors as Pattern A pub fns (cfg(test)-on-impl doesn't reach integration test crates per codex Round-1 P1).
  - Hardware smoke: TBD (user-owned). The load-bearing test: bee + fuji under adapta-nokto + mate-cc. If `bee` improves, AMD-specific investigation is no longer next-priority.
  - Results: `docs/superpowers/plans/2026-05-14-pixmap-allocation-pool-results.md`

- [x] **GPU trap rasterization — RENDER Trapezoids / Triangles on the GPU** (`151c8be` (plan) + `0f5e605` + `4dd56a6` + `b7d0e77` + `c819a52` + `4fead28` + `5bf046b` + this T6 commit)
  - Moved RENDER `Trapezoids` / `Triangles` coverage-mask generation off the CPU (where it was 19.73% of CPU per the bee/RDNA2 + adapta-nokto perf trace) onto the GPU via a new `TrapPipeline`. The synchronous CPU rasterize + `MaskScratch::record_upload_r8` upload pair on the X protocol request handler's hot path is gone — `try_vk_render_traps_or_tris` is pure-recording (deferred through `record_paint_batch_op`), so the input loop returns in microseconds instead of blocking per request. The 19.73% CPU cost is zero by construction (code path no longer exists).
  - T1 (`0f5e605`): `TrapPipeline` infrastructure — new `crates/yserver/src/kms/vk/trap_pipeline.rs` with two pipelines (trap + triangle) sharing one push-const layout (no descriptor sets — per-instance data via vertex attributes). `TrapInstanceData` (40 bytes), `TriangleInstanceData` (24 bytes), `TrapDrawPushConsts` (32 bytes: mask_extent + bbox). `Trapezoid::to_instance_data` + `Triangle::to_instance_data` conversion helpers. `BatchUploadArena` buffer usage gained `VERTEX_BUFFER` flag. Two new shaders (trap.vert/frag.glsl) compiled to SPIR-V via existing build.rs. No caller wired.
  - T2 (`4dd56a6` + fix-up `b7d0e77`): trapezoid arm wired to GPU. `KmsBackend.trap_pipeline` field + dual ctor init. Per-batch arena upload + `vkCmdDraw(4, n_traps)` into MaskScratch. Mask-LOCAL vertex emit + bbox-origin pushed to fragment so MaskScratch coords always start at (0, 0) where `record_render_composite` reads. Fix-up deferred `set_current_layout` until after fallible record steps succeed (codex T2 P2 — prevents CPU-tracked layout drifting from GPU state on failure).
  - T3 (`c819a52` + fix-up `4fead28`): triangle arm wired to GPU. Triangle shader handles RENDER's no-fixed-winding-convention via vertex-shader signed-area-orientation + flat-interpolated `orient` sign. Fix-up flipped winding sign (codex T3 P1 — `-1.0` for CCW per the `(-d.y, d.x)` perpendicular convention) and added explicit `orient = 0.0` degenerate handling with fragment `discard` (codex T3 P2 — collinear-but-nonzero-length-edge triangles weren't rejected).
  - T4 (partial): rendercheck triangles 456/456 PASS, blend 4/4 PASS, coord PASS; composite/cacomposite/gradients didn't complete in T4's time budget. No regressions on what ran. Full validation deferred to user-owned hardware smoke (the linear coverage approximation does NOT need the wedge fallback based on the data so far).
  - T5 (`5bf046b`): dead-code deletion — `rasterize_trapezoids` + `rasterize_triangles` + `MaskScratch::record_upload_r8` deleted. `rg` returns one doc-comment historical note; zero call sites. The 19.73% CPU cost in the bee adapta-nokto perf trace is gone by construction.
  - Hardware smoke: TBD (user-owned). Load-bearing test: bee adapta-nokto + mate-cc post-pool + post-GPU-trap should be dramatically improved; window-drag CPU should drop materially; rendercheck full run should be no-regression vs Phase 5 baseline.
  - Results: `docs/superpowers/plans/2026-05-14-gpu-trap-rasterization-results.md`

### Remaining — in priority order

- [ ] **Phase 6 — Resource lifetime: batch-owned refcounted handles**
  - Codex's long-term recommendation from 3B salvage: instead of relying on protocol destruction barriers + `queue_wait_idle`, adopt destroyed VkImages into the open `PaintBatch` via `BatchResource` so destruction defers automatically.
  - Subsumes the 3D needs_grow + pre-resize-flush pattern for `CopyScratch`, the analogous patterns 3F introduced for `MaskScratch` + `dst_readback`, the 3B destruction-barrier collection, the Phase-5 `Retired*Image` flavours, and the pixmap-pool `PooledPixmapReturn` — all into a uniform refcounted-handle model.
- [ ] **AMD-specific investigation — DEPRIORITIZED pending bee smoke validation of GPU trap rasterize + pool**
  - Both load-bearing root causes (pixmap-pool's kernel-allocator burst + GPU-trap's CPU-rasterize-on-hot-path) are addressed structurally and are vendor-agnostic.
  - If bee hardware smoke confirms adapta-nokto + mate-cc is smooth post-pool + post-GPU-trap: AMD-specific investigation drops off the critical path entirely.
  - If `bee` is still slow: amdgpu ftrace + ioctl-rate measurement per `project_amd_lag_investigation.md` memory is the next move; the residual cost is somewhere unexpected (libdrm_amdgpu was 4.62% pre-GPU-trap — a candidate but not enough on its own to explain catastrophic lag).

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
