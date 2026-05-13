# Phase 3C — rendering re-architecture — upload-backed paint migration

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate `PutImage` and `upload_bgra_to_mirror` off the shared `OpsStaging` host-mapped buffer onto the per-batch `BatchUploadArena` so two upload-backed paint ops can coexist in one `PaintBatch` without aliasing their staging bytes.

**Architecture:** Use the existing `RenderScheduler::record_paint_batch_op` API (lands in 3B T1) — it already exposes `&mut PaintBatch` to the closure, so the closure can call `batch.upload_arena_mut().alloc(size, alignment)` to get a stable `(buffer, offset, mapped_ptr)` triple that lives until batch retirement. No new API surface; pure migration. `OpsStaging` stays on `KmsBackend` for the readback paths (`record_get_image` consumers) which keep their existing `flush_if_needed(Readback) + run_one_shot_op` shape.

**Tech Stack:** Rust, ash (Vulkan), the existing 3A infrastructure (`BatchUploadArena`, `record_paint_batch_op`, `paint_resources()`, `renderer_failed` gate, drawable-destruction barriers from 3B salvage).

---

## Phase context

Read `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3b-results.md` first. Key invariants from 3B that 3C inherits:

1. **Drop-order**: `KmsBackend.scheduler` is declared BEFORE `KmsBackend.ops_command_pool`. Don't touch field order. (`feedback_kmsbackend_drop_order` memory.)
2. **Drawable-destruction barriers**: `DestroyWindow`, `configure_window` resize, `FreePixmap`, `RenderFreePicture`, `RenderCreateCursor` rescued path already flush the batch before dropping `VkImage`s.
   - **PutImage** (T1): targets are windows and pixmaps; `DestroyWindow` + `configure_window` resize + `FreePixmap` barriers cover these. ✓
   - **Mirror upload** (T2, called by `create_cursor` / `create_glyph_cursor` / `install_default_cursor`): cursor mirrors are stored in `self.cursors`, NOT in `self.windows` / `self.pixmaps`. There is **no FreeCursor handler today** in this codebase, so the in-flight batch can never observe a cursor mirror dropping mid-frame. This is safe today, but **if a FreeCursor handler is ever added**, it MUST `flush_if_needed(ProtocolBarrier)` before dropping `CursorState.vk_mirror` — same pattern as 3B's other destruction sites. (Codex review of this plan flagged the original "cursor mirrors get destroyed via render_create_cursor's rescue path" wording as incorrect; corrected here.) (`feedback_paintbatch_destruction_barrier` memory.)
3. **`renderer_failed` gate**: `paint_resources()` returns `None` when latched. Every migrated recorder must go through it.
4. **`record_paint_batch_op` is the load-bearing API** for recorders that need batch resources. `record_paint_op` (shim) is only for recorders that ignore the batch handle (3B fill/copy).

## Out of scope (deferred to 3D or follow-on)

The 3B results doc mentioned `MaskScratch::upload_r8`, glyph atlas, and gradient upload as 3C candidates. They're **not** in this plan:

- **MaskScratch + glyph atlas**: their consumers are `text::record_text_run` and `render::record_render_composite`, neither of which migrates until 3D. Migrating just the mask/glyph upload code without migrating its consumers gives us a recorder that records into the batch CB followed by a legacy paint op (text/render run_one_shot_op) that reads from the mask scratch — the latter would have to flush the former, defeating the point. Defer to 3D where mask + consumer migrate together.
- **Gradient upload** (`GradientPicture::new_linear` / `new_radial`): runs at `RenderCreate*Gradient` time, not per-frame. Currently uses its own one-shot with internal wait. The freshly-created gradient has a brand-new XID so no in-flight batch can reference it — this is **conservative cleanup, not load-bearing UAF protection**. T3 adds a `flush_if_needed(ProtocolBarrier)` at create-time as a hygiene boundary between batched paint and the one-shot upload. Could be dropped if it ever shows up as overhead; current frequency (handful per session) makes it free.

3C scope is exactly: `try_vk_put_image` + `upload_bgra_to_mirror` migrations + gradient barrier + cleanup.

## File structure

