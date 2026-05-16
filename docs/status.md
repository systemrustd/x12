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
- Spec branch: `rendering-model-v2`, off `graphics-followups`.
  Contains only the v2 spec at HEAD. Approved by codex across
  multiple review rounds; ready to drive implementation.
- Abandoned branch: `render-convolution-filter`. Left untouched
  as historical reference for T1-T4 of the Manual-redirect work,
  convolution Phase 1+2, the rotate fix, and the
  parallel-implementation lessons. Don't ship anything from there.

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
  - [ ] **3a-atlas+text** — port `GlyphAtlas` off v1's
    persistent staging buffer (per-upload arena slice owned
    by `SubmittedOp` + `atlas_last_upload_ticket` for CPU
    lifetime); port `TextPipeline`; `DrawableImageRef` adapter
    over v1's `record_text_run`; `RenderEngine::image_text`
    method; `KmsBackendV2` `open_font`/`close_font` delegation
    to `KmsCore`; `image_text8`/`image_text16` + `poly_text8`/
    `poly_text16` decoders (`0xFF` font-change items
    supported). Back-to-back upload race test load-bearing.
  - [ ] **3b — picture records + pipelines.** Promote every
    `render_create_*` / `render_change_picture` /
    `render_set_picture_*` stub; lazy-build
    `RenderPipelineCache` + SolidFill / DstReadback /
    white-mask scratch images on `RenderEngine`; gradient
    parsing.
  - [ ] **3c — `render_composite` + `render_fill_rectangles`.**
    Standard PictOps + Saturate + Disjoint/Conjoint shader
    blend; per-rect picture-clip scissoring; self-composite
    aliasing routed through arena scratch.
  - [ ] **3d — `render_composite_glyphs` + glyphsets.**
    v1-parity scope (Over + SolidFill source +
    A8/A1/ARGB32-as-A8 glyphsets); fixes v1's latent
    "_clip unused" bug for the dst picture clip; broader op /
    source coverage risk-listed for a post-Stage-3 follow-up.
  - [ ] **3e — trapezoids + triangles + `copy_plane`.**
  - [ ] **3f — Core remainder + GC.function + planemask +
    acceptance.** Real-app matrix on hardware, bee 30-min
    stability run, fuji perf captures (v1 baseline taken
    fresh in same session). Stage 3 close.
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
