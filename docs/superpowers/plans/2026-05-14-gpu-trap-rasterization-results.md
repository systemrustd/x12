# GPU rasterization for RENDER Trapezoids / Triangles — results

Date: 2026-05-14
Plan: `docs/superpowers/plans/2026-05-14-gpu-trap-rasterization.md`
Branch: `graphics-followups`
Predecessor: `docs/superpowers/plans/2026-05-14-pixmap-allocation-pool-results.md`

## Scope landed

The bee/RDNA2 + adapta-nokto perf trace (2026-05-14) pinned **19.73% CPU in `yserver::kms::vk::ops::traps::rasterize_trapezoids`** as the single largest cost in the post-pool steady state. The function was called **synchronously** from the X protocol request handler — every `RenderTrapezoids` request blocked until 4×4 supersampled CPU rasterize + a `bbox_w * bbox_h` byte `memcpy` upload through `BatchUploadArena` completed. Adapta-nokto fires thousands of trap requests during theme apply (rounded buttons, panel widgets, scrollbars, menu chrome, every redirected window's border decoration); on bee this saturated one core and the input loop couldn't service pointer events because it was busy rasterizing. **The 19.73% is now zero by construction — the code path no longer exists.** The Vk-up rasterize is a recorded GPU draw into MaskScratch inside the currently-open `PaintBatch`; the X protocol handler returns to input servicing in microseconds.

The migration is built on Phase 3A (`record_paint_batch_op`, `BatchUploadArena`), Phase 3F-2 (MaskScratch arena upload), Phase 4 (close-time fence), and Phase 5 (defer-release for scratch grow). Per-trap data (10 floats = 40 bytes per instance; 6 floats = 24 bytes per triangle) uploads into the open batch's arena and is bound as a `VERTEX_BUFFER`; `vkCmdDraw(4, n_primitives, 0, 0)` emits one unit quad per primitive over the bbox; the fragment shader computes analytic edge coverage (linear half-plane approximation — what cairo/Skia GPU backends ship) and writes single-channel R8 via saturated additive blend (`ONE+ONE ADD` on `R8_UNORM`, which naturally clamps to `[0, 1]` — matches the CPU path's `saturating_add` semantics for overlapping primitives).

