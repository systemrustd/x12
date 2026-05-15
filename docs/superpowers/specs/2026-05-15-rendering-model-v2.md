# Rendering model v2 — high-level design

Date: 2026-05-15
Status: draft, for codex + author review
Author: jos + claude session
Supersedes (in motivation, not in code): `2026-05-12-rendering-rearchitecture-hld.md`
on the question of what gets composited and where.

## Why this exists

The 2026-05-12 rendering re-architecture HLD scoped its goal narrowly to
sync and lifetime: replace `vkQueueWaitIdle`-driven scheduling with an
explicit in-flight frame model. That work delivered (Phases 3-5, pixmap
pool, GPU traps). Inside its scope it was correct, perf-positive on
fuji, and rendercheck-clean.

What that HLD did not touch — because it wasn't the question it was
asked — is **what yserver's screen pixmap actually is, and where client
paint lands**. The choice was inherited: every InputOutput window has
its own `vk_mirror`, the scanout pass walks per-window mirrors in
stacking order, root's mirror exists but is never displayed,
`bg_pixmap` is the only thing drawn at the root layer.

That model fits the non-composited case cleanly. WMs that decorate
their own frame windows (fvwm3, e16, wmaker, marco-no-compositor, xfwm4
without compositing) draw into their own per-window mirrors and the
scanout walk shows them at the right z-order. Most of yserver's
ecosystem testing has lived here.

The model breaks at the boundary with **compositing WMs**:

- `RedirectSubwindows(root, Manual)` puts every top-level under
  compositor control. The compositor is supposed to draw the resulting
  scene to the root window (or the Composite Overlay Window).
- yserver's scanout pass never displays root's mirror or any COW
  contents. The compositor's RENDER output goes into a buffer that
  scanout never reads. Visible result: blank where the compositor
  expected to paint.
- The 2026-05-15 attempt to bridge this (the abandoned
  `render-convolution-filter` branch, T1-T4 of the Manual-redirect
  plan) fixed the obvious symptom (NameWindowPixmap → BadAlloc) but
  exposed the structural mismatch underneath. Panel chrome went
  invisible on xfce because per-window mirrors were correctly skipped
  from scanout, and the compositor's replacement paint had no path
  to the screen.

There is a parallel, equally load-bearing problem in **GPU lifetime
safety**. Repeated bee/RDNA2 GPU faults under load (most recently
during the timeline-semaphore migration that was paused on
`pre-timeline-rework`) indicate the current rendering layer is not
just semantically wrong for compositors — it's also unsafe around
resource ownership and retirement. RADV's TCP / CB/DB hardware catches
freed-VA reads/writes that ANV swallows silently; fuji "working" may
also have latent UAFs.

The v2 model addresses both axes together. Patching either in
isolation would mean retrofitting onto assumptions the other axis
needs to change.

## Load-bearing outcomes

1. **Compositing WMs work.** xfwm4 with compositing enabled, picom's
   xrender backend, xcompmgr, compton render their visible output
   correctly. xfce drop-shadows are an alpha gradient, not opaque bars.
2. **No GPU faults on RDNA2 under reproducer workloads.** bee +
   mate-cc + adapta-nokto and bee + window-drag do not produce
   `amdgpu PERMISSION_FAULTS` over a 30-minute session.
3. **The non-composited path stays working.** fvwm3, e16, wmaker,
   marco, mate-without-compositor, xfce without compositing all
   continue to render correctly.
4. **rendercheck passes at least at parity with current baseline.**

Out of scope (handled later, or never):

- Direct scanout (zero-copy from a client buffer to the scanout BO).
  v2 disables this entirely. Composed-path-only.
- Performance equal to current model out of the gate. Correctness
  first. Perf parity is verified after the model lands, regressions
  budget-bound and addressed in follow-ups.
- **Reintroducing pixman.** v1 deliberately removed pixman; v2 does
  not bring it back. The "CPU baseline first" instinct is real but
  the complexity that bit v1 was sync, not Vk operations. v2's
  baseline is minimal-Vk (fill / copy / blit only) under strict I6,
  which avoids both a temporary pixman dependency and the CPU/GPU
  readback trap that mixed paint+composite stages would create.
- ynest backend changes. ynest forwards paint to a host X server and
  inherits host compositing; it doesn't go through the v2 model.
- New threads or locks on the hot path. Single-threaded core invariant
  holds.

