# Phase 4.1.4.2 — `CopyArea` + `CopyPlane` (mini-plan)

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans
> to implement this plan task-by-task.

**Goal:** Port `CopyArea` to a Vulkan-direct path
(`vkCmdCopyImage`) for cross-target draws and same-target
non-overlapping draws. Pixman remains the fallback for cases this
slot doesn't handle (same-target overlap, depth mismatch,
`CopyPlane`). Establishes the **non-render-pass** transfer pattern
(barrier → `cmd_copy_image` → barrier) that 4.1.4.3 (PutImage /
GetImage) reuses.

**Parent plan:**
[`2026-05-08-phase4-1-vulkan-compositor.md`](2026-05-08-phase4-1-vulkan-compositor.md)
§"4.1.4.2 — `CopyArea` + `CopyPlane`".

**Scope (in):**
- `CopyArea`, **different** src/dst drawables: GPU-to-GPU copy
  via `vkCmdCopyImage` with one `VkImageCopy` region per
  GC-clipped sub-rect.
- `CopyArea`, **same** src/dst drawable, **non-overlapping**
  src/dst rect: same path (Vulkan spec allows non-overlapping
  same-image copies).

**Scope (out — pixman fallback for now):**
- `CopyArea`, same src/dst drawable, **overlapping** src/dst
  rect (xterm scrollback / line shift): needs a staging image
  per design §"Same-target overlap". Defer to follow-up commit
  in this slot if real-world testing exercises it; pixman path
  handles it correctly today.
- `CopyArea` between drawables of different format / depth (e.g.
  depth-1 src → depth-24 dst). `vkCmdCopyImage` requires matching
  formats; format conversion needs a shader.
- `CopyPlane` (depth-1 mask + plane bit + fg/bg substitution):
  needs a shader. Plane-mask 4.1.4.8 territory.
- The lavapipe integration test mentioned in the parent plan —
  per the host-pure precedent (2.7) deferred until 4.1.5.

---

## Architecture

### `vkCmdCopyImage` — non-render-pass transfer

Distinct from 4.1.4.1's `cmd_clear_attachments`:
`vkCmdCopyImage` is a **transfer-stage** op outside any render
pass. The pattern:

1. Transition src `SHADER_READ_ONLY_OPTIMAL → TRANSFER_SRC_OPTIMAL`.
2. Transition dst `SHADER_READ_ONLY_OPTIMAL → TRANSFER_DST_OPTIMAL`.
3. `vkCmdCopyImage` with `&[VkImageCopy]` — one region per
   GC-clipped sub-rect, all in one call.
4. Transition src back to `SHADER_READ_ONLY_OPTIMAL`.
5. Transition dst back to `SHADER_READ_ONLY_OPTIMAL`.

For same src/dst (non-overlapping): step 1 and 2 collapse into a
single barrier transitioning the image into a state that allows
both read and write. We track `current_layout` per image; if both
roles need the image at the same time, `GENERAL` is the
permissive layout. (Validation may complain about not-ideal
layouts — `cmd_copy_image` accepts `TRANSFER_SRC` + `TRANSFER_DST`
on the same image when the regions don't overlap; in practice we
emit one barrier `SHADER_READ_ONLY → TRANSFER_DST_OPTIMAL` and
another `SHADER_READ_ONLY → TRANSFER_SRC_OPTIMAL` — the same
image can be in both read and write subresource roles via
separate access masks per the Vulkan spec, but the layout is
single-valued. Use `GENERAL` for the same-image case.)

### Overlap detection

```rust
let overlapping = src_xid == dst_xid && rects_overlap(
    src_x, src_y, dst_x, dst_y, width, height,
);
```

`rects_overlap` is a cheap axis-aligned rect-intersection. When
true, route to the pixman fallback for this slot.

### GC clip

`current_clip_rects_in_dst_space()` already produces the clipped
sub-rect list; one `VkImageCopy` per sub-rect (with the matching
src offset shift) is emitted in one `cmd_copy_image` call.

