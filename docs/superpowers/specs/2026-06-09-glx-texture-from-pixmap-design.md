# GLX_EXT_texture_from_pixmap — design

**Date:** 2026-06-09
**Status:** design (awaiting review → plan)

## Problem

Cinnamon (muffin's GLX compositor) composites **stale** window content on yserver: cinnamon-settings doesn't redraw on a pane switch (backing fresh, scanout stale; intermittent under timing shifts). Root-caused (xtrace yserver-vs-Xorg + `COGL_DEBUG=winsys`, see `project_cinnamon_settings_norefresh` memory):

- yserver's GLX server string omits **`GLX_EXT_texture_from_pixmap`** → muffin logs "Not using GLX TFP!" and drops its live window-texture path (Xorg: `glXCreatePixmap` 17×; yserver: 0×).
- muffin's GLX backend therefore has no live texture of redirected window content, and the fallback it lands on serves stale/racy content.
- Even if TFP were advertised, the export of a window's redirect backing would fail: `dri3_export_pixmap` (kms/v2/backend.rs:13248) only exports pixmaps with an `imported_drawable`; internally-allocated backings have none.

The damage path is correct and exonerated (DamageNotify well-formed + delivered; muffin `DamageSubtract`s and recomposites). The gap is purely the live-texture mechanism.

## Goal

Implement **GLX_EXT_texture_from_pixmap** completely (full spec, not just muffin's subset, per the scope decision), so a GLX compositor binds yserver window pixmaps as **live, coherent** GL textures and composites fresh content. Success = the cinnamon-settings pane-switch repro redraws live on hardware, with no regression to existing GL clients (chromium, gtk).

## Architecture

Four components. The engine is (1)+(2); (3) is the GLX protocol surface; (4) is a DRI3 completeness check.

### 1. Exportable-pixmap promotion (the engine) — matches Xorg glamor

Xorg's reference: `glamor_make_pixmap_exportable` (glamor/glamor_egl.c:265) lazily promotes a pixmap on its **first export** — allocates a fresh exportable GBM bo (`GBM_BO_USE_RENDERING | GBM_BO_USE_SCANOUT`, DRM modifiers when available, linear fallback), migrates the pixmap's backing onto it **permanently**, so subsequent rendering lands in it and the exported dmabuf stays live. No redirect-backing special case — uniform for all pixmaps; permanence is what guarantees liveness.

yserver mirror: on `dri3_export_pixmap`, if the pixmap's Vulkan image is not already external-memory/dmabuf-exportable, **promote it permanently**:
- allocate a new **external-memory, dmabuf-exportable** Vulkan image (the `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT` path already used by `tests/dri3_fd_leak.rs`; DRM-format-modifier-aware via `VK_EXT_image_drm_format_modifier` if available, `VK_IMAGE_TILING_LINEAR` fallback),
- copy current content into it (and **carry the layout** — new `Storage` defaults to `UNDEFINED`, but the engine uses `storage.current_layout` as the barrier source; transition the new image to match, or re-seed via a defined-layout copy),
- **swap the `DrawableStore` storage** for the pixmap's `DrawableId` to the new image, so `resolve_paint_target` / `copy_area` / scene sampling all target the exportable image thereafter,
- **invalidate the cached `VkImageView`** for that `DrawableId` in the engine's `drawable_view_cache` (keyed `(DrawableId, SamplerConfig, SwizzleClass)`, engine.rs ~6530 — it never re-checks the `VkImage` handle, so a swap without invalidation keeps rendering/sampling the OLD image). Flush/retire any in-flight command buffers referencing the old `VkImage` before freeing it. RENDER Pictures are safe (re-resolved by xid each op).
- extend `dri3_export_pixmap` to export this (drop the `imported_drawable`-only gate; export the image's bound external memory via `vkGetMemoryFdKHR`).

After promotion, client `copy_area`s land in the exportable image → the dmabuf muffin holds is live. (This is the assessment's "main blocker" — see Prior research.)

### 2. Cross-API sync (bidirectional, via dma-buf implicit fencing)

The exported dmabuf is **shared live**: yserver writes it via Vulkan `copy_area`, muffin's Mesa GL samples it. Two hazards, BOTH must be handled:
- **write → read:** muffin must not sample mid-write (the observed intermittency).
- **read → next write:** yserver must not overwrite while muffin's GL is still sampling.

For a *live* window texture this is NOT solved by ping-pong/buffer-exchange (that defeats the live-shared-backing model — glamor only exchanges on its import/flip path, not window-TFP sampling). The Xorg-equivalent mechanism is **kernel dma-buf implicit fencing** on the shared bo: the producer's GPU work signals the dmabuf's exclusive fence and the consumer waits on it (and vice-versa via shared fences). **Vulkan does NOT participate in dma-buf implicit sync by default on external memory** — this is the likely true cause of the intermittency and the meatiest piece of the feature. The plan must make yserver's Vulkan submissions that touch an exported image export/signal the implicit fence (e.g. `VK_EXT_external_memory` + dma-buf fence import/export, or `VK_EXT_queue_family_foreign` ownership transfers) so Mesa's implicit-sync GL reads order correctly, and conversely wait on the dmabuf's read fences before the next overwrite. The existing `dri3_fence_from_fd` (backend.rs:13280) + `wait_dmabuf_read_ready` (backend.rs:8129) machinery is a starting point but only covers one direction. **Lifetime:** the `NameWindowPixmap` alias + the exported dmabuf must keep the backing alive until the GL consumer releases (refcount through the export, mirroring the alias_registry incref already in `name_window_pixmap`).

### 3. GLX protocol surface

- Add `GLX_EXT_texture_from_pixmap` to the advertised extensions **only when the backend can actually satisfy it** (Vulkan + external-memory dmabuf export available) — do NOT hardcode it into the static `SERVER_EXTENSIONS`; gate it at runtime (Xorg advertises it only when the DRI provider exposes `__DRI_TEX_BUFFER`, glxdri2.c:865). This needs the GLX ext-string path (currently `x11/glx.rs:112`) to become capability-conditional.
- `synthesise_glx_fb_configs` (served at GET_FB_CONFIGS:8196) + `drawable_attributes_for` (already reports `GLX_TEXTURE_TARGET_EXT` / `GLX_Y_INVERTED_EXT` per the assessment): on depth-24/32 configs add `GLX_DRAWABLE_TYPE |= GLX_PIXMAP_BIT` and the bind-to-texture pairs **in Xorg's exact reply order** (glxcmds.c:1094-1100): `GLX_BIND_TO_TEXTURE_RGB_EXT`, `GLX_BIND_TO_TEXTURE_RGBA_EXT` (RGB for depth-24, RGBA for depth-32), **`GLX_BIND_TO_MIPMAP_TEXTURE_EXT`** (=`0`/false — yserver doesn't back mipmap-from-pixmap, but the pair MUST be present to match the contract; was omitted), `GLX_BIND_TO_TEXTURE_TARGETS_EXT`, `GLX_Y_INVERTED_EXT`.
  - **`GLX_BIND_TO_TEXTURE_TARGETS_EXT` is NOT 2D-only.** Xorg ORs the driver-supported targets (glxdricommon.c:165-173: `GLX_TEXTURE_1D_BIT_EXT` / `GLX_TEXTURE_2D_BIT_EXT` / `GLX_TEXTURE_RECTANGLE_BIT_EXT`). Advertise the targets a dmabuf-imported pixmap texture can actually bind to — **`GLX_TEXTURE_2D_BIT_EXT | GLX_TEXTURE_RECTANGLE_BIT_EXT`** (1D excluded — pixmaps aren't 1D-bindable). Hard-coding 2D-only under-advertises and can make clients skip a usable TFP config.
  - **`GLX_Y_INVERTED_EXT` is fixed, not open:** Xorg writes `GLX_DONT_CARE` in FBConfig replies (glxcmds.c:1093/1100) and `GL_FALSE` in drawable attributes (glxcmds.c:1900) — use those exact values (yserver pixmaps are top-left origin).
- **Track GLXPixmap resources** (creation/destruction/attributes/mapping back to the underlying X pixmap / redirect backing). `glXCreatePixmap`/`glXDestroyPixmap` are dispatched (process_request.rs:8311/8368) but must correctly associate the GLX drawable with the X pixmap for the TFP path.
- Implement the **indirect-context** `BindTexImageEXT` / `ReleaseTexImageEXT` vendor-private handlers (op 1330/1331; Xorg glxcmds.c:1731). NOTE: `VendorPrivate`/`VendorPrivateWithReply` are currently rejected as unsupported — this path must be opened. Direct contexts (muffin) ride the DRI3 export and do not hit these, but "complete" requires them.

### 4. DRI3 export contract (single-plane first, no Mesa-trace blocker)

Xorg's `dri3_fd_from_pixmap` (dri3_screen.c:112) *prefers* the single-fd `fd_from_pixmap` interface and only uses multi-plane `fds_from_pixmap` when the old one is absent — rejecting multi-plane unless it collapses to one plane at offset 0. So for yserver's single-plane BGRA8 backings, **`BufferFromPixmap` (op 3, already implemented) is the correct minimal contract**; land it first. `BuffersFromPixmap` (op 7, deferred — process_request.rs:7620) is a later capability, only needed if/when yserver exports multi-plane or modifier-bearing buffers. This component does NOT block on tracing Mesa's calls.

## Data flow (direct compositor, the muffin case)

1. Client (cinnamon-settings) `copy_area`s into its window → lands in the (now exportable) redirect backing's Vulkan image.
2. muffin gets DamageNotify (already works) → `glXCreatePixmap` over the NameWindowPixmap'd X pixmap → Mesa `BufferFromPixmap` → yserver `dri3_export_pixmap` promotes-if-needed + returns the live dmabuf + size/stride/modifier.
3. Mesa imports the dmabuf as an EGLImage/GL texture; `glXBindTexImageEXT` is client-side.
4. yserver's Vulkan write and muffin's GL read are ordered via dma-buf implicit fencing on the shared image (component 2).
5. muffin composites the live texture → present → scanout fresh.

## Prior research

`docs/wip-texture-from-pixmap-assessment-2026-05-20.md` (codex, 2026-05-20) independently scoped this and is corroborated by the 2026-06-09 codex spec review. Key carry-overs: the "main blocker" is exporting yserver-owned redirect backings (this spec's component 1); advertise only when the backend can satisfy it (component 3); GLXPixmap resource tracking + the rejected `VendorPrivate` bind/release path (component 3); sync + alias lifetime (component 2). It also flagged the COW/reparent-redirect work as the prerequisite — **now satisfied** (COW structural redesign merged 2026-06-09), and the damage path is independently exonerated, so "TFP only exposes the same stale pixels" is no longer a risk: the staleness IS the missing live texture this feature provides.

**Effort estimate (from the assessment):** ≈1–2 weeks for a narrow MVP (muffin's direct path working end-to-end), 3–6 weeks for robust Xorg-like behavior (full FBConfig matrix, indirect contexts, multi-plane/modifiers). The "complete extension" scope chosen here targets the robust end; the phasing below lets the MVP land and HW-validate first.

## Error handling

- Promotion failure (no external-memory support / modifier query fails): fall back to linear tiling; if that fails, return `BadAlloc` on `BufferFromPixmap` (matches Xorg's "Failed to make pixmap exportable" → request error). Compositor degrades to its non-TFP fallback (current behavior — no worse than today).
- FBConfig attrs only added when Vulkan/external-memory export is actually available; otherwise don't advertise the extension (don't promise TFP we can't back).

## Testing

- **Unit (yserver-protocol/core):** `synthesise_glx_fb_configs` emits `GLX_PIXMAP_BIT` in drawable type and the **exact** Xorg-order pairs on depth-24/32 — `BIND_TO_TEXTURE_RGB/RGBA`, `BIND_TO_MIPMAP_TEXTURE_EXT=0`, `BIND_TO_TEXTURE_TARGETS_EXT = 2D|RECTANGLE`, `Y_INVERTED=GLX_DONT_CARE`; the ext appears in the advertised string **only when** the capability gate is on (and is absent when off); FBConfig reply round-trips the pairs.
- **Unit/integration (yserver, `--ignored` Vulkan):** promote a regular pixmap → `dri3_export_pixmap` yields a valid dmabuf; after promotion, a `copy_area` is visible in a re-export / readback (liveness); reuse the dmabuf export harness from `tests/dri3_fd_leak.rs`.
- **HW (user):** cinnamon-settings pane-switch redraws live (the repro); chromium/gtk GL clients unaffected; xtrace shows muffin now issuing `glXCreatePixmap` + `BufferFromPixmap` like Xorg.

## Phasing (the plan turns these into tasks)

Per codex's sequencing; lets the MVP land + HW-validate before the robust tail:

1. **Exportable-pixmap promotion** (component 1): external-memory image alloc, content+layout copy, storage swap, **view-cache invalidation**, in-flight-CB retire, extend `dri3_export_pixmap`. Unit-test the export + liveness.
2. **Bidirectional dma-buf sync** (component 2): make yserver's Vulkan writes participate in dma-buf implicit fencing on exported images (both directions) + alias lifetime. This is the riskiest piece — prototype + HW-verify the intermittency is gone here.
3. **GLX surface** (component 3): runtime capability-gate the extension; bind-to-texture FBConfig/drawable attrs with the exact Xorg values; GLXPixmap resource tracking; open the `VendorPrivate` bind/release path (indirect).
4. **DRI3**: confirm `BufferFromPixmap` suffices (it does for single-plane BGRA8); defer `BuffersFromPixmap`.

MVP = phases 1–3 enough for muffin's direct path → cinnamon-settings redraws live (the HW gate). Robust tail = indirect `BindTexImageEXT`, full FBConfig matrix, multi-plane/modifiers.

## Open questions (resolve during implementation)

- **The exact Vulkan↔dma-buf implicit-sync mechanism** (component 2). Concrete candidate path to validate (the standard Vulkan/Mesa dmabuf-interop pattern, e.g. wlroots / mutter-native): bridge Vulkan explicit sync ↔ the dmabuf's implicit reservation fences via **sync_file ioctls** —
  - *write→read:* after the Vulkan `copy_area` submission to the exported image, signal a binary semaphore, export it as a `SYNC_FD` (`vkGetSemaphoreFdKHR`, `VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT`), and attach it to the dmabuf as a WRITE fence (`DMA_BUF_IOCTL_IMPORT_SYNC_FILE`, `DMA_BUF_SYNC_WRITE`). Mesa's implicit-sync GL read then waits on it automatically.
  - *read→write:* before the next Vulkan write, snapshot the dmabuf's fences (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`), import as a Vulkan semaphore (`vkImportSemaphoreFdKHR` SYNC_FD), and wait on it in the submission.
  yserver already has the Vulkan↔sync_file half (`import_sync_file`, the Present path's sync_file export, `dri3_fence_from_fd`); the **new** piece is the `DMA_BUF_IOCTL_{IMPORT,EXPORT}_SYNC_FILE` on the exported dmabuf. Resolve/prototype early in phase 2 against the cinnamon repro — if RADV's external memory turns out to already be implicitly synced for same-GPU sharing, this collapses to a no-op, but do not assume that.
- Whether promotion can reuse the *existing* image's memory if it was already allocated exportable, vs always reallocating — optimize only if measured.