## Invariants

The model is defined by seven invariants. Each component owns
enforcement of at least one. Violating any of them is the bug.

| # | Invariant | Owner |
|---|-----------|-------|
| I1 | A drawable is **storage** plus metadata. Not "a window that gets walked by scanout" or "a pixmap forwarded to the host." | `DrawableStore` |
| I2 | Client drawing mutates **only drawable storage**. Paint paths do not write to scanout BOs, do not bypass storage, do not have a "live window" fast path. | `RenderEngine` |
| I3 | Window visibility is produced **only by a compositor pass**. Scanout is the output of a scene-graph composition over drawable storage, never a direct view onto window mirrors. | `SceneCompositor` |
| I4 | COMPOSITE redirect only changes the **target storage** for a window. It does not change which paint paths run, which RENDER ops are valid, which damage fires. Manual-mode redirect additionally **removes the window from server-managed scene composition** — yserver routes client paint into the backing and exposes it via `NameWindowPixmap`, but the external compositor decides what becomes visible (typically by painting to root or COW). The scene does **not** automatically draw the redirected backing; doing so would duplicate or bypass the compositor's policy. Automatic-mode redirect keeps the window in the scene (the compositor only observes damage, not presents). | `DrawableStore` + `SceneCompositor` |
| I5 | Damage has **two sources** that both contribute to **output damage** (the per-output region that the next composite re-blits). (a) **Storage damage** — paint mutated a storage region (recorded in `DrawableStore`, in storage-local coords); projected through scene-entry transforms onto outputs. (b) **Scene-structure damage** — map/unmap/configure/stacking/shape/redirect-state/cursor/output changes that alter visible composition without changing storage pixels; recorded directly as output-coord rects. The scene re-blits any scene entry intersecting output damage, not just entries with storage damage — restacks, occlusion changes, and re-exposures hit this path. Damage is **acked only on successful present**, so a failed submit does not lose repaint state. | `DrawableStore` (storage) + `SceneCompositor` (scene-structure + output projection) |
| I6 | Every image / buffer / fence has an **owner** and a **retirement generation**. v2 tracks **two distinct retirement signals** because they describe different consumers: **I6a — GPU render-completion fence** (signaled when the queue finishes executing a submitted CB; source storage sampled by the compose pass is releasable once this fires). **I6b — KMS page-flip retirement** (signaled when KMS hands back the previously-scanned-out BO; that BO is releasable for reuse only on this signal). Resources are freed only after **the relevant** retirement signal for their last consumer has fired. | Cross-cutting; `PlatformBackend` provides both fence primitives |
| I7 | Direct scanout is disabled. The scanout BO is filled only by `SceneCompositor`'s composed pass. No per-window scanout shortcuts, no HW cursor plane shortcut (until it's reintroduced under invariant I6's retirement model). | `PlatformBackend` |

## Shape of the new code

Four components, separable, form the rendering core of a new
parallel implementation `KmsBackendV2`. `KmsBackendV2` implements
the existing `Backend` trait and translates each call into one or
more operations on these four components. The legacy `KmsBackend`
(v1) is unchanged and continues to work; see "Parallel implementation"
below.

### `PlatformBackend` — hardware + OS surface

Owns:

- DRM device, KMS outputs, page-flip event handling, scanout BOs.
- libinput device handles + input event pump.
- DRI3 file descriptor / device capability advertisement.
- HW cursor plane (parked initially; reintroduced once I6 covers it).
- Vulkan device, queues, command pools, fence pool.

Provides primitives to other components:

- `submit(cb, signal_fence)` — submit a command buffer, return when
  the queue accepts it. Does not wait for completion.
- `present(scanout_image)` — page-flip a composed scanout image to
  an output. Returns a retirement fence.
- `allocate_storage(width, height, format, usage) -> StorageHandle`
- `import_storage(...)` for DRI3 dmabufs.
- Input event polling.

Does **not** know about windows, pixmaps, or X11 semantics.

### `DrawableStore` — drawable storage + lifetime

Owns:

- Every drawable's storage, keyed by `DrawableId`.
- Root storage (a single big image, depth 24 or 32, sized to the
  virtual-screen extent).
- Per-window storage (one image per InputOutput window, plus zero or
  one redirected-backing image when under COMPOSITE redirect).
- Per-pixmap storage (one image per X11 Pixmap resource).
- Cursor storage.

Tracks:

- Reference counts (windows, pixmaps, picture-rescue, named-pixmap
  aliases, redirected backings — all converge on a single refcount per
  storage).
- Retirement generation per storage: storage is freed only after the
  last frame that referenced it has retired (I6).
- Damage region per storage (I5): a rectangle set, accumulated by paint
  ops, peeked by `SceneCompositor` during composition and acked
  after successful present (snapshot semantics — see `peek_damage`
  / `ack_damage` below).

Exposes:

- `allocate(width, height, format, owner) -> DrawableId`
- `borrow(id) -> &Storage` (read-only access for paint, composition)
- `borrow_mut(id) -> &mut Storage` (write access for paint)
- `damage(id, rect)` (accumulate damage)
- `peek_damage(id) -> DamageSnapshot { epoch: u64, region: RegionSet }`
  — return a versioned snapshot of the current damage. The epoch
  is a monotonic counter incremented every time damage is
  acked; the region is the union of all damage rects accumulated
  so far. Used by `SceneCompositor` when building the composed CB.
- `ack_damage(id, frame_id, snapshot: DamageSnapshot)` — subtract
  **only the snapshot's region** from the live damage state, gated
  on the snapshot's epoch still being current (i.e. no other ack
  has happened since this snapshot was taken). Damage that arrived
  between peek and ack (typically: paint that landed while frame N
  was in flight) has a higher implicit epoch and **survives the
  ack**, so the next composite tick re-blits it. Called only on the
  I6b page-flip retirement for the frame the snapshot was built
  into. A failed submit means no ack call is made; the next tick
  re-peeks and sees both the original snapshot AND any new damage,
  re-composes the union.
- `retire(id)` (drop the reference; storage freed when last ref +
  I6a render-completion + I6b page-flip retirement allow it)
- `set_redirected_target(window_id, backing_id)` (COMPOSITE redirect —
  see I4)

### `RenderEngine` — drawing primitives into storage

Owns:

- Vulkan pipelines for core drawing ops (rect, line, copy, image
  upload, text glyphs, RENDER composite, RENDER trapezoids/triangles,
  RENDER convolution).
- Per-batch state: command buffer recording, descriptor pool, upload
  arena. Roughly today's `PaintBatch` shape, kept clean.
- Glyph atlas, gradient cache, scratch images.

Exposes:

- `fill_rect(target: DrawableId, gc, rect)`
- `copy_area(src: DrawableId, dst: DrawableId, ...)`
- `put_image(target: DrawableId, bytes, ...)`
- `get_image(src: DrawableId, ...) -> bytes`
- `render_composite(op, src, mask, dst, ...)`
- `render_fill_rectangles(...)`, `render_trapezoids(...)`,
  `render_triangles(...)`, `render_glyphs(...)`
- `set_filter(picture, filter)`, `set_picture_transform(...)`, etc.
- `flush() -> Fence` — close the current batch, submit, return a
  retirement fence.

Every op accepts only `DrawableId`s. None of them know about windows
vs. pixmaps vs. root. All take their target from `DrawableStore`.
Damage is recorded on the target storage in `DrawableStore` on every
op.

`RenderEngine` does **not** know about output, scanout, or scene
composition. It writes into storage and signals damage.

### `SceneCompositor` — composed output pass

Owns:

- Stack of `(DrawableId, transform, region)` describing what to put
  on screen each frame.
- Per-output scanout image rotation.
- Scene-structure damage (I5 source b): a flag set by map / unmap /
  configure / restack / SHAPE change / redirect-state change /
  cursor move / output reconfigure.
- The single composed pass that produces a scanout-ready image.

**Fixed scene layering (bottom to top):**

1. **Root storage — always**. This is the bottom layer of every
   frame, regardless of compositor presence. For a non-composited
   session, root contains the `bg_pixel` clear and any `bg_pixmap`
   tile (RenderEngine paints both into root storage at the
   appropriate moments). For a composited session, the compositor
   paints its full composed output into root via RENDER, and root
   storage carries that. Either way, root is the canvas.
2. **Top-level windows + descendants** in z-order — **only the
   non-redirected ones**. A window under Manual-mode redirect is
   removed from this pass; its visual representation is whatever
   the external compositor paints into root or COW (see I4). A
   window under Automatic redirect stays in this pass (Automatic
   mode does not transfer presentation; it only gives the compositor
   a damage feed).