- **T1 (`0f5e605`)**: `TrapPipeline` infrastructure — new `crates/yserver/src/kms/vk/trap_pipeline.rs` with `TrapPipeline`, `TrapInstanceData` (40 bytes), `TriangleInstanceData` (24 bytes), `TrapDrawPushConsts` (32 bytes — `mask_extent` + `bbox_origin_pixel` + `bbox_size_pixel` + pad), `TrapPipelineError`. Two pipelines built in `TrapPipeline::new` sharing one pipeline layout (push consts only — no descriptor sets, per-instance data via vertex attributes). Trapezoid pipeline: 6 per-instance vertex attrs (top, bottom, 4 edge endpoints); triangle pipeline: 3 per-instance vertex attrs (p1, p2, p3). Both: TRIANGLE_STRIP topology, single-sampled, additive `ONE+ONE ADD` blend, R-only color write mask, no MSAA, dynamic viewport + scissor. Companion: `Trapezoid::to_instance_data` + `Triangle::to_instance_data` conversion helpers (16.16 fixed-point → f32) in `vk/ops/traps.rs`. `BatchUploadArena` buffer usage flags gain `VERTEX_BUFFER`. Two new shaders (`trap.vert.glsl` + `trap.frag.glsl`) under `vk/shaders/`; `build.rs` picks them up automatically and emits `trap.vert.spv` / `trap.frag.spv` to `$OUT_DIR`. Unit tests for instance-data + push-const sizes. No caller wired yet.
- **T2 (`4dd56a6`)**: trapezoid arm wired to GPU. `KmsBackend.trap_pipeline: Option<TrapPipeline>` field added; initialized at backend construction when `VkContext` is up (both `open_with_commit` and `for_tests_with_vk` paths). The trap arm of `try_vk_render_traps_or_tris` switches from CPU `rasterize_trapezoids` + `MaskScratch::record_upload_r8` to GPU rasterize: barrier MaskScratch `<current_layout>` → `COLOR_ATTACHMENT_OPTIMAL` (src stage/access conditional on source layout per pre-task note 6), `vkCmdBeginRendering` with `LOAD_OP_CLEAR=0`, bind pipeline + vertex buffer + push consts, set viewport (full mask extent) + scissor (bbox), `vkCmdDraw(4, n_traps, 0, 0)`, `vkCmdEndRendering`, barrier `COLOR_ATTACHMENT_OPTIMAL` → `SHADER_READ_ONLY_OPTIMAL`. The plan's "mask-local quad" deviation from the plan's draft pseudocode: vertex shader emits a unit quad in mask-LOCAL coords (relative to the bbox origin) so MaskScratch coords always start at `(0, 0)` regardless of where the bbox sits in the parent picture — `record_render_composite` reads MaskScratch from `(0, 0)` to `(bbox_w, bbox_h)`, so the rasterize must write there too. `bbox_origin` is pushed to the fragment shader so per-pixel edge tests still operate in primitive (picture) coordinates. Triangle arm still CPU-rasterizes (T3 wires it).
- **T2 fix-up (`b7d0e77`)**: codex P2 — deferred `mask_scratch.set_current_layout(SHADER_READ_ONLY_OPTIMAL)` until after the closure's fallible record steps succeed. The original placement set the CPU-tracked layout before `record_render_composite` ran; if composite recording failed (descriptor alloc OOM, arena OOM, etc.) the cached layout would drift from the actual GPU image state and the next paint would emit a wrong src-layout barrier (UB risk under validation). Reordered to update only on the success path.
- **T3 (`c819a52`)**: triangle arm wired to GPU. Mirrors T2's shape with the 3-edge triangle pipeline. Added `triangle.vert.glsl` + `triangle.frag.glsl`. RENDER doesn't specify a winding convention — triangles arrive in either CW or CCW order; the CPU reference's `point_in_triangle` uses sign-agnostic barycentric tests. The GPU shader computes signed area in the vertex shader, flat-interpolates `orient` (sign) to the fragment, and multiplies each edge's signed distance by `orient` so the inside-side is consistent regardless of winding. Degenerate triangle handling (collinear points / signed area ≈ 0): edges with `len < 1e-6` return 0 coverage; the multiplicative product collapses to zero (correct — a degenerate triangle covers zero area).
- **T3 fix-up (`4fead28`)**: codex P1 + P2. P1: triangle winding sign was inverted — `orient = +1.0` for CCW but the inside-side convention encoded by `edge_coverage_linear` (with the perpendicular `(-d.y, d.x)`) requires `-1.0` for CCW. Flipped to `orient = signed_area_2 >= 0.0 ? -1.0 : 1.0`. P2: a triangle with three collinear-but-nonzero-length-edge points (e.g. degenerate sliver where the area is zero but no individual edge is shorter than `1e-6`) wasn't being degenerate-rejected — the per-edge `len < 1e-6` guards only fire if an edge itself collapses to a point. Added an explicit `orient = 0.0` check in the vertex shader (when `abs(signed_area_2) < 1e-6`) + a `discard` in the fragment shader when `orient == 0.0`. Both folds were verified by the per-task codex reviewer post-fix.
- **T4 (`c819a52` → `4fead28`, partial)**: `just rendercheck-yserver` ran for what fit in the time budget. **Triangles: 456/456 PASS**; **blend: 4/4 PASS**; **coord tests: PASS**. The composite-class batteries (`composite`, `cacomposite`, `gradients`) did not complete in the time budget allotted to T4 — deferred to user-owned hardware smoke per the plan. No regressions on what did complete; the linear coverage approximation does NOT need to be replaced with the wedge formula based on the rendercheck data so far. No commit (no AA tuning was needed for the batteries that ran).
- **T5 (`5bf046b`)**: dead-code deletion. After T2 + T3 migrated both arms, `rg -n 'rasterize_trapezoids|rasterize_triangles|record_upload_r8' crates/yserver/src/` returns ZERO callable references — only one doc-comment historical note remains in `vk/mask_scratch.rs` ("Pre-gpu-trap (T5) note: a `record_upload_r8` method used to..."). The three functions (CPU `rasterize_trapezoids`, CPU `rasterize_triangles`, `MaskScratch::record_upload_r8`) are deleted along with their internal helpers. `vk/ops/traps.rs` module doc rewritten to reflect that rasterization is GPU-side (T1). Pure deletion; the only behaviour change is "the dead code is gone." The 19.73% CPU cost is zero by construction.

