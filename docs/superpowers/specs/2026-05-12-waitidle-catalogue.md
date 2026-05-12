# `vkQueueWaitIdle` / `vkDeviceWaitIdle` Site Catalogue

**Date:** 2026-05-12
**Branch:** graphics-followups
**Purpose:** Inventory every wait-idle call site in `crates/yserver/` with a
lifetime classification, so phase 3 (recorder migration to `PaintBatch`) and
phase 4 (sync rework — timeline-semaphore replacement of hot-path
`vkQueueWaitIdle`) have a concrete target list. Phase numbering follows the
HLD at `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md`
("Phasing" section).

## Regeneration

Line numbers will drift as soon as phase 3 work begins. To rebuild this
catalogue against the current tree, run from the repo root:

```bash
rg -n 'queue_wait_idle' crates/yserver/
rg -n 'vkQueueWaitIdle|device_wait_idle' crates/yserver/
```

The first command is the primary enumeration; the second is a spot-check that
catches `vkDeviceWaitIdle` sites and any stray raw-Vulkan calls. The expected
total at the time of writing was **22 hits across 12 files** (20
`queue_wait_idle` + 2 `device_wait_idle`).

**Consumers must re-verify line numbers against `HEAD` before acting on any
row in this table** — the line column is a snapshot, not a live reference.

## How to read this table

| Column | Meaning |
|---|---|
| **File:Line** | Path relative to repo root; line verified against HEAD at commit time |
| **Surrounding function** | Enclosing `fn` or `impl Drop for` block |
| **Classification** | See key below |
| **Removal phase** | When the wait disappears or is replaced |
| **Notes** | One-line rationale |

### Classification key

- **sync** — gates a *frame's* GPU work so the next CPU step can proceed. The canonical hot-path drain the rework eliminates.
- **readback** — CPU is about to read GPU-written bytes (`GetImage` / readback paths). Replaced by a targeted fence wait.
- **teardown** — gates an object's lifetime end (`Drop`, image destroy on resize, pipeline cache rebuild). Stays permanently.
- **temporary** — placeholder scaffolding that exists because there is no in-flight resource tracking yet. Once `PaintBatch` owns the resource (phase 3) and the retire-queue-equivalent bookkeeping is in place, the wait moves into the batch's drain logic.

A resize path that frees an old buffer/image is **temporary** rather than
**teardown** when a retire queue could defer the free without a synchronous
wait — i.e. the wait is an artifact of the current eager-submit cadence, not a
fundamental lifetime requirement.

---

## Site table

