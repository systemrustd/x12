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

## End-goal (project-wide, not v2-scoped)

**On non-ancient hardware, yserver flies.** Concretely: 60+ fps
sustained for typical desktop interactions (window drag, terminal
text scroll, GTK rendering, compositor blur), subjectively snappy
input → display response, no catastrophic-lag failure modes like
the current bee + adapta-mate-cc cliff. "Non-ancient" = anything
that already meets yserver's Vulkan 1.3 requirement (~Skylake+
Intel, RDNA2+ AMD, M-series Apple, modern discrete cards).

This is the project's perf goal, not v2's. v2's job is to be the
**substrate that makes reaching this goal possible**. The "flies"
outcome itself is delivered by a series of perf plans built on
v2 — submit aggregation, glyph atlas rework, hardware plane
assignment, etc.

## Load-bearing outcomes (v2-specific)

What v2 itself ships:

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
5. **The end-goal is unblocked.** Every optimization needed to make
   yserver fly on non-ancient hardware (output-damage clipping,
   submit aggregation, hardware-plane assignment, direct scanout,
   async via DRM in-fences) must be implementable as a strategy
   choice inside v2's four components, not requiring another model
   change. See the "Performance" section for the full list.

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
| I5 | Damage is **two separate concerns** with overlapping inputs. **Presentation damage** drives the scene's next re-blit and has two sources: (a) per-storage paint-mutated region (in storage-local coords, only on scene-participating storage); (b) scene-structure damage from map/unmap/configure/stacking/shape/redirect-state/cursor/output changes (in output coords). Both feed **per-output damage**, which is what the compositor re-blits — restacks and occlusion re-expose regions even when storage didn't change. Acked only on successful present; failed submits don't lose repaint state. **Protocol damage** is the X11 DAMAGE-extension's per-drawable region: accumulates on every paint to any storage, regardless of scene participation, drives DamageNotify events to subscribed clients, acked by `DamageSubtract`. Manual-redirected backings get protocol damage but **no** presentation damage — the external compositor reads via NameWindowPixmap on the DamageNotify, repaints root, root's presentation damage drives the scene. | `DrawableStore` (both kinds) + `SceneCompositor` (presentation + scene-structure) + DAMAGE-ext dispatcher (protocol) |
| I6 | Every image / buffer / fence has an **owner** and a **retirement generation**. v2 tracks **two distinct retirement signals** because they describe different consumers: **I6a — GPU render-completion fence** (signaled when the queue finishes executing a submitted CB; source storage sampled by the compose pass is releasable once this fires). **I6b — KMS page-flip retirement** (signaled when KMS hands back the previously-scanned-out BO; that BO is releasable for reuse only on this signal). Resources are freed only after **the relevant** retirement signal for their last consumer has fired. | Cross-cutting; `PlatformBackend` provides both fence primitives |
| I7 | **Initial invariant**: direct scanout is disabled; the scanout BO is filled only by `SceneCompositor`'s composed pass. No per-window scanout shortcuts, no HW cursor plane shortcut, no bypass paths. **Future relaxation**: direct scanout, HW cursor plane, and HW plane assignment may be reintroduced later, but only as `SceneCompositor`-owned **strategy choices** over scene entries — never as side paths that bypass scene ownership. The contract stays "output equals the resolved scene"; the strategy chooses how scene entries reach the screen (composition, plane, direct present). | `PlatformBackend` (initial); `SceneCompositor` decides strategy when relaxed |

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
- **Two damage regions per storage** (I5). They accumulate from the
  same paint ops but serve different consumers and have independent
  ack lifecycles:
  - **Presentation damage** — region the scene needs to re-blit.
    Only meaningful for storage that participates in the scene
    (root, mapped non-redirected windows + descendants, COW,
    cursor). Peeked by `SceneCompositor` at composite time, acked
    after successful present. Pixmaps not in the scene, unmapped
    windows, and Manual-redirected backings do **not** accumulate
    presentation damage — the scene doesn't draw from them, so
    there's nothing for `SceneCompositor` to ack.
  - **Protocol damage** — region the X11 DAMAGE extension reports
    to clients that have called `DamageCreate` on this drawable.
    Accumulates on every storage regardless of scene participation.
    Peeked + acked by the DAMAGE-extension dispatch path on
    `DamageSubtract`. Distinct from presentation damage so that
    paint to an offscreen pixmap (e.g., RENDER source) fires the
    correct `DamageNotify` events without polluting scene state,
    and so paint to a Manual-redirected backing fires
    `DamageNotify` to the external compositor (which then reads
    via NameWindowPixmap → RENDER Composite → root's presentation
    damage → scene redraw).