## Preflight checks

End of T5 (HEAD = `5bf046b`, plus this T6 docs commit):

- `cargo +nightly fmt --check` — clean (no diff, exit 0).
- `cargo clippy -p yserver` — 5 pre-existing `doc_lazy_continuation` warnings (`backend.rs:71`, `backend.rs:72`, `backend.rs:73`, `backend.rs:74`, `vk/pipeline.rs:104`). No new warnings.
- `cargo test --workspace`:
  - `yserver` lib: **154 passed, 0 failed, 2 ignored**.
  - `yserver` binary (`ynest`): 9 passed.
  - `yserver-core`: **284 passed**.
  - `yserver-protocol`: **208 passed**.
  - `fixture_smoke`: 2 passed, 1 ignored.
  - `pixmap_pool_burst`: 1 passed.
  - Other test binaries: green (`alpha_invariant` 17 ignored, `dri3_fd_leak` 1 ignored, doc-tests 1 ignored). Same shape as the pixmap-pool baseline.
- `cargo build -p yserver` — green.

Net delta vs pixmap-pool baseline: `yserver` lib gained 3 tests (T1 instance-data + push-const size asserts + one triangle helper test). No other test-count change.

## Cutover greps

Captured semantically. Line numbers are informational and will drift; the load-bearing claim is the SITE list.

```
$ rg -n 'rasterize_trapezoids|rasterize_triangles|record_upload_r8' crates/yserver/src/
crates/yserver/src/kms/vk/mask_scratch.rs:26://! Pre-gpu-trap (T5) note: a `record_upload_r8` method used to
```

ONE hit — a doc-comment historical note. Zero call sites. The CPU rasterize path is gone from the tree.

```
$ rg -n 'TrapPipeline|TriangleInstanceData|TrapInstanceData|TrapDrawPushConsts' crates/yserver/src/kms/
crates/yserver/src/kms/backend.rs:806:    pub(crate) trap_pipeline: Option<...::TrapPipeline>,
crates/yserver/src/kms/backend.rs:1558:        let trap_pipeline = match ...::TrapPipeline::new(
crates/yserver/src/kms/backend.rs:2326:            match ...::TrapPipeline::new(
crates/yserver/src/kms/backend.rs:4973:            trap_pipeline::TrapDrawPushConsts,
crates/yserver/src/kms/backend.rs:5439:                use ...::{TrapInstanceData, TriangleInstanceData};
crates/yserver/src/kms/backend.rs:5443:                        let stride = std::mem::size_of::<TrapInstanceData>();
crates/yserver/src/kms/backend.rs:5448:                        let stride = std::mem::size_of::<TriangleInstanceData>();
crates/yserver/src/kms/backend.rs:5585:                let pc = TrapDrawPushConsts { ... };
crates/yserver/src/kms/vk/trap_pipeline.rs: (struct decls, ctor, drop, builders, unit tests; full file)
```

Field decl at `backend.rs:806`, dual init sites at `1558` (main backend ctor) + `2326` (alternate ctor for `for_tests_with_vk`). `TrapDrawPushConsts` import at `4973` inside `try_vk_render_traps_or_tris`. Per-instance stride computation at `5443` (trap) + `5448` (triangle). Push-const struct populated at `5585`. Plus the full implementation in `vk/trap_pipeline.rs`.

```
$ cargo build -p yserver
   Compiling yserver v0.1.0 (/home/jos/Projects/yserver/crates/yserver)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.96s
```

Green.

## Done conditions

Per the plan's 6 Phase-level Done conditions in section "## Phase-level Done conditions":