`ClipState::Pixmap` (depth-1 mask) → pixman fallback; that path
lands in 4.1.4.8.

---

## Tasks

### Task 1: `record_copy_area` recorder

**File:** `crates/yserver/src/kms/vk/ops/copy.rs` (new),
`vk/ops/mod.rs` (add `pub mod copy;`).

Public surface:

```rust
pub fn record_copy_area(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    src: &mut DrawableImage,
    dst: &mut DrawableImage,
    regions: &[vk::ImageCopy],
) -> Result<(), vk::Result>;
```

Records the barriers + `cmd_copy_image` + reverse barriers as
described. Sets `src.current_layout` and `dst.current_layout` to
`SHADER_READ_ONLY_OPTIMAL` on return.

When `src as *const _ == dst as *const _` (same image), uses
`GENERAL` as the transient layout so a single barrier covers
both roles.

Build, no call sites yet.

### Task 2: Wire `copy_area`

**File:** `backend.rs`

**Step 1:** Compute the GC-clipped sub-rect list (existing
`intersect_with_current_clip`).

**Step 2:** Decide whether the Vulkan path applies:

- Both drawables have a `vk_mirror`.
- Backend has Vulkan + ops pool.
- Same-target case: src and dst rects don't overlap (or it's not
  same-target).
- Both mirrors share the same Vulkan format (4.1.4.2 v1: skip if
  formats differ; pixman handles it).

**Step 3:** Build the `VkImageCopy` region list from the clipped
sub-rects (each sub-rect's src offset = original src + dx_shift,
matching the existing pixman code's per-sub-rect src translation).

**Step 4:** Re-borrow `src` and `dst` mirrors mutably from
`self.windows` / `self.pixmaps`. **HashMap-coexistence tax:** when
src_xid == dst_xid we can't grab two `&mut` from the same map at
once. Special-case to a one-mirror call (we already pass `src ==
dst` to the recorder; it switches to the same-image path).

**Step 5:** Run via `run_one_shot_op`.

**Step 6:** Pixman fallback unchanged, runs on every condition
the Vulkan path declines.

### Task 3: `copy_plane`

Stays on pixman in this slot (depth-1 mask + plane bit
substitution needs a shader). No changes; documented in mini-plan
header as "out of scope".

### Task 4: Smoke + commit

User-driven bare-metal smoke under `just yserver-fvwm3-xterm-hw`
(`scanout=vk_composite`). xterm scrollback should still work
(uses the pixman fallback for same-target overlap). Window
decorations / icon copies should now route through Vulkan.

**Commit:** `feat(kms/vk/ops): CopyArea via cmd_copy_image (4.1.4.2)`.

---

## Acceptance criteria

1. Build / fmt / clippy / tests clean.
2. Bare-metal smoke: WM matrix renders cleanly under
   `vk_composite`. No `vk copy:` warnings under normal use.
3. xterm scrollback (same-target overlap) doesn't visibly
   regress — falls back to pixman path which is the pre-4.1.4.2
   behaviour.
4. **Cross-family fix.** A drawable that received a 4.1.4.1
   solid-fill should now be readable correctly via `CopyArea`
   under Vulkan (because `cmd_copy_image` reads the mirror, not
   pixman). The "stale-pixman read" hazard from 4.1.4.1
   shrinks for `CopyArea` consumers.

## Risks

- **Layout-tracking divergence on same-image copy.** The
  `GENERAL` shortcut bypasses optimal layouts; minor perf cost
  acceptable for the rare same-image-non-overlap case.
- **Format mismatch.** A pixmap depth-32 → window depth-24 copy
  (or vice versa) hits the format-equality skip and falls back
  to pixman. Should be invisible to clients but worth verifying
  via rendercheck `dcoords` (when we run the full sweep).
- **Sub-rect count blowup with complex GC clip.** Pathological
  clips with many tiny rects produce many `VkImageCopy` regions
  in one call. Vulkan accepts arbitrary counts; perf cliff is
  unlikely in real WM workloads.
