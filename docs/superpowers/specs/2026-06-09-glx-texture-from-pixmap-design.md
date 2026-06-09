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

yserver mirror: on `dri3_export_pixmap`, if the pixmap's Vulkan image is not already external-memory/dmabuf-exportable:
- allocate a new **external-memory, dmabuf-exportable** Vulkan image (the `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT` path already used by `tests/dri3_fd_leak.rs`; DRM-format-modifier-aware via `VK_EXT_image_drm_format_modifier` if available, `VK_IMAGE_TILING_LINEAR` fallback),
- copy current content into it,
- **swap the `DrawableStore` storage** for the pixmap's `DrawableId` to the new image, so `resolve_paint_target` / `copy_area` / scene sampling all target the exportable image thereafter,
- extend `dri3_export_pixmap` to export this (drop the `imported_drawable`-only gate; export the image's bound external memory via `vkGetMemoryFdKHR`).

After promotion, client `copy_area`s land in the exportable image → the dmabuf muffin holds is live.

### 2. Cross-API sync (Vulkan write → GL read)

The exported dmabuf is read by muffin's Mesa GL while yserver writes it via Vulkan `copy_area`. Without ordering, muffin reads mid-render (the observed intermittency). Use the DRI3 fence muffin already creates and yserver imports (`FenceFromFD` → `dri3_fence_from_fd`, backend.rs:13280; xshmfence + sync_file paths exist) to gate the GL read behind yserver's Vulkan write completion. Reuse the existing `wait_dmabuf_read_ready` / present-source-ready machinery (backend.rs:8129) where it applies. This is the same hazard class as the Firefox `wait_fence` fix — honor the consumer's fence rather than copying eagerly.

### 3. GLX protocol surface

- Add `GLX_EXT_texture_from_pixmap` to `SERVER_EXTENSIONS` (yserver-protocol x11/glx.rs:112).
- `synthesise_glx_fb_configs` (process_request.rs, served at GET_FB_CONFIGS:8196): on depth-24/32 configs add `GLX_DRAWABLE_TYPE |= GLX_PIXMAP_BIT`, `GLX_BIND_TO_TEXTURE_RGB_EXT`, `GLX_BIND_TO_TEXTURE_RGBA_EXT` (per format: RGB for depth-24, RGBA for depth-32), `GLX_BIND_TO_TEXTURE_TARGETS_EXT = GLX_TEXTURE_2D_BIT_EXT`, `GLX_Y_INVERTED_EXT`. Attribute values + reply layout per Xorg glxcmds.c:1094-1100.
- Implement the **indirect-context** `BindTexImageEXT` / `ReleaseTexImageEXT` vendor-private handlers (op 1330/1331; Xorg glxcmds.c:1731) for spec completeness. Direct contexts (muffin) ride the DRI3 export and do **not** hit these — but the extension is only "complete" with them.
- `glXCreatePixmap` / `glXDestroyPixmap` are already dispatched (process_request.rs:8311/8368); verify they associate the GLX drawable with the X pixmap correctly for the TFP path.

### 4. DRI3 completeness

Determine (at build time, by tracing Mesa's actual calls) whether direct-rendering TFP uses single-plane `BufferFromPixmap` (implemented, op 3) or multi-plane `BuffersFromPixmap` (op 7, currently deferred — process_request.rs:7620). If Mesa on DRI3 ≥ 1.2 requires `BuffersFromPixmap`, implement it (single-plane content + modifier metadata); otherwise ensure the version negotiation keeps Mesa on the supported path.

## Data flow (direct compositor, the muffin case)

1. Client (cinnamon-settings) `copy_area`s into its window → lands in the (now exportable) redirect backing's Vulkan image.
2. muffin gets DamageNotify (already works) → `glXCreatePixmap` over the NameWindowPixmap'd X pixmap → Mesa `BufferFromPixmap` → yserver `dri3_export_pixmap` promotes-if-needed + returns the live dmabuf + size/stride/modifier.
3. Mesa imports the dmabuf as an EGLImage/GL texture; `glXBindTexImageEXT` is client-side.
4. yserver's Vulkan write is ordered before muffin's GL read via the DRI3 fence (component 2).
5. muffin composites the live texture → present → scanout fresh.

## Error handling

- Promotion failure (no external-memory support / modifier query fails): fall back to linear tiling; if that fails, return `BadAlloc` on `BufferFromPixmap` (matches Xorg's "Failed to make pixmap exportable" → request error). Compositor degrades to its non-TFP fallback (current behavior — no worse than today).
- FBConfig attrs only added when Vulkan/external-memory export is actually available; otherwise don't advertise the extension (don't promise TFP we can't back).

## Testing

- **Unit (yserver-protocol/core):** `synthesise_glx_fb_configs` emits the bind-to-texture attrs + `GLX_PIXMAP_BIT` on depth-24/32; `SERVER_EXTENSIONS` contains the ext; FBConfig reply round-trips the new pairs.
- **Unit/integration (yserver, `--ignored` Vulkan):** promote a regular pixmap → `dri3_export_pixmap` yields a valid dmabuf; after promotion, a `copy_area` is visible in a re-export / readback (liveness); reuse the dmabuf export harness from `tests/dri3_fd_leak.rs`.
- **HW (user):** cinnamon-settings pane-switch redraws live (the repro); chromium/gtk GL clients unaffected; xtrace shows muffin now issuing `glXCreatePixmap` + `BufferFromPixmap` like Xorg.

## Open questions (resolve during implementation)

- Single- vs multi-plane DRI3 for Mesa's direct TFP (component 4) — trace it.
- Whether promotion can reuse the *existing* image's memory (if it happened to be allocated exportable) vs always reallocating — optimize only if measured.
- `GLX_Y_INVERTED_EXT` value: Xorg writes `GLX_DONT_CARE`; confirm muffin/Cogl handles our chosen value (yserver pixmaps are top-left origin → not Y-inverted).