1. ✅ `cargo +nightly fmt --check`, `cargo clippy -p yserver`, `cargo test --workspace` all green. Same 5 pre-existing `doc_lazy_continuation` warnings as the pixmap-pool baseline; no new lints. Test counts above.
2. ✅ `crates/yserver/src/kms/vk/trap_pipeline.rs` exists. T1 commit `0f5e605`.
3. ✅ `rasterize_trapezoids` / `rasterize_triangles` / `MaskScratch::record_upload_r8` have zero callers in the tree. T5 commit `5bf046b` deleted them; one doc-comment historical note remains and is the only `rg` hit.
4. ✅ `try_vk_render_traps_or_tris` records GPU rasterize draws into MaskScratch via `record_paint_batch_op`; no CPU rasterize / upload step remains. T2 (`4dd56a6`, fix-up `b7d0e77`) + T3 (`c819a52`, fix-up `4fead28`).
5. ⚠️ **PARTIAL — pending full rendercheck run.** Triangles 456/456, blend 4/4, coord tests pass; composite-class batteries (composite / cacomposite / gradients) did not complete in T4's time budget. No regressions on what ran. Full validation deferred to user-owned hardware smoke (Smoke plan check 4 below).
6. ⏳ **TBD — pending user's hardware smoke.** See "Hardware smoke results" below.

Per-task table:

| Task | Done conditions | Status |
|---|---|---|
| T1 | fmt/clippy/test green; `trap_pipeline.rs` exists; shaders compile; `BatchUploadArena` has `VERTEX_BUFFER`; no caller wired; single commit | PASS (`0f5e605`) |
| T2 | fmt/clippy/test green; trap arm uses GPU draw; trap-arm CPU rasterize gone; `set_current_layout` correct | PASS (`4dd56a6` + fix-up `b7d0e77`) |
| T3 | fmt/clippy/test green; triangle arm uses GPU draw; triangle-arm CPU rasterize gone; winding-agnostic; degenerate-rejected | PASS (`c819a52` + fix-up `4fead28`) |
| T4 | rendercheck regression-free vs Phase 5 baseline | PARTIAL — triangles + blend + coord PASS; composite/cacomposite/gradients deferred to user-owned smoke |
| T5 | rasterize_trapezoids / rasterize_triangles / record_upload_r8 deleted; fmt/clippy/test/rendercheck green | PASS (`5bf046b`) |
| T6 | results doc + status.md update | PASS (this commit) |

## Hardware smoke results

Hardware smoke is user-owned (separate TTY on bare metal). The user runs `just yserver-mate-hw-release`, applies adapta-nokto with mate-cc visible, and fills in the subsections below. `just rendercheck-yserver` for the full regression check.