3. **Composite Overlay Window storage** — only if a compositor has
   called `GetOverlayWindow`. Drawn on top of all regular windows.
4. **Cursor** — always on top.

On each composite tick:

1. Compute **per-output damage region** in output coords. Two
   contributing sources:
   - **Projected storage damage**: for each scene entry, take
     `snap = peek_damage(storage_id)` and project `snap.region`
     through the entry's transform onto the output. Record `snap`
     keyed by `storage_id` for the upcoming ack. Union the
     projected region into output damage.
   - **Scene-structure damage**: map / unmap / move / restack /
     SHAPE change / redirect-state change / cursor-position change
     emit output-coord rects describing affected screen regions
     (the union of pre-change and post-change positions of the
     involved entries). Union into output damage. Take a snapshot
     of the scene-structure damage state to ack later.
   - Previous-submit-failed damage (pending repaint state from a
     failed prior tick): union into output damage.
2. If output damage is empty, skip — no flip, no submit, no ack.
3. Build one command buffer that:
   - Clears the output-damage region (or skips clear if root
     storage covers it — usually).
   - For each scene entry in z-order **whose projected bounds
     intersect output damage**: blit `(storage, intersection of
     entry-bounds with output-damage)` into the scanout image,
     applying transform. The blit source-region uses the
     intersected rect projected back to storage-local coords.
     Scene entries that don't intersect output damage are skipped.
4. Submit via `PlatformBackend::submit(cb, render_fence)`. Wait on
   `render_fence` (I6a) before driving the page-flip — the scanout
   image's content is only valid after the queue has finished
   executing the compose CB.
5. `PlatformBackend::present(scanout)` returns a page-flip fence
   (I6b). On **page-flip retirement only**, for each storage that
   contributed: call `ack_damage(storage_id, frame_id, snap)` with
   the snapshot recorded in step 1. Damage that arrived between
   the peek and the ack survives — it's not in the snapshot, so
   the next composite tick re-blits it. Also clear the
   scene-structure damage state captured in step 1. A failed submit
   (or a flip that never lands) leaves all damage pending — no ack
   call — so the next tick re-peeks (a new snapshot that includes
   both the old region and anything new) and re-composites.

The key correction over a "damage-on-storage only" model: occlusion
changes, restacks, and map/unmap re-expose regions of the screen
where source storage didn't change but the output did. Output
damage is the union of all such causes, not a per-storage drain.
And the snapshot semantics keep paint arriving mid-frame safe: it
doesn't get ack'd away with the in-flight frame's damage.

There are no scanout shortcuts. There is no per-window scanout walk.
Root storage, top-level storage, COW storage all go through the same
blit step.

### Parallel implementation + shared `KmsCore`

v2 ships as a **new `KmsBackendV2` struct living alongside the
existing `KmsBackend`**, not as an in-place rewrite of `KmsBackend`'s
internals. Both implement the `Backend` trait. Startup picks one
or the other based on `YSERVER_RENDER_MODEL=v1|v2` (or a feature
flag); within a session the choice is fixed. There is no per-method
fallback (which would re-create the split-brain failure mode v2
exists to eliminate — see Stage 2).

The duplication is bounded:

- Of the ~120 `Backend` trait methods, only ~30-40 actually touch
  the rendering model (paint paths, scene/compositor, RENDER ops,
  resource lifetime). The other ~80 are **protocol bookkeeping** —
  resource tables, xid maps, host-side event masks, font/cursor
  metadata, register/unregister calls, capability advertisement.
- Extract a shared `KmsCore` struct that owns **protocol-bookkeeping
  state only**. Both `KmsBackend` and `KmsBackendV2` embed
  `KmsCore`. The ~80 bookkeeping methods delegate straight through
  to `KmsCore`. The ~30-40 rendering methods diverge: v1 paints into
  its own storage tables, v2 routes through the four-component core.

**KmsCore scope — narrowly drawn.** This is load-bearing: putting
v1 rendering state in `KmsCore` would smuggle v1 storage
assumptions into v2 and defeat the parallel-impl isolation.

`KmsCore` **owns**:

- XID maps (`xid_map: HashMap<u32, ResourceId>`, etc.)
- Window metadata: tree (parent/children), geometry (x, y, w, h,
  border, depth), attributes (background_pixel, override_redirect,
  win_gravity, bit_gravity, do_not_propagate, etc.), map state,
  save-set membership, event masks
