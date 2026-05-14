# GPU rasterization for RENDER Trapezoids / Triangles

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move RENDER `Trapezoids` / `Triangles` coverage-mask generation off the CPU (where it's currently the dominant cost on bee/RDNA2 under adapta-nokto: 19.73% CPU in `yserver::kms::vk::ops::traps::rasterize_trapezoids` per perf trace) onto the GPU. Replace the synchronous CPU rasterize + `MaskScratch::record_upload_r8` upload pair with a recorded GPU draw that writes the coverage mask directly into MaskScratch. **The headline win is taking the rasterize off the X protocol request handler's hot path** — `try_vk_render_traps_or_tris` becomes pure-recording (deferred through `record_paint_batch_op`), so the input loop returns in microseconds instead of blocking on CPU rasterization per request.

**Architecture:** Four structural pieces:

1. **`TrapPipeline`** — new Vulkan pipeline. One vertex shader, two fragment shaders (trap + triangle; or one shader with a primitive-type push-constant). Output target: existing `MaskScratch` (R8_UNORM). Vertex input: per-instance primitive data (one trap or one triangle per instance). One-quad-per-instance via instanced rendering — `vkCmdDraw(4, n_primitives, 0, 0)` with TRIANGLE_STRIP. Mirrors the existing `LogicFillPipelineCache` shape (simplest analogue).

2. **Analytic edge coverage in fragment shader.** Per-pixel coverage is computed analytically — for each of the 4 edges (top, bottom, left, right), compute the signed-distance from `gl_FragCoord.xy` (pixel center) to the edge line, convert to a 0..1 cover-fraction, multiply. Identical or better quality vs current 16-sample CPU supersampling; no MSAA buffer overhead.

3. **Per-batch vertex buffer via `BatchUploadArena`.** Today's arena (host-visible, mapped) is `TRANSFER_SRC` only. T1 adds `VERTEX_BUFFER` to the usage flags — one-line change. Then trap/tri data is uploaded into an arena allocation and bound as `vertex_buffer_binding 0` for the draw.

4. **Per-trap draw cadence: one draw call with `instance_count = n_primitives`.** All primitives in the same Trapezoids / Triangles request collapse into one draw. Saturated additive blend (Vulkan's `SrcAlpha=ONE`, `DstAlpha=ONE`, `BlendOp=ADD` on R8_UNORM) gives the same "union with saturating add" semantics as the CPU path's `out[idx] = out[idx].saturating_add(cov)` — `R8_UNORM` clamps to `[0, 1]` in the framebuffer.

**Tech Stack:** Rust, ash (Vulkan), GLSL (shaderc/glslc via existing `build.rs`). Builds on `MaskScratch` (3F-2), `record_paint_batch_op` (Phase 3), `BatchUploadArena` (Phase 3A), `PaintBatch` defer-release (Phase 5 T2).

---

## Prerequisite — confirm post-pixmap-pool baseline

```bash
cd /home/jos/Projects/yserver
git log --oneline graphics-followups | head -10
rg -n 'rasterize_trapezoids\|rasterize_triangles' crates/yserver/src/kms/
rg -n 'record_upload_r8' crates/yserver/src/kms/
```

Expected:
- Branch tip `560c3a1` (pixmap-pool T6) or descendant.
- `rasterize_trapezoids` / `rasterize_triangles` defined in `vk/ops/traps.rs:144`/`:232`, called from `backend.rs:4732`/`:4837`.
- `record_upload_r8` defined in `vk/mask_scratch.rs`, called from `backend.rs:5308` (single caller; this whole call site goes away in T2/T3).
- `cargo +nightly fmt --check`, `cargo clippy -p yserver`, `cargo test --workspace` all green.

If any of the above don't hold, STOP.

## Phase context

The bee adapta-nokto perf snapshot (2026-05-14):

```
19.73%  yserver  yserver         [.] yserver::kms::vk::ops::traps::rasterize_trapezoids
 4.62%  yserver  libdrm_amdgpu   [.] 0x35b3
 2.66%  yserver  libc.so.6       [.] 0x1ac099
 0.88%  yserver  yserver         [.] free_pixmap
 0.88%  yserver  yserver         [.] record_paint_batch_op
```

Trap rasterization is the single largest cost. Crucially, the function is called **synchronously** from the X protocol request handler (`backend.rs:4732`) — every `RenderTrapezoids` request blocks until rasterize completes. Adapta-nokto fires thousands of trap requests during theme apply (rounded buttons, panel widgets, scrollbars, menu chrome, every redirected window's border decoration). On bee this saturates one core; the input loop can't service pointer events because it's busy rasterizing; mouse becomes unresponsive.

The current CPU rasterizer:
- 4×4 super-sample per pixel (16 samples)
- Per-subsample: 4 line evaluations (`Edge::x_at(y)` each does 1 div + 1 mul + 1 add)
- Per-pixel: ~80 ops (with branches)
- For a 100×100 trap: ~800K ops
- For 200 traps of 50×50 average: ~40M ops per request, repeated dozens of times during theme apply

The current path also does a CPU→GPU upload of the rasterized mask via `MaskScratch::record_upload_r8`, which is `bbox_w * bbox_h` bytes of memcpy through `BatchUploadArena`. For a 1024×1024 worst-case bbox this is 1 MB per trap request.

**Both costs go away with GPU rasterization.** The CPU work is replaced by a per-trap data upload (10 floats = 40 bytes × n_primitives, typically a few KB) and a single draw call that fills MaskScratch from the GPU side. The CPU returns to servicing X protocol immediately.

### Why analytic coverage in the fragment shader (vs MSAA)

The chosen AA strategy is **analytic edge coverage** rather than MSAA:

- **Same or better quality** vs current 16-sample CPU supersampling, for typical cairo workloads. The "analytic" approach here is a linear half-plane approximation (not exact area integration); it is exact for edges through pixel centers and within 1-2 LSB for edges grazing pixel corners. This matches what cairo/Skia GPU backends ship. The exact "wedge" formula is available as a T4 fallback if rendercheck flags the linear approximation.
- **No multi-sample framebuffer** required — MaskScratch stays a single-sampled `R8_UNORM` image. MSAA would require either a separate multi-sample image + resolve pass, or rendering at higher resolution (4× memory).
- **Simpler MaskScratch lifetime** — same image, same barriers, same `MaskScratch::ensure_image_size`/`needs_image_grow` interface from Phase 5 T5. No multi-sample variant to bookkeeping.

The cost: a more numerical fragment shader (~50 lines GLSL). Standard pattern — there are well-known closed-form solutions for "fractional area of unit square inside a half-plane defined by a line" and the trap reduces to the intersection of four such half-planes.

### What stays on the CPU

- **Bbox computation** (`trapezoid_bbox` / `triangle_bbox`). The GPU pipeline needs to know the viewport extent + scissor rect; the bbox is computed CPU-side from the primitive list. Same as today.
- **Fixed-point → f32 conversion**. Per-trap, 10 i32→f32 conversions (top + bottom + 4 endpoint × 2 coords). Trivial.
- **Vk-down fallback**. When Vulkan is unavailable, the backend's pixman trap path stays in place — it's a separate code path (not `rasterize_trapezoids`). This plan only retires the *Vk-up* CPU rasterizer.

### Out of scope (deferred / explicitly skipped)

- **CPU-rasterize fallback for tiny traps**. Per the user's planning answer: always GPU when Vk is up. The pipeline-bind cost amortizes well; no tunable threshold.
- **Pixman path replacement**. The Vk-down branch keeps its pixman trap rasterizer.
- **Triangle FAN strip optimization**. The simplest implementation draws one quad per instance via TRIANGLE_STRIP with `n_vertices=4, n_instances=n_primitives`. A future optimization could pack multiple primitives per draw via instance attributes.
- **GPU-side bbox computation**. Could be done with a compute pre-pass but adds complexity; CPU bbox is cheap.
- **`MaskScratch` `record_upload_r8` removal**. T5 deletes this function ONLY IF no other caller remains. Need to verify; current grep shows the trap path is the only caller, but T5 verifies.

### Key invariants preserved

1. **Drop order**: `KmsBackend.scheduler` before `trap_pipeline` (new) before `pixmap_pool` before `ops_command_pool` before `vk`. Trap pipeline holds Vk handles; must drop before VkContext.
2. **`MaskScratch::ensure_image_size_returning_old`** (Phase 5 T5): the GPU rasterize draws into MaskScratch. If the bbox grows, the Phase-5-T5 defer-release path still applies — `try_vk_render_traps_or_tris` continues to call `ensure_image_size_returning_old` + `scheduler.defer_resource_release` for the old image. T2's draw uses the new image.
3. **Saturating-add semantics for overlapping primitives**: replaced by Vulkan additive blend on R8_UNORM. R8 clamps to `[0, 1]` per the format spec; additive blend naturally saturates.
4. **`renderer_failed` gate**: the trap path still goes through `paint_resources()`. If `renderer_failed`, falls through to pixman (existing shape).
5. **CPU-side layout tracking** (3F-2 #8): the new path transitions MaskScratch from its tracked `current_layout()` (likely `SHADER_READ_ONLY_OPTIMAL` from the previous draw, or `UNDEFINED` after `ensure_image_size_returning_old` grew the image) → `COLOR_ATTACHMENT_OPTIMAL` → `SHADER_READ_ONLY_OPTIMAL`. The first barrier's src stage/access must be conditional on the source layout (see pre-task note 6). `mask_scratch.set_current_layout(SHADER_READ_ONLY_OPTIMAL)` at the recorder's tail, matching the previous `record_upload_r8` convention.

## File structure

| File | Role | Touched in |
|---|---|---|
| `crates/yserver/src/kms/vk/trap_pipeline.rs` | New file. `TrapPipeline` struct + pipeline layout + 2 pipelines (trapezoid + triangle) + `TrapInstanceData` push/vertex shape + Drop. | T1 |
| `crates/yserver/src/kms/vk/mod.rs` | `pub mod trap_pipeline;` | T1 |
| `crates/yserver/src/kms/vk/shaders/trap.vert.glsl` | New. Vertex shader: emits a unit-quad (4 verts, TRIANGLE_STRIP) per instance; pulls per-instance trap data; passes trap params + bbox interpolants to fragment. | T1 |
| `crates/yserver/src/kms/vk/shaders/trap.frag.glsl` | New. Fragment shader: analytic edge coverage for 4 trap edges → R8 output. | T1 |
| `crates/yserver/src/kms/vk/shaders/triangle.vert.glsl` | New (T3). Vertex shader for triangle primitive (3 edges instead of 4). | T3 |
| `crates/yserver/src/kms/vk/shaders/triangle.frag.glsl` | New (T3). | T3 |
| `crates/yserver/src/kms/scheduler/batch_upload_arena.rs` | Add `VERTEX_BUFFER` to buffer usage flags (one line). | T1 |
| `crates/yserver/src/kms/vk/mask_scratch.rs` | Add `record_clear_to_zero_and_to_color_attachment(cb)` helper (or open-coded in backend) that transitions MaskScratch to `COLOR_ATTACHMENT_OPTIMAL` and emits an LOAD_OP_CLEAR via `vkCmdBeginRendering`. May not need a helper — backend can open-code. | T1 |
| `crates/yserver/src/kms/vk/ops/traps.rs` | Add `pub fn primitive_to_instance_bytes(primitive: &Trapezoid) -> [u8; N]` (or similar) so the upload side has a one-source-of-truth conversion. CPU `rasterize_trapezoids` + `rasterize_triangles` stay until T5 deletes them. | T1 + T5 |
| `crates/yserver/src/kms/backend.rs` | `trap_pipeline: Option<TrapPipeline>` field. Init at backend construction. Trapezoid arm (T2) + triangle arm (T3) of `try_vk_render_traps_or_tris` replace CPU rasterize + upload with GPU draw. | T1 + T2 + T3 + T5 |
| `docs/superpowers/plans/2026-05-14-gpu-trap-rasterization-results.md` | Results doc. | T6 |
| `docs/status.md` | Move GPU-trap-rasterization entry from new "Remaining" to "Done" after T6. | T6 |

## Pre-task notes (read before starting)

1. **`TrapInstanceData` layout**: per-trap data is **10 floats = 40 bytes**. Only the trap's geometry; bbox lives in `TrapDrawPushConsts` (one bbox per draw, shared across all instances in that draw). Per-instance vertex-input attribute layout:
   ```rust
   #[repr(C)]
   pub struct TrapInstanceData {
       pub top: f32,        // 0
       pub bottom: f32,     // 4
       pub left_p1: [f32; 2],  // 8, 12
       pub left_p2: [f32; 2],  // 16, 20
       pub right_p1: [f32; 2], // 24, 28
       pub right_p2: [f32; 2], // 32, 36
   }
   const _: () = assert!(std::mem::size_of::<TrapInstanceData>() == 40);
   ```
   Triangle equivalent is **6 floats = 24 bytes** (3 vec2 vertex coords; no bbox — push consts carry it). Use separate types `TrapInstanceData` (T1) and `TriangleInstanceData` (T3).

2. **Vertex input bindings** (per-instance attributes):
   - Binding 0: `TrapInstanceData`, `VK_VERTEX_INPUT_RATE_INSTANCE`, stride = 40
   - Attributes 0..N: each `f32` or `vec2` slice of the struct
   - **Alternative**: use a Vulkan storage buffer indexed by `gl_InstanceIndex` instead of vertex attributes. Cleaner shader, no attribute-layout boilerplate, but requires descriptor-set binding (more setup). Per-instance vertex attributes is simpler — pick that.

3. **Per-draw push constants** (constant across all instances in the draw):
   ```rust
   #[repr(C)]
   pub struct TrapDrawPushConsts {
       pub mask_extent: [f32; 2],     // mask scratch extent (for viewport scaling)
       pub bbox_origin_pixel: [f32; 2], // top-left of the bbox in mask coords
       pub bbox_size_pixel: [f32; 2],   // bbox size in pixels
       pub _pad: [f32; 2],
   }
   const _: () = assert!(std::mem::size_of::<TrapDrawPushConsts>() == 32);
   ```
   Vertex shader uses `bbox_origin_pixel` + `bbox_size_pixel` to position its unit quad. Fragment computes pixel-relative coords from `gl_FragCoord.xy - bbox_origin_pixel`.

4. **Coverage shader (sketch)** — note this is a **linear approximation** of pixel coverage, NOT exact analytic area integration. The approximation is standard practice (pixman / Skia / cairo's GPU backends use similar) and is the rendercheck-pass risk T4 explicitly gates on.
   ```glsl
   // trap.frag.glsl — linear-approximation edge coverage
   #version 450

   // Per-instance flat-interpolated trap params
   layout(location = 0) flat in float top;
   layout(location = 1) flat in float bottom;
   layout(location = 2) flat in vec2 left_p1;
   layout(location = 3) flat in vec2 left_p2;
   layout(location = 4) flat in vec2 right_p1;
   layout(location = 5) flat in vec2 right_p2;

   layout(location = 0) out float coverage; // R8 single-channel

   // Coverage approximation for a unit pixel at center (px+0.5, py+0.5)
   // bounded by 4 half-planes (top, bottom, left, right). Each
   // half-plane's contribution is approximated by clamping the
   // signed distance from the pixel center to [0, 1]. Coverage is
   // the product of the 4 contributions.
   //
   // For axis-aligned top/bottom edges: exact (1D clamp).
   // For slanted left/right edges: linear approximation — exact for
   // edges passing through the pixel center, slightly off for edges
   // grazing the corners. T4 gates on rendercheck; tune to wedge
   // formula if needed.

   void main() {
       vec2 p = gl_FragCoord.xy; // pixel center

       // Top edge: y >= top (inside is below). c_top = 1 if pixel
       // fully below; 0 if fully above; 0..1 if straddling.
       float c_top = clamp(p.y - top, 0.0, 1.0);
       // Bottom edge: y <= bottom (inside is above).
       float c_bot = clamp(bottom - p.y, 0.0, 1.0);

       // Left edge: pixel is "inside" if x >= left_x_at_y. The
       // inside-side sign is +1: signed_dist > 0 means outside.
       float cov_left = edge_coverage_linear(p, left_p1, left_p2, +1.0);
       // Right edge: pixel is "inside" if x <= right_x_at_y. The
       // inside-side sign is -1.
       float cov_right = edge_coverage_linear(p, right_p1, right_p2, -1.0);

       coverage = c_top * c_bot * cov_left * cov_right;
   }
   ```

   The `edge_coverage_linear` helper computes signed distance from pixel center to the edge line and clamps to `[0, 1]`:
   ```glsl
   float edge_coverage_linear(vec2 p, vec2 a, vec2 b, float inside_side) {
       vec2 d = b - a;
       float len = length(d);
       // Degenerate (zero-length) edge: the trap has a collapsed
       // side. Treat as "fully outside" — return zero coverage so
       // the multiplicative product collapses to zero. Returning 1.0
       // would over-cover, painting where the trap doesn't exist.
       if (len < 1e-6) return 0.0;
       vec2 n = vec2(-d.y, d.x) / len; // perpendicular (rotate 90° CCW)
       // Signed distance: positive means pixel is on the "outside"
       // half (per the inside_side convention encoded by the caller).
       float signed_dist = dot(p - a, n) * inside_side;
       return clamp(0.5 - signed_dist, 0.0, 1.0);
   }
   ```

   **Inside-side convention**: the RENDER trapezoid spec defines `left_p1`/`left_p2` and `right_p1`/`right_p2` so that traversing each side from p1 to p2 keeps the trap's interior on a consistent side. The perpendicular `(-d.y, d.x)` rotates the edge direction 90° CCW; with `inside_side = +1` for the left edge and `-1` for the right, signed_dist > 0 means "pixel is outside." This convention assumes RENDER's standard p1/p2 ordering (top→bottom on each side). Verify against the actual `Trapezoid` field semantics in `vk/ops/traps.rs` — if RENDER's convention differs, flip the signs.

   **Quality fallback (T4 only if needed)**: replace the linear `clamp(0.5 - signed_dist, 0, 1)` with the exact "wedge" formula that integrates pixel area against the half-plane. ~20 more lines of GLSL involving `if (signed_dist > 0.5) return 0; if (signed_dist < -0.5) return 1; ... piecewise polynomial`. Don't ship this unless T4 demands it; the linear form is what cairo's GPU backends use and rendercheck's traps tests are typically tuned to the 1-2 LSB tolerance it gives.

5. **Vertex shader (sketch)**:
   ```glsl
   // trap.vert.glsl — emits unit quad per instance
   #version 450

   layout(push_constant) uniform PushConsts {
       vec2 mask_extent;
       vec2 bbox_origin_pixel;
       vec2 bbox_size_pixel;
   } pc;

   // Per-instance attributes
   layout(location = 0) in float in_top;
   layout(location = 1) in float in_bottom;
   layout(location = 2) in vec2 in_left_p1;
   layout(location = 3) in vec2 in_left_p2;
   layout(location = 4) in vec2 in_right_p1;
   layout(location = 5) in vec2 in_right_p2;

   layout(location = 0) flat out float top;
   layout(location = 1) flat out float bottom;
   layout(location = 2) flat out vec2 left_p1;
   layout(location = 3) flat out vec2 left_p2;
   layout(location = 4) flat out vec2 right_p1;
   layout(location = 5) flat out vec2 right_p2;

   void main() {
       // Emit unit quad over the bbox.
       vec2 quad = vec2(float(gl_VertexIndex & 1),
                        float((gl_VertexIndex >> 1) & 1));
       vec2 pixel = pc.bbox_origin_pixel + quad * pc.bbox_size_pixel;
       vec2 ndc = pixel / pc.mask_extent * 2.0 - 1.0;
       gl_Position = vec4(ndc, 0.0, 1.0);

       top = in_top;
       bottom = in_bottom;
       left_p1 = in_left_p1;
       left_p2 = in_left_p2;
       right_p1 = in_right_p1;
       right_p2 = in_right_p2;
   }
   ```

6. **MaskScratch barrier sequence in the new draw**:
   ```
   1. Barrier: MaskScratch <current_layout> → COLOR_ATTACHMENT_OPTIMAL
      Source layout is mask_scratch.current_layout() — likely
      SHADER_READ_ONLY_OPTIMAL from the previous draw, or UNDEFINED
      after MaskScratch::ensure_image_size_returning_old grew the
      image. Source stage/access depend on the source layout:
        - UNDEFINED: src_stage=TOP_OF_PIPE, src_access=NONE
          (no prior op to wait on; the LOAD_OP_CLEAR discards contents)
        - SHADER_READ_ONLY_OPTIMAL: src_stage=FRAGMENT_SHADER,
          src_access=SHADER_SAMPLED_READ (last consumer was the
          previous composite's mask sample)
        - Any other layout: ALL_COMMANDS / SHADER_SAMPLED_READ
          (defensive default — should not happen in practice)
      dst_stage=COLOR_ATTACHMENT_OUTPUT, dst_access=COLOR_ATTACHMENT_WRITE.
   2. vkCmdBeginRendering with:
      - renderArea = { offset: bbox.origin, extent: bbox.size }
      - colorAttachments[0]: MaskScratch view, loadOp=CLEAR, clearValue=0.0
   3. Bind TrapPipeline
   4. Bind vertex buffer (per-instance arena allocation)
   5. Push constants (mask_extent, bbox_origin, bbox_size)
   6. vkCmdSetViewport (0, 0, mask_extent.width, mask_extent.height)
   7. vkCmdSetScissor (bbox.origin, bbox.size)
   8. vkCmdDraw(4 verts, n_instances, 0, 0)
   9. vkCmdEndRendering
   10. Barrier: MaskScratch COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL
       (src_stage=COLOR_ATTACHMENT_OUTPUT, src_access=COLOR_ATTACHMENT_WRITE,
        dst_stage=FRAGMENT_SHADER, dst_access=SHADER_SAMPLED_READ)
   11. mask_scratch.set_current_layout(SHADER_READ_ONLY_OPTIMAL)
   ```

   After step 11, the rest of `try_vk_render_traps_or_tris` proceeds as today (`record_render_composite` uses MaskScratch as mask).

7. **Saturated additive blend** (for overlapping primitives):
   ```rust
   .blend_enable(true)
   .src_color_blend_factor(vk::BlendFactor::ONE)
   .dst_color_blend_factor(vk::BlendFactor::ONE)
   .color_blend_op(vk::BlendOp::ADD)
   .src_alpha_blend_factor(vk::BlendFactor::ONE)
   .dst_alpha_blend_factor(vk::BlendFactor::ONE)
   .alpha_blend_op(vk::BlendOp::ADD)
   .color_write_mask(vk::ColorComponentFlags::R) // R8_UNORM single channel
   ```

8. **`BatchUploadArena` buffer-usage tweak**:
   ```rust
   // batch_upload_arena.rs ~line 145
   .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::VERTEX_BUFFER)
   ```
   No other change. The arena's mapping + alignment remain. T1 only adds the flag.

9. **MaskScratch sizing**: the existing `ensure_image_size_returning_old` (Phase 5 T5) gates the bbox. The GPU rasterize uses the same gating — `mask_scratch.needs_image_grow(bbox_w, bbox_h)` check + defer-release stays.

10. **Test plan**:
    - Unit: pipeline construction tests (the SPIR-V byte arrays loaded correctly, layout valid). Mirrors `LogicFillPipelineCache::tests`.
    - Integration / smoke: `just rendercheck-yserver` — trap-related tests should pass. The current CPU rasterizer is tuned to pass them; analytic coverage might shift pixel values slightly. Per the file's own comment: "test suites are the arbiter, not pixel equality." If individual tests regress, tune the coverage formula or use the exact wedge formula.
    - Hardware smoke: bee with adapta-nokto + mate-cc. Should be **dramatically** better (the 19.73% rasterize CPU goes to ~0%). Window dragging on bee should also improve (same code path serves it).

11. **clippy / fmt**: standard. 5 pre-existing `doc_lazy_continuation` warnings.

12. **Build-side**: glslc compiles every `.glsl` under `src/kms/vk/shaders/`. T1's two new shaders (and T3's two more) just need to land in that directory; the build picks them up automatically.

---

## Task 1: `TrapPipeline` infrastructure (no callers wired yet)

**Goal:** Add the new pipeline + 2 shaders + `BatchUploadArena` usage flag. Pure addition; no caller wired.

**Files:**
- Create: `crates/yserver/src/kms/vk/trap_pipeline.rs`
- Create: `crates/yserver/src/kms/vk/shaders/trap.vert.glsl`
- Create: `crates/yserver/src/kms/vk/shaders/trap.frag.glsl`
- Modify: `crates/yserver/src/kms/vk/mod.rs` (add `pub mod trap_pipeline;`)
- Modify: `crates/yserver/src/kms/scheduler/batch_upload_arena.rs` (add `VERTEX_BUFFER` flag)
- Modify: `crates/yserver/src/kms/vk/ops/traps.rs` — expose `TrapInstanceData` (or equivalent) and a conversion `Trapezoid → TrapInstanceData`.

### Step 1: Add `VERTEX_BUFFER` usage to `BatchUploadArena`

- [ ] **Step 1**: in `batch_upload_arena.rs:~145`, change:

```rust
.usage(vk::BufferUsageFlags::TRANSFER_SRC)
```

to:

```rust
.usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::VERTEX_BUFFER)
```

That's it — no other change in the arena. The mapping, allocation, and chunk-growth logic stay.

### Step 2: Write the two shaders

- [ ] **Step 2**: create `shaders/trap.vert.glsl` and `shaders/trap.frag.glsl` per the sketches in pre-task notes 4 + 5. Compile via `glslc` (build.rs handles this; verify it produces `trap.vert.spv` and `trap.frag.spv` under `$OUT_DIR`).

### Step 3: Write `trap_pipeline.rs`

- [ ] **Step 3**: define `TrapPipeline` mirroring `LogicFillPipelineCache`'s shape (it's the closest analogue: single shared layout, R-only color write, lazy or eager pipeline build). The trap pipeline is eager (only one variant — no per-op key like LogicFill has for the 16 GcFunctions).

```rust
//! GPU rasterization pipeline for RENDER Trapezoids (and Triangles
//! in T3) into the R8_UNORM MaskScratch image. Replaces the CPU
//! supersampled rasterizer.
//!
//! ... (full doc-comment per the existing pipeline files' style)

use std::sync::Arc;
use ash::vk;
use super::device::VkContext;

const TRAP_VERT_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/trap.vert.spv"));
const TRAP_FRAG_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/trap.frag.spv"));

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TrapInstanceData {
    pub top: f32,
    pub bottom: f32,
    pub left_p1: [f32; 2],
    pub left_p2: [f32; 2],
    pub right_p1: [f32; 2],
    pub right_p2: [f32; 2],
}
const _: () = assert!(std::mem::size_of::<TrapInstanceData>() == 40);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TrapDrawPushConsts {
    pub mask_extent: [f32; 2],
    pub bbox_origin_pixel: [f32; 2],
    pub bbox_size_pixel: [f32; 2],
    pub _pad: [f32; 2],
}
const _: () = assert!(std::mem::size_of::<TrapDrawPushConsts>() == 32);

impl TrapDrawPushConsts {
    pub fn as_bytes(&self) -> &[u8] { /* same pattern as LogicFillPushConsts */ }
}

#[derive(Debug, thiserror::Error)]
pub enum TrapPipelineError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("trap shader SPIR-V malformed (length not multiple of 4): {0} bytes")]
    SpirvUnaligned(usize),
}

impl From<vk::Result> for TrapPipelineError {
    fn from(r: vk::Result) -> Self {
        TrapPipelineError::Vk(r)
    }
}

pub struct TrapPipeline {
    vk: Arc<VkContext>,
    pipeline_layout: vk::PipelineLayout,
    /// Single pipeline; one shader per primitive type. In T1, just
    /// the trapezoid pipeline. T3 adds the triangle pipeline as a
    /// second field (or a HashMap<PrimitiveKind, vk::Pipeline>).
    trapezoid_pipeline: vk::Pipeline,
}

impl TrapPipeline {
    pub fn new(vk: Arc<VkContext>, mask_format: vk::Format) -> Result<Self, TrapPipelineError> {
        // build pipeline layout (push consts only — no descriptor sets
        // since per-instance data is via vertex attributes, not SSBO)
        // ...
        // build trapezoid_pipeline via build_trap_pipeline helper
        // ...
    }

    pub fn pipeline_layout(&self) -> vk::PipelineLayout {
        self.pipeline_layout
    }

    pub fn trapezoid_pipeline(&self) -> vk::Pipeline {
        self.trapezoid_pipeline
    }
}

impl Drop for TrapPipeline {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_pipeline(self.trapezoid_pipeline, None);
            self.vk.device.destroy_pipeline_layout(self.pipeline_layout, None);
        }
    }
}

fn build_trap_pipeline(
    vk: &VkContext,
    pipeline_layout: vk::PipelineLayout,
    mask_format: vk::Format,
) -> Result<vk::Pipeline, TrapPipelineError> {
    // Mirror LogicFill::build_pipeline. Key differences:
    //   - Vertex input: one per-instance binding (stride=40) with 6 attrs
    //     (top:f32, bottom:f32, left_p1:vec2, left_p2:vec2, right_p1:vec2, right_p2:vec2)
    //   - Blend: ONE+ONE additive ADD on R only (saturated by R8_UNORM clamp)
    //   - Color format: mask_format (R8_UNORM)
    //   - Logic op: disabled
    //   - Multisample: TYPE_1 (no MSAA — analytic coverage in fragment)
    // ...
}

fn create_shader_module(
    device: &ash::Device,
    spv_bytes: &[u8],
) -> Result<vk::ShaderModule, TrapPipelineError> {
    // Same as LogicFillPipelineCache::create_shader_module.
}
```

### Step 4: Provide a `Trapezoid → TrapInstanceData` conversion helper in `vk/ops/traps.rs`

- [ ] **Step 4**: append to `vk/ops/traps.rs`:

```rust
impl Trapezoid {
    /// Convert this trapezoid (16.16 fixed-point) into the
    /// f32-based instance struct the GPU pipeline expects.
    #[must_use]
    pub fn to_instance_data(&self) -> crate::kms::vk::trap_pipeline::TrapInstanceData {
        crate::kms::vk::trap_pipeline::TrapInstanceData {
            top: fixed_to_f32(self.top),
            bottom: fixed_to_f32(self.bottom),
            left_p1: [fixed_to_f32(self.left_p1.0), fixed_to_f32(self.left_p1.1)],
            left_p2: [fixed_to_f32(self.left_p2.0), fixed_to_f32(self.left_p2.1)],
            right_p1: [fixed_to_f32(self.right_p1.0), fixed_to_f32(self.right_p1.1)],
            right_p2: [fixed_to_f32(self.right_p2.0), fixed_to_f32(self.right_p2.1)],
        }
    }
}
```

`rasterize_trapezoids` and `rasterize_triangles` stay (T5 deletes them).

### Step 5: Register module

- [ ] **Step 5**: add `pub mod trap_pipeline;` in `vk/mod.rs`.

### Step 6: Unit tests in `trap_pipeline.rs`

- [ ] **Step 6**: minimal pure-logic tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn instance_data_size_is_40() {
        assert_eq!(std::mem::size_of::<TrapInstanceData>(), 40);
    }
    #[test]
    fn push_consts_size_is_32() {
        assert_eq!(std::mem::size_of::<TrapDrawPushConsts>(), 32);
    }
}
```

### Step 7: Gates + commit

- [ ] **Step 7**:

```bash
cargo +nightly fmt
cargo clippy -p yserver
cargo test --workspace
```

Expected:
- fmt clean; clippy: 5 pre-existing only.
- Tests green; 2 new tests pass.
- Build emits `trap.vert.spv` + `trap.frag.spv`.

Commit message:

```text
refactor(kms): GPU trap-rasterize pipeline infrastructure (gpu-trap T1)

Add TrapPipeline + vertex/fragment shaders for analytic edge-coverage
rasterization of RENDER Trapezoids into MaskScratch (R8_UNORM).

Pipeline draws one unit quad per instance via vkCmdDraw(4, n_traps).
Per-instance vertex attributes encode trap edges (top, bottom, two
edge endpoints each). Fragment shader computes analytic coverage =
horizontal-clamp × horizontal-clamp × signed-distance-clamp²; output
is single-channel R8.

Saturated additive blend (ONE+ONE ADD) gives the same union-with-
saturating-add semantics as the CPU path. R8_UNORM's natural clamp
to [0, 1] is the saturation.

Companion changes:
- BatchUploadArena buffer usage gains VERTEX_BUFFER bit.
- Trapezoid::to_instance_data conversion helper.
- TrapDrawPushConsts (32 bytes) for per-draw mask_extent + bbox.

Pure addition; no caller wired yet (T2 wires the trapezoid arm).
```

### Done conditions for T1

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` produces only pre-existing warnings.
3. `cargo test --workspace` green; 2 new tests pass.
4. `crates/yserver/src/kms/vk/trap_pipeline.rs` exists.
5. `crates/yserver/src/kms/vk/shaders/trap.vert.glsl` + `trap.frag.glsl` exist.
6. `TrapPipeline::new(vk, R8_UNORM)` constructs successfully (covered by an init smoke if the agent can hit a Vulkan path).
7. `BatchUploadArena` buffer usage includes `VERTEX_BUFFER`.
8. No call site of `TrapPipeline` yet (verified by grep).
9. Single new commit.

---

## Task 2: Wire trapezoid arm of `try_vk_render_traps_or_tris` to GPU

**Goal:** Replace CPU `rasterize_trapezoids` + `MaskScratch::record_upload_r8` with a GPU draw using `TrapPipeline`. **This is the headline change** — moves the rasterize off the X protocol hot path.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

### Step 1: Add `trap_pipeline` field on `KmsBackend`

- [ ] **Step 1**: add a field between `pixmap_pool` and `ops_command_pool` (drop-order requirement: trap_pipeline holds Vk handles; must drop after scheduler and before VkContext).

```rust
pub(crate) trap_pipeline: Option<crate::kms::vk::trap_pipeline::TrapPipeline>,
```

Init at backend construction:
```rust
let trap_pipeline = vk.as_ref().and_then(|vkctx| {
    match crate::kms::vk::trap_pipeline::TrapPipeline::new(
        std::sync::Arc::clone(vkctx),
        ash::vk::Format::R8_UNORM,
    ) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("TrapPipeline::new failed: {e:?} — traps will fall back to pixman");
            None
        }
    }
});
backend.trap_pipeline = trap_pipeline;
```

Plumb through both construction paths: `open_with_commit` AND `for_tests_with_vk`.

### Step 2: Refactor trapezoid call site

- [ ] **Step 2**: The current call site at `backend.rs:4731-4733`:

```rust
let mask = vk_traps::rasterize_trapezoids(&decoded, bx, by, bw, bh);
self.try_vk_render_traps_or_tris(op, host_src, host_dst, &mask, bx, by, bw, bh)
```

becomes:

```rust
// gpu-trap T2: no CPU rasterize. Hand the traps directly to
// try_vk_render_traps_or_tris, which builds the coverage mask on
// the GPU inside the open paint batch.
self.try_vk_render_traps_or_tris_traps(op, host_src, host_dst, &decoded, bx, by, bw, bh)
```

Note: T2 introduces a NEW function `try_vk_render_traps_or_tris_traps` (trapezoid-only) so the existing `try_vk_render_traps_or_tris(..., coverage_mask: &[u8], ...)` stays alive for the triangle arm until T3 replaces it. T5 cleans up the naming once both are migrated.

**Alternative**: keep the existing function name and change its signature to take an enum `TrapsOrTris<'a> { Traps(&'a [Trapezoid]), Tris(&'a [Triangle]) }`. T2 handles `Traps` branch via GPU; the `Tris` branch falls through to a temporary CPU bridge that calls `rasterize_triangles` + the old upload path. T3 wires the `Tris` branch to GPU. This avoids the dual-function naming churn.

Recommended: the enum approach. Cleaner state across T2/T3. Implementer's choice if rustc objects.

### Step 3: New body — trapezoid branch GPU draw

Inside `try_vk_render_traps_or_tris` (or whichever function holds the trap path), the new body for the trap arm:

```rust
// Acquire prereqs.
let Some((vk_arc, pool_handle)) = self.paint_resources() else {
    return false;
};
let Some(trap_pipeline) = self.trap_pipeline.as_ref() else {
    return false; // pipeline not initialized; pixman fallback
};
// ... existing mask_scratch + dst_readback prereq checks ...

// Bbox + sizing (Phase 5 T5 defer-release for mask grow stays unchanged).
// ... existing ensure_image_size_returning_old + defer_resource_release block ...

let mask_view = self.mask_scratch.as_ref().expect("checked").image_view();
let mask_extent = self.mask_scratch.as_ref().expect("checked").extent();
// ... existing src_view, dst_readback_view, etc.

let trap_pipeline_handle = trap_pipeline.trapezoid_pipeline();
let trap_layout = trap_pipeline.pipeline_layout();

let result = self.scheduler.record_paint_batch_op(vk_arc, pool_handle, |vk, batch, cb| {
    // Upload per-instance data to the arena. Size = n_traps * 40.
    let needed = (decoded.len() as u64) * (std::mem::size_of::<TrapInstanceData>() as u64);
    let alloc = match batch.upload_arena_mut().alloc(needed, 4) {
        Ok(a) => a,
        Err(e) => { /* arena_oom flag pattern */ },
    };

    // memcpy the instance data into the arena. Trapezoid -> TrapInstanceData
    // conversion done via Trapezoid::to_instance_data.
    let mut instance_bytes: Vec<u8> = Vec::with_capacity(needed as usize);
    for t in decoded {
        let data = t.to_instance_data();
        instance_bytes.extend_from_slice(/* SAFETY: cast &TrapInstanceData to bytes */);
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            instance_bytes.as_ptr(),
            alloc.mapped_ptr.as_ptr(),
            instance_bytes.len(),
        );
    }

    // --- GPU rasterize pass ---
    let mask_scratch = self.mask_scratch.as_mut().expect("checked");
    let mask_image = mask_scratch.image();
    let bbox_origin = vk::Offset2D { x: bbox_x, y: bbox_y };
    let bbox_size = vk::Extent2D { width: bbox_w, height: bbox_h };

    // 1. Barrier MaskScratch <current_layout> → COLOR_ATTACHMENT_OPTIMAL.
    //    src stage/access depend on the source layout (see
    //    pre-task note 6): UNDEFINED → (TOP_OF_PIPE, NONE);
    //    SHADER_READ_ONLY_OPTIMAL → (FRAGMENT_SHADER, SHADER_SAMPLED_READ);
    //    fallback → (ALL_COMMANDS, SHADER_SAMPLED_READ).
    let src_layout = mask_scratch.current_layout();
    let (src_stage, src_access) = match src_layout {
        vk::ImageLayout::UNDEFINED => (
            vk::PipelineStageFlags2::TOP_OF_PIPE,
            vk::AccessFlags2::NONE,
        ),
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
        _ => (
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
    };
    let to_attach = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(src_stage)
        .src_access_mask(src_access)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .old_layout(src_layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .image(mask_image)
        .subresource_range(/* color level=1 layer=1 */)];
    let dep = vk::DependencyInfo::default().image_memory_barriers(&to_attach);
    unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };

    // 2. Begin rendering with LOAD_OP_CLEAR
    let clear = vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 0.0] } };
    let color_attachment = vk::RenderingAttachmentInfo::default()
        .image_view(mask_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .clear_value(clear);
    let color_attachments = [color_attachment];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(vk::Rect2D { offset: bbox_origin, extent: bbox_size })
        .layer_count(1)
        .color_attachments(&color_attachments);
    unsafe { vk.device.cmd_begin_rendering(cb, &rendering_info) };

    // 3. Bind pipeline + vertex buffer
    unsafe {
        vk.device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, trap_pipeline_handle);
        vk.device.cmd_bind_vertex_buffers(cb, 0, &[alloc.buffer], &[alloc.offset]);
    }

    // 4. Push constants
    let pc = TrapDrawPushConsts {
        mask_extent: [mask_extent.width as f32, mask_extent.height as f32],
        bbox_origin_pixel: [bbox_x as f32, bbox_y as f32],
        bbox_size_pixel: [bbox_w as f32, bbox_h as f32],
        _pad: [0.0; 2],
    };
    unsafe {
        vk.device.cmd_push_constants(
            cb,
            trap_layout,
            vk::ShaderStageFlags::VERTEX,
            0,
            pc.as_bytes(),
        );
    }

    // 5. Viewport + scissor (dynamic state)
    let viewport = vk::Viewport {
        x: 0.0, y: 0.0,
        width: mask_extent.width as f32,
        height: mask_extent.height as f32,
        min_depth: 0.0, max_depth: 1.0,
    };
    let scissor = vk::Rect2D { offset: bbox_origin, extent: bbox_size };
    unsafe {
        vk.device.cmd_set_viewport(cb, 0, &[viewport]);
        vk.device.cmd_set_scissor(cb, 0, &[scissor]);
    }

    // 6. Draw
    unsafe { vk.device.cmd_draw(cb, 4, decoded.len() as u32, 0, 0) };

    // 7. End rendering
    unsafe { vk.device.cmd_end_rendering(cb) };

    // 8. Barrier MaskScratch COLOR_ATTACHMENT→SHADER_READ_ONLY
    let to_read = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(mask_image)
        .subresource_range(/* ... */)];
    let dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
    unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };
    mask_scratch.set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);

    // --- continue with existing render_composite using MaskScratch as mask ---
    // ... (existing descriptor allocation, src_clear, dst_readback, record_render_composite) ...

    Ok(())
});
```

Borrow-checker note: the `mask_scratch.as_mut()` borrow inside the closure conflicts with `mask_scratch.as_ref()` outside (for `mask_view` / `mask_extent`). Standard fix: snapshot `mask_view` + `mask_extent` + `mask_image` BEFORE entering the closure (immutable copies); pass them into the closure by value. Then inside the closure, only `mask_scratch.set_current_layout(...)` needs the `&mut`.

### Step 4: Gates + commit

- [ ] **Step 4**:

```bash
cargo +nightly fmt
cargo clippy -p yserver
cargo test --workspace
```

Hardware smoke (user-owned): bee with adapta-nokto + mate-cc. Should be dramatically better.

Commit message:

```text
refactor(kms): wire trapezoid arm to GPU rasterize (gpu-trap T2)

Replaces CPU rasterize_trapezoids + MaskScratch::record_upload_r8
in the trapezoid arm of try_vk_render_traps_or_tris with a GPU
draw using TrapPipeline (T1). Per-instance trap data uploads to
the open batch's BatchUploadArena and is consumed as a vertex
buffer; vkCmdDraw(4, n_traps) emits one unit quad per trap; the
fragment shader computes analytic edge coverage and writes R8 to
MaskScratch via additive blending.

The CPU's rasterize_trapezoids call site is gone from the trapezoid
arm. The triangle arm still uses the CPU path (T3 wires it).

Headline win: the X protocol request handler no longer blocks on
CPU rasterization. On the bee/RDNA2 + adapta-nokto perf trace, the
19.73% CPU in rasterize_trapezoids becomes near-zero for the
trap path; ditto the CPU→GPU mask memcpy.

MaskScratch layout transitions: UNDEFINED → COLOR_ATTACHMENT →
SHADER_READ_ONLY_OPTIMAL. set_current_layout updated. Phase 5 T5's
ensure_image_size_returning_old defer-release for mask grow stays.
```

### Done conditions for T2

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` clean.
3. `cargo test --workspace` green.
4. `rasterize_trapezoids` is no longer called from the trapezoid arm of `try_vk_render_traps_or_tris` (verify by grep — it's still called from the triangle arm until T3, then deleted in T5).
5. `MaskScratch::record_upload_r8` is no longer called from the trapezoid arm (same caveat).
6. `trap_pipeline.trapezoid_pipeline()` is bound + drawn at the new call site.
7. Single new commit.

---

## Task 3: Wire triangle arm to GPU

**Goal:** Same shape as T2 for triangles. Triangle pipeline has 3 edges instead of 4. Add a `triangle.vert.glsl` + `triangle.frag.glsl` pair (or extend the trap pipeline with a primitive-type push-const if shader code can be shared).

**Files:**
- Create: `crates/yserver/src/kms/vk/shaders/triangle.vert.glsl`
- Create: `crates/yserver/src/kms/vk/shaders/triangle.frag.glsl`
- Modify: `crates/yserver/src/kms/vk/trap_pipeline.rs` (add `triangle_pipeline` field, `triangle_pipeline()` accessor)
- Modify: `crates/yserver/src/kms/vk/ops/traps.rs` (`Triangle::to_instance_data` helper)
- Modify: `crates/yserver/src/kms/backend.rs` (triangle arm uses the new pipeline + new shaders)

### Step 1: Add triangle instance data + shaders

`TriangleInstanceData`:
```rust
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TriangleInstanceData {
    pub p1: [f32; 2],
    pub p2: [f32; 2],
    pub p3: [f32; 2],
}
const _: () = assert!(std::mem::size_of::<TriangleInstanceData>() == 24);
```

Triangle fragment shader uses 3 half-plane tests (one per edge). RENDER does NOT specify a winding convention — triangles can arrive in either CW or CCW order. The CPU reference (`point_in_triangle` in `vk/ops/traps.rs:287`) handles this with sign-agnostic barycentric tests: "interior" means all three signs agree (all positive OR all negative). Replicate that on the GPU:

```glsl
// Compute orientation once per triangle (could be a vertex-shader
// computation passed through as flat, or done per-fragment — it's
// cheap). The signed area of (p1, p2, p3) determines orientation:
// positive = CCW, negative = CW. Multiply each edge's coverage by
// this sign so the inside-side is consistent regardless of winding.
float signed_area_2 = (p2.x - p1.x) * (p3.y - p1.y) - (p2.y - p1.y) * (p3.x - p1.x);
float orient = signed_area_2 >= 0.0 ? 1.0 : -1.0;

float c1 = edge_coverage_linear(p, p1, p2, orient);
float c2 = edge_coverage_linear(p, p2, p3, orient);
float c3 = edge_coverage_linear(p, p3, p1, orient);
coverage = c1 * c2 * c3;
```

Do NOT use `abs(c_i)` — `c_i` is in `[0, 1]` so abs is a no-op. The actual handling is in the sign passed to `edge_coverage_linear`. Compute `orient` once per triangle (in the vertex shader if possible, flat-interpolated to fragment) to avoid 3 redundant cross-products per pixel.

Degenerate triangle (signed_area_2 ≈ 0): the three points are collinear; the triangle covers zero area. The `len < 1e-6` guard inside `edge_coverage_linear` makes one or more edges return 0 coverage, which makes the product zero. Correct.

### Step 2: Triangle pipeline construction in `TrapPipeline::new`

Build a second pipeline with the triangle vertex/fragment shaders. Same pipeline layout (push consts identical).

### Step 3: Wire triangle arm in backend

Same pattern as T2's trap branch — replace `rasterize_triangles` + upload with GPU draw.

### Step 4: Gates + commit

```text
refactor(kms): wire triangle arm to GPU rasterize (gpu-trap T3)
```

### Done conditions for T3

1-3. fmt / clippy / test green.
4. `rasterize_triangles` no longer called from the triangle arm (only the to-be-deleted function definition remains).
5. `MaskScratch::record_upload_r8` has zero callers in the trap+tri paths.
6. Single new commit.

---

## Task 4: rendercheck pass + AA tuning

**Goal:** Run `just rendercheck-yserver` and confirm trap-related tests pass. If individual tests regress on pixel values, tune the coverage formula in `trap.frag.glsl` / `triangle.frag.glsl`.

### Step 1: Run rendercheck

```bash
just rendercheck-yserver
```

Expected: same pass shape as Phase 5 baseline (pre-pool + pre-GPU-trap). Trap tests in particular: `Trapezoids/*`, `CompositeGlyphs*` (which sometimes route through traps for complex glyph shapes), `Triangles/*`.

### Step 2: Diagnose regressions

If a trap test regresses:
- Inspect the failure log at `target/rc-logs/rc-*.log` (per the rendercheck recipes memory).
- Compare actual vs reference at the pixel level. The linear coverage approximation can be off by 1-2/255 on edges grazing pixel corners.
- Tuning options:
  - Increase coverage gain: replace `clamp(0.5 - signed_dist, 0, 1)` with `smoothstep(-0.5, 0.5, -signed_dist)` (slightly steeper falloff).
  - Use the exact wedge formula (more shader code, exact).
  - If a specific test is over-sensitive, document the diff (~1-2 LSB) as acceptable per the file's "test suites are the arbiter, not pixel equality" comment.

### Step 3: Commit (only if AA tuning was needed)

```text
refactor(kms): tune GPU trap coverage shader to pass rendercheck (gpu-trap T4)
```

If no tuning needed, T4 is a no-op (result doc captures rendercheck-pass status).

### Done conditions for T4

1. `just rendercheck-yserver` produces results in `target/rc-logs/`; the pass shape is no worse than the Phase 5 baseline.
2. Any pixel-value regressions are documented (with rendercheck log paths) in the T6 results doc.
3. Single commit (if tuning was needed; otherwise no commit).

---

## Task 5: Delete dead CPU rasterizer paths

**Goal:** Remove `rasterize_trapezoids` + `rasterize_triangles` + `MaskScratch::record_upload_r8` if no other consumer remains.

### Step 1: Audit callers

```bash
rg -n 'rasterize_trapezoids|rasterize_triangles|record_upload_r8' crates/yserver/src/
```

Expected after T2 + T3: zero callers.

### Step 2: Delete

Remove the functions. Update `vk/ops/traps.rs`'s module doc to reflect that rasterization is now GPU-side.

`MaskScratch::record_upload_r8` may be the only function in `MaskScratch` that's CPU-upload-related; if so, also clean up the staging-buffer-via-arena upload path. But keep `record_clear_to_zero` / similar helpers if introduced in T2 (they're needed for future MaskScratch uses).

### Step 3: Gates + commit

```bash
cargo +nightly fmt
cargo clippy -p yserver
cargo test --workspace
just rendercheck-yserver  # confirm no regression vs T4
```

Commit message:

```text
refactor(kms): delete dead CPU trap/triangle rasterizers (gpu-trap T5)

After T2+T3 migrated both arms of try_vk_render_traps_or_tris to
GPU rasterization, rasterize_trapezoids + rasterize_triangles +
MaskScratch::record_upload_r8 have zero callers. Delete.

The 19.73% CPU cost from the bee adapta-nokto perf trace is now
zero by construction — the code path doesn't exist.

vk/ops/traps.rs module doc updated to reflect GPU rasterization.
```

### Done conditions for T5

1-3. fmt / clippy / test / rendercheck green.
4. `rasterize_trapezoids` / `rasterize_triangles` / `MaskScratch::record_upload_r8` no longer exist (verify by grep — only string-literal references in commit messages remain).
5. Single new commit.

---

## Task 6: Results doc + status update

**Goal:** Write `docs/superpowers/plans/2026-05-14-gpu-trap-rasterization-results.md`. Update `docs/status.md`.

Sections (mirror Phase 5 / pool results-doc shape):
1. Scope landed (T1–T5).
2. Preflight checks (fmt / clippy / test output captured fresh).
3. Cutover greps:
   - `rg -n 'rasterize_trapezoids|rasterize_triangles' crates/yserver/src/` → zero hits.
   - `rg -n 'TrapPipeline' crates/yserver/src/kms/` → field decl, init, accessor calls.
4. Done conditions table (phase-level + per-T).
5. **Hardware smoke results** — bee adapta-nokto + mate-cc TBD (user-owned); fuji no-regression TBD; rendercheck baseline shift TBD.
6. Plan bugs caught (codex rounds — fold this section after dispatch).
7. Commit summary.
8. Known deferred items:
   - Pixman trap path (Vk-down fallback) — unchanged.
   - Exact wedge formula vs linear approximation — could revisit if specific tests regress.
9. What's next.

Status doc: move "GPU trap rasterization" from new Remaining to Done. Phase 6 (refcounted handles) stays next.

### Step 1-3: Write, edit, commit (per Phase 5 / pool T6 template).

### Done conditions for T6

1. Results doc exists.
2. status.md reflects this work done.
3. Single commit.

---

## Phase-level Done conditions

1. `cargo +nightly fmt --check`, `cargo clippy -p yserver`, `cargo test --workspace` all green.
2. `crates/yserver/src/kms/vk/trap_pipeline.rs` exists.
3. `rasterize_trapezoids` / `rasterize_triangles` / `MaskScratch::record_upload_r8` have zero callers in the tree (T5 deletes them entirely).
4. `try_vk_render_traps_or_tris` (or its replacement) records GPU rasterize draws into MaskScratch; no CPU rasterize / upload step remains.
5. `just rendercheck-yserver` regression-free vs the Phase 5 baseline (T4 confirms; T6 documents).
6. Hardware smoke: bee adapta-nokto + mate-cc shows dramatic improvement vs the post-pool baseline. (The load-bearing user-observed validation.)

---

## Smoke plan (T6 hardware section)

### Check 1 — bee (RDNA2 + Arch) adapta-nokto + mate-cc

The load-bearing test. Apply adapta-nokto. Pre-GPU-trap: catastrophic (the user just reported "takes down the machine"). Post-GPU-trap expectation: dramatic improvement — mouse responsive, btop redraws, theme apply completes in reasonable time.

### Check 2 — bee window-drag

The user also reported "very high CPU load when dragging windows." If window-drag uses trap-based decoration (probable on adapta), GPU rasterize should fix it too. Verify CPU usage during a 5-second drag drops materially.

### Check 3 — fuji non-regression

Fuji was responsive post-Phase-5. GPU rasterize should at least match fuji's pre-change behaviour. Sanity check: MATE session under default theme, mate-cc apply with various themes.

### Check 4 — rendercheck regression

`just rendercheck-yserver`. Trap-related tests must pass. T4 should have surfaced any AA regressions; T6 captures the final pass shape.

### Check 5 — perf re-snapshot

```bash
perf record -F 99 -g -p $(pgrep yserver) -o /tmp/bee-post-gpu-trap.data -- sleep 10
# (trigger adapta-nokto apply during the sleep)
perf report -i /tmp/bee-post-gpu-trap.data --stdio --no-children --sort=overhead,comm,dso,sym | head -40
```

Expected: `rasterize_trapezoids` not in the top 30 (it doesn't exist anymore). Top samples should be dominated by either composite-side work (acceptable; we're now bottlenecked elsewhere) or libdrm_amdgpu (still some kernel overhead but lower since one fewer per-request GPU upload is happening).

---

## Codex review checkpoints

Per Phase 5 / pool pattern: codex review after each task; fold P0/P1 as fix-ups.

Focus areas:
- **T1**: shader correctness — the analytic coverage formula. Especially the winding/sign convention and degenerate edge handling. Also pipeline state (additive blend, R-only write mask, no MSAA).
- **T2**: borrow-checker pattern around the closure. MaskScratch field access (`&mut` for `set_current_layout` vs immutable snapshots for `image_view` / `extent` / `image`).
- **T3**: triangle winding-order handling. Less symmetric than traps.
- **T4**: rendercheck-pass status. If AA regressions, evaluate whether the linear approximation is "close enough" or wedge formula is needed.
- **T5**: complete deletion (no leftover dead code).
- **T6**: hardware smoke results (user-owned; doc captures placeholders).

---

## Glossary

- **`TrapPipeline`**: new Vulkan pipeline for GPU rasterization of RENDER Trapezoids + Triangles into MaskScratch.
- **`TrapInstanceData`**: 40-byte per-trap struct uploaded as per-instance vertex attributes.
- **`TriangleInstanceData`**: 24-byte per-triangle struct (T3).
- **`TrapDrawPushConsts`**: 32-byte per-draw constants (mask_extent + bbox).
- **Analytic edge coverage**: fragment-shader-side computation of exact (or near-exact) fractional pixel-area inside the primitive, replacing CPU 4×4 supersampling.