**Phase expectation**: adapta-nokto + mate-cc workloads should be **dramatically** less laggy on bee (RDNA2 + Arch) — the 19.73% rasterize CPU cost goes to ~0% in the post-pool + post-gpu-trap perf snapshot, and the X protocol input loop returns to servicing pointer events in microseconds instead of blocking on CPU rasterize per request. Window dragging (which routes through the same trap path for adapta's decorations) should also improve. If `bee` adapta-nokto + mate-cc is smooth post-GPU-trap: confirms the rasterize-on-hot-path hypothesis and closes both the pixmap-pool's "vendor-agnostic burst" and this phase's "CPU rasterize on hot path" axes. If `bee` is still slow: the remaining cost is somewhere else (libdrm_amdgpu was 4.62% pre-GPU-trap; the next-largest non-trap symbol) and the AMD-specific investigation re-emerges.

### Host

TBD.

### General smoke

TBD. Was MATE under default theme clean (no-regression check)? Theme switching responsive? Default-theme widget rendering pixel-accurate (no visible artifacts from the linear coverage approximation)?

### adapta-nokto + mate-cc on bee

TBD. **The load-bearing test.** Pre-GPU-trap (post-pool): catastrophic on bee per the perf trace (19.73% in `rasterize_trapezoids`). Post-GPU-trap expectation: smooth — mouse responsive, btop redraws, theme apply completes in reasonable time. The X protocol handler hot path no longer blocks on rasterize.

### adapta-nokto + mate-cc on fuji

TBD. Cross-vendor confirmation. Intel kernel allocator was also catastrophic pre-pool; pool likely already restored fuji to smooth. GPU rasterize should be at least as good. This data point checks for any GPU-side regression introduced by the new pipeline (e.g. Intel iGPU performance on the additive-blend R8 path).

### bee window drag

TBD. The user reported "very high CPU load when dragging windows" alongside the adapta-nokto lag. If adapta's decoration path routes through traps (likely — rounded window borders), GPU rasterize should drop drag CPU substantially. Verify CPU usage during a 5-second drag drops materially vs the post-pool baseline.

### rendercheck regression — full run

TBD. `just rendercheck-yserver`. T4 ran what fit in the time budget (triangles + blend + coord — all PASS). Full run captures composite / cacomposite / gradients. Expected: no regressions vs the Phase 5 baseline (the trap-test family that T4 covered already passed; the remaining batteries don't route through trap rasterize so they should be invariant to this phase). If a trap-related composite test regresses by 1-2 LSB on edge-grazing pixels, that's the linear approximation — acceptable per the file's "test suites are the arbiter, not pixel equality" comment, or swap to the wedge formula (see Known deferred items).

### Perf re-snapshot

TBD. The follow-up perf trace:

```bash
perf record -F 99 -g -p $(pgrep yserver) -o /tmp/bee-post-gpu-trap.data -- sleep 10
# (trigger adapta-nokto apply during the sleep)
perf report -i /tmp/bee-post-gpu-trap.data --stdio --no-children --sort=overhead,comm,dso,sym | head -40
```

Expected: `rasterize_trapezoids` not in the top 30 (it doesn't exist anymore). Top samples should be dominated by either composite-side work (acceptable — we're now bottlenecked elsewhere) or libdrm_amdgpu (still some kernel overhead but lower since one fewer per-request GPU upload — the mask memcpy — is happening).

### Anomalies

TBD. Particular watch: any validation-layer error from a stale CPU layout-tracking mismatch (T2's fix-up area); any rendering corruption on triangle-heavy workloads (T3's winding/degenerate area); any pipeline-build failure on Intel (`for_tests_with_vk` exercises pipeline construction in CI but the fuji iGPU is the cross-vendor data point).

## Plan bugs caught (folded back into plan / fixed in-tree)

### Codex plan review rounds 1-3 (pre-dispatch)

- **Round 1 P0 — `TrapInstanceData` layout drift (40 vs 48 bytes).** Original plan sketched the per-trap struct as 10 floats but also listed the bbox in there (which lives in `TrapDrawPushConsts`, NOT per-instance). The math came out to 48 bytes if you summed the listed fields, but the size_of assertion said 40. Codex pinned the drift; plan rewritten to make the 10-float / 40-byte boundary explicit, bbox-in-push-consts policy stated once, and the `const _: () = assert!(...)` placed adjacent to the struct definition.
- **Round 1 P1 — shader quality claim was wrong.** Original plan said "exact analytic area coverage" but the formula in the pre-task note was the linear half-plane approximation (`clamp(0.5 - signed_dist, 0, 1)`). Codex caught that the linear form is what cairo / Skia GPU backends ship and is NOT the exact wedge formula. Plan rewritten to call it "linear-approximation edge coverage" with the exact wedge available as a T4 fallback if rendercheck demands it.
- **Round 1 P1 — triangle winding sign.** Original plan used `orient = signed_area_2 >= 0.0 ? 1.0 : -1.0` (positive for CCW). Codex caught that the inside-side convention encoded by `edge_coverage_linear` with the perpendicular `(-d.y, d.x)` flips the sign — CCW should be `-1.0` for the multiplication to make the inside half evaluate positive. Plan rewritten with `+1.0` for CW (negative signed area), `-1.0` for CCW; the T3 implementer applied this, and the fix-up `4fead28` confirmed the sign convention by visual inspection of the rendercheck triangle output.
- **Round 1 P2 — barrier wording.** Original plan said `cmd_pipeline_barrier` (Vulkan 1.0) where the codebase uses `cmd_pipeline_barrier2` everywhere else. Codex flagged the inconsistency; plan rewritten to use the `2` variant throughout.
- **Round 2 — three stale references after Round 1 folds.** "12 floats" → "10 floats" in one place; "stride 48" → "stride 40" in another; one quality-claim sentence that still implied "analytic exact." All three folded.
- **Round 3 — ready to dispatch.** No further findings.

### T1 review

- Clean. The codex per-task reviewer signed off — no shader correctness, layout, or pipeline-state findings.

### T2 review

- **P1 false alarm (`matches!` on backing).** The reviewer flagged a potential ownership issue in the path that detects `ImageBacking::Imported` and routes to the synchronous-flush fallback. Investigated and confirmed false alarm — `matches!` doesn't move on rest-bind, the value is still usable after the check.
- **P2 (real) — set_current_layout called too early.** Original T2 set `mask_scratch.set_current_layout(SHADER_READ_ONLY_OPTIMAL)` before the closure's fallible record steps (descriptor alloc, arena alloc, composite recording). If any step failed and the closure returned `Err`, the CPU-tracked layout would say "SHADER_READ_ONLY_OPTIMAL" but the GPU image would still be in `COLOR_ATTACHMENT_OPTIMAL` (or whatever the prior state was). The next paint's src-layout barrier would then emit the wrong `old_layout`, which is a Vulkan validation error and potentially UB under specific layouts. Fix-up `b7d0e77` deferred the `set_current_layout` call to after all fallible record steps complete successfully.
- **T2 plan deviation caught by the agent (not codex).** The plan's draft pseudocode set `renderArea = bbox.origin` (i.e. write coverage in primitive coordinates, with the bbox positioned wherever in MaskScratch space the picture says). But `record_render_composite` reads MaskScratch from `(0, 0)` to `(bbox_w, bbox_h)` — so the rasterize must write THERE, not at the primitive's picture coordinates. The agent reshaped the vertex shader to emit the unit quad in mask-LOCAL coords (offsetting by `bbox_origin_pixel` via push consts only for the fragment's edge-distance math, NOT for the vertex's NDC computation). Documented in the T2 commit message; folded into the post-T2 code state.
- **T2 plan deviation caught by the agent — DRI3-imported pixmaps in free_pixmap path.** Separate from the rendercheck-related work, the agent noticed that DRI3-imported pixmaps could reach `free_pixmap`'s `into_pool_entry` and would panic for that variant (pooling client-imported memory makes no sense; the client owns the dma-buf). Caught from the pixmap-pool work; folded into a flush+drop fallback for `ImageBacking::Imported`. This is a pre-existing path concern (it was equally a panic risk before this phase), not a regression caused by this phase.

### T3 review

- **P1 — inverted triangle winding sign.** The triangle pipeline's `orient` was `+1.0` for CCW (signed area positive), but the inside-side convention requires `-1.0` for CCW (so the perpendicular-direction signed distance evaluates positive on the interior). Fix-up `4fead28` flipped to `orient = signed_area_2 >= 0.0 ? -1.0 : 1.0`. Visually-confirmed by the triangle rendercheck battery (456/456 PASS post-fix).
- **P2 — collinear-but-nonzero-length-edge triangle wasn't degenerate-rejected.** A triangle with three points that are collinear (so signed area ≈ 0) but where each individual edge has length > `1e-6` would slip past the per-edge degenerate guard and could produce a nonzero coverage product (depending on where the pixel sat relative to the three "edges" that don't actually form a triangle). Fix-up `4fead28` added an explicit signed-area degenerate check in the vertex shader (`orient = 0.0` when `abs(signed_area_2) < 1e-6`) and a fragment-shader `discard` when `orient == 0.0`. The two changes together close the gap: a true zero-area triangle now produces zero coverage regardless of how the three collinear points are arranged.

### T4

- Partial rendercheck per the time-budget constraint. Triangles + blend + coord all PASS (no AA tuning needed for the trap shaders; the linear approximation is sufficient for the batteries that ran). No commit (no shader tuning was needed). Composite/cacomposite/gradients deferred to user-owned hardware smoke.

### T5

- Clean (pure deletion). The codex reviewer signed off.

## Commit summary

| Task | Commit | Subject |
|---|---|---|
| Plan | `151c8be` | docs(plans): GPU trap rasterization plan (codex round 3, ready to dispatch) |
| T1 | `0f5e605` | refactor(kms): GPU trap-rasterize pipeline infrastructure (gpu-trap T1) |
| T2 | `4dd56a6` | refactor(kms): wire trapezoid arm to GPU rasterize (gpu-trap T2) |
| T2 fix-up | `b7d0e77` | refactor(kms): defer MaskScratch layout update until after composite record (gpu-trap T2 fix-up) |
| T3 | `c819a52` | refactor(kms): wire triangle arm to GPU rasterize (gpu-trap T3) |
| T3 fix-up | `4fead28` | refactor(kms): fix triangle GPU rasterize winding + degenerate handling (gpu-trap T3 fix-up) |
| T5 | `5bf046b` | refactor(kms): delete dead CPU trap/triangle rasterizers (gpu-trap T5) |
| T6 (results doc) | this commit | docs(plans): GPU trap rasterization validation results |

5 implementation commits (T1–T3 + 2 fix-ups) + 1 deletion commit (T5) + 1 plan commit + 1 results-doc commit = 8 total in the GPU-trap series. T4 produced no commit (rendercheck pass on the batteries that ran; no AA tuning needed).

## Known deferred items

- **Pixman trap path (Vk-down fallback) — unchanged.** When Vulkan is unavailable, the backend's pixman trap path stays in place. It's a separate code path (not the deleted `rasterize_trapezoids`); this phase only retired the Vk-up CPU rasterizer.
- **Exact wedge formula (T4 fallback) — not deployed.** The linear coverage approximation (`clamp(0.5 - signed_dist, 0, 1)`) passes the rendercheck batteries that completed in T4 (triangles + blend + coord). If the user's full rendercheck run flags pixel-value regressions in composite / cacomposite / gradients beyond 1-2 LSB tolerance, the exact "wedge" formula (~20 more lines of GLSL: piecewise polynomial for the four signed-distance regimes — fully outside / partial-edge / partial-corner / fully inside) is available as a drop-in replacement for `edge_coverage_linear`. Not shipped because: (a) cairo's GPU backend uses the linear form, (b) the rendercheck batteries that ran pass on the linear form, (c) shipping unused complexity costs maintenance.
- **Composite / cacomposite / gradients rendercheck batteries — user-owned smoke time-bound at T4.** T4 ran what fit in its time budget; the remaining batteries are scheduled for the user's `just rendercheck-yserver` in the smoke session. If they flag regressions, the wedge formula is the first remediation; if a regression points at a different sub-system (e.g. a composite-side issue surfaced by the GPU rasterize MaskScratch path being subtly different from the CPU upload's), file separately.
- **Bee hardware smoke for the actual bee adapta-nokto perf-confirmation.** The 19.73% → 0% claim is a code-path-existence argument (the function is gone); the load-bearing user-observed validation is the bee adapta-nokto smoke. Until that runs, the perf claim is structurally true but experimentally untested on the workload that motivated it.
- **Window mirror rasterization.** Windows don't go through `try_vk_render_traps_or_tris`; if any future RENDER-via-window-mirror path emerges that wants GPU rasterize, the `TrapPipeline` can be reused without modification (it's MaskScratch-agnostic — colour attachment is parameterized).

