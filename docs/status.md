# Status — Rendering model v2

Working doc for the rendering-model-v2 program. The spec is at
`docs/superpowers/specs/2026-05-15-rendering-model-v2.md`; this
file tracks execution against it.

Earlier program docs are archived:

- `status-archive-2026-05-15.md` — the v1 rendering re-architecture
  (Phases 3A–3F-2, sync rework, pixmap pool, GPU traps, the paused
  timeline-semaphore migration, the abandoned convolution filter +
  Manual-redirect work). Re-read it for context on what's already
  in tree, what was tried and reverted, and what was deliberately
  paused.
- `status-archive-2026-05-13.md` — pre-rework history (Phases 1–6
  + host-X11 era).

Cross-cutting bugs and followups that don't fit a stage live in
[`known-issues.md`](known-issues.md).

---

## Where we are

- Baseline branch: `graphics-followups` (working — xfce + mate
  verified 2026-05-15). HEAD includes five cherry-picks from the
  abandoned `render-convolution-filter` branch: QueryFilters
  standard list, NameWindowPixmap diagnosis docs, Justfile xtrace
  `rm`, picom validation harness.
- Active dev branch: `rendering-model-v2`, off
  `graphics-followups`. v2 spec at base; Stages 1a/1b/2a–2f and
  3a through 3f.15 landed on top (see "Done" / "In progress"
  below for commit hashes). `YSERVER_RENDER_MODEL=v2` is the
  **boot default** as of `3afa5bd` (2026-05-17 — previously the
  status doc claimed v2 default but `dispatch.rs` had v1 as the
  fallthrough; three smoke sessions silently tested v1). v1
  still selectable via `YSERVER_RENDER_MODEL=v1`.
- Abandoned branch: `render-convolution-filter`. Left untouched
  as historical reference for T1-T4 of the Manual-redirect work,
  convolution Phase 1+2, the rotate fix, and the
  parallel-implementation lessons. Don't ship anything from there.

### What runs on v2 today (after 3f.15 + hardware-smoke fixes)

- xeyes pupils render correctly (post-`dae5769` MaskScratch
  IDENTITY attachment view).
- xeyes eye whites render without horizontal stripes (post-
  `e76a6f6` trap-shader AA off-by-0.5).
- Cairo / GTK gradient widget backgrounds render with actual
  ramps, not first-stop flat colour (3f.13).
- Fresh pixmaps read back as zero (3f.14 + `fcd2521` —
  Vk DEVICE_LOCAL no longer surfaces recycled GPU memory
  through SHAPE-clipped client drawing).
- xeyes resize-UP renders cleanly (post-`3afa5bd` xid-detach
  + `fcd2521` pixmap clear).
- xeyes resize-DOWN still shows artefacts — see "Open follow-up
  from 2026-05-17 smoke" below.

### What runs on v2 today (after 3b)

- Core paint ops via Stage 2: `xsetroot -solid <color>` cycles
  colours on bee (verified 2026-05-16).
- Core text via Stage 3a: `image_text8`/`image_text16`/
  `poly_text8`/`poly_text16` against any drawable; glyph atlas
  + FenceTicket discipline live; back-to-back upload race test
  passes under lavapipe. **No real app exercises this in
  isolation** — Core-text-only clients (twm window labels,
  pre-Xrender `xsetroot -name`) are the only path; xclock /
  xeyes / xterm / Cairo / Pango all reach for RENDER first.
- Picture-record protocol via Stage 3b: every
  `RenderCreatePicture` / `RenderChangePicture` /
  `RenderCreateSolidFill` / gradient lifecycle / clip / filter /
  transform op stores its record correctly. **No paint side
  effect** — Stage 3c is the substage where these records start
  driving draws.

**RENDER-painting apps stay black on v2 until 3c–3e land.** A
`just yserver-xclock-only-hw` smoke on 2026-05-16 confirmed this
shape: clean v2 boot + RADV ready + first pageflip + xclock
connects + `render_create_picture` / `render_create_solid_fill`
/ `render_trapezoids` gap-logs fire on first paint — i.e.
expected. No crash, no atomic-commit failure.

## v1 → v2 transition

The v1 model (per-window mirrors + scanout-walk) hit a structural
wall: compositing WMs (xfwm4 with compositing, picom, xcompmgr)
paint to root or COW, but yserver's scanout never displays root's
mirror. T1-T4 of the Manual-redirect plan made the
`NameWindowPixmap → BadAlloc` symptom go away but couldn't make
the compositor's paint visible — the model itself was the
blocker.

v2 replaces the rendering layer (not the protocol / input / WM /
KMS-setup layers) with a four-component core:

- `PlatformBackend` (HW + OS surface)
- `DrawableStore` (storage + lifetime + damage)
- `RenderEngine` (paint ops into storage)
- `SceneCompositor` (composed output pass with damage-clipped
  redraw + buffer-age correctness)

Ships as a **parallel implementation** `KmsBackendV2`, alongside
the existing `KmsBackend` (v1). `YSERVER_RENDER_MODEL` env var
picks at startup. Both embed a shared `KmsCore` for protocol
bookkeeping (XID maps, window/pixmap/font/cursor metadata,
COMPOSITE redirect records, SHAPE regions, picture records — no
Vk / GPU types).

End-goal: yserver flies on non-ancient hardware. v2's job is to
be the substrate that makes reaching that goal possible.

## Stages

Per the spec (`docs/superpowers/specs/2026-05-15-rendering-model-v2.md`).

### Done

- [x] **Stage 1a — `KmsCore` extraction.** Landed 2026-05-16
  (`56ad631`). ~30 protocol-bookkeeping fields moved from
  `KmsBackend` into a new `KmsCore` struct. Pure refactor; v1
  behaviour bit-identical. Hardware smoke green
  (`just yserver-xfce-hw` confirms no regression).
- [x] **Stage 1b — `KmsBackendV2` skeleton + startup selector.**
  Landed 2026-05-16 (`982f1eb`). Sibling backend embedding the
  same `KmsCore`; all 118 `Backend` trait methods stubbed with
  once-per-method `v2: <method> not yet implemented` warn.
  `YSERVER_RENDER_MODEL=v1` (default) → `KmsBackend`;
  `=v2` → `KmsBackendV2`. Wires `kms::dispatch::KmsBackendKind`
  selector + shared `platform_init` helper. Hardware smoke
  green: v1 unchanged, v2 boots through capability queries +
  logs gaps on first paint.

### Done (continued)