| File | Role | Touched in |
|---|---|---|
| `crates/yserver/src/kms/backend.rs` | Migrate `try_vk_put_image` (~line 3781) and `upload_bgra_to_mirror` (~line 2603); update the run_legacy_paint_op audit catalogue (~line 1697); add `flush_if_needed(ProtocolBarrier)` to `render_create_gradient` | T1, T2, T3 |
| `crates/yserver/src/kms/vk/ops/image.rs` | `record_put_image` signature unchanged — the recorder is already arena-friendly (takes `staging: vk::Buffer` + `regions: &[BufferImageCopy]`) | (read only) |
| `crates/yserver/src/kms/vk/target.rs` | `DrawableImage::record_upload_rect` signature unchanged — already takes `staging_offset_bytes` | (read only) |
| `crates/yserver/src/kms/scheduler/batch_upload_arena.rs` | No changes; we use the existing `BatchUploadArena::alloc(size, alignment)` | (read only) |
| `crates/yserver/src/kms/scheduler/mod.rs` | No changes; `record_paint_batch_op` is already the load-bearing API | (read only) |
| `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3c-results.md` | Results doc | T4 |

## Pre-task notes (lessons from 3A/3B — read before starting)

1. **`#[derive(Debug)]` on types holding `Arc<VkContext>` does not compile.** Not relevant in 3C since we add no new types, but be aware in case a refactor tempts you.
2. **`unsafe impl Send`** anywhere needs a `// SAFETY:` comment block citing the single-threaded-core invariant (phase 6.8). Not adding new `Send` impls in 3C.
3. **Borrow split via field paths**: `self.scheduler.record_paint_batch_op(...)` and `self.windows.get_mut(...)` can be live simultaneously. The pattern from 3B's `try_vk_solid_fill` (backend.rs:3665) is the template — re-borrow the mirror via `self.windows.get_mut()` / `self.pixmaps.get_mut()` BEFORE calling `record_paint_batch_op`, and capture it in the closure.
4. **`paint_resources()` MUST be called first** so the closure's recording is gated by `renderer_failed`. Do not call `self.scheduler.record_paint_batch_op` directly with raw `self.vk.clone()`.
5. **The `vk` parameter inside the closure shadows the `ash::vk` module** unless you name it differently. The 3B convention is `vk_arc` for the outer Arc and `vk` (or `vkdev`) for the closure parameter — keep `vk` inside the closure since `ash::vk::*` is only used at call-site scope, not inside recorder closures. If you find you need `ash::vk::Foo` inside the closure, use the full path `ash::vk::Foo` to dodge the shadow.
6. **`upload_arena_mut()` lazy-initialises the arena** on first call within a batch. Subsequent calls return the same arena, so two `alloc(...)` calls in one closure give disjoint sub-allocations of the same chunk (or a fresh chunk if the first didn't fit).
7. **Alignment**: `vkCmdCopyBufferToImage`'s `bufferOffset` must be a multiple of the texel block size (1 byte for `R8`, 4 bytes for `B8G8R8A8_UNORM`). Pass `alignment = 16` to `BatchUploadArena::alloc` — comfortably bigger than any texel-block requirement and matches typical SIMD alignment.
8. **Damage marking**: `try_vk_put_image` today does NOT mark damage post-write because the mirror is GPU-canonical. Keep that — `MirrorUploader` is gone in 4.1.5 so this is moot, but keep the existing comment intact.

---

## Task 1: Migrate `try_vk_put_image` to `record_paint_batch_op`

**Goal:** Replace the `OpsStaging::ensure + run_one_shot_op` shape with `record_paint_batch_op` + `batch.upload_arena_mut().alloc()`. Two PutImages in one batch now get disjoint staging sub-allocations.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (`try_vk_put_image`, lines ~3781–4080)

### Step 1: Read the existing `try_vk_put_image` end-to-end

- [ ] **Step 1: Open `backend.rs` and read lines 3781–4080**

Note the structure:
1. Lines 3781–3866: parameter unpack, depth → bpp mapping, clip intersection, mirror extent snapshot, source row-stride table. **No Vulkan work; keeps as-is.**
2. Lines 3868–3916: per-sub-rect plan build (`PutPlan` struct, `staging_offset` accumulates within the upload, `total_bytes` is the upload size). **Keeps as-is.**
3. Lines 3921–3932: `ops_staging.ensure(total_bytes)`. **Removed in this task.**
4. Lines 3942–4007: host→staging memcpy with byte permutation. **Moves into the closure (it needs `alloc.mapped_ptr`).**
5. Lines 4010–4033: `BufferImageCopy::buffer_offset(p.staging_offset)`. **`buffer_offset` becomes `alloc.offset + p.staging_offset`. Moves into the closure.**
6. Lines 4035–4044: inline `flush_if_needed(ProtocolBarrier)` borrow-conflict fallback. **REMOVED** — `record_paint_batch_op` appends to the open batch instead of needing a pre-flush.
7. Lines 4047–4057: re-borrow mirror, take staging buffer. **Mirror re-borrow stays; staging buffer is replaced by `alloc.buffer`.**
8. Lines 4059–4079: `run_one_shot_op(...) { record_put_image(...) }`. **Replaced by `self.scheduler.record_paint_batch_op(...)` with closure body that builds regions + memcpys + calls `record_put_image`.**

- [ ] **Step 2: Verify there is no test covering `try_vk_put_image` end-to-end**

Run: `grep -n "try_vk_put_image\|put_image_test\|test_put_image" crates/yserver/src/kms/backend.rs`
Expected: zero hits (this function has no unit test; coverage is via xts5 + hardware smoke).

If a test exists, read it and ensure the migration preserves its behavior (likely a state-poke test that doesn't actually run Vulkan).

### Step 2: Rewrite the function

- [ ] **Step 3: Apply the migration**

Replace the body from the `if self.ops_staging.is_none()` check at line ~3813 through the end of the function (line 4080) with the structure below. Keep lines 3781–3812 (parameter unpack and early returns) and lines 3816–3920 (clip + mirror snapshot + plans build) UNCHANGED. Replace from line 3921 onward.

Concretely, the **new tail of `try_vk_put_image`** (from after `if plans.is_empty() { return true; }`) is:

```rust
        // Acquire batch resources (gated by renderer_failed).
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            return false;
        };

        // Re-borrow the mirror mutably for the recording. The borrow
        // is held across `self.scheduler.record_paint_batch_op` — that
        // mutates `self.scheduler` only, disjoint from
        // `self.windows`/`self.pixmaps`.
        let mirror = if let Some(w) = self.windows.get_mut(&host_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&host_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(mirror) = mirror else {
            return false;
        };

        // Snapshot the rendezvous values the closure needs. `plans`
        // and `total_bytes` come from the planning pass above.
        let result = self
            .scheduler
            .record_paint_batch_op(vk_arc, pool_handle, |vk, batch, cb| {
                // Per-batch staging allocation (replaces OpsStaging).
                let alloc = match batch.upload_arena_mut().alloc(total_bytes, 16) {
                    Ok(a) => a,
                    Err(e) => {
                        log::warn!(
                            "vk put_image: arena alloc {total_bytes} bytes failed: {e:?}"
                        );
                        return Err(ash::vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
                    }
                };

                // Host → staging memcpy with depth-specific byte
                // permutation. Bytes go to `alloc.mapped_ptr +
                // plan.staging_offset`. The permutation is identical
                // to the pre-migration code; only the destination
                // pointer base changes.
                let staging_base = alloc.mapped_ptr.as_ptr();
                for plan in &plans {
                    let row_dst_bytes = plan.extent_w as usize * src_bpp;
                    for row in 0..plan.extent_h {
                        let host_row = (plan.src_y + row) as usize;
                        let src_row_byte_start = host_row * src_row_stride;
                        if src_row_byte_start + src_row_stride > data.len() {
                            // Truncated source — zero-fill the staging row.
                            unsafe {
                                let dst = staging_base.add(
                                    plan.staging_offset as usize + row as usize * row_dst_bytes,
                                );
                                std::ptr::write_bytes(dst, 0, row_dst_bytes);
                            }
                            continue;
                        }
                        unsafe {
                            let dst_row = staging_base.add(
                                plan.staging_offset as usize + row as usize * row_dst_bytes,
                            );
                            let src_row = data.as_ptr().add(src_row_byte_start);
                            match depth {
                                1 => {
                                    for col in 0..plan.extent_w as usize {
                                        let bit_index = plan.src_x as usize + col;
                                        let byte = *src_row.add(bit_index >> 3);
                                        let bit = (byte >> (bit_index & 7)) & 1;
                                        *dst_row.add(col) = if bit != 0 { 0xFF } else { 0x00 };
                                    }
                                }
                                8 => {
                                    let src = src_row.add(plan.src_x as usize);
                                    std::ptr::copy_nonoverlapping(src, dst_row, row_dst_bytes);
                                }
                                24 | 32 => {
                                    let src = src_row.add(plan.src_x as usize * 4);
                                    for col in 0..plan.extent_w as usize {
                                        let s = src.add(col * 4);
                                        let d = dst_row.add(col * 4);
                                        let r = *s;
                                        let g = *s.add(1);
                                        let b = *s.add(2);
                                        let a = if depth == 32 { *s.add(3) } else { 0xFFu8 };
                                        *d = b;
                                        *d.add(1) = g;
                                        *d.add(2) = r;
                                        *d.add(3) = a;
                                    }
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                }

                // Build BufferImageCopy regions with the alloc-relative
                // offset (alloc.offset is the chunk-local offset, the
                // per-plan staging_offset is the within-upload offset).
                let regions: Vec<ash::vk::BufferImageCopy> = plans
                    .iter()
                    .map(|p| {
                        ash::vk::BufferImageCopy::default()
                            .buffer_offset(alloc.offset + p.staging_offset)
                            .buffer_row_length(0)
                            .buffer_image_height(0)
                            .image_subresource(
                                ash::vk::ImageSubresourceLayers::default()
                                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                                    .layer_count(1),
                            )
                            .image_offset(ash::vk::Offset3D {
                                x: p.image_x,
                                y: p.image_y,
                                z: 0,
                            })
                            .image_extent(ash::vk::Extent3D {
                                width: p.extent_w,
                                height: p.extent_h,
                                depth: 1,
                            })
                    })
                    .collect();

                crate::kms::vk::ops::image::record_put_image(
                    vk,
                    cb,
                    mirror,
                    alloc.buffer,
                    &regions,
                )
            });

        match result {
            Ok(()) => {
                // The Vk-direct write made the mirror current; we do
                // NOT mark damage here. Same reasoning as before
                // migration: damage would re-upload pixman bytes over
                // the GPU-canonical mirror.
                true
            }
            Err(e) => {
                log::warn!(
                    "vk put_image: record failed on xid {host_xid:#x}: {e:?} — \
                     falling back to pixman"
                );
                false
            }
        }
```

Also delete:
- The `use crate::kms::vk::ops::{image as vk_image, run_one_shot_op};` line at the top of the function (line 3791).
- The `if self.ops_staging.is_none() { return false; }` check at line 3813 (the arena is the new staging path; `ops_staging` is irrelevant here).
- Lines 3807–3812 (`vk_arc` and `pool_handle` are now acquired via `paint_resources()` after the planning pass).

- [ ] **Step 4: Build and verify the migration compiles**

Run: `cargo check -p yserver`
Expected: clean build, no errors.

If you see "cannot borrow `self.scheduler` as mutable because `self` is also borrowed mutably" — you forgot to switch to field-path borrows. The fix is to call `paint_resources()` and re-borrow the mirror BEFORE the `record_paint_batch_op` call; the closure captures the mirror by move (move semantics of `&mut T`).

If you see "cannot use `ash::vk::Foo` because the local `vk` shadows the module" — use the full path `ash::vk::Foo` inside the closure body (the existing code at the call site does this already; just preserve the `ash::vk::*` prefixes everywhere inside the closure).

- [ ] **Step 5: Run the existing yserver tests**

Run: `cargo test -p yserver --lib`
Expected: 133 passed, 0 failed, 3 ignored (same as 3B baseline).

- [ ] **Step 6: Run fmt + clippy**

Run: `cargo +nightly fmt --check`
Expected: no diff.

Run: `cargo clippy -p yserver 2>&1 | tail -20`
Expected: 5 pre-existing `doc_lazy_continuation` warnings. No new warnings.

- [ ] **Step 7: Commit T1**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "$(cat <<'EOF'
refactor(kms): migrate try_vk_put_image to record_paint_batch_op + arena

Replaces the per-PutImage `OpsStaging::ensure + run_one_shot_op` shape
with `record_paint_batch_op` + `batch.upload_arena_mut().alloc(...)`.
Two PutImages in one PaintBatch now get disjoint staging sub-
allocations from the per-batch arena and submit together at composite
flush time — no aliasing, no per-PutImage queue_wait_idle.

OpsStaging stays for the readback paths (`record_get_image`
consumers). The pre-record `flush_if_needed(ProtocolBarrier)`
borrow-conflict fallback is removed; it's no longer needed since the
recorder appends to the open batch instead of requiring a fresh
one-shot CB.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Migrate `upload_bgra_to_mirror` to `record_paint_batch_op`

**Goal:** Same shape change as T1, applied to the BGRA blob → mirror upload helper used by `create_cursor` and `create_glyph_cursor`. Eliminates aliasing if two cursor mirrors are created in the same batch (low frequency, but the fix is cheap).

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (`upload_bgra_to_mirror`, lines ~2603–2640)

### Step 1: Read the existing helper

- [ ] **Step 1: Open `backend.rs` and read lines 2603–2640**

Existing shape:
1. Borrows `ops_staging`, ensures size, takes mapped_ptr + buffer.
2. memcpys `pixels` into staging.
3. Calls `self.run_legacy_paint_op(|_vk, cb| mirror.record_upload_rect(cb, staging_buffer, 0, ...))`.

The migration shape:
1. Acquire `(vk_arc, pool_handle)` via `paint_resources()`.
2. Call `self.scheduler.record_paint_batch_op(vk_arc, pool_handle, |vk, batch, cb| { alloc + memcpy + record_upload_rect })`.
3. Return the Result via the closure.

### Step 2: Rewrite the function

- [ ] **Step 2: Replace the function body**

Replace lines 2607–2639 (the body, keeping the signature) with:

```rust
        let needed = pixels.len() as u64;
        if needed == 0 {
            return Ok(());
        }

        let (vk_arc, pool_handle) = self
            .paint_resources()
            .ok_or(ash::vk::Result::ERROR_INITIALIZATION_FAILED)?;

        let extent = mirror.extent;
        let pixels_ptr = pixels.as_ptr();
        let pixels_len = pixels.len();

        self.scheduler
            .record_paint_batch_op(vk_arc, pool_handle, |_vk, batch, cb| {
                let alloc = batch.upload_arena_mut().alloc(needed, 16).map_err(|e| {
                    log::warn!(
                        "vk upload_bgra_to_mirror: arena alloc {needed} bytes failed: {e:?}"
                    );
                    ash::vk::Result::ERROR_OUT_OF_DEVICE_MEMORY
                })?;
                // SAFETY: `alloc.mapped_ptr` is a HOST_VISIBLE |
                // HOST_COHERENT mapped pointer at `alloc.buffer +
                // alloc.offset` covering `needed` bytes;
                // `pixels_ptr` is valid for `pixels_len` bytes and
                // we checked `pixels_len == needed`.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pixels_ptr,
                        alloc.mapped_ptr.as_ptr(),
                        pixels_len,
                    );
                }
                mirror.record_upload_rect(
                    cb,
                    alloc.buffer,
                    alloc.offset,
                    ash::vk::Rect2D {
                        offset: ash::vk::Offset2D { x: 0, y: 0 },
                        extent,
                    },
                );
                Ok(())
            })
    }
```

The `&[u8] pixels` argument is captured by extracting `pixels_ptr` + `pixels_len` before the closure (so the closure doesn't capture `pixels` itself — which would require lifetime juggling). This is the same pattern as 3B's `try_vk_solid_fill` snapshotting scalar values.

- [ ] **Step 3: Build**

Run: `cargo check -p yserver`
Expected: clean build.

- [ ] **Step 4: Run tests**

Run: `cargo test -p yserver --lib`
Expected: 133 passed.

- [ ] **Step 5: fmt + clippy**

Run: `cargo +nightly fmt --check && cargo clippy -p yserver 2>&1 | tail -5`
Expected: no diff; same 5 pre-existing warnings.

- [ ] **Step 6: Commit T2**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "$(cat <<'EOF'
refactor(kms): migrate upload_bgra_to_mirror to record_paint_batch_op + arena

Cursor mirror creation (create_cursor, create_glyph_cursor) now
appends a buffer→image upload into the open PaintBatch via the per-
batch upload arena instead of a self-contained `run_legacy_paint_op`
+ `OpsStaging::ensure`. Two cursor creations in one batch get
disjoint staging sub-allocations.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Update audit catalogue + add gradient-create protocol barrier

**Goal:** Two cleanup items that are too small to be their own tasks but matter for correctness + future grep-ability.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (run_legacy_paint_op audit catalogue ~line 1697; `render_create_linear_gradient` and `render_create_radial_gradient` — find their `GradientPicture::new_*` call sites. There is no conic-gradient handler in this codebase.)

### Step 1: Update the audit catalogue

- [ ] **Step 1: Find the catalogue**

Run: `grep -n "Phase-3B T0 catalogue\|run_legacy_paint_op catalogue" crates/yserver/src/kms/backend.rs`
Expected: the doc comment block in `run_legacy_paint_op` (~line 1697).

- [ ] **Step 2: Update the catalogue entries**

In the doc comment, change:

```
///   upload_bgra_to_mirror:            mirror.record_upload_rect          — wrapped
```

to:

```
///   upload_bgra_to_mirror:            mirror.record_upload_rect          — migrated 3C (record_paint_batch_op + arena)
```

And change:

```
///   try_vk_put_image:                 image::record_put_image             — borrow-conflict fallback
```

to:

```
///   try_vk_put_image:                 image::record_put_image             — migrated 3C (record_paint_batch_op + arena)
```

### Step 2: Add prophylactic `flush_if_needed(ProtocolBarrier)` to gradient creation

- [ ] **Step 3: Find the gradient-create handlers**

Run: `grep -n "render_create_linear_gradient\|render_create_radial_gradient\|GradientPicture::new_linear\|GradientPicture::new_radial" crates/yserver/src/kms/backend.rs`

Expected: the two Render extension entry points (`render_create_linear_gradient` ~line 10997, `render_create_radial_gradient` ~line 11058) and their `GradientPicture::new_linear` / `new_radial` call sites.

**Note from codex review:** the existing `render_free_picture` barrier at backend.rs:10600 already covers `PictureState::Gradient` destruction — the comment there mentions "rescued mirrors" but the barrier applies to any picture state. So T3 does NOT need a new destruction barrier. The create-time flush we're adding is a **protocol boundary** (cheap, low-frequency), not the primary UAF protection.

- [ ] **Step 4: For each gradient-create entry point, add a flush before `GradientPicture::new_*`**

Both handlers return `io::Result<Option<PictureHandle>>`. The signatures and surrounding context look like:

```rust
fn render_create_linear_gradient(
    &mut self,
    _origin: Option<OriginContext>,
    body: &[u8],
) -> io::Result<Option<PictureHandle>> {
    // ... body parsing, stops, etc.
    let Some(vkctx) = self.vk.as_ref().cloned() else { return Ok(None); };
    let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
        return Ok(None);
    };
    // ADD THE FLUSH HERE — right before GradientPicture::new_*
    let gradient = match GradientPicture::new_linear(vkctx, pool_handle, ...) { ... };
    // ...
}
```

Insert at each site, just before the `GradientPicture::new_linear` / `new_radial` call:

```rust
// Conservative protocol boundary: GradientPicture::new_* runs its
// own one-shot upload CB outside the PaintBatch. The new gradient
// has a fresh XID, so no in-flight batch can race it — this flush
// is hygiene cleanup between the batched paint pipeline and the
// gradient one-shot, not a UAF fix. Cheap because gradient creates
// are low-frequency. On flush Err return Ok(None), matching the
// handler's existing "vk init failed" fallback shape.
if let Err(e) = self.flush_if_needed(
    crate::kms::scheduler::paint_batch::BatchFlushReason::ProtocolBarrier,
) {
    log::warn!("render_create_<kind>_gradient: pre-build flush failed ({e:?})");
    return Ok(None);
}
```

Use the connector name (`linear`/`radial`) in the log line, not `<kind>`. Return `Ok(None)` — that matches both handlers' existing failure paths (`vk init failed → Ok(None)`). Returning `Err(io::Error)` would change the protocol-error code emitted to the client.

- [ ] **Step 5: Build**

Run: `cargo check -p yserver`
Expected: clean.

- [ ] **Step 6: Tests + fmt + clippy**

Run: `cargo test -p yserver --lib && cargo +nightly fmt --check && cargo clippy -p yserver 2>&1 | tail -5`
Expected: 133 passed; no fmt diff; same 5 pre-existing warnings.

- [ ] **Step 7: Commit T3**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "$(cat <<'EOF'
chore(kms): protocol-barrier flush at gradient create; update 3C audit

GradientPicture::new_linear / new_radial run their own one-shot
upload CB outside the PaintBatch. Flush any open batch first so
the two pipelines don't interleave. This is a cheap protocol
boundary (gradient creates are low-frequency, handful per session);
the actual lifetime protection for gradient images on destruction
is already provided by render_free_picture's existing barrier.

Also updates the run_legacy_paint_op audit catalogue (~line 1697)
to reflect 3C migration of try_vk_put_image and upload_bgra_to_mirror.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Validation + results doc

**Goal:** End-to-end verification under hardware and a results doc following the 3A/3B template.

**Files:**
- Create: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3c-results.md`