Exposes:

- `allocate(width, height, format, owner, scene_participating: bool)
  -> DrawableId` — `scene_participating` decides whether
  presentation damage accumulates. Root, windows (set/unset at
  map/unmap), COW, cursor: true. Pixmaps, Manual-redirected
  backings: false.
- `set_scene_participating(id, bool)` — flip the flag when a
  window maps/unmaps or its redirect state changes.
- `borrow(id) -> &Storage` (read-only access for paint, composition)
- `borrow_mut(id) -> &mut Storage` (write access for paint)
- `damage(id, rect)` — accumulate damage on **both** lists for the
  storage. Presentation list is updated only if `scene_participating`.
  Protocol list is always updated.
- `peek_presentation_damage(id) -> DamageSnapshot { epoch: u64, region: RegionSet }`
  — return a versioned snapshot of the presentation damage. Used
  by `SceneCompositor` when building the composed CB.
- `ack_presentation_damage(id, frame_id, snapshot: DamageSnapshot)`
  — subtract **only the snapshot's region** from the live
  presentation-damage state, gated on the snapshot's epoch still
  being current (no other ack happened since this snapshot was
  taken). Damage that arrived between peek and ack (paint that
  landed while frame N was in flight) has a higher implicit epoch
  and **survives the ack**, so the next composite tick re-blits
  it. Called only on the I6b page-flip retirement for the frame
  the snapshot was built into. A failed submit means no ack call;
  the next tick re-peeks and re-composes the union.
