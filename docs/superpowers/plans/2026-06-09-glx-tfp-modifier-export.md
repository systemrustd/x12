# GLX TFP — modifier export path (handoff for a fresh session)

**Branch:** `feat/glx-texture-from-pixmap` (TFP Phases 1–4 already landed).
**Read first:** `docs/superpowers/findings/2026-06-09-glx-tfp-radv-export-rootcause.md`
(full root cause + evidence) and memory `project_glx_tfp_radv_export`.

## One-paragraph context

TFP is implemented and the GLX protocol surface is **correct** (verified by probe),
but it does **not engage** on bee (6900HX/RADV): muffin logs "Not using GLX TFP!"
and falls back. Root cause is server-side — yserver's dma-buf **export** image
(`allocate_exportable`) uses `LINEAR + COLOR_ATTACHMENT + dma-buf`, which RADV
rejects (`VK_ERROR_FORMAT_NOT_SUPPORTED`). RADV requires `DRM_FORMAT_MODIFIER_EXT`
tiling. The plan's "LINEAR-only MVP" is invalid on RADV/RDNA2. (Wobbly windows do
NOT prove TFP — that's muffin's copy-upload fallback.)

## Hard validation rule

This only matters on real RADV HW (bee). Every phase ends with a **user-run** HW
gate — do NOT claim a phase works without it. The harness for that is already in
place:
```
just yserver-tfp-probe-hw      # yserver alone on :7 + sole-client probe
# reads: tfp-probe.out, yserver-hw-bare.log (both in CWD)
```
Assets: `tools/glx-tfp-probe.c`, recipe `yserver-tfp-probe-hw` (both uncommitted).

## Phase 0 — de-risk the Vulkan half (do this first)

Change `allocate_exportable` (`crates/yserver/src/kms/vk/target.rs:1093`) to
`DRM_FORMAT_MODIFIER_EXT` tiling:
- Build a `VkImageDrmFormatModifierListCreateInfoEXT` from RADV's supported
  modifiers for `B8G8R8A8_UNORM` + the export usage + `DMA_BUF` external handle
  (intersect via `vkGetPhysicalDeviceImageFormatProperties2` with the external +
  `VkPhysicalDeviceImageDrmFormatModifierInfoEXT` in pNext). Reuse the modifier
  enumeration already in `dri3.rs` (`accepted_modifiers`, `can_import_modifier`)
  as a model.
- Read back the chosen modifier: `vkGetImageDrmFormatModifierPropertiesEXT`.
- Layout query: `VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT` (single plane), not
  `COLOR`. Thread the real modifier + plane stride/offset into `ExportableImage`.

**Gate:** user runs `just yserver-tfp-probe-hw` on bee → the
`VK_ERROR_FORMAT_NOT_SUPPORTED` is gone and the image allocates. (TFP still won't
work end-to-end yet — that needs Phase 1 — but this proves the tiling fix.)

## Phase 1 — DRI3 `BuffersFromPixmap` (op 8) export

yserver advertises DRI3 (1,3), so mesa uses op-8 (multi-plane + modifier) for
pixmap export — op-3 (single-fd, no modifier) can't carry a modifier-tiled buffer.
Implement the op-8 export path (un-defer it), reporting the modifier + per-plane
fd/stride/offset. Mirror the existing import-side modifier plumbing
(`GetSupportedModifiers`, `PixmapFromBuffers`). Also fix `dri3.rs::export_dmabuf`
(line ~166-169) to use the `MEMORY_PLANE_0` aspect.

**Gate (user, bee):** full cinnamon — `COGL_DEBUG=winsys` no longer prints "Not
using GLX TFP!"; `glXCreatePixmap` + `BuffersFromPixmap` counts non-zero; and the
plan's Task 5.1 repro (cinnamon-settings pane switch) redraws live.

## Phase 2 — capability-probe correctness

`probe_dmabuf_export_support` (`backend.rs:405`) advertised TFP even though export
was unsupported (RADV's `vkCreateImage` returns success for the bad combo while the
validation layer flags it). Make it authoritative — query
`vkGetPhysicalDeviceImageFormatProperties2` for the real export combo / probe the
modifier path — so yserver only advertises TFP it can deliver. (After Phase 0+1
this should report supported on bee; keep it correct so non-RADV / unsupported HW
falls back cleanly.)

## Notes for the implementer

- Use `superpowers:systematic-debugging` for any "still not working" loop — get
  HW evidence (the probe + `yserver-hw-bare.log`) before theorizing.
- Vulkan/HW tests are `#[ignore]`d and only meaningful on bee; unit-test what you
  can headless, but the real signal is the HW gate.
- Do NOT mark `project_cinnamon_settings_norefresh` resolved — that paint issue is
  separate from TFP (confirmed: TFP off, issue persists).
- Keep `tools/glx-tfp-probe.c` + `just yserver-tfp-probe-hw` as the iteration loop.