- [x] **Stage 2 — minimal-Vk correct baseline.** Plan landed
  2026-05-16 (`88d3f3d`) after three codex review rounds; six
  substages 2a–2f. Cross-cutting concepts: two-sync-object model
  (FenceTicket for CPU lifetime + per-ScanoutBo export semaphore
  for KMS IN_FENCE_FD), image-layout state machine per drawable,
  compose-after-paint via same-queue submit order + in-CB barrier
  (zero `vkQueueWaitIdle` on hot path), v2-native RenderBatch
  (not v1's PaintBatch), compose-read consumer tracking on
  FenceTicket Rc.
  - [x] **2a — PlatformBackend real.** Landed 2026-05-16
    (`6f3f423`). Real DRM/Vk/libinput owner replaces the flat
    field set from Stage 1b. FenceTicket + FencePool (recyclable
    VkFence allocator), per-BO BoGenerationEntry tracking
    (last_present_generation + content_invalidated) parallel to
    ScanoutBoPool's BoState, ScanoutBoToken + PageFlipRetirement
    + invalidate_bo + record_present + commit_bo_present hooks.
    KmsBackendV2 delegates fb_dimensions/randr_outputs/take_input_ctx/
    disable_output/poll_fds through it. Paint paths still log
    gaps; v1 untouched. 11 v2 unit tests + 297 yserver tests green;
    clippy clean.
  - [x] **2b — DrawableStore real.** Landed 2026-05-16
    (`4bda93d`). Real storage + lifetime + damage bookkeeping:
    DrawableId/DrawableKind, Storage (Vk image + layout tracker),
    RegionSet, Drawable with refcount + scene_participating + the
    two damage lists per I5 + presentation_damage_epoch for
    snapshot/ack + last_render_ticket for I6a retirement. allocate
    / lookup / incref / decref → RetireDecision,
    set_scene_participating (clears unpresented presentation
    damage + bumps epoch per codex round 1 point 5), damage,
    peek/ack_presentation_damage with the epoch-survival rule,
    touch_render_fence, poll_pending_retire. Storage allocation
    split from metadata so test fixtures (no VkContext) and
    production paths flow uniform. 14 new unit tests; backend
    wiring of allocation methods (create_pixmap etc.) lands
    incrementally across Stages 2c–2d.
  - [x] **2c — RenderEngine minimal (fill / put_image / get_image).**
    Landed 2026-05-16. Three Vk paint ops on the v2 path:
    `vkCmdClearAttachments`-driven fill_rect; staging-buffer
    `vkCmdCopyBufferToImage` put_image (depths 1/8/24/32 with
    MSB-first depth-1 unpack); synchronous `vkCmdCopyImageToBuffer`
    get_image. Each op self-contained — one CB + one
    FenceTicket per op (per-batch coalescing deferred to Stage 5);
    `submitted` deque retires on signal; `get_image` is the only
    `wait` path. `create_pixmap`/`free_pixmap`/`fill_rectangle`/
    `poly_fill_rectangle`/`put_image`/`get_image` wired through
    KmsBackendV2 → engine → store. Offscreen acceptance: 3
    Vk-backed roundtrip tests (depth-32 PutImage→GetImage,
    fill→GetImage, PutImage-then-fill) passing under lavapipe.
    11 logic-only unit tests for byte-stride math + clipping.
    v1 path unchanged.
  - [x] **2d — copy_area + scene graph + blit pipeline.**
    **Part 1** (`6151e34`): RenderEngine `copy_area` with
    disjoint + same-image-scratch paths. 2 Vk-backed acceptance
    tests under lavapipe.
    **Part 2 landed 2026-05-16**: SceneCompositor real with
    v1's `CompositorPipeline` reuse, per-output
    `CompositePoolRing`. Window lifecycle wired on KmsBackendV2
    — `register_top_level` / `register_subwindow` /
    `create_subwindow` / `configure_subwindow` /
    `map_subwindow` / `unmap_subwindow` / `destroy_subwindow` /
    `reparent_subwindow`, all maintaining `windows_v2` map +
    `DrawableStore` allocation/decref + `scene_participating`
    flip + scene-structure-dirty bumps. `composite_and_flip` /
    `maybe_composite` / `mark_dirty` / `on_page_flip_ready`
    wired through `scene.tick` / `scene.handle_page_flip_complete`
    + engine retirement + store retirement. Full-redraw every
    tick; bg_pixel is the compose clear color; cursor + bg_pixmap
    deferred (Stage 3/4). 36 unit tests + 5 Vk-backed
    integration tests passing under lavapipe.
    Hardware smoke for first-visible-scanout pending — Stage 2f
    user-run smoke covers it.
  - [x] **2e — Buffer-age clipping + I6b retirement + failed-flip
    recovery.** Landed 2026-05-16. `BufferAgeRing` per output,
    `pick_repaint_region` algorithm (full-redraw fallback on
    invalidated BO / fresh BO / history loss; clipped repaint
    otherwise). `OutputSceneState` tracks
    `scene_structure_damage` + `pending_repaint_after_failed_submit`
    as `RegionSet`s with the transactional snapshot/ack rule
    (codex round 2 point 2). v2-specific `record_compose_v2`
    function forks v1's `record_and_present_composite` to add
    `loadOp=LOAD` + clip-region scissor for buffer-age paths.
    9a (queue-submit failed) and 9b (atomic-commit failed)
    recovery paths share descriptor-pool slot release + repaint
    deferral; only 9b invalidates the BO's content tracking.
    `handle_page_flip_complete` subtracts submitted snapshots
    from live state (post-submit damage survives) and pushes
    output damage onto the history ring.
    6 new unit tests for ring trim / history-window math /
    repaint picker fallback cases. Stage 2f's synthetic harness
    is the load-bearing buffer-age oracle test (deferred).
  - [x] **2f — Telemetry + acceptance harness + hardware smoke.**
    **Telemetry + acceptance landed 2026-05-16**:
    `kms::v2::telemetry::Telemetry` owns per-second counter
    bucket + lifetime aggregates per spec §"Required counters".
    Counter sites wired (paint_submit on fill/put/copy success,
    one_shot_submit + fence_wait on get_image, composite_submit
    on scene tick, frame_present on page-flip retirement,
    storage_allocation + image_view_create on pixmap/window
    alloc). `YSERVER_LOOP_TELEMETRY=1` enables the per-second
    summary line via `maybe_emit` from `maybe_composite`.
    `crates/yserver/tests/v2_acceptance.rs` ships 3 Vk-backed
    acceptance tests (PutImage→GetImage byte-identical;
    fill+gradient compose oracle; CopyArea disjoint oracle;
    telemetry-lifetime assertion confirming `vk_queue_wait_idle
    == 0`), driving `KmsBackendV2` through the `Backend` trait
    surface. Functionally equivalent to the plan's "synthetic
    harness binary" minus the X11 protocol encoding layer —
    correctness is at the Backend-trait boundary, not protocol
    bytes. `KmsBackendV2::for_tests_with_vk` constructs a live-Vk
    fixture for these. 4 lib telemetry unit tests + 3 acceptance
    tests all green under lavapipe.

    **vng smoke under Venus passthrough (2026-05-16)** —
    `just yserver-v2` / `just yserver-v2-fvwm3-xterm` recipes
    added, run via `-display egl-headless,gl=on` (headless) or
    `gtk,gl=on` (when host has DISPLAY / Xwayland). Verified
    end-to-end in vng:
      - `driver_id=MESA_VENUS`, `1 scanout pools live`,
        `first pageflip complete on output 0 (bo 0)` — Stage 2d
        atomic-flip path lives on Venus-exported dma-buf scanout.
      - `xdpyinfo` (post `xkb_proxy` fix, see below) returns a
        full reply — 16 advertised extensions, depths, screen
        info.
      - `xsetroot -solid red`/`green` drives **7 composite_submits
        + 7 frame_present_count in a 1s telemetry window**,
        `vk_queue_wait_idle/s=0` holds throughout.
      - **Stage 2e 9b atomic-commit-failed recovery fires live**
        under back-to-back composes racing the vblank
        (`atomic commit failed ... Device or resource busy; BO
        invalidated`); the scene loop recovers cleanly and
        keeps going. The recovery path is verified outside the
        unit-test scaffolding.

    **`xkb_proxy` wired in v2** (`b001911`, 2026-05-16). The
    Stage-1b stub returned `Ok(None)` for every XKB minor
    opcode, which kills any Xlib-based client at the XKEYBOARD
    UseExtension handshake — verified: xterm, xeyes, xsetroot,
    xdpyinfo all disconnected silently under the old stub.
    Since `KmsCore.xkb_keymap` is shared with v1 and the
    `kms::xkb::reply_*` helpers are `pub(super)`, v2 mirrors
    v1's body verbatim. This was the hidden blocker for any
    real-client smoke: without it, hardware smoke on bee + fuji
    would have produced "boot + first pageflip" and nothing
    more, identical to the pre-fix vng result.

    **Known polish item (Stage 5 territory)**: under rapid
    back-to-back composites the scene tick can pick a Free BO
    and submit before the prior flip retires, causing KMS to
    return EBUSY on the second `present_scanout` (9b path).
    Recovery handles it correctly (BO invalidated + repaint
    deferred) but the warning is noisy. Fix is to gate
    `tick_one_output` on per-output flip-pending state so we
    only submit when we know KMS will accept; tracked for the
    perf-plans cycle on top of v2.

    **Hardware smoke on bee — green 2026-05-16.**
    `YSERVER_RENDER_MODEL=v2 just yserver-v2-xsetroot-hw` on
    bee (RDNA2 / RADV, 2560x1440) cycles through 11 solid
    colors with `xsetroot -solid <color>`. Two hardware-only
    bugs surfaced + fixed during the run:
      - `bc6718a` — **per-output flip-pending gate.** On RADV
        at 2560x1440, mate-session's connect-flurry fires
        composite-after-composite faster than vblank; KMS only
        allows one pending atomic commit per CRTC and returns
        EBUSY on each subsequent submit. The 9b recovery path
        invalidated every BO faster than they could retire, so
        no BO ever landed → frozen at the boot frame. Fix
        skips `tick_one_output` when `state.pending_acks` is
        non-empty; scene_structure_dirty stays set so the next
        tick (post page-flip-complete) picks up deferred
        damage. KMS submit rate is now structurally bounded to
        one flip per vblank per output.
      - `3d90a0c` — **empty-scene Clipped/LOAD bug.** With the
        flip-pending gate in place, compose ran cleanly (9
        composite_submits/s + 9 frame_present/s) but the
        screen stayed black after xsetroot. Buffer-age picked
        `Clipped` (loadOp=LOAD) once the history ring had
        enough generations; `scene.draws` was empty (xsetroot
        only mutates root.background_pixel — no top-level
        windows), so LOAD preserved each BO's pre-update black
        and we drew nothing on top of it. Force `Repaint::Full`
        whenever the scene has zero draw entries so loadOp=CLEAR
        paints the current `bg_color`. Stage 4's root storage
        makes the empty-scene case rare again.

    **Real client smoke** (xterm/xeyes/xdpyinfo/xsetroot)
    requires the `xkb_proxy` fix from `b001911`. Without it,
    Xlib clients abort at the XKEYBOARD UseExtension
    handshake; with it, xdpyinfo returns a full reply and
    xsetroot drives `bg_color` flips per the bee result above.

    **Stage 2 effectively closed.** mate/xfce/xfwm4 will not
    paint until Stage 3 (RENDER + glyphs + fonts) lands —
    they abort during their first open_font /
    render_create_picture calls and never reach
    set_container_background_pixel-equivalent paint paths.
    That's expected per spec; Stage 2's job was the substrate.

### In progress

- [~] **Stage 3 — RENDER + glyphs coverage.** Plan landed
  2026-05-16 (`142cda8`) after four codex review rounds; six
  substages 3a–3f.

  Plan: `docs/superpowers/plans/2026-05-16-stage-3.md`.

  Cross-cutting concepts: atlas-fence ownership (drops v1's
  per-glyph `queue_wait_idle` by routing uploads through
  per-call `BatchUploadArena` slices owned by `SubmittedOp`,
  with same-graphics-queue submission ordering as the GPU
  dependency and `atlas_last_upload_ticket` cloned onto every
  consume for CPU lifetime); picture-record split between
  `KmsCore` and `RenderEngine`; clipping-domain separation
  (RENDER ops consult picture clip only; Core ops consult GC
  clip only; per-rect scissoring for multi-rect clips
  replaces v1's union-bbox); damage op-aware override for
  destructive PictOps + `GXclear`-class GC.functions;
  `drawable_view_cache` keyed on
  `(DrawableId, SamplerConfig, SwizzleClass)`.

  Acceptance gates: rendercheck parity, real-app smoke matrix
  (xterm/xclock/xeyes/gedit/MATE/xfce4/xfd + xterm-tail
  terminal-text load), bee 30-min stability run, fuji perf
  gates split into correctness-hard-fail (no
  `vk_queue_wait_idle/s` outside `get_image`,
  `composite_glyphs_dropped_unsupported == 0` across smoke
  matrix unless whitelisted, etc.) and headroom-observed-and-
  recorded (≤ 2× v1 envelope per workload).

  - [x] **3a-telemetry primer** — `atlas_intern/s`,
    `glyph_uploads/s`, `glyphs_dropped_atlas_full` (lifetime),
    `composite_glyphs_dropped_unsupported` (lifetime + per-s,
    sites wired in 3d), `disjoint_readback_count` (per-s,
    sites in 3c). Landed 2026-05-16 (`5d88295`). Counter
    storage + `record_*` hooks + emitter line additions; no
    call sites yet. Pure additions; no v1 path touched.
  - [x] **3a-atlas+text** — landed 2026-05-16 (`459da11`). New
    `kms::v2::glyph_atlas::V2GlyphAtlas` forks v1's atlas to
    drop the persistent staging buffer; each upload owns its
    own `StagingBuffer` slice for the lifetime of its
    `SubmittedOp` (`FenceTicket`-gated). `RenderEngine` grows
    `glyph_atlas` + `text_pipeline` + `atlas_last_upload_ticket`;
    every atlas-sampling op clones the latest upload ticket
    onto its own `SubmittedOp.atlas_ticket` (CPU-side lifetime
    gate; same-graphics-queue submission ordering is the GPU
    dependency). `record_text_run` generalized to a
    `TextRunTarget` trait so both v1 `DrawableImage` and v2's
    `StorageTextTarget` (a borrow over v2's `Drawable` storage
    fields) flow through the same recorder. `KmsBackendV2`
    `open_font` / `close_font` mirror v1's body (KmsCore owns
    FontLoader + fonts already); `image_text8`/`image_text16`
    parse + lower a background fill via `engine.fill_rect`
    then call `engine.image_text`; `poly_text8`/`poly_text16`
    iterate TextItem8/16 with inline `0xFF` font-change items
    rotating `core.current_font`. Telemetry sites:
    `record_atlas_intern` / `record_glyph_upload` /
    `record_glyph_dropped_atlas_full` driven from the engine's
    returned `ImageTextStats`. 5 new tests (1 logic + 4
    Vk-backed under lavapipe): back-to-back-upload-no-
    corruption (load-bearing per codex round 1),
    image_text_run_records_damage, atlas_intern_uses_fence_ticket,
    atlas_full_drops_glyph_and_increments_counter,
    v2_poly_text8_font_change_advances_current_font. All
    211 yserver lib tests + 9 ignored Vk tests pass under
    lavapipe; clippy clean. **Plan claim that 3a unlocks
    xclock/xeyes was wrong** — both of those use Xrender for
    their analog/eye geometry and won't render until 3c–3e;
    Core-text-only apps (twm labels, fvwm decorations) are the
    actual 3a smoke surface. FreeType-path real-app text
    correctness gates land at 3f via xterm + gedit.
  - [x] **3b — picture records + pipelines.** Landed
    2026-05-16 (`4a01e68`).
    `KmsCore.pictures: HashMap<u32, PictureRecord>`
    added with v2-side `PictureRecord` enum (`Drawable` +
    `SolidFill` + `LinearGradient` + `RadialGradient` variants,
    plus `PictureFilter` for nearest/bilinear/convolution) and
    `GradientStop` helper. `KmsBackendV2` promotes every
    `render_create_picture` / `render_change_picture` /
    `render_free_picture` / `render_create_solid_fill` /
    `render_create_{linear,radial}_gradient` /
    `render_create_glyphset` / `render_free_glyphset` /
    `render_add_glyphs` / `render_free_glyphs` /
    `render_set_picture_clip_rectangles` / `_filter` /
    `_transform` stub to a real record on `KmsCore.pictures`
    (or `core.glyphsets`). 13 RENDER value-mask bits (CPRepeat
    through CPComponentAlpha) flow through a shared
    `change_picture_apply_mask` helper; gradient bodies parse
    endpoints + stops; clip-rectangles pre-shift by the
    clip-origin; filter names map by ASCII (`nearest`/`fast` →
    Nearest, `bilinear`/`good`/`best` → Bilinear,
    `convolution` → Convolution). DrawableStore refcount
    raised on `render_create_picture` against a wrapped
    Drawable, dropped on `render_free_picture` — `free_pixmap`
    now survives so long as a picture still references the
    pixmap (verified). `RenderEngine` grows a
    `picture_paint: HashMap<u32, PicturePaintState>` slot with
    `Empty` placeholder; `render_free_picture` calls
    `engine.picture_paint_remove` so the teardown hook is in
    place for 3c's gradient LUT lazy-build. `parse_add_glyphs`
    promoted from v1's `pub(super)` to `pub(crate)` so v2
    reuses it verbatim. No paint side effects yet — Stage 3c
    lights up `render_composite`. 5 new unit tests: lifecycle
    (every value-mask bit), drawable-refcount blocks
    free_pixmap, solid-fill stores wire color as-is (v1-parity
    against rendercheck premul convention), linear gradient
    parsing, clip-rectangles pre-shift. 216 yserver lib tests
    + 9 Vk-backed ignored tests pass under lavapipe; clippy
    clean. Application smoke unchanged (3b doesn't draw); RENDER
    paint apps stay broken until 3c.
  - [x] **3c — `render_composite` + `render_fill_rectangles`.**
    Standard PictOps + Saturate + Disjoint/Conjoint shader
    blend; per-rect picture-clip scissoring; self-composite
    aliasing routed through alias-readback scratch.
    - **3c.1 landed 2026-05-16 (`bcca8a3`)** — foundation only.
      `RenderEngineInner` grows `render_pipelines`,
      `solid_src_image`, `solid_mask_image`, `white_mask_image`,
      `dst_readback`, `drawable_view_cache` slots (lazy);
      `ensure_render_assets` builds them on first paint;
      `notify_drawable_retired` evicts cached views;
      `SubmittedOp.descriptor_arena` retires per-op pools.
      `record_render_composite` is now generic over a new
      `CompositeTarget` trait (impl'd for v1's `DrawableImage`
      and v2's `StorageTextTarget`). No new dispatch. v1 paths
      bit-identical.
    - **3c.2 landed 2026-05-16 (`ccac8c9`)** — paint bodies +
      wiring + telemetry. `RenderEngine::render_composite` /
      `render_fill_rectangles` are real: lazy ensure-assets,
      pipeline cache lookup, view cache for src/mask, synthetic
      1×1 scratch clears, dst_readback for Disjoint/Conjoint,
      per-rect picture-clip scissor (plan §4 deviation from v1's
      union-bbox shortcut — `record_render_composite` now takes
      a scissor slice). `KmsBackendV2::render_composite` /
      `render_fill_rectangles` resolve picture records from
      `KmsCore.pictures` and dispatch; `disjoint_readback_count`
      telemetry primer wired live. Three `kms::backend` helpers
      (`repeat_to_shader_const` / `compose_affines` /
      `pixman_transform_to_affine`) promoted to `pub(crate)` so
      v2 reuses verbatim.
    - **3c.3 landed 2026-05-16** — self-alias scratch routing
      + 7 Vk-backed acceptance tests under lavapipe. New
      `RenderEngineInner.src_alias_readback: DstReadback`
      slot (sibling to `dst_readback`, same growable per-format
      scratch shape). `render_composite` detects
      `src.drawable_id() == dst_id` (or same for mask), pre-allocates
      the alias scratch + extracts its sampled view, then in the
      per-op CB issues `record_copy_from` (dst → alias scratch →
      SHADER_READ_ONLY) before `record_render_composite`. The
      composite descriptor binds the alias view as `src_tex` /
      `mask_tex` in place of dst's own drawable view; Vulkan
      can't sample an image while it's bound as a color
      attachment in the same draw, the scratch breaks the alias.
      `CompositeStats.used_src_alias_scratch` is the observable
      signal. 7 acceptance tests added — 6 in
      `kms::v2::engine::tests::` (`render_composite_over_renders
      _alpha_blended`, `_picture_clip_per_rect`,
      `_solid_fill_source_path`,
      `_disjoint_clear_uses_readback`, `_self_alias`,
      `render_fill_rectangles_src_clears_to_color`); the
      seventh, `v2_render_composite_no_gc_clip_leak`, lives in
      `tests/v2_acceptance.rs` because the "GC clip must not
      leak into RENDER paint" property is a Backend-trait
      invariant (engine has no GC-clip notion). Gradient source
      still bails to 3e. 216 lib tests + 15 ignored v2-engine Vk
      tests + 4 v2_acceptance tests all green under lavapipe;
      clippy pedantic clean for the touched lines.
  - [x] **3d — `render_composite_glyphs` + glyphsets.** Landed
    2026-05-16. v1-parity scope: PictOp Over + SolidFill source
    + A8/A1/ARGB32-as-A8 glyphsets. Fixes v1's latent
    `_clip unused` bug (`kms::backend.rs:5313`) — the dst picture
    clip is now honoured via per-rect scissoring, plan §4.

    `record_text_run` factored into a single-scissor convenience
    wrapper + `record_text_run_scissored` that takes a
    `&[vk::Rect2D]` slice. Same `cmd_begin_rendering` →
    loop `set_scissor` + glyph-draw batches → `cmd_end_rendering`
    shape; v1's existing `record_text_run` site stays at
    full-extent scissor via the wrapper. New
    `RenderEngine::composite_glyphs` mirrors `image_text`'s
    atlas-intern path (same key namespace, atlas_last_upload_ticket
    discipline, FenceTicket-gated per-glyph staging buffers); the
    new `CompositeGlyphInput` borrows pre-A8 glyph pixels from
    `KmsCore.glyphsets` or a per-call A1→A8 scratch.
    `KmsBackendV2::render_composite_glyphs` parses the items
    stream (8-byte elements + inline `0xFF 0 0 0 new_gs_xid`
    glyphset-change form, ids 1/2/4 bytes per CompositeGlyphs8/16/32
    minor), looks up each glyph from `core.glyphsets`, A1→A8
    expands per v1's MSB-first bit order, then calls into the
    engine. Two-pass parse avoids a borrow conflict on the
    A1-expansion Vec (the inner Vec<u8>'s heap buffers are stable
    through outer-Vec pushes, but the borrow checker doesn't
    track that — pass 1 fills the scratch + records indices, pass
    2 resolves slices). Gate: op != Over OR source not SolidFill
    bumps `composite_glyphs_dropped_unsupported`; stale handles
    (picture / glyphset / dst drawable) log gap + return Ok
    without bumping (protocol errors, not unsupported features).
    Telemetry sites: atlas_intern, glyph_uploads,
    glyphs_dropped_atlas_full, paint_submits — same shape as
    image_text.

    3 unit tests: `v2_composite_glyphs_unsupported_op_drops`,
    `v2_composite_glyphs_non_solidfill_source_drops` (uses a
    LinearGradient source so the gate fires at the source-type
    check, not a stale-handle path),
    `v2_composite_glyphs_inline_glyphset_change_parsed` (asserts
    Over+SolidFill stays in-envelope when the items stream
    rotates the active glyphset mid-run). 1 Vk-backed
    acceptance: `v2_composite_glyphs_clip_intersects_picture` —
    paints two 4×4 white glyphs across an 8×4 blue dst with a
    4×4 top-left clip; asserts left half white, right half blue.
    This is the v1-bug-fix gate — v1 paints both glyphs.
    219 lib tests + 15 ignored v2-engine Vk tests + 5
    v2_acceptance tests all green under lavapipe.
  - [x] **3e — trapezoids + triangles + `copy_plane`.** Landed
    2026-05-16 in two substages.

    **3e.1 — copy_plane (`dc3853d`).** GXcopy scope: pull src wire
    bytes via `engine.get_image`, classify each pixel by
    `(pixel & plane) != 0` into fg/bg rect lists, drive
    `poly_fill_rectangle` bg-first then fg. Depths 1/8/24/32
    supported via per-depth wire row-stride + pixel extraction
    (MSB-first bit unpacker for depth-1). Non-GXcopy logs a gap +
    skips (Stage 3f LogicFillPipeline). xfd/xfontsel are the
    canonical callers and both use GXcopy.

    **3e.2 — trapezoids + triangles (TrapPipeline port).** New
    `trap_pipeline: Option<TrapPipeline>` + `mask_scratch:
    Option<MaskScratch>` slots on `RenderEngineInner`, lazy-init
    via `ensure_trap_assets`. New engine method
    `render_traps_or_tris(prim_kind, instance_data, instance_count,
    bbox, ...)` ports v1's `try_vk_render_traps_or_tris`
    (kms/backend.rs:4500) into v2's per-op CB shape:
      1. Allocate per-call `StagingBuffer::new_with_usage(...,
         VERTEX_BUFFER)` (sibling to the existing `TRANSFER_*`
         constructor) sized for `instance_count × stride`; memcpy
         the wrapper-cooked instance bytes in.
      2. `mask_scratch.ensure_image_size_returning_old(bbox_w,
         bbox_h)` — retired old image currently dropped on the
         floor (same shape as `dst_readback` grow-leak; flagged
         in the `mask_scratch` doc note for Stage 5 polish).
      3. Trap rasterize phase inside the CB:
         mask → COLOR_ATTACHMENT, `begin_rendering(mask_view,
         LOAD_OP_CLEAR)` at `(0,0)..(bbox_w, bbox_h)`, bind
         trap-or-tri sibling pipeline + vertex buffer, push
         `TrapDrawPushConsts`, set viewport + scissor, draw
         `(4, instance_count)`, end_rendering, mask
         → SHADER_READ_ONLY.
      4. Composite phase: `needs_full_dst` op set (Clear/Src/etc.
         and every Disjoint/Conjoint variant) drives a full-dst
         draw with `mask_off = -bbox`; other ops draw only the
         bbox. dst_readback snapshot fires for Disjoint/Conjoint.
         Goes through the existing `record_render_composite` with
         the scratch view bound as `mask_tex`, `REPEAT_NONE` so
         out-of-bbox samples yield mask=0. Per-rect picture-clip
         scissoring (plan §4) honoured at the composite stage.
      5. Push the `SubmittedOp` with the instance buffer in
         `staging` so its retirement releases the upload buffer.

    Out-of-scope at 3e.2: gradient sources (Stage 3e gradient
    work is risk-listed for follow-up), src self-alias scratch
    (rare in trap workloads), all the broader op coverage
    Disjoint/Conjoint already accepts via the existing pipeline.

    Backend wiring: `KmsBackendV2::render_trapezoids` /
    `render_triangles_op` decode wire bytes (40 B traps; 24 B
    triangles via minor 11/12/13 dispatch), apply `(x_off, y_off)`
    in 16.16 fixed-point, compute bbox via the existing
    `trapezoid_bbox` / `triangle_bbox` helpers, pre-pack instance
    data, and call into the engine.

    Tests: 2 unit (`trapezoid_decoder_x11_wire_layout`,
    `triangle_to_trap_degenerate`) + 1 Vk-backed acceptance
    (`v2_render_trapezoids_renders_filled_rect` — axis-aligned
    4×4 trap with `Over` + SolidFill source, interior red over a
    blue dst). 221 lib + 15 ignored v2-engine Vk + 7
    v2_acceptance tests all green under lavapipe.
  - [~] **3f — Core remainder + GC.function + planemask +
    acceptance.** Substaged for incremental landing.
    - [x] **3f.1 — poly_* + fill_poly + poly_fill_arc landed
      2026-05-16.** `poly_line` / `poly_segment` /
      `poly_rectangle` / `poly_arc` / `poly_point` /
      `poly_fill_arc` / `fill_poly` all real on KmsBackendV2.
      `bresenham_segment` / `scanline_fill_polygon` /
      `clip_rects_to_image` / `read_i16_pair` / `read_rect`
      promoted from v1's free fns to `pub(crate)` and reused
      verbatim — wire-byte parsing + rasterisation is
      v1-identical, so rendercheck results carry across. v2
      grows `current_clip_rects_in_dst_space` +
      `intersect_with_current_clip` (mirrors v1) +
      `drawable_dims_v2` + `fill_solid_rects`; the latter is
      the shared "lower a list of solid rects to
      `engine.fill_rect`" lowering, with non-GXcopy
      `GcFunction` logged as a gap until 3f.2 lands
      `LogicFillPipeline`. **v1 latent bug fixed in v2 (codex
      plan §4):** v2's `fill_rectangle` + `poly_fill_rectangle`
      now intersect with `KmsCore.current_clip` before
      submitting; v1 has the same call path but bypasses the
      clip helper, so Core paint silently overflows the GC
      clip there. 3 new unit tests:
      `poly_line_origin_mode_offsets_correctly`,
      `fill_poly_scanline_correctness`,
      `poly_fill_rectangle_honours_gc_clip`. 224 lib tests +
      17 ignored v2 Vk tests + 7 v2_acceptance tests all green
      under lavapipe.
    - [x] **3f.2 — LogicFillPipeline landed 2026-05-16.**
      v1's `LogicFillPipelineCache` reused verbatim (no fork —
      the `vk::Format`-bound cache + `(GcFunction, opaque_alpha)`
      sub-key are already format-agnostic). `RenderEngineInner`
      grows `logic_fill_caches:
      HashMap<vk::Format, LogicFillPipelineCache>` (lazy,
      sharded by dst format — BGRA8 is the typical entry; R8
      enters on the rare depth-1/8 logic-fill path). New
      engine method `RenderEngine::logic_fill(target, function,
      opaque_alpha, fg, rects)` ports v1's
      `try_vk_fill_with_function` body into v2's per-op CB:
      ensure_cache → `begin_op_cb` → drawable layout
      transition COLOR_ATTACHMENT_OPTIMAL → `cmd_begin_rendering(LOAD)`
      → `bind_pipeline` → per-rect (`set_scissor` +
      `cmd_push_constants` + `cmd_draw(4, 1)`) →
      `end_rendering` → transition back to SHADER_READ_ONLY →
      `end_and_submit_op` + ticket-clone-onto-Drawable +
      damage. `opaque_alpha` derived from `Drawable.depth !=
      32` (L1 server-α invariant). `KmsBackendV2::fill_solid_rects`
      now diverts to `engine.logic_fill` when `function !=
      Copy`; `copy_plane`'s pre-3f.2 non-`GXcopy` gate dropped
      (decomposes into `poly_fill_rectangle` calls which now
      honour function). 2 new tests:
      `gxcopy_planemask_diverts_to_logic_fill` (logic, asserts
      no gap fires) + `logic_fill_xor_applies_per_pixel`
      (Vk-backed, GXxor on a 4×4 BGRA8 dst — XOR per byte +
      alpha preserved via opaque_alpha pipeline). 225 lib +
      18 ignored v2 Vk + 7 v2_acceptance tests green under
      lavapipe. **Out of 3f.2 scope (v1-parity gap, tracked
      separate):** `put_image_non_gxcopy` and `copy_area_non_gxcopy`
      stay logged-gap — v1 doesn't honour function on these
      either (put_image is a byte copy, copy_area is a Vk
      image-to-image blit; neither passes through the
      fragment-shader stage that LogicOp acts on). Real apps
      use logic-op only on fill paths; rendercheck covers it
      there.
    - [x] **3f.3 — set_clip_pixmap + set_gc_fill_tiled landed
      2026-05-16.** `set_clip_pixmap` stores `ClipState::Pixmap
      { origin, pixmap }` instead of logging a gap +
      clearing the clip (v1-parity bookkeeping; mask sampling
      itself is deferred — no real-app smoke matrix client
      drives clip-pixmap, v1 stores-but-doesn't-enforce too).
      `set_gc_fill_tiled` stores `FillState::Tiled { pixmap,
      origin }` instead of logging a gap (the dispatcher also
      pushes via `apply_fill_state`; keeping the dedicated
      entrypoint correct keeps the Backend trait uniform).
      `fill_solid_rects` split: stroke ops (`poly_line` /
      `poly_segment` / `poly_rectangle` / `poly_arc` /
      `poly_point`) still call it directly (X11 strokes are
      always solid foreground); fill ops (`fill_rectangle` /
      `poly_fill_rectangle` / `poly_fill_arc` / `fill_poly`)
      now go through new `fill_rects_honoring_fill_state`,
      which dispatches `FillState::Tiled` + GcFunction::Copy
      to `try_tiled_fill` (engine.render_composite with
      `OP_SRC` + `Repeat::Normal` + `(src_x, src_y) = (dst -
      tile_origin)`). Non-Copy + Tiled and `Stippled` /
      `OpaqueStippled` degenerate to solid (v1-parity).
      Tile aliasing (`tile_id == dst_id`) and non-BGRA8 tile
      formats fall back to solid. 2 new unit tests
      (`set_clip_pixmap_stores_pixmap_clip`,
      `set_gc_fill_tiled_stores_fill_state`) + 1 Vk-backed
      acceptance (`v2_tiled_fill_replicates_tile_pixmap` —
      4×4 dst pre-filled blue, 2×2 red tile, `apply_fill_state(Tiled)`
      + `poly_fill_rectangle` over whole dst, asserts every
      pixel reads red). 227 lib + 18 ignored v2 Vk + 8
      v2_acceptance tests green under lavapipe.
    - [x] **3f.4 — cursor + `xfixes_change_cursor_by_name`
      landed 2026-05-16.** Closes the pre-Stage-4 cursor
      stubs: `create_cursor` / `create_glyph_cursor` /
      `render_create_cursor` / `define_cursor` /
      `xfixes_change_cursor_by_name` all return without
      logging `v2:` gaps. Handles are well-formed (xid minted
      via `core.next_host_xid()` for the three create paths),
      so Cairo / GTK / Qt theme-cursor clients see a clean
      reply and don't trip on a zero handle. Pixel
      rasterisation + scene blit stays Stage 4 territory (no
      cursor scene-layer yet; cursor defaults to bare KMS HW
      cursor — see spec § scene layering "Cursor — always on
      top"). `xfixes_change_cursor_by_name` is a v1-parity
      no-op (yserver has no cursor-theme registry; the name
      hint is silently dropped). 1 unit test
      (`cursor_paths_do_not_log_gaps`) asserts the closure.
      228 lib + 18 ignored v2 Vk + 8 v2_acceptance tests green
      under lavapipe.
    - [x] **3f.6 — subwindow scene composition landed
      2026-05-16.** `WindowGeometryV2` grows `parent:
      Option<u32>` + `bg_pixel` + `bg_pixmap`.
      `create_subwindow` records the passed-in parent xid +
      bg-pixel; `reparent_subwindow` updates parent on tree
      moves. `change_subwindow_attributes` ports v1's
      `CWBackPixmap` (0x01) + `CWBackPixel` (0x02) parse —
      pre-3f.6 stub logged a `v2:` gap, now stores the values
      into `windows_v2[xid]`. `allocate_window_storage`
      (called from `create_subwindow` / `register_top_level`
      / `register_subwindow`) clears fresh storage to
      `bg_pixel` via `engine.fill_rect` so freshly-mapped
      windows have a defined initial colour. `configure_subwindow`
      re-fills bg_pixel on resize.

      `build_scene` factored: top-level iteration calls a new
      `emit_window_subtree` recurse that walks top-level →
      mapped descendants, projecting through accumulated
      parent offsets into output coords. Unmapped parents
      short-circuit the entire subtree per X11 MapWindow
      cascade semantics. Sibling z-order between children
      of the same parent is HashMap-iteration-order — proper
      stack tracking is post-3f.6 polish (most real apps have
      one child per parent so the gap rarely matters at
      Stage 3).

      4 new tests:
      `build_scene_recurses_into_mapped_children` (parent +
      child draw entries with absolute coords),
      `build_scene_unmapped_parent_hides_subtree` (cascade),
      `change_subwindow_attributes_stores_bg_state`
      (value-mask 0x03 lands both fields; bookkeeping no
      longer logs a gap),
      `create_subwindow_records_parent_and_bg_pixel` (parent
      xid + bg-pixel hint stored on the geometry record).

      232 lib + 18 ignored v2 Vk + 8 v2_acceptance tests
      green under lavapipe. Hardware smoke (xterm under no-WM,
      xclock) pending — runs at 3f.5.
    - [x] **3f.7 — input dispatch landed 2026-05-16.**
      `on_host_input` + `cook_host_key` +
      `process_pointer_absolute` + `process_pointer_button` +
      the 7-helper cluster (`serialize_modifiers`,
      `window_under_cursor`, `event_relative_coords`,
      `emit_pointer` / `emit_crossing` / `emit_motion_only`,
      `update_pointer_window`, `dispatch_motion_event`) all
      ported on `KmsBackendV2`. v1's body went over almost
      verbatim — KmsCore-only helpers (xkb_state,
      cursor_x/y, button_mask, pending_pointer_events,
      xid_map) port byte-for-byte; the three v1-specific
      touches got v2-shape substitutions:
      - `self.windows` → `self.windows_v2` (uses the parent-
        aware geometry record landed in 3f.6 for hit-testing).
      - `self.fb_w` / `self.fb_h` → `self.platform.outputs[0]`
        extent (single-output is the only path exercised;
        multi-output input mapping is risk-listed for later).
      - HW cursor calls (`hw_cursor_active` / `hw_cursor_move`
        / `hw_cursor_refresh`) → no-op + `scene.mark_scene_structure_dirty`
        per spec § I7 (HW cursor plane parked in v2 until
        Stage 5 reintroduces it as a SceneCompositor
        strategy).

      6 new unit tests: `serialize_modifiers_zero_on_fresh_state`,
      `cook_host_key_fills_coords_and_modifier_state`,
      `process_pointer_button_state_field_is_pre_press`
      (X11 spec pre-press `state` discipline + post-event
      `button_mask` update),
      `process_pointer_absolute_clamps_to_output`,
      `window_under_cursor_finds_topmost_mapped`
      (back-to-front walk + unmapped-skip),
      `on_host_input_does_not_log_gap`.

      238 lib + 18 ignored v2 Vk + 8 v2_acceptance tests
      green under lavapipe; clippy clean.

      Software cursor lands as 3f.8 below so hardware smoke has
      visible pointer feedback; the full theme/`define_cursor`
      pipeline stays Stage 4.
    - [x] **3f.8 — software cursor sprite landed 2026-05-16.**
      Stage-4 preview so the user has visible pointer feedback
      during 3f.5 smoke. `SceneCompositorInner` grows
      `cursor: Option<CursorEntry>`; `SceneCompositor::register_cursor`
      records a (DrawableId, extent, hotspot) tuple. `build_scene`
      grows a `cursor: Option<CursorEntry>` parameter; when
      `Some`, appends a top-of-z `CompositeDraw` at
      `(cursor_x - hot_x, cursor_y - hot_y)` with
      `alpha_passthrough=true` so the sprite's transparent
      border actually blends.

      `KmsBackendV2::init_cursor_sprite` (called from `open`)
      allocates a 16×16 BGRA8 Pixmap-kind Drawable, uploads a
      baked default-arrow sprite (12-row right-triangle, black
      fill, 1-px white diagonal outline) via `engine.put_image`,
      and registers it on the scene. Best-effort — failure
      logs + leaves the cursor invisible (no regression).

      Full theme support + per-window `define_cursor` swap-in +
      `xfixes_change_cursor_by_name` integration stays Stage 4.
      `process_pointer_absolute` already calls
      `scene.mark_scene_structure_dirty` on every motion so the
      cursor draw refreshes per tick.

      1 new unit test
      (`build_scene_appends_cursor_draw_at_top_of_z` —
      cursor draw is the last `scene.draws` entry at the
      expected position with `alpha_passthrough=true`).

      239 lib + 18 ignored v2 Vk + 8 v2_acceptance tests green
      under lavapipe; clippy clean.

      Bonus fix in the same range: v2's `get_keyboard_mapping`
      and `get_modifier_mapping` stubs were returning empty
      tables. Hardware smoke after 3f.7 showed xterm taking
      pointer events but dropping every keypress because xlib
      saw zero keysyms per code; v1's body ported verbatim
      (commit `6b0ffb6`).

      *Post-3f.8 hardware smoke surfaced two follow-up bugs
      that 3f.9 (below) fixes — the cursor trail and the
      ENOMEM → DEVICE_LOST cascade. The bare-3f.8 cursor draw
      is correct in isolation but unsound at scene level
      without root storage; the buffer-age clipped/LOAD path
      left prior cursor pixels in inter-window gaps, and the
      failed-commit path was leaking pinned resources.*
    - [x] **3f.9 — scene + commit recovery + root storage
      landed 2026-05-16 (codex review/fix).** Three root-cause
      fixes for the post-3f.8 hardware-smoke regressions, all
      under the v2 spec's invariants:

      - **Root storage promoted forward.** Spec §scene-
        layering item 1 ("Root storage — always") was deferred
        to Stage 4 in our stage plans but the cursor-trail
        diagnosis proved the deferral was unsound under
        buffer-age clipping. `KmsBackendV2::init_root_storage`
        allocates a virtual-screen-extent BGRA8 drawable as
        `DrawableKind::Root`, fills it with `bg_pixel`, and
        marks it scene-participating. `build_scene` now
        prepends a root-layer `CompositeDraw` at the bottom
        of z before walking top-levels. `set_container_background_pixel`
        / `set_container_background_pixmap` repaint root
        storage via `engine.fill_rect` /
        `engine.copy_area` instead of just mutating
        `core.bg_pixel`. The cursor trail is gone because
        there are no longer gap regions between scene draws —
        root covers everything, so the buffer-age repaint
        re-blits the area behind the prior cursor every tick.
        Means 3f.6's "trail fix" via prior-rect damage was
        the wrong layer; the right layer was the missing
        scene-layering bottom entry.

      - **`FenceTicket` false-positive leak detection.**
        `FenceTicketInner::drop` previously checked only the
        `signaled_cache` bool, which is set explicitly via
        `poll_signaled`/`update_signaled` calls. Tickets that
        dropped signaled-but-stale-cache logged
        `FenceTicket: leaked unsignaled fence ... renderer_failed
        will be set` and flipped `platform.renderer_failed =
        true` — once that happens, every engine + scene op
        returns `RendererFailed` and the screen freezes.
        Fix: drop falls back to
        `vk.device.get_fence_status(self.fence)` when the
        cache says false, and updates the cache. This was the
        proximate cause of the "screen freezes after a couple
        minutes" symptom in the post-3f.8 logs.

      - **Failed-atomic-commit BO recovery.** The pre-3f.9
        recovery path did `bo.state = BoState::default()`
        immediately after a failed flip — but the GPU had
        already executed `queue_submit2` writing into that
        BO. Next `acquire_scanout_bo` could hand back the
        same BO for recording while GPU writes were still in
        flight, and the OUT_FENCE_PTR (defensively set to
        `-1`, but some drivers/kernels do write it alongside
        an error) was never closed. Under RADV/bee this
        accumulated sync_file fds in the kernel until
        atomic-commit started returning ENOMEM, which the
        prior recovery path then busy-looped retrying, which
        eventually triggered the GPU context-lost
        (`VK_ERROR_DEVICE_LOST`).
        Fix:
        - `record_compose_v2` takes a `signal_fence` (the
          compose fence) + signals `gpu_submitted: &mut bool`
          to the caller. Stops resetting BoState on failure.
        - New `failed_submit_bos: VecDeque<FailedSubmitBo>`
          per `OutputSceneState` parks each
          (bo_idx, pool_slot, compose_ticket) so the BO + the
          descriptor-pool slot survive the failed flip.
        - New `retire_failed_submit_bos` polls each ticket
          every tick; once signaled, calls the new
          `platform.recycle_failed_submit_bo(output_idx, bo_idx)`
          helper (which resets `BoState` properly) and
          releases the pool slot.
        - `next_submit_retry_at` adds a 100 ms commit-retry
          back-off so we don't spin re-submitting in the
          failure window (with `TODO(stage-5 perf)` note for
          per-driver tuning).
        - Defensive: closes `out_fence` if the kernel returns
          one alongside the error.

      Additional polish landed in the same diff:
      - **`do_dump_scanout_v2`** (~150 LoC): real PPM
        readback via Vk staging buffer + image-to-buffer
        copy; was a `log_v2_gap` before. `yserver-v2-scanout-<n>-out<i>.ppm`
        per dump.
      - **`drain_page_flip_events`**: drains DRM events and
        returns per-output indices; `on_page_flip_ready` only
        ticks the outputs that actually flipped (was looping
        over all outputs blindly).
      - **`SceneCompositor::wake_for_damage`** (cheap dirty
        bit) vs **`mark_scene_structure_dirty`** (full-output
        rect) split — cursor motion + `mark_dirty` use the
        cheap path; map/unmap/configure/restack/redirect/
        root-background changes use the coarse path.
      - **`add_projected_damage`** helper clips to output
        extent before adding to `RegionSet` (was missing —
        could push out-of-bounds rects into the scissor list).
      - **Compose fence threading**: `PendingAck.ticket` was
        `None` before. Now a real `FenceTicket` is stored and
        `touch_render_fence` runs against every sampled
        drawable so retire can't free them mid-compose. This
        closes a latent I6a hole.
      - **Telemetry additions**: `compose_cb_record_ns`,
        `damage_pixels`, `scene_entries`,
        `full_redraw_fallback`, `descriptor_allocations`,
        `missed_pageflip`. Most listed as "Required counters"
        in the spec's perf gates section but never wired.

      239 lib + 18 ignored v2 Vk + 8 v2_acceptance tests
      green; clippy clean. Hardware smoke green per
      user-report (cursor tracks, server no longer locks up).

      Open follow-ups:
      - **No unit tests for the failed-submit recovery
        path** — `retire_failed_submit_bos`,
        `recycle_failed_submit_bo`, the back-off gate all
        landed without coverage. Hard to test without a
        fault-injection harness, but a synthetic "atomic
        commit fails N times" test would catch regressions.
      - **100 ms back-off is hardcoded** with `TODO(stage-5
        perf)` — should become a tunable + observable via
        `commit_retry_backoff_ms` telemetry counter for
        per-driver tuning.
      - **`set_container_background_pixmap`** uses
        `copy_area` which doesn't honour X11 bg_pixmap
        tiling. v1-parity for now; proper tiling is a
        follow-up.
    - [x] **3f.10 — port v1's PixmapPool to v2 landed
      2026-05-16.** MATE hardware-smoke (post-3f.9) showed
      ~90 pixmap allocate/free cycles per second (3447
      CreatePixmap + 3422 FreePixmap over 38 s). Without a
      pool, every CreatePixmap pays a full
      `vkCreateImage` + `vkAllocateMemory` + `vkBindImageMemory`
      + `vkCreateImageView` cycle, freed symmetrically per
      `FreePixmap`. v1's `kms::vk::pixmap_pool::PixmapPool`
      already has the right shape — bucket-cap=32,
      max_pooled_dim=128, keyed by (w, h, format), thread-safe
      Mutex internals — and ported verbatim:
      - `PlatformBackend` grows
        `pixmap_pool: Option<Arc<PixmapPool>>` (None on the
        test fixture; Some on `open_with_commit`).
        `register_for_telemetry` runs so the existing
        `GLOBAL_LATEST_POOL` telemetry hook also surfaces v2
        pool stats.
      - `Storage` grows a `from_pooled` constructor that
        adopts a recycled (image, view, memory) triple +
        inherits the pool entry's tracked `current_layout`
        so the next op's barrier issues the correct
        `old_layout`.
      - `PlatformBackend::allocate_drawable_storage` tries
        `pool.try_take(key)` before falling through to fresh
        Vk allocate.
      - `Storage::destroy` tries `pool.try_return(key, entry)`
        before destroying handles; bucket-full / ineligible
        falls through to synchronous destroy.
      - `PlatformBackend::disable_output` calls `pool.drain()`
        after `device_wait_idle` so recycled triples don't
        leak across VkContext teardown.

      1 new unit test
      (`storage_from_pooled_inherits_layout_and_dims` —
      pool-take inherits SHADER_READ_ONLY_OPTIMAL not the
      fresh-alloc UNDEFINED). Pool-internal logic was already
      tested in `kms::vk::pixmap_pool::tests` from v1.

      240 lib + 18 ignored v2 Vk + 8 v2_acceptance tests
      green under lavapipe; clippy clean. Hardware smoke
      gate: pool telemetry counters (`total_takes_hit`,
      `total_takes_miss`) under MATE should show high hit
      rate after 30 s of GTK churn — Stage 3f.5 will capture.

      Out-of-scope: pool stats are not yet emitted on v2's
      per-second telemetry line. `GLOBAL_LATEST_POOL` hook is
      registered, so the existing v1 emitter sees them too;
      a v2-side per-second emitter line is post-3f.5 polish.
    - [x] **3f.11 — reparent removes from top_level_order +
      ConfigureWindow stack_mode for top-levels landed
      2026-05-16.** Two related fixes after the MATE clock-
      applet PPM dump showed a left-edge ghost of the right-
      edge clock:
      - `reparent_subwindow` now reconciles
        `core.top_level_order` with the resolved parent. Pre-
        fix, an xid registered as a top-level (parent=root)
        stayed in the order even after being reparented INTO
        another container. `build_scene` then emitted the
        same window twice — once at child-relative coords via
        the top-level walk (treated as absolute → (0,0)) and
        once at its real position via the parent's recurse.
        MATE clock-applet was the reproducer: created with
        parent=root, reparented into mate-panel's container,
        rendered at both edges. Click on the LEFT ghost hit-
        tested against the RIGHT clock's geom (since geom.x
        was correctly updated by reparent) — visual at left
        + input-route at right confirmed the same-window-
        emitted-twice diagnosis.
        Belt-and-suspenders fix: `destroy_subwindow` also
        drops from top_level_order so a destroyed top-level
        doesn't ghost-render until the next register_top_level
        fills the slot.
      - `configure_subwindow` honours `stack_mode`: caja-
        desktop occluded mate-panel because v2 dropped the
        `stack_mode` field from `HostSubwindowConfig`. marco's
        `ConfigureWindow stack_mode=Below` was a no-op in v2;
        caja-desktop (last-registered top-level) drew on top
        of everything. Ported v1's `restack_window` for the
        top-level case — Above/Below/TopIf/BottomIf/Opposite
        all route through `core.top_level_order`. Subwindow
        sibling stack order is still HashMap-iteration-order
        (post-3f.11 polish; doesn't affect caja-on-top).
      Tests: 5 new unit tests covering both bugs +
      restack-corner cases. 245 lib tests green; clippy
      clean. Hardware smoke confirmed by user:
      cursor tracks, MATE panels render correctly, caja-
      desktop stays beneath.
    - [x] **3f.12 — gradient src/mask collapses to first-stop
      SolidFill landed 2026-05-16.** MATE caja PPM dump
      revealed caja's offscreen render buffer was painted
      black-with-isolated-widget-rects. Cause: every Cairo
      widget background uses an XRender Composite op with a
      gradient source; v2's `render_composite` logged a gap
      and bailed; caja's pixmap stayed mostly UNDEFINED Vk
      content; CopyArea propagated that to the on-screen
      window.

      Stage planning miss (same shape as 3f.6 / 3f.7): a 3c.2
      comment promised "gradient src bails to 3e", but 3e's
      actual plan was trapezoids + triangles + copy_plane and
      never picked up gradients. Gradients are mentioned only
      in 3e's Risks list as "risk-listed for follow-up." Fell
      through the gap.

      Pragmatic fallback (not the real fix): in
      `resolve_picture_for_render`, collapse a LinearGradient
      / RadialGradient picture to a SolidFill of its first
      stop's premultiplied colour. The existing SolidFill
      path in `engine.render_composite` already works end-to-
      end. Most GTK gradients are mild light→lighter so flat
      first-stop colour is visually approximate. Same
      collapse benefits `render_composite_glyphs` — used to
      drop with `composite_glyphs_dropped_unsupported++`,
      now gradient glyph paints flow through.

      v1's `kms::vk::gradient::GradientPicture` (256×1 LUT
      for linear, 256×256 for radial) is fully built and in
      tree — just not wired into v2. **Real fix tracked as
      3f.13 below.**

      245 lib tests pass; clippy clean. Test renamed:
      `v2_composite_glyphs_non_solidfill_source_drops` →
      `v2_composite_glyphs_gradient_source_collapses_to_solidfill`
      (counter stays at 0 since gradient now flows through).
    - [x] **3f.13 — full gradient LUT sampling landed
      2026-05-16 (`5031e39`).** v1's `GradientPicture` (linear
      256×1, radial 256×256) wired into v2's
      `engine.render_composite` + `render_traps_or_tris`.
      `render_create_linear_gradient` /
      `render_create_radial_gradient` eagerly build a
      `GradientPicture` and store on
      `engine.picture_paint[xid]`; `ResolvedSource::Gradient`
      arms bind the gradient image_view + extent and compose
      the `axis_projection` affine with the user transform.
      `render_free_picture` drops the entry via the existing
      `picture_paint_remove` hook. `composite_glyphs` path
      keeps the 3f.12 first-stop SolidFill collapse — glyph
      pipeline is SolidFill-only — but no longer bumps
      `composite_glyphs_dropped_unsupported` (factored into
      `first_stop_premul_of_gradient` helper). 5 new tests (3
      Vk-backed: linear ramp pixel-correctness, radial centre+
      rim, missing-picture gap; 2 logic: resolve-as-gradient,
      free-drops-record). Fuji hw-smoke confirmed gradient
      rendering by user 2026-05-16.
    - [x] **3f.14 — bg_pixmap tiling + window-storage init
      landed 2026-05-16 (`408e197`).** Two fixes from post-
      3f.10 smoke:
      - `set_container_background_pixmap` routes through
        `engine.render_composite` with `OP_SRC + Repeat::Normal`
        across full root extent (single submit), not a single
        copy_area at (0,0). fvwm3 wallpaper now tiles edge-to-
        edge.
      - `default_window_init_color(depth)` paints fresh window
        storage when `bg_pixel == None` (caja drag artefact
        from 3f.10 pool-take). Depth-32 → transparent black
        (premul no-op for compositing); other depths → opaque
        black. Applied in both `allocate_window_storage` and
        the `configure_subwindow` resize path.
      3 new tests (2 Vk-backed, 1 logic).
    - [x] **3f.15 — submit aggregation for stroke ops landed
      2026-05-17.** PolySegment / PolyLine / PolyRectangle /
      PolyArc / PolyPoint fan-outs no longer pay O(N) submits.
      New `RenderEngine::fill_rect_batch(target, color, &[Rect2D])`
      records every rect into a single `cmd_clear_attachments`
      call (Vulkan natively accepts a `ClearRect` slice) inside
      one CB + one queue submit + one `SubmittedOp`. Single
      layout-transition pair per batch instead of per rect.
      `RenderEngine::fill_rect` keeps its one-rect signature for
      the create_pixmap / bg_pixel / image_text-bg / root-init
      / window-init call sites by delegating to `fill_rect_batch`
      with a 1-slice. `KmsBackendV2::fill_solid_rects` (the
      shared lowering for every solid stroke op) drops its
      per-rect for-loop in the `GcFunction::Copy` arm and calls
      `fill_rect_batch` with the full slice; `record_paint_submit`
      fires once per call, matching the non-Copy `logic_fill`
      path's shape. Zero-sized rects are filtered up-front so an
      all-empty batch never burns a CB / fence ticket. Non-Copy
      logic-op stroke ops were already coalesced by
      `logic_fill` (3f.2); no change there.

      Effect on the worst-case workload: an 8-segment
      `PolySegment` request that pre-3f.15 drove ~50 paint
      submits now drives 1. fvwm3 drag stutter + caja "hang"
      should both ease; Stage 3f.5 hardware smoke is the
      load-bearing gate.

      2 new tests: `fill_rect_batch_one_submit_for_n_rects`
      (engine-level Vk-backed: 3 disjoint rects on a 16×4 BGRA8
      pixmap pre-cleared blue, asserts `inner.submitted` grew
      by exactly 1 across the call + pixel-correct per-rect red
      + blue background); `v2_poly_segment_coalesces_to_one_submit`
      (acceptance Vk-backed: drives 8 segments via
      `Backend::poly_segment`, asserts `telemetry.lifetime
      .paint_submits` delta is 1 and `queue_submit2` delta is
      1). 249 lib + 23 ignored v2 Vk + 16 v2_acceptance tests
      green under lavapipe (3 pre-existing failures unrelated
      to 3f.15: see "Open follow-up from 2026-05-17 smoke"
      below); clippy clean. Hardware smoke (fvwm3 drag,
      caja-on-mate) deferred to 3f.5.

  ### Shared-Vk and v2-storage fixes landed 2026-05-16/17
  ### (from xeyes-on-mate-marco hardware smoke)

  All Vk path; v1 and v2 share the underlying code (shaders +
  `MaskScratch` + `DrawableStore`). xeyes was the load-bearing
  reproducer — it exercises every weak point: SolidFill traps,
  shape-clipped offscreen pixmaps, Present-Pixmap, and rapid
  resize via the WM frame.

    - [x] **MaskScratch IDENTITY attachment view (`dae5769`).**
      `MaskScratch` viewed its R8 image with `a=R` swizzle for
      the composite-side mask sample. The SAME view was bound as
      a color attachment for the trap rasterize phase. Vulkan
      VUID-VkFramebufferCreateInfo-pAttachments-00891 requires
      IDENTITY swizzle on attachment views; lavapipe is lenient,
      Intel + RADV strict — the rasterize writes were undefined
      (typically zero) → mask sampled 0 → trap composite added
      nothing to dst. xeyes' pupils never appeared on hardware;
      eye whites (different geometry / coverage) sometimes did.
      Fix: two views on the same image — `view` (a=R swizzle)
      for sampling, `attachment_view` (IDENTITY) for the
      attachment binding. Wired in both v2 (`engine.rs`) and v1
      (`kms/backend.rs`). 3 new acceptance tests
      (back-to-back-trap-different-SolidFill, large-bbox, single
      trap).
    - [x] **Synthetic-1×1 `REPEAT_PAD` override in
      `render_traps_or_tris` (same commit).** Mirrors
      `render_composite`'s existing override. SolidFill sources
      with `Repeat::None` were sampling a 1×1 src image with
      shader-side `REPEAT_NONE` — UV outside `[0, 1]` returns 0
      from `apply_repeat`. Fragments at `dst_offset > 0` zeroed
      the source; the composite added nothing. Latent — only
      surfaced once the MaskScratch swizzle fix made the
      rasterized mask non-zero.
    - [x] **`trap.frag.glsl` horizontal-edge AA off-by-0.5
      (`e76a6f6`).** `c_top = clamp(p.y - top, 0, 1)` was the
      formula; should be `clamp(0.5 + (p.y - top), 0, 1)` to
      match the slanted-edge convention (pixel center on edge =
      0.5 coverage). Adjacent stacked traps sharing a non-
      integer Y boundary (xeyes' eye whites are 16 such
      trapezoids) under-covered the shared row by ~0.7. Visible
      as horizontal stripes inside the eye whites. Same shader
      shipped with v1; v1 had the bug too. Regression test
      `v2_adjacent_trapezoids_share_horizontal_boundary_cleanly`.
    - [x] **`decref → PendingFence` detaches `by_xid` (`5027cc2`).**
      When the parked drawable's xid mapping stayed alive,
      `configure_subwindow`'s `decref → alloc(same_xid)` got
      `XidInUse` → silently kept old storage. xeyes resize
      visibly broken.
    - [x] **`destroy_now` only removes `by_xid` if still mapped
      to this id (`4115fc8`).** Follow-on to 5027cc2. When the
      parked old drawable's fence finally signaled,
      `destroy_now`'s blanket `by_xid.remove(xid)` nuked the NEW
      drawable's xid mapping. Scene then couldn't find the
      resized window storage; stale prior content surfaced.
    - [x] **`detach_xid` runs unconditionally on resize
      (`3afa5bd`).** A Picture wrapping a window holds an extra
      refcount on the backing drawable; `decref` returned
      `StillReferenced` and left `by_xid` intact. Re-alloc
      failed with `XidInUse`. New `DrawableStore::detach_xid`
      removes the mapping without touching refcount;
      `configure_subwindow` calls it before the
      decref + allocate sequence. Picture's next
      `store.lookup(xid)` returns the NEW DrawableId (matches
      X11 RENDER semantics).
    - [x] **`create_pixmap` zero-fills new storage (`fcd2521`).**
      X11 leaves pixmap content "undefined" but real X servers
      get away with it because system allocators zero pages.
      Vk DEVICE_LOCAL memory is fully undefined (random
      recycled GPU bytes). xeyes uses SHAPE-clipped drawing
      into an offscreen pixmap; the non-eye-shape pixels of the
      pixmap held garbage; Present-Pixmap copied that to the
      window storage; massive visible noise around the eyes.
      Fix: `engine.fill_rect` on every `create_pixmap` with
      `default_window_init_color(depth)`. Regression test
      `v2_fresh_pixmap_reads_back_zero`.
    - [x] **Dispatch default flipped to v2 (`3afa5bd`).** Status
      doc claimed v2 was the boot default but `dispatch.rs`
      had v1 as the fallthrough. Three consecutive hardware-
      smoke sessions silently tested v1 because the `yserver-*-hw`
      Justfile recipes don't set `YSERVER_RENDER_MODEL`. Now:
      unset / empty → v2; `=v1` is the explicit fallback.

  ### Open follow-up from 2026-05-17 smoke (not yet diagnosed)

    - [ ] **xeyes resize-DOWN artefact on mate + marco.**
      Resize-UP works clean after the above fixes. Resize-DOWN
      shows xeyes with eyes drawn for a *wider* geometry than
      the current window — eye 2 visibly cut off at the right
      edge. No new v2 warnings during the shrink other than
      marco's existing COMPOSITE-related gaps (`name_window_
      pixmap` stubbed `Err`, Stage 4 territory). Two
      hypotheses, neither confirmed:
      1. xeyes internal state stale — its eye geometry trails
         the pixmap dims on rapid drag-shrink. Would be an
         xeyes-side race; verify by stopping the drag and
         waiting 2-3 seconds before dumping the scanout (if the
         eyes resolve to the correct smaller shape, it's this).
      2. v2 scene compositor blits stale storage / mismatched
         storage extent vs window geom. The `decref → detach →
         alloc` chain seems correct but might still have a
         pending-ack path that captures the old DrawableId.
      Also visible during the shrink: many `render_composite
      gap: host_src 0x40xxxx not resolvable` lines from marco's
      decoration compositing — depends on `name_window_pixmap`
      (stubbed `Err` on v2). Real fix is Stage 4; the noise
      isn't a v2 regression.
    - [ ] **MATE panel flicker on v2.** Reported during the
      same session, not yet diagnosed. Could share a root cause
      with the xeyes-shrink bug (rapid configure_subwindow on
      panel applet activity) or be its own scene-damage issue.
      Worth capturing a focused trace+log when picked up.
    - [ ] **Lavapipe-only Vk test flakes** (surfaced during
      3f.15 close, reproduce on the 3f.14 baseline — *not* a
      3f.15 regression). Hardware-smoke on RADV / Intel is the
      gate of record; these are diagnosis owed for the
      lavapipe-only CI loop:
      1. `render_composite_linear_gradient_horizontal_two_stop`
         — reads `BGRA=(0,0,0,255)` instead of the colored
         ramp at the right edge. 3f.13 commit message
         explicitly notes "Fuji hw-smoke confirmed gradient
         rendering by user 2026-05-16," so the live wire path
         works; failure likely sits in lavapipe's gradient-
         sampler corner.
      2. `render_composite_radial_gradient_centred` — rim
         pixel reads black instead of near-white. Same
         3f.13 LUT path; same lavapipe-vs-real-HW shape.
      3. `v2_set_container_background_pixmap_tiles_across_root`
         — SIGSEGV in the test binary (3f.14 tile path).
         Hard crash, not a pixel-mismatch; this one needs
         a real triage pass (binding lifetime? `Repeat::Normal`
         sampler under lavapipe?) since it affects the rest
         of the acceptance run by aborting the binary.

    - [ ] **3f.5 — acceptance.** rendercheck parity, real-app
      smoke matrix (xterm / xclock / xeyes / gedit / MATE /
      xfce4 / xfd), bee 30-min stability, fuji v1/v2 perf
      capture diff. Stage 3 close. **Depends on 3f.6 + 3f.7
      + 3f.11** (subwindow + input + stacking) — visual,
      input, and z-order all required for matrix clients to
      reach their first paint. 3f.12-3f.15 are observed/
      recorded but not blocking.

  ### Stage 3f planning-gap retrospective

  Substages landed during 3f close that were NOT in the
  original Stage 3 plan:
  - **3f.6 subwindow scene composition** — spec
    §scene-layering item 2 ("top-level + descendants") was
    deferred to Stage 4 in our stage plan; the cursor-trail
    diagnosis proved the deferral was unsound. Codex picked
    this up in 3f.9 with root storage + descendant recurse.
  - **3f.7 input dispatch** — no substage owned it; the spec
    only listed input as a `PlatformBackend` primitive.
  - **3f.13 full gradient LUT** — Stage 3c.2 comment
    promised 3e, but 3e's plan didn't include it.
  - **MaskScratch + trap-shader fixes (2026-05-17)** — the
    Vk-spec attachment-swizzle violation + AA off-by-0.5
    were latent v1+v2 bugs in shared shader/Vk code that
    lavapipe accepted but Intel/RADV rejected. Stage 3a
    landed `MaskScratch` shape verbatim from v1; no
    cross-check against the Vk spec or against multi-driver
    hardware caught the swizzle issue. xeyes was the first
    real-app that exercised the trap path enough to surface
    it.
  - **DrawableStore xid-detach semantics on PendingFence /
    refcount > 1 (2026-05-17)** — Stage 2b's `decref` was
    designed for the simple "refcount → 0 → destroy" case.
    Resize-with-Picture-refcount and resize-with-in-flight-
    fence weren't planned; both surfaced via xeyes on mate
    + marco. The fix split (5027cc2 + 4115fc8 + 3afa5bd) is
    iterative because each layer of the bug only became
    visible after the prior was addressed.

  Common pattern: spec-correct invariant got deferred / lost
  in stage planning; or Vk-spec-correct code wasn't verified
  against strict drivers because lavapipe was lenient. Future
  stages: an explicit "spec invariant coverage" checklist
  per stage (X11 + Vk both) would catch these. Lavapipe
  smoke is *necessary but not sufficient*; real-GPU smoke
  (Intel KBL or fuji minimum, RDNA2 / bee for the strictest
  driver coverage) needs to gate stage close, not be
  reserved for the final acceptance pass.
- [ ] **Stage 4 — re-enable COMPOSITE + COW.** Manual-redirect
  backing routing, NameWindowPixmap, scene treats COW as
  always-on-top entry. xfce drop-shadow renders correctly. picom
  composites and updates per Damage event.
- [ ] **Stage 5 (optional) — advanced perf strategies.**
  Strategy plug-ins on the existing components: damage-strategy
  selection per frame, HW cursor return, direct scanout, HW
  plane assignment, submit aggregation, multi-queue,
  DRM in-fence / syncobj submission.

### v1 deletion gates (post-Stage-4, see Risk 4 in the spec)

v1 stays in tree past Stage 3 close. Deletion happens only when
**all** hold: v2 has been the default for ≥1 month, no v2-only
regression open, Stage 4 landed and validated on hardware,
measured perf gates pass (correctness gates v1 fails + no
regression on v1's good cases + headroom gates where v2 should
be measurably better), maintenance cost felt to outweigh
fallback value.

---

## Followups not on the v2 critical path

See `known-issues.md` for the full ticklist. Highlights tracked
here for awareness during stage planning:

- [ ] **`disable_output` atomic EINVAL** — recurring shutdown
  warn; disarm path mitigates but per-property split is the real
  fix. Survives the rewrite (lives in `PlatformBackend`'s
  shutdown sequence).
- [~] **Per-glyph queue_wait_idle in `GlyphAtlas::intern`** —
  v1-era TODO. Stage 3 plan §3a removes the persistent staging
  buffer + per-upload wait by routing through arena slices
  owned by `SubmittedOp`. Landed when 3a-atlas+text commits.
- [ ] **AMD-specific investigation** — bee + adapta-mate-cc
  catastrophic mouse lag. Independent of model choice (submit-
  rate bound). Tackled via separate perf plans built on v2; see
  spec's per-hardware-class expectations.
- [~] **GTK wheel-scroll warm-up race** — non-deterministic
  initial-app residual after the XI2 valuator-scroll fix.
  Unrelated to rendering; lives at `process_request.rs` /
  pointer-event pump. Unaffected by v2.

---

## Source-of-truth pointers

- v2 spec: `docs/superpowers/specs/2026-05-15-rendering-model-v2.md`
- v1 rendering re-architecture HLD:
  `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md`
  (v2 supersedes in motivation; the v1 delivered work — Phases
  3-5, pool, GPU traps — is still in tree on `graphics-followups`)
- Cross-cutting bugs: `known-issues.md`
- v1 history: `status-archive-2026-05-15.md`
- Pre-rework history: `status-archive-2026-05-13.md`
- Per-skill memory: `~/.claude/projects/-home-jos-Projects-yserver/memory/MEMORY.md`