### Step 1: Static verification

- [ ] **Step 1: Cutover greps (semantic, not numeric)**

Run: `rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs`
Expected: zero hits inside `try_vk_put_image` or `upload_bgra_to_mirror`. Remaining hits should be: `run_legacy_paint_op` body, the 3 readback handlers (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`), the 3D-deferred borrow-conflict fallbacks (text / render / traps / copy-same-overlap), `open_with_commit` (constructor), and `dump_scanout_one` (diagnostic).

Run: `rg -n 'ops_staging' crates/yserver/src/kms/backend.rs`
Expected: zero hits inside `try_vk_put_image` or `upload_bgra_to_mirror`. Remaining hits are the struct field, the initializer, the three readback handlers, and any hw_cursor_refresh/readback path. (The exact count drifts with comments — don't gate on a number; gate on **where** it appears.)

Run: `rg -n 'record_paint_batch_op' crates/yserver/src/kms/backend.rs`
Expected: at least 2 call sites — one in `try_vk_put_image`, one in `upload_bgra_to_mirror`. 3B used `record_paint_op` (the shim) throughout, so any usage of the wide API is from 3C.

- [ ] **Step 2: Tree green**

Run: `cargo +nightly fmt --check && cargo clippy -p yserver 2>&1 | tail -10 && cargo test --workspace 2>&1 | tail -10`

Expected:
- fmt: no diff
- clippy: 5 pre-existing warnings
- tests: all green (yserver lib 133 passed + workspace passes as in 3B)

### Step 2: Hardware smoke

- [ ] **Step 3: Build release and run MATE**

Per `feedback_codex` use the Skill for review/spec but hardware smoke is just running the binary:

Run: `just yserver-mate-hw-release` (or local equivalent — check the justfile).

Verify in `yserver-hw.log` and the kernel journal:
- No `paint batch submit failed` / `poison` / `renderer_failed` hits.
- No `arena alloc * bytes failed` warns under normal load (the arena should never OOM for typical workloads; if it does, the workload exposed an arena-sizing bug).
- No GPU driver crashes / VM_CONTEXT faults / kernel oops.
- `vk composite: deferred frames in last 5s: ...` info-line summaries continue working (3B-stabilization preserved).

- [ ] **Step 4: xts5 + rendercheck (deferred — user choice)**

The user may run these at leisure:

```bash
just xts-yserver
just rendercheck-yserver
```

Failure to run xts in 3C is acceptable; the migration is correctness-preserving by construction (the same `record_put_image` / `record_upload_rect` recorders execute, just inside the batch CB instead of a one-shot). xts gates 3D more than 3C.

### Step 3: Write results doc

- [ ] **Step 5: Create the results doc**

Path: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3c-results.md`

Follow the 3A/3B template. Sections:

1. **Scope landed**: one paragraph + bullets summarizing T1/T2/T3 changes.
2. **Preflight checks**: fmt, clippy, test counts.
3. **Cutover greps**: actual numbers from `rg` (don't guess; run the commands).
4. **Done conditions** (matching the section below).
5. **Hardware smoke**: result summary.
6. **Plan bugs caught (folded back into plan)**: any recipe-level issues hit during execution.
7. **Commit summary**: T1, T2, T3 SHAs and one-line subjects.
8. **Known deferred items**: explicit list of what 3D and 3E now own (MaskScratch, glyph atlas, text, render-composite, traps, copy_same_overlap).
9. **What's next**: pointer to 3D planning.

- [ ] **Step 6: Commit T4 + results**

```bash
git add docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3c-results.md
git commit -m "$(cat <<'EOF'
docs(plans): phase-3C validation results

T1 + T2 migrated try_vk_put_image and upload_bgra_to_mirror to the
per-batch BatchUploadArena via record_paint_batch_op. T3 added a
prophylactic flush at gradient-create. OpsStaging retained for
readback paths (3B+ unchanged).

Hardware smoke on <host>: <result>. xts/rendercheck deferred to user.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Done conditions

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` produces 5 pre-existing `doc_lazy_continuation` warnings; no new warnings.
3. `cargo test --workspace` green.
4. `try_vk_put_image` recorder path uses `record_paint_batch_op` + `batch.upload_arena_mut()`; the inline `flush_if_needed(ProtocolBarrier)` fallback at the top of the recorder is gone.
5. `upload_bgra_to_mirror` similarly migrated.
6. `OpsStaging` is still used by the three readback handlers (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`) and nowhere else in paint-side code.
7. Each of the gradient-create entry points calls `flush_if_needed(ProtocolBarrier)` before `gradient::build`.
8. The `run_legacy_paint_op` audit catalogue (~backend.rs:1697) reflects the T1/T2 migrations.
9. Hardware smoke green on the user's host; no `paint batch submit failed`, no `arena alloc * failed` under load, no GPU faults.

## Cutover greps (post-3C — semantic, not numeric)

```
$ rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
# Expected SITES (not a count): run_legacy_paint_op body, 3 readback handlers,
# 3D-deferred borrow-conflict fallbacks (text/render/traps/copy-same-overlap),
# open_with_commit, dump_scanout_one. ZERO hits inside try_vk_put_image
# or upload_bgra_to_mirror.

$ rg -n 'ops_staging' crates/yserver/src/kms/backend.rs
# Expected SITES: struct field, initializer, three readback handlers.
# ZERO hits inside try_vk_put_image or upload_bgra_to_mirror.

$ rg -n 'record_paint_batch_op' crates/yserver/src/kms/backend.rs
# Expected: at least 2 call sites — one each in try_vk_put_image and
# upload_bgra_to_mirror.
```

## Out-of-scope deferred to 3D

- `text::record_text_run` (2 sites: `try_vk_text_run`, `try_vk_render_composite_glyphs`) — descriptor-heavy + uses glyph atlas
- `render::record_render_composite` (2 sites: `try_vk_render_traps`, `try_vk_render_composite`) — descriptor-heavy + uses MaskScratch
- `copy::record_copy_area_same_overlap` (`try_vk_copy_area` same-overlap arm) — uses CopyScratch
- `MaskScratch::upload_r8` migration — co-moves with text/render in 3D
- Glyph atlas incremental upload via batch CB — co-moves with text in 3D
- `record_get_image` — phase 5 (targeted VkFence per HLD)

## Notes for the implementer

- **The borrow split pattern is the only structural risk.** If `cargo check` complains about `&mut self.scheduler` conflicting with `&mut self.windows`, you forgot to use direct field paths. See `try_vk_solid_fill` (backend.rs:3665) for the template.
- **The arena auto-grows on alloc**, so there is no `ensure(N)` analog. If a PutImage is bigger than the active chunk's remaining space, the next `alloc` allocates a fresh chunk sized `max(2x previous, requested)` up to 64 MiB. No code change needed in the recorder.
- **`alloc(0, _)` returns `Err(ERROR_VALIDATION_FAILED_EXT)`**. The recorders already short-circuit on empty plans/zero size before the alloc call — preserve that.
- **`alloc.mapped_ptr` is `NonNull<u8>`** — call `.as_ptr()` to get the raw `*mut u8`. Pre-3C code used `staging_ptr: *mut u8` directly via `OpsStaging::mapped_ptr()`; pattern is unchanged below the unsafe block.
- **No new `unsafe impl Send`** needed; both `UploadAllocation` and `Chunk` already have them with SAFETY comments.
- **Test coverage**: the migrations are correctness-preserving by construction. If you feel a test would help, add a unit test that two `batch.upload_arena_mut().alloc(N, 16)` calls return non-aliasing offsets — but the arena's own unit tests already cover this. Skip unless something surprises you.