| File:Line | Surrounding function | Classification | Removal phase | Notes |
|---|---|---|---|---|
| `crates/yserver/src/kms/vk/ops/mod.rs:59` | `OpsCommandPool::drop` | teardown | stays | Pool drop must drain the queue before `vkDestroyCommandPool`; CBs allocated from this pool may be in-flight. |
| `crates/yserver/src/kms/vk/ops/mod.rs:100` | `run_one_shot_op` | sync | phase 4 | The canonical per-op hot-path drain: submit then immediately wait idle so the caller's next op sees a clean queue. Every drawing op (fill, copy, text, traps…) funnels through here. |
| `crates/yserver/src/kms/vk/ops/mod.rs:168` | `OpsStaging::ensure` | temporary | phase 4 | Grow path for the shared staging buffer. Wait is conservative ("eager-submit means nothing is in-flight") but unnecessary once the retire queue defers old-buffer frees. |
| `crates/yserver/src/kms/vk/ops/mod.rs:184` | `OpsStaging::drop` | teardown | stays | Staging buffer destruction; must drain before unmap + free. |
| `crates/yserver/src/kms/vk/glyph.rs:444` | `GlyphAtlas::grow_staging` | temporary | phase 4 | Grow path for the atlas staging buffer; same pattern as `OpsStaging::ensure` — conservative wait that a retire queue renders unnecessary. |
| `crates/yserver/src/kms/vk/glyph.rs:460` | `GlyphAtlas::drop` | teardown | stays | Atlas drop must drain before freeing the staging buffer, atlas image, and view. |
| `crates/yserver/src/kms/vk/target.rs:735` | `DrawableImage::initialize_clear` | sync | phase 4 | One-shot CB that clears a freshly-created mirror to (0,0,0,0) and transitions it to `SHADER_READ_ONLY_OPTIMAL`. Could use a signalled fence instead of `wait_idle`; same pattern as `run_one_shot_op`. |
| `crates/yserver/src/kms/vk/copy_scratch.rs:76` | `CopyScratch::ensure_size` | temporary | phase 4 | Grow path that destroys the old scratch image. Retire queue could defer the old image free so the wait is unnecessary. |
| `crates/yserver/src/kms/vk/copy_scratch.rs:137` | `CopyScratch::drop` | teardown | stays | Scratch image drop; must drain before `vkDestroyImage` + free. |
| `crates/yserver/src/kms/vk/dst_readback.rs:105` | `DstReadback::ensure` | temporary | phase 4 | Grow path that replaces the per-format readback scratch image. Old image destroy could be deferred by a retire queue. |
| `crates/yserver/src/kms/vk/dst_readback.rs:264` | `DstReadback::drop` | teardown | stays | Readback scratch images drop; must drain before destroying views, images, and freeing memory. |
| `crates/yserver/src/kms/vk/gradient.rs:250` | `GradientPicture::drop` | teardown | stays | Gradient image drop; must drain before `vkDestroyImageView` + `vkDestroyImage` + free. |
| `crates/yserver/src/kms/vk/pipeline.rs:314` | `CompositorPipeline::drop` | teardown | stays | Compositor pipeline teardown; must drain before destroying descriptor pool, pipelines, layout, and sampler. |
| `crates/yserver/src/kms/vk/text_pipeline.rs:329` | `TextPipeline::drop` | teardown | stays | Text pipeline teardown; must drain before destroying descriptor pool, pipeline, layout, set layout, and sampler. |
| `crates/yserver/src/kms/vk/mask_scratch.rs:110` | `MaskScratch::ensure_image_size` | temporary | phase 4 | Grow path for the mask scratch image; retire queue could defer the old image free. |
| `crates/yserver/src/kms/vk/mask_scratch.rs:133` | `MaskScratch::ensure_staging` | temporary | phase 4 | Grow path for the mask scratch staging buffer; same pattern as `OpsStaging::ensure`. |
| `crates/yserver/src/kms/vk/mask_scratch.rs:239` | `MaskScratch::drop` | teardown | stays | Mask scratch drop; must drain before freeing staging buffer, view, image, and memory. |
| `crates/yserver/src/kms/vk/logic_fill_pipeline.rs:137` | `LogicFillPipelineCache::drop` | teardown | stays | Logic-fill pipeline cache teardown; must drain before destroying cached pipelines and layout. |
| `crates/yserver/src/kms/vk/render_pipeline.rs:510` | `RenderPipelineCache::drop` | teardown | stays | Render (RENDER Composite) pipeline cache teardown; must drain before destroying pipelines, descriptor pool, layout, set layout, and sampler. |
| `crates/yserver/src/kms/vk/render_pipeline.rs:652` | `SolidColorImage::drop` | teardown | stays | 1×1 solid-colour image drop; must drain before destroying view, image, and freeing memory. |
| `crates/yserver/src/kms/vk/scanout.rs:549` | `ScanoutBoPool::drain_all_pending` | teardown | stays | Modeset / hot-config reset path. `vkDeviceWaitIdle` ensures no submitted scanout CBs are racing a DRM tear-down. The function itself is a regular `pub fn`, but today its only consumer is `ScanoutBoPool::Drop` (the 4.1.2.6 modeset path that the `#[allow(dead_code)]` comment refers to is not yet wired up). A future fence-based drain could replace the heavy `vkDeviceWaitIdle` but the operation is inherently a full-device drain since scanout goes through a different queue family path. |
| `crates/yserver/src/kms/vk/device.rs:333` | `VkContext::drop` | teardown | stays | Top-level Vulkan context destruction. `vkDeviceWaitIdle` before `vkDestroyDevice` is mandatory by spec. |

---

## Summary