- Pixmap metadata: dimensions, depth, ownership (`ClientId`).
  **Not** the storage backing — only the metadata describing it.
- Font metadata: XLFD strings, FreeType face handle (the bytes are
  protocol/text data, not rendering output), metrics.
- Cursor metadata: dimensions, hotspot, owner. **Not** the cursor
  image storage.
- COMPOSITE redirect records (`composite_redirects`,
  `composite_named_pixmaps` — the alias *records* listing
  `client_pixmap` resource IDs, not the backing storage handles).
- SHAPE region tables (`shape_bounding`, `shape_clip`,
  `shape_input`) — region geometry is metadata.
- Picture **records** (op, src/dst xid, transform, filter name,
  clip region, repeat mode, alpha-map xid). **Not** the underlying
  GPU sampler, image view, or pipeline cache.
- Resource ownership tracking (client → owned resource IDs, for
  disconnect cleanup).

`KmsCore` **does NOT own**:

- `vk_mirror: DrawableImage` (v1 per-window/per-pixmap GPU
  storage). v1 keeps these in its own side tables.
- `solid_src_image`, `solid_mask_image`, `white_mask_image`,
  `glyph_atlas`, `gradient_cache`, `dst_readback`, `mask_scratch`,
  `copy_scratch` — Vk pipelines, scratch buffers, atlases.
- `ops_command_pool`, `ops_staging`, `scanout_pools`,
  `compositor_pipeline`, render pipelines — Vk command and pipeline
  resources.
- Scanout BO state, page-flip in-flight tracking, output
  configuration's framebuffer side.
- Alias registry's refcount-on-storage-handle: the protocol-side
  "how many references" lives in `KmsCore`'s `composite_named_pixmaps`,
  but the "free pixmap storage when refcount hits zero" is a
  backend-specific callback (v1 frees `DrawableImage`; v2 retires
  through `DrawableStore`).
- Anything keyed by `vk::Image`, `vk::ImageView`, `vk::Pipeline`,
  `vk::CommandBuffer`, scanout `Buffer` — those are rendering
  state.

The litmus test for a field belonging in `KmsCore`: it describes
**what the X11 protocol says exists** (windows, pixmaps, fonts as
logical entities; their geometry, attributes, ownership) rather
than **how the backend stores it** (Vk images, scanout BOs, GPU
pipelines, sampler views). Phrased differently: a hypothetical
all-CPU yserver backend would need the same `KmsCore` fields; a
v2-only-CPU backend wouldn't need any of the excluded fields.

Why parallel, not in-place:

- **Zero-touch on the working system.** Stage 1 changes nothing
  about v1 behavior. v1 keeps running bit-identical until Stage 3
  proves v2 parity.
- **Bug isolation.** Bugs introduced while developing v2 paint
  paths can only manifest under `YSERVER_RENDER_MODEL=v2`.
- **Rollback is an env var.** No commits to revert.
- **Hardware smoke is unambiguous.** Same recipe, two env vars,
  diff the results.
- **Side-by-side review.** Two impl files in adjacent module dirs;
  divergences are visible by file, not buried in `if v2 { ... }`
  branches.
- **The render-convolution-filter postmortem taught us this.**
  In-place editing of paint paths regressed a working desktop on
  hardware before we noticed — exactly the failure mode parallel
  impls eliminate.

Code organization:

```
crates/yserver/src/kms/
├── core.rs           # NEW: KmsCore — shared protocol-bookkeeping state + helpers
├── backend.rs        # KmsBackend (v1) — embeds KmsCore, paints via per-window mirrors
└── v2/
    ├── backend.rs    # KmsBackendV2 — embeds KmsCore, paints via the four components
    ├── platform.rs   # PlatformBackend
    ├── store.rs      # DrawableStore
    ├── engine.rs     # RenderEngine
    └── scene.rs      # SceneCompositor
```

Examples of `Backend` method translation in `KmsBackendV2`:

| `Backend` method | v2 translation |
|---|---|
| `put_image(host_xid, ...)` | resolve `host_xid → DrawableId` via `self.core.lookup_drawable(host_xid)`; call `self.engine.put_image(id, ...)` |
| `copy_area(src_xid, dst_xid, ...)` | resolve both via `self.core`; `self.engine.copy_area(src_id, dst_id, ...)` |
| `name_window_pixmap(host_window)` | `self.store.ensure_redirected_backing(window_id) → DrawableId`; `self.core.alias_registry.incref(...)`; return |
| `set_window_scanout_skipped(...)` | **gone.** Scene includes/excludes the window based on COMPOSITE redirect state alone. |
| `register_top_level(...)` | metadata recorded in window tree, picked up by `SceneCompositor` next frame |
| `composite_and_flip()` | `SceneCompositor::tick()` |
| `dump_scanout()` | read back the last presented scanout image |