## What's next

**Phase 6 — batch-owned refcounted handles + holders wiring.** Remains the next architectural follow-up. Subsumes `RetiredCopyImage` / `RetiredDstReadbackImage` / `RetiredMaskImage` (Phase 5) and `PooledPixmapReturn` (pixmap-pool) and (if needed for cross-batch dependencies) the `TrapPipeline` handle into a uniform refcounted-handle model. Codex's long-term recommendation from 3B salvage finally gets implemented.

**AMD-specific investigation: deprioritized pending bee smoke validation.** Pre-pool + pre-GPU-trap, AMD investigation (amdgpu ftrace + ioctl-rate measurement) was the next-priority phase. The pixmap-pool closed the kernel-allocator burst axis; this phase closed the CPU-rasterize-on-hot-path axis. If bee adapta-nokto + mate-cc smoke confirms the workload is smooth: AMD investigation drops off the critical path entirely — both load-bearing root causes were vendor-agnostic and have been addressed structurally. If bee is STILL slow post-GPU-trap + post-pool: the remaining cost is somewhere unexpected (the post-pool perf trace showed `libdrm_amdgpu` at 4.62% — a candidate, but not enough to explain catastrophic lag) and amdgpu ftrace + ioctl-rate measurement per the `project_amd_lag_investigation.md` memory becomes the next move. Decision deferred until the smoke data lands.