- `peek_protocol_damage(id) -> RegionSet` / `subtract_protocol_damage(id, region)`
  — the X11 DAMAGE extension's `DamageSubtract` semantics, used
  by the DAMAGE dispatcher, independent of `SceneCompositor`'s
  lifecycle.
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
- **Per-BO repaint generation tracking** for buffer-age-style
  damage accumulation across the rotation (see "Damage-clipped
  redraw + scanout BO rotation" below).
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

#### Damage-clipped redraw + scanout BO rotation

KMS rotates between N scanout BOs (typically 2 or 3). Each frame
the compositor renders into a BO that was previously presented and
is now off-screen. If we only draw the damaged region of the next
BO, **the rest of that BO contains whatever was last drawn N
frames ago** — stale content from a prior generation. Damage
clipping without accounting for this produces visible corruption.

v2 uses the **buffer-age pattern** (wlroots / Weston-style) to
keep partial redraws correct:

- Each scanout BO carries a "last present generation" (`u64`
  monotonic counter, bumped per output per successful present).
- `SceneCompositor` maintains a **per-output damage history**: a
  ring of generation → output-damage-region entries, one per
  recent frame. Ring depth = max(BO count) + 1.
- When picking the next BO to render into:
  - If the BO's last-present-generation is known AND every
    generation between it and the current frame is in the history:
    **repaint region = union of all output-damage regions from
    (BO's generation + 1) through (current frame's generation)**.
    Render only that region; the rest of the BO already holds
    pixels equivalent to the front buffer at the BO's generation,
    plus all intervening damage (which we're about to repaint).
  - If the BO is fresh (never presented), the history was lost
    (a long pause where damage accumulated past the ring), or
    we're recovering from a failed flip: **fall back to full
    redraw**. The whole BO gets re-composed. Log `full_redraw_fallback`
    counter increment so we can tune ring depth + detect pathology.
- After successful present, push the current frame's
  output-damage region onto the history; record the BO's new
  last-present-generation; trim the ring.

This is load-bearing for v2: the design promise of "damage-clipped
composition is the v2 perf win" only holds if partial redraws into
rotating BOs are correct. Full-redraw fallback is the safety
net, not the default path.

#### Composite tick algorithm

On each composite tick:

1. Compute **per-output damage region** in output coords. Two
   contributing sources:
   - **Projected storage damage**: for each scene entry, take
     `snap = peek_presentation_damage(storage_id)` and project `snap.region`
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
3. **Pick next scanout BO + decide repaint region** per the
   buffer-age algorithm above. Result is either a full-output
   region (fallback path) or a buffer-age-clipped region (steady
   state).
4. Build one command buffer that:
   - Clears the repaint region (or skips clear if root storage
     covers it — usually).
   - For each scene entry in z-order **whose projected bounds
     intersect the repaint region**: blit `(storage, intersection
     of entry-bounds with repaint-region)` into the chosen scanout
     BO, applying transform. The blit source-region uses the
     intersected rect projected back to storage-local coords.
     Scene entries that don't intersect the repaint region are
     skipped.
5. Submit via `PlatformBackend::submit(cb, render_fence)`. Wait on
   `render_fence` (I6a) before driving the page-flip — the scanout
   image's content is only valid after the queue has finished
   executing the compose CB.
6. `PlatformBackend::present(scanout)` returns a page-flip fence
   (I6b). On **page-flip retirement only**:
   - For each storage that contributed: call
     `ack_presentation_damage(storage_id, frame_id, snap)` with
     the snapshot recorded in step 1.
     Damage that arrived between the peek and the ack survives —
     it's not in the snapshot, so the next composite tick re-blits
     it.
   - Push the current frame's **output damage** (not the per-BO
     repaint region) onto the per-output damage history ring,
     keyed by this frame's generation.
   - Record the BO's new last-present-generation.
   - Clear the scene-structure damage state captured in step 1.

   A failed submit (or a flip that never lands) leaves all damage
   pending and **does not advance the BO's generation** — no ack
   call, history unchanged. The next tick re-peeks (a new snapshot
   union'd with the old) and re-composites; the BO's age means the
   next pick may have to full-redraw, which is correct.

The key correction over a "damage-on-storage only" model: occlusion
changes, restacks, and map/unmap re-expose regions of the screen
where source storage didn't change but the output did. Output
damage is the union of all such causes, not a per-storage drain.
And the snapshot semantics keep paint arriving mid-frame safe: it
doesn't get ack'd away with the in-flight frame's damage. Combined
with buffer-age accumulation, partial redraws into rotating BOs
stay correct.

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
prove the model. No pixman, no CPU shadow buffers, **no readback
on the rendering / composition hot path**. The complexity that bit
v1 was sync, not Vk operations — v2's I6 addresses sync directly,
so we don't need a CPU intermediate. The existing scanout-dump
path (SIGUSR1 → PPM file, the debug / test harness route) is
explicitly exempt: it's an off-hot-path readback used only when
asked for and doesn't bypass scene ownership.

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
  `peek_presentation_damage` / `ack_presentation_damage` per I5.
- Acceptance is **synthetic**, not real-app. Real apps (xterm,
  MATE) need glyphs/text/RENDER which Stage 2 deliberately doesn't
  implement; gating Stage 2 on them confuses the milestone.
  - rendercheck subset that touches only fill/copy/put_image
    passes on the v2 path.
  - A custom test harness drives a sequence of fills + copies +
    image uploads through the X11 protocol, observes the scanout
    via the existing dump path, asserts pixel-correctness against
    a CPU oracle.
  - **Buffer-age partial-redraw correctness test**: a synthetic
    test that paints alternating small damaged rects across the
    output for **at least 2× the scanout BO count** consecutive
    frames (so every BO gets reused multiple times). The test
    dumps every presented frame and compares the full output
    (not just the damaged rect) against a CPU oracle. This is
    the load-bearing test for the buffer-age algorithm — if the
    BO-rotation history is wrong, unchanged regions show stale
    pixels from prior generations and the diff catches it.
    **Fallback budget** (counted in frames, not per-second, since
    the test is short and BO warmup biases a rate metric):
    at most one `full_redraw_fallback` per scanout BO during the
    initial warmup phase (one full redraw per BO is expected
    while each BO acquires its first valid generation entry);
    **zero sustained fallback** after every BO has a recorded
    generation, for the remainder of the test. Any fallback past
    warmup is a buffer-age bug.
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
- Stage done when:
  - **Correctness gates**: rendercheck passes; real-app smoke
    (xterm under fvwm3, xclock + xeyes under e16, gedit text
    scroll, MATE desktop no-compositor) renders identical to v1.
  - **Stability gate**: bee, 30-minute session under window-drag
    + gedit text scroll + adapta-mate-cc, **zero** GPU faults
    (counter: `amdgpu PERMISSION_FAULTS` from dmesg) and
    **zero** `vk_queue_wait_idle` calls in steady state (counter:
    `vk_queue_wait_idle/sec` from v2 instrumentation).
  - **Per-workload perf gates** with named counters (see
    "Instrumentation — required, not optional" below), each
    measured on fuji under the same recipe v1 was measured on:
    - Window drag (xfce4 + xfwm4-no-compositor + xterm dragged
      across screen for 10s): `compose_cb_record_ns/frame` and
      `gpu_render_ns/frame` ≤ 2× v1 baseline; sustained 60 fps
      (`frame_present_count/sec` ≥ 59); `damage_fraction`
      noticeably <1.0 (proving damage clipping is engaging).
    - Terminal scroll (gedit page-down 100 times):
      `paint_submits/sec` ≤ 2× v1 baseline; `cpu_fence_wait_ns/sec`
      ≤ v1 baseline.
    - GTK redraw (mate-control-center open with default theme,
      no compositor): `composite_submits/sec` ≤ v1 baseline;
      no `full_redraw_fallback` spikes.

  All numbers logged via the v2 instrumentation panel
  (`YSERVER_LOOP_TELEMETRY=1`); the "≤" comparisons are against
  v1 captures taken with the same recipe under the same hardware.
  Verbal "feels fine" claims do not close this stage.

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

### Stage 5 (optional, follow-up) — advanced perf strategies

Basic output-damage clipping is **load-bearing for v2** and lives
in Stages 2/3 (Stage 2's `SceneCompositor` already operates on
output-damage regions per I5; Stage 3 keeps that as RENDER paths
come online). Stage 5 owns the advanced policy:

- **Strategy selection per frame**: full-output redraw vs
  clipped redraw choice based on damage fragmentation / coverage
  thresholds; occlusion-driven scene entry skip; partial clears.
- HW cursor plane returns under I6's retirement model, as a
  `SceneCompositor` strategy choice over the cursor entry.
- Direct scanout for eligible full-output entries (Compiz, games
  eventually), as a `SceneCompositor` strategy choice.
- Hardware plane assignment for video / overlay entries.
- Submit aggregation across PaintBatch + scene compose.
- Multi-queue (graphics + transfer split) if profiling justifies.
- DRM in-fence / syncobj submission to replace CPU fence waits.

The pattern: Stage 5 work is **strategy plug-ins** that the
existing components select between. No new model, no new
component, no new invariant.

## Performance — what the design allows, what it doesn't promise

The previous (v1) rework was a sync/lifetime spec, not a perf spec.
It scoped to "no `vkQueueWaitIdle` on hot path" and delivered that.
Some users read "snappy" into it more broadly than the doc claimed,
and bee's residual lag (submit-rate-bound, not wait-bound) became a
disappointment relative to those implied promises.

v2 is intentionally honest about this: **the model change does not
promise perf wins by itself**. What it promises is that the perf
optimizations we'll eventually need are **allowed by the design as
strategy choices, not as alternate rendering models**. The
correctness contract (invariants I1-I7) stays stable; perf work
becomes pluggable.

### Optimization strategies the design allows (Stage 5+)

All of these are implementation choices inside `SceneCompositor`,
`RenderEngine`, and `PlatformBackend`. None require changing the
core model, the invariants, or the four-component split.

- **Output-damage clipped redraw.** Already I5. `SceneCompositor`
  blits only entries intersecting per-output damage.
- **Whole-screen fallback.** When damage is too fragmented or
  edge cases bite, fall back to full-output redraw. Same model;
  identical output.
- **Strategy-per-frame choice.** `SceneCompositor` is free to pick
  per frame: full redraw / clipped redraw / direct scanout /
  hardware cursor / hardware plane assignment. The contract: the
  output must equal what the scene says, and I6 lifetime rules
  must hold. Strategies are not alternate models.
- **Submit aggregation.** `RenderEngine` can collapse paint ops
  into fewer command buffers; `SceneCompositor` can submit one CB
  per output frame. Bee's queue_submit2 rate problem becomes
  tractable here.
- **Persistent storage handles + descriptor caching.**
  `DrawableStore`'s `DrawableId`s are stable across frames, so
  descriptor sets / image views for "draw storage X with these
  bindings" can be cached and reused. Later: bindless indices.
- **Async GPU work via DRM in-fences / syncobj.** I6 separates
  GPU render completion (I6a) from KMS page-flip retirement (I6b).
  Stage 2 may CPU-wait on I6a fences for safety. Later swap in
  DRM in-fence / syncobj submission so the CPU never blocks — no
  semantic change to `SceneCompositor`.
- **Hardware cursor plane.** I7 parks it. When reintroduced, it's
  a strategy: the cursor entry in the scene is *assigned* to a
  plane instead of blitted. Same scene, different render target
  for that one entry.
- **Direct scanout.** "Scene has one eligible full-output entry,
  present that storage directly." Strategy choice, not a separate
  model. Must not reintroduce live-window scanout.
- **Hardware plane assignment.** Per-entry strategy: if a plane
  is available and the entry's transform/format fit, assign;
  else fall back to composition. The scene model doesn't change.
- **Occlusion culling.** Scene entries are explicit, so entries
  fully covered by opaque entries above them can be skipped.
- **Damage as a region, not a rect.** Output damage is a
  `RegionSet`, not a bounding box; clipping is per-rect.

### Red flags — if any of these surface, stop and revise the model

Before adding more features. These are the signals that the design
is wrong, not the implementation.

- Stage 2 cannot implement output-damage-clipped redraw without
  redesign of `DrawableStore` or `SceneCompositor`.
- Storage ownership makes descriptor caching hard — e.g.,
  storage handles aren't stable across frames, or alias-bookkeeping
  forces re-binding.
- Every paint op needs an immediate submit. (PaintBatch from v1
  already solved this; v2 must not regress.)
- Composition requires CPU readback anywhere. (Including the
  Stage 2 → Stage 3 transition; if Stage 2's choices force a
  readback to make Stage 3 work, the model has a flaw.)
- COMPOSITE redirect forces special scanout rules again. I4 says
  redirect is "target storage moves"; if the scanout/scene logic
  needs a special branch for redirected windows, the model leaked.
- The v2 path needs `vkQueueWaitIdle` to stay stable.
- Hardware-cursor or direct-scanout strategies require breaking
  invariants (especially I3 — "visibility produced only by a
  compositor pass") rather than being expressible *as* compositor
  strategies.

### Instrumentation — required, not optional

The perf gates (Stage 3's per-workload thresholds on
`compose_cb_record_ns/frame`, `gpu_render_ns/frame`,
`paint_submits/sec`, `frame_present_count/sec`,
`damage_fraction`, `full_redraw_fallback`, etc.; "no GPU faults
on bee"; v1-deletion measured gates) are only enforceable if v2
emits the counters that let us judge them objectively. Hand-waving
"feels fast" is what got us to the current "bee is laggy and
we're not sure why" state.

**Required counters / log lines** — wired from Stage 2 onwards,
before judging "fast enough" at any stage:

- **Submit counters** (separately, not aggregated):
  - `paint_submits/sec` — `RenderEngine`'s `flush()` count.
  - `composite_submits/sec` — `SceneCompositor` tick submits.
  - `one_shot_submits/sec` — readback, dump, init clears.
  - Total `queue_submit2/sec` — sum check.
- **Wait counters**:
  - `vk_queue_wait_idle/sec` — **target zero** in steady state.
  - `cpu_fence_wait_ns/sec` — CPU time spent in `wait_for_fences`.
  - `cpu_fence_wait_count/sec` — number of fence-wait calls.
- **Damage / scene counters**:
  - `damaged_pixels/frame` (per output) — sum of output-damage
    region areas.
  - `output_pixels/frame` (per output) — total scanout area.
  - `damage_fraction` = damaged ÷ output (logged percentile).
  - `scene_entries_visited/frame` — total entries the compositor
    looked at.
  - `scene_entries_drawn/frame` — entries that intersected damage
    and got blitted.
  - `full_redraw_fallback/sec` — count of frames where the
    buffer-age algorithm couldn't reconstruct a clipped repaint
    region and fell back to a full-output redraw. **Required
    from Stage 2** (the buffer-age algorithm depends on it as the
    safety net). Allowed during BO warmup (a freshly-acquired BO
    with no recorded last-present-generation), damage-history
    loss (a pause that overflowed the ring), failed-submit /
    failed-flip recovery (present history is incomplete or the
    BO's generation is unknown after a prior failure), and
    output reconfigure (mode change, hotplug). Expected
    **near-zero in steady-state clipped redraw**; sustained
    non-zero rates indicate either ring depth needs tuning or a
    bug in the buffer-age logic.
- **Resource counters**:
  - `storage_allocations/sec` — `DrawableStore` allocations.
  - `descriptor_allocations/sec` — per-frame vs cached split.
  - `image_view_creates/sec` — should approach zero in steady
    state with caching.
  - `pixmap_pool_hit_rate` — keep parity with v1's pool numbers
    or improve.
- **Output / timing counters**:
  - `frame_present_count/sec` — successful flips.
  - `missed_pageflips/sec` — vblank misses.
  - `gpu_render_ns/frame` — via `vkCmdWriteTimestamp` pair if
    timestamp queries are available; otherwise skipped.
  - `compose_cb_record_ns/frame` — CPU time building the
    compose CB.

**Emission shape**: same per-second-summary form as v1's existing
telemetry (gated by `YSERVER_LOOP_TELEMETRY=1` or similar; verbose
log lines once per second, parsable by grep+awk).

**Acceptance discipline**: every stage's "stage done when" gate
that names a perf criterion (e.g. "no `vkQueueWaitIdle` on hot
path", "queue_submit2 rate no higher than v1") must cite the
specific counter being checked. Verbal claims without counter
evidence don't close a stage.

### Expectations per hardware class

The **end-goal** is "yserver flies on non-ancient hardware"
(see top of document). v2 doesn't deliver that by itself; it's
the substrate the perf plans run on. v1 is the **minimum bar**:
v1 is incorrect (no compositing) and slow on RDNA2 / high-end
discrete. Matching v1 isn't a win; it's the floor.

At each stage:

- **fuji (Intel, Kaby Lake)**: at v2 Stage 3 close — no worse than
  v1. At Stage 4 — measurably better on damage-intensive workloads
  (damage-clipped composition is the load-bearing v2 win). With
  perf plans on top — solidly in "flies" territory.
- **bee (RDNA2)**: at Stage 2 — no GPU faults under reproducer
  workloads (I6 fault-safety, the load-bearing correctness win).
  At Stage 3 — no worse than v1 (no fix to submit-rate cliff yet;
  that needs separate work). With submit-aggregation + glyph
  rework plans on top of v2 — "flies" territory.
- **silence (high-end discrete)**: same path as bee. Model isn't
  the bottleneck; absolute submit / driver-tax dominates. v2 puts
  the substrate in place; subsequent perf work delivers.
- **air / m4 (Apple M-series)**: untested currently. Should be
  fine — the model is hardware-agnostic and Apple's drivers are
  conservative about lifetime. "Flies" should fall out without
  hardware-specific work.

The honest framing: **v2 is the model change that unblocks
"yserver flies"**. It is not itself the perf work. Anyone
expecting v2-only to make bee fly is being mis-sold; anyone
expecting "flies on non-ancient hardware" to be unreachable
after v2 + the subsequent perf plans is selling v2 short.

## Risks + open questions

**Risk 1 — storage representation.** I1 says "drawable is storage."
Open question: is "storage" one type (a `VkImage` + metadata) or
multiple (`Window-storage`, `Pixmap-storage`, `Root-storage`)? Real
X11 distinguishes Window from Pixmap drawables in spec language but
the differences are entirely metadata (no Pixmap has its own backing
window for `ClearArea` etc.). Lean toward one storage type with a
metadata field.

**Risk 2 — projecting storage-local damage onto outputs.** I5
fixes the coordinate space (presentation damage is storage-local;
the scene projects to per-output coords at composite time). The
remaining risk is the projection's correctness across the cases
that exercise transform / clipping logic:

- **Multi-output layouts.** A window straddling two outputs has a
  single storage-local damage region but needs to project onto
  two output-damage regions, each clipped to that output's
  geometry. Both outputs' history rings must record consistent
  contributions.
- **Output transforms / scales.** Rotated / scaled outputs: the
  projection has to compose the entry transform with the output
  transform. Sign of the transform must not flip the damaged
  region.
- **SHAPE-bounded windows.** The window's bounding region clips
  storage→output projection: damage outside the SHAPE region
  must not appear in output damage.
- **Reparented or moved windows.** Between damage accumulation
  and composite, the window might move (ConfigureWindow) or
  reparent. Scene-structure damage covers the move itself, but
  pre-existing storage damage projects through the **new**
  position — buggy if the scene takes pre-move coords or uses
  stale entry-bounds.
- **Manual-redirect backings that the scene doesn't draw.** Per
  I4 these don't participate in presentation damage at all; this
  risk only confirms that the early-skip in projection doesn't
  accidentally fire on a still-scene-participating window.

These need targeted tests during Stage 4 onwards. None of them
threaten the model; they're projection-implementation correctness.

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
production default**. v1 itself is **not the goal — it's the
minimum bar**. v1 has known correctness gaps (no compositing
support) and known perf gaps (bee/silence submit-rate-bound,
glyph atlas churn, no damage-clipped composition). "Match v1"
would normalize a broken floor. The target is meaningfully better
than v1 on damage-driven workloads + correct where v1 is broken,
while **not regressing below v1** on the workloads v1 handles
well.

v1 is deletable when **all** of the following hold:

- v2 has been the `YSERVER_RENDER_MODEL` default for ≥1 month
  across daily use.
- No v2-only regression has been filed and stayed open over a
  recent window.
- **Correctness gates v1 doesn't meet, that v2 does**:
  - Compositing WM support: xfwm4 with compositing + picom
    xrender backend both render correctly on v2 (Stage 4 work
    landed and validated on hardware).
  - bee/RDNA2: no GPU faults over a 30-minute session under
    real workloads (e.g. window-drag, gedit text scroll, MATE
    + compositor).
- **No-regression gates against v1**:
  - rendercheck full-suite passes on v2; pass time no worse than
    v1's. (v2's damage clipping should make it *faster* in
    practice; the gate is just "not worse.")
  - fuji + xfce4 typical session (idle + occasional drag) feels
    no less responsive than v1 — explicit subjective check,
    because "snappy" is what users notice.
  - Steady-state composite queue_submit2 rate no higher than v1.
- **Headroom gates — places v2 should be measurably better,
  because that's what the model change is for**:
  - Damage-clipped composition demonstrably reduces per-frame
    GPU work on typical workloads (compose-time microbenchmark
    on cursor-only-moves, single-window-typing, etc).
  - Steady-state `vkQueueWaitIdle` call count is zero (v1 has a
    handful in lifecycle paths; v2 should have none by I6).
- The cost of maintaining `KmsBackend` is felt to exceed its
  value as a fallback (subjective judgment call, made deliberately).

Note these gates are **deletion gates**, not Stage 3 acceptance.
Stage 3 close = "v2 is a viable default to switch on." v1
deletion = "we no longer need the escape hatch, AND v2 has
delivered on its design promises beyond just not regressing."
The gap is deliberate.

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
  when COMPOSITE comes back in Stage 4.

## Review checklist (codex)

- [ ] Are the seven invariants tight? Any missing? Any redundant?
- [ ] Four components — right cut? Should `DrawableStore` and
      `RenderEngine` be one thing, or further split?
- [ ] Does the `Backend` adapter translation table capture every
      shape of Backend method? Counter-examples welcome.
- [ ] Migration stages — right granularity at four stages (minimal-Vk
      baseline → RENDER coverage → COMPOSITE → optional perf)?
- [ ] Open questions — anything load-bearing missing?