ynest's `HostX11Backend` is unaffected — it doesn't use any of v2's
internals; it forwards X11 requests to a host server as before.

`RecordingBackend` is unaffected — still records `Backend` trait calls
for protocol-handler tests.

## Migration

Four stages. Each stage produces a usable yserver build. Each stage
has a clear "this stage is done" criterion before the next begins.
Hardware smoke gates every stage transition.

### Stage 1 — spec + scaffolding

Two **independently-verifiable** commits, in this order:

**Stage 1a — `KmsCore` extraction (behavior-preserving move).**

- Move the protocol-bookkeeping fields from `KmsBackend` into a new
  `crates/yserver/src/kms/core.rs::KmsCore` struct, following the
  "KmsCore scope — narrowly drawn" rules above. **No Vk types, no
  pipelines, no scratch images, no scanout BOs, no `vk_mirror`
  fields.** `KmsBackend` embeds `KmsCore` and delegates the
  bookkeeping methods through to it.
- This is a **pure move/refactor**, not a redesign. Nothing else
  changes shape. The diff should read as "moved fields + added
  delegations." No new behavior; no API changes.
- Acceptance gates before progressing:
  - `cargo test --workspace` green.
  - `cargo clippy --workspace --lib --tests` clean (no new warnings).
  - One hardware smoke pass: `just yserver-xfce-hw` or `yserver-mate-hw`
    confirms v1 desktop is visually unchanged.
- If the move surfaces a field that's hard to classify (e.g. a
  field that's "metadata" but currently has a `vk::*` typed
  reference embedded), **leave that field in `KmsBackend` until
  it can be split cleanly**. The narrow rule is: nothing enters
  `KmsCore` until its metadata half can be separated from its
  storage/GPU half. The metadata may join later via a follow-up
  refactor when v2 actually needs it; until then, v1-only fields
  stay v1-only.

**Stage 1b — `KmsBackendV2` skeleton + startup selector.**

- Create `crates/yserver/src/kms/v2/{backend,platform,store,engine,scene}.rs`
  with stub types matching the four-component shape.
- `KmsBackendV2` skeleton: embeds `KmsCore`; implements `Backend`
  by delegating bookkeeping methods to `KmsCore` and returning
  "v2 not yet implemented" (logged, no fall-through) for paint
  ops, scene/compositor ops, RENDER ops.
- Startup switch in `crates/yserver/src/main.rs`: read
  `YSERVER_RENDER_MODEL`; default `v1` constructs the existing
  `KmsBackend`; `v2` constructs `KmsBackendV2`. Both go through
  the same `Backend` trait to `process_request`.
- Acceptance gates:
  - `cargo test --workspace` green.
  - `YSERVER_RENDER_MODEL=v1` behavior bit-identical to pre-Stage-1
    (smoke confirms).
  - `YSERVER_RENDER_MODEL=v2` boots far enough to open a connection
    + service `GetGeometry` / `InternAtom` / capability queries;
    the first paint op produces a logged "v2 not yet implemented"
    gap.

### Stage 2 — minimal-Vk correct baseline

The whole vertical slice in Vk, narrowly scoped to ops we need to
prove the model. No pixman, no CPU shadow buffers, no readback path.
The complexity that bit v1 was sync, not Vk operations — v2's I6
addresses sync directly, so we don't need a CPU intermediate.

**Critically: no per-method fallback to v1.** `YSERVER_RENDER_MODEL=v2`
instantiates `KmsBackendV2` as the whole `Backend` impl for the
session. Unimplemented paint ops are explicit no-ops + logged gaps
inside `KmsBackendV2`, not falls-through to v1. Per-method fallback
would mean v1 paint paths mutate the old per-window mirror and v2
scene reads v2 `DrawableStore` storage — split-brain rendering, the
exact failure mode v2 is designed to eliminate.

- `PlatformBackend` real: DRM, KMS, libinput, Vk device + queue +
  command pool + fence pool, scanout BO rotation. Both I6a render-
  completion fences and I6b page-flip retirement signals exposed.
