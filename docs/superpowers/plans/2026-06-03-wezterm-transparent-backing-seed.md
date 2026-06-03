# wezterm/Firefox transparent client content — RESOLVED (2026-06-03)

Status: **FIXED and HW-verified** on the laptops (eiger/air/yoga). A GPU
acquire-semaphore optimization is filed as a follow-up (see
`docs/known-issues.md`).

## Symptom

On the non-AMD laptops, wezterm rendered only partway — large regions of
the terminal were transparent and the desktop showed through, and the
holes shifted on typing/hover. Firefox showed the same see-through
content. Not reproducible on AMD (bee/silence).

## Root cause

GPU clients present via DRI3: they render into a dma-buf pixmap and
`PresentPixmap` it with `wait_fence=0`, relying on **implicit dma-buf
sync** to order the server's read after their render.

yserver's present handler runs the copy **immediately** at request-parse
time (`process_request.rs`; `wait_fence` only gates the *completion*
events, not the copy). So correctness depended entirely on the GPU stack
honouring implicit dma-buf sync for yserver's read queue. amdgpu does;
**Turnip/Adreno and Apple are explicit-sync drivers and don't** — so the
copy raced the client's still-pending render and captured a partly-
rendered frame (α=0 where the GPU hadn't written yet). The scene then
blended those transparent pixels over the desktop.

Confirmed by instrumentation: the present copy is **full and unclipped**
(`copy_area` `req_dst` == the whole 1420×1375, `gc_clip=none`,
`sub_rects` == requested) — the source was simply not ready when read.

The earlier guesses were wrong and abandoned: it is **not** clip-by-children
(`e7a1ba0` — wezterm issues zero RENDER/FillRectangles), **not** a leaked
GC clip, and **not** the backing seed.

## Fix (shipped)

Wait for the source's producer writes before the present copy:

- `DrawableImage::imported_dma_buf_fd()` (target.rs) — borrow the retained
  DRI3 dma-buf fd; `None` for server-owned storage.
- `dri3::wait_dmabuf_read_ready()` — `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`
  (`DMA_BUF_SYNC_READ` → the write fence a reader must wait on) → `poll()`,
  **bounded 50 ms** → on timeout proceed (one stale frame, never a hang).
- `Backend::wait_present_source_ready()` (defaulted no-op) → v2 impl waits
  only for imported sources; server-owned storage is ordered by our own
  queue barriers (also why this never blocks lavapipe/server-owned paths).
- Called in the present handler immediately before the copy.

Validation: `cargo +nightly fmt`, `cargo clippy`, `cargo test` (404 pass);
lavapipe `--ignored` Vk suite unchanged (35 pass / 18 pre-existing baseline
fails, no new failures, no hang). HW-verified on the laptops: wezterm and
Firefox/YouTube render correctly.

## Follow-up (filed, not blocking)

Replace the CPU `poll()` with a GPU acquire-semaphore on the present copy's
submit (no core stall), landed with the composite-into-frame-builder work.
Details + deadlock guards in `docs/known-issues.md`.

## Note on lavapipe

This box (aarch64) has lavapipe (`lvp_icd.aarch64.json`), so the
`#[ignore]`d Vk tests run locally via
`VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.aarch64.json cargo test
-p yserver --lib -- --ignored`. It validates the Vk submit/render paths and
would catch a hang, but it has no real dma-buf producer, so the actual
present-source race only reproduces on hardware.
