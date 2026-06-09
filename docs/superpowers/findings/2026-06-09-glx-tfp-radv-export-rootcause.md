# GLX TFP — why it doesn't engage on bee (RADV/RDNA2) + modifier-export plan

**Date:** 2026-06-09 · **HW:** bee (Ryzen 6900HX APU, RDNA2, RADV) · **Branch:** `feat/glx-texture-from-pixmap`

## Verdict

The TFP HW gate (plan Task 5.1) is **NOT met**. muffin logs "Not using GLX TFP!",
issues **0** `glXCreatePixmap` / **0** `BufferFromPixmap`, and falls back to its
copy-upload path (which is why **wobbly windows still work without TFP** — they
deform a copy-uploaded texture, not a bound pixmap). The cinnamon-settings paint
issue is **separate** and not fixed by TFP.

## Root cause (server-side, concrete)

yserver's dma-buf **export** image allocation is rejected by RADV. From
`yserver-hw-bare.log` (probe as sole client):

```
vkCreateImage(): handle type DMA_BUF_BIT_EXT with
  format VK_FORMAT_B8G8R8A8_UNORM, tiling VK_IMAGE_TILING_LINEAR,
  usage TRANSFER_SRC|TRANSFER_DST|SAMPLED|COLOR_ATTACHMENT
  → VK_ERROR_FORMAT_NOT_SUPPORTED
vkGetImageSubresourceLayout(): aspectMask (COLOR_BIT) must be MEMORY_PLANE_0_BIT_EXT  [modifier path]
```

`allocate_exportable` (`crates/yserver/src/kms/vk/target.rs:1093`) builds the
exportable image as **LINEAR + COLOR_ATTACHMENT + dma-buf**. RADV does not allow a
renderable/sampleable LINEAR image to be dma-buf-exported — it requires
`DRM_FORMAT_MODIFIER_EXT` tiling. **The plan's "LINEAR-only MVP" assumption
(Open-item #4) is invalid on RADV/RDNA2.** The exportable image must remain a
render target (the promoted window backing is painted into via COLOR_ATTACHMENT),
so dropping COLOR_ATTACHMENT isn't viable without changing promote-swap to
copy-into-export — hence the modifier path is the correct fix.

## What's already correct (ruled out)

- **GLX protocol surface is correct.** The client-side probe (`tools/glx-tfp-probe.c`)
  confirmed yserver returns 4 FBConfigs with the right TFP attrs (depth 24/32,
  `BIND_TO_TEXTURE_RGB/RGBA`, buffer_size==depth), correct GLX token constants
  (0x20D0–0x20D4 vs glxext.h), correct `xGLXGetFBConfigsReply` encoding, and visual
  IDs (0x102/0x103) matching the connection setup. `USABLE TFP depth 24/32: YES`.
- **DRI3 fresh-fd-per-client** is correct (`render_node.rs` opens, never dups) —
  the `feedback_dri3_open_fresh_fd` hazard is not present.

## Why muffin renders but can't TFP

muffin renders via the **import** direction (mesa/radeonsi allocates its own
buffers, yserver imports them — normal GL, works). TFP is the **export** direction
(server pixmap → client texture), which is the broken path.

## Scope of the real fix (modifier export path)

1. **Modifier-tiled exportable image** — `allocate_exportable` uses
   `DRM_FORMAT_MODIFIER_EXT` tiling with a `VkImageDrmFormatModifierListCreateInfoEXT`
   of RADV-supported modifiers for BGRA8 + the export usage + DMA_BUF handle type
   (intersect via `vkGetPhysicalDeviceImageFormatProperties2` with the external +
   modifier info in pNext). Read back the chosen modifier with
   `vkGetImageDrmFormatModifierPropertiesEXT`. Query layout with
   `VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT` (single plane), not `COLOR`. Thread the
   real modifier + per-plane stride/offset into `ExportableImage`. Fix the same
   `COLOR`→`MEMORY_PLANE_0` aspect in `dri3.rs::export_dmabuf` (line ~166-169).

2. **DRI3 `BuffersFromPixmap` (op 8) export** — yserver advertises DRI3 **(1,3)**,
   so mesa uses op-8 (multi-plane + modifier) for pixmap export, NOT the single-fd
   op-3 yserver currently implements. A modifier-tiled buffer MUST communicate its
   modifier to the client, which op-3's reply cannot. So the deferred op-8 export
   must be implemented (un-defer it). yserver already has the import-side modifier
   plumbing (`GetSupportedModifiers`, `PixmapFromBuffers`) to mirror.

3. **Capability-probe correctness** — the init `probe_dmabuf_export_support`
   (`backend.rs:405`) advertised TFP even though export is unsupported on RADV
   (RADV's `vkCreateImage` appears to return success for the bad combo while the
   validation layer flags it, so the probe passed). Make the probe authoritative
   (query `vkGetPhysicalDeviceImageFormatProperties2` for the real export combo,
   and/or probe the modifier path) so yserver only advertises TFP it can deliver —
   otherwise muffin sees TFP advertised, can't use it, and we're in a half-broken
   state instead of a clean fallback.

4. **HW gate (bee)** — re-run `just yserver-tfp-probe-hw` (sole-client probe):
   expect no `VK_ERROR_FORMAT_NOT_SUPPORTED`, `dri3 screen` created,
   `texture_from_pixmap: YES`, `USABLE depth 24: YES`. Then full cinnamon:
   `COGL_DEBUG=winsys` no longer prints "Not using GLX TFP!", and
   `glXCreatePixmap` / `BuffersFromPixmap` counts are non-zero.

## De-risk first (cheap)

Before building op-8: change `allocate_exportable` to modifier tiling and confirm
on bee that the `VK_ERROR_FORMAT_NOT_SUPPORTED` clears (image allocates). That
validates the Vulkan half before the larger DRI3 op-8 work. (Mirrors the original
plan's "Task 2.0 characterize-first" discipline.)

## Diagnostic assets (uncommitted, in repo)

- `tools/glx-tfp-probe.c` — client-side probe replicating muffin's
  `get_fbconfig_for_depth` decision.
- `just yserver-tfp-probe-hw` — bring up yserver alone on :7 + run the probe as
  sole client; output to `tfp-probe.out` / `yserver-hw-bare.log` (CWD).