- `DrawableStore` real: storage = `VkImage` + metadata, refcount,
  retirement-generation, damage region. No alternate CPU
  representation. Storage allocations go through `PlatformBackend`.
- `RenderEngine` minimal: **only** fill (`vkCmdClearColorImage` or
  trivial pipeline), copy (`vkCmdCopyImage`), put_image (staging
  upload + copy). RENDER ops, glyphs, traps, triangles, text are
  stubs that log "unimplemented v2 op: <name>" and return without
  painting. I6 enforced from day one.
- `SceneCompositor` minimal: one Vk pipeline that reads storage
  images and writes the scanout image with z-order blits.
  Scene-structure damage + storage damage both wired through
  `peek_damage` / `ack_damage` per I5.
- Acceptance is **synthetic**, not real-app. Real apps (xterm,
  MATE) need glyphs/text/RENDER which Stage 2 deliberately doesn't
  implement; gating Stage 2 on them confuses the milestone.
  - rendercheck subset that touches only fill/copy/put_image
    passes on the v2 path.
  - A custom test harness drives a sequence of fills + copies +
    image uploads through the X11 protocol, observes the scanout
    via the existing dump path, asserts pixel-correctness against
    a CPU oracle.
  - Zero `vkQueueWaitIdle` calls on the hot path under that harness.
  - Cold-start session into a non-WM `xsetroot`-style flow: yserver
    comes up, root is cleared to a known color, the cursor renders,
    a synthetic test client does PutImage + CopyArea, the scanout
    shows the expected result.
  - I6 properties verified: dropping a referenced storage while a
    composite is in flight does not free the underlying VkImage
    until the I6a fence fires; recycling a scanout BO only happens
    on I6b retirement.

### Stage 3 — RENDER + glyphs coverage

- Add the RENDER pipelines on the same Vk substrate built in Stage 2:
  `render_composite`, `render_fill_rectangles`, `render_trapezoids`,
  `render_triangles`, `render_composite_glyphs`.
- Core-X text path: `image_text8/16`, `poly_text8/16` — glyphs out
  of the same atlas.
- Glyph atlas, gradient cache, scratch images — adopted from v1
  shape but living inside `RenderEngine` cleanly.
- `set_filter`, `set_picture_transform`, `set_picture_clip` —
  metadata on `DrawableStore`'s picture-side state.
- `KmsBackend` (v1) **stays in tree** through Stage 3 and beyond
  as a known-good fallback. Stage 3 closing means v2 has parity,
  not production-readiness; deletion waits for v2 to be the proven
  default under real usage over time. See Risk 4 for the deletion
  criteria.
- This is the **first stage where real apps make sense as
  acceptance gates** — Stage 2 deliberately skipped them because
  it doesn't paint text.
- Stage done when: rendercheck passes at parity with v1, real-app
  smoke (xterm under fvwm3, xclock + xeyes under e16, gedit text
  scroll, MATE desktop no-compositor) renders identical to v1, no
  GPU faults on bee under window-drag + gedit + adapta-mate-cc for
  30 minutes, perf within 2× of v1 on representative workloads.

### Stage 4 — re-enable COMPOSITE redirect + COW

- `DrawableStore::set_redirected_target` activated. Window paint
  routes into the backing instead of the window's own storage.
- `NameWindowPixmap` produces a real alias of the redirected backing.
- `SceneCompositor` per I4: Manual-redirected windows are **removed**
  from the regular window pass. The compositor reintroduces them by
  painting into root or COW.
- Composite Overlay Window: a real window with storage; if a
  compositor has called `GetOverlayWindow`, the scene puts it above
  all top-levels.
- xfwm4 + xfce drop-shadow renders correctly. picom xrender backend
  composites and updates per Damage event.
- Stage done when: xfce menu shadows are alpha-correct, picom blur
  renders, no regression on non-composited WMs.

### Stage 5 (optional, follow-up) — perf parity

- HW cursor plane returns under I6's retirement model.
- Direct scanout for full-screen clients (Compiz/games eventually).
- Damage-region clipping in the composed pass to skip whole-output
  blits when only a corner changed.
- Multi-queue if profiling justifies.

## Risks + open questions