| Classification | Count | Sites |
|---|---|---|
| **sync** | 2 | `crates/yserver/src/kms/vk/ops/mod.rs:100`, `crates/yserver/src/kms/vk/target.rs:735` |
| **readback** | 0 | — |
| **temporary** | 6 | `crates/yserver/src/kms/vk/ops/mod.rs:168`, `crates/yserver/src/kms/vk/glyph.rs:444`, `crates/yserver/src/kms/vk/copy_scratch.rs:76`, `crates/yserver/src/kms/vk/dst_readback.rs:105`, `crates/yserver/src/kms/vk/mask_scratch.rs:110`, `crates/yserver/src/kms/vk/mask_scratch.rs:133` |
| **teardown** | 14 | `crates/yserver/src/kms/vk/ops/mod.rs:59`, `crates/yserver/src/kms/vk/ops/mod.rs:184`, `crates/yserver/src/kms/vk/glyph.rs:460`, `crates/yserver/src/kms/vk/copy_scratch.rs:137`, `crates/yserver/src/kms/vk/dst_readback.rs:264`, `crates/yserver/src/kms/vk/gradient.rs:250`, `crates/yserver/src/kms/vk/pipeline.rs:314`, `crates/yserver/src/kms/vk/text_pipeline.rs:329`, `crates/yserver/src/kms/vk/mask_scratch.rs:239`, `crates/yserver/src/kms/vk/logic_fill_pipeline.rs:137`, `crates/yserver/src/kms/vk/render_pipeline.rs:510`, `crates/yserver/src/kms/vk/render_pipeline.rs:652`, `crates/yserver/src/kms/vk/scanout.rs:549`, `crates/yserver/src/kms/vk/device.rs:333` |
| **unclear** | 0 | — |
| **Total** | **22** | — |

### Notes on `readback` classification

No site is classified as **readback** (CPU reads GPU-written bytes for
`GetImage`). The `DstReadback::ensure` grow path
(`crates/yserver/src/kms/vk/dst_readback.rs:105`) is classified **temporary**
because the wait there guards destroying the *old* scratch image on resize,
not the actual readback transfer. The actual readback copy
(`vkCmdCopyImageToBuffer`) is currently gated by `run_one_shot_op` at
`crates/yserver/src/kms/vk/ops/mod.rs:100` (classified **sync**); once phase 4
replaces `run_one_shot_op` with a timeline fence, the `GetImage` path will
use a targeted fence wait — that future targeted wait is where the
**readback** classification will live.

### Phase 4 target lists

Per the HLD's "Phasing" section, phase 4 ("Sync rework") replaces the
hot-path `vkQueueWaitIdle` between paint and composite with timeline
semaphores. The two **sync** sites and the six **temporary** sites are
all phase-4 targets — sync sites are deleted, temporary sites are replaced
with retire-queue / `PaintBatch` resource-ownership plumbing that defers
the free past the submission's fence value.

**Phase 4 — delete (sync, replace with timeline fence):**
- `crates/yserver/src/kms/vk/ops/mod.rs:100` (`run_one_shot_op`) → submit returns a fence; callers that need CPU-visible results wait on that fence.
- `crates/yserver/src/kms/vk/target.rs:735` (`initialize_clear`) → one-shot CB already; use a fence instead of `wait_idle`.

**Phase 4 — replace with deferred free (temporary):**
- `crates/yserver/src/kms/vk/ops/mod.rs:168`, `crates/yserver/src/kms/vk/glyph.rs:444`, `crates/yserver/src/kms/vk/copy_scratch.rs:76`, `crates/yserver/src/kms/vk/dst_readback.rs:105`, `crates/yserver/src/kms/vk/mask_scratch.rs:110`, `crates/yserver/src/kms/vk/mask_scratch.rs:133` → enqueue old resource into retire queue; drop when the queue drains past the submission's fence value.

**Stays forever (teardown):**
All 14 teardown sites. The `crates/yserver/src/kms/vk/device.rs:333`
`vkDeviceWaitIdle` is spec-required. The rest are `Drop` impls (and one
`pub fn` consumed only from `Drop`) whose waits are correct and permanent.