**Risk 1 — storage representation.** I1 says "drawable is storage."
Open question: is "storage" one type (a `VkImage` + metadata) or
multiple (`Window-storage`, `Pixmap-storage`, `Root-storage`)? Real
X11 distinguishes Window from Pixmap drawables in spec language but
the differences are entirely metadata (no Pixmap has its own backing
window for `ClearArea` etc.). Lean toward one storage type with a
metadata field.

**Risk 2 — damage projection from storage to outputs.** I5 says
damage attaches to storage; the scene projects to outputs. Open
question: does damage carry coords in storage-local space or
virtual-screen space? Storage-local seems right but the scene then
has to translate via window position at composite time. Probably
fine; need to validate with multi-output cases.

**Risk 3 — `Backend` adapter completeness.** Some Backend methods
mix protocol semantics and rendering semantics
(`change_subwindow_attributes` with `CWBackPixmap` → both updates
metadata AND paints). Spec needs to enumerate every Backend method
and which v2 components it touches.

**Risk 4 — scope of v2 vs v1 coexistence.** Coexistence is at the
**session level via parallel implementations** (`KmsBackend` for
v1, `KmsBackendV2` for v2), picked by `YSERVER_RENDER_MODEL` at
startup. Both embed the same `KmsCore` for protocol bookkeeping.
Per-method fallback is explicitly NOT supported (would create
split-brain storage). **v1 stays in tree until v2 is the proven
production default** — not just "parity at Stage 3 close" but
"used as the default for an extended period without regressions,
across real workloads and hardware classes." Concretely, v1 is
deletable when:

- v2 has been the `YSERVER_RENDER_MODEL` default for ≥1 month
  across daily use,
- no v2-only regression has been filed and stayed open over a
  recent window,
- compositor support (Stage 4) and any perf work (Stage 5) have
  landed and stabilized,
- the cost of maintaining `KmsBackend` is felt to exceed its value
  as a fallback (subjective judgment call, made deliberately).

Keeping both indefinitely is a maintenance cost; deleting too
early loses a known-good fallback during a stuck point. The
shared `KmsCore` survives v1's eventual deletion —
`KmsBackendV2` continues to use it.

**Risk 5 — composite-overlay-window semantics.** Today's
`GetOverlayWindow` returns a fake xid (`COMPOSITE_OVERLAY_WINDOW.0`)
with no backing window. v2 needs the COW to be a real first-class
window in the scene. Open question: is COW a normal window with
special z-order, or its own scene category? Probably a normal
window with metadata flag.

**Risk 6 — `RecordingBackend` doesn't model v2.** Tests against
`Backend` continue to work but they don't exercise the v2 internals.
Open question: do we add a `RecordingDrawableStore` / `RecordingRenderEngine`
test scaffold, or rely on rendercheck + hardware for v2 coverage?
Probably the latter, with `RecordingBackend` retained for protocol-
handler-level tests.

**Risk 7 — ynest divergence.** `HostX11Backend` doesn't use v2; it
forwards to a host X server. The risk is that ynest's behavior
diverges from KMS's over time as v2 implementations refine. Mitigation:
ynest stays as a dev-loop tool, not a correctness target. KMS is the
production target.

## What this spec does not say

This is a shape-of-the-code spec, not a sync algorithm spec, not an
allocation strategy spec, not a damage-region datatype spec. The
four-component division and the seven invariants are what's load-
bearing; details inside each component live in the per-stage plans
that follow.

## References

- `2026-05-12-rendering-rearchitecture-hld.md` — sync/lifetime HLD
  that v2 supersedes in motivation, builds on in delivered work.
- `docs/known-issues.md` — NameWindowPixmap → BadAlloc fault chain
  + render_set_picture_filter entry, both deferred to v2.
- `docs/status-archive-2026-05-13.md` — pre-v2 history.
- Abandoned `render-convolution-filter` branch — T1-T4 Manual-redirect
  + convolution Phase 2 implementation; reference material for v2
  when COMPOSITE comes back in stage 5.

## Review checklist (codex)

- [ ] Are the seven invariants tight? Any missing? Any redundant?
- [ ] Four components — right cut? Should `DrawableStore` and
      `RenderEngine` be one thing, or further split?
- [ ] Does the `Backend` adapter translation table capture every
      shape of Backend method? Counter-examples welcome.
- [ ] Migration stages — right granularity at four stages (minimal-Vk
      baseline → RENDER coverage → COMPOSITE → optional perf)?
- [ ] Open questions — anything load-bearing missing?
