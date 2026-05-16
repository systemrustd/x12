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

### Pending

- [ ] **Stage 2 — minimal-Vk correct baseline.** Whole vertical
  slice in Vk: `PlatformBackend` real, `DrawableStore` real,
  `RenderEngine` minimal (fill / copy / put_image), `SceneCompositor`
  minimal (z-order blit, buffer-age clipped redraw with full-redraw
  fallback). Synthetic acceptance (no real apps yet — Stage 2
  doesn't paint text).
- [ ] **Stage 3 — RENDER + glyphs coverage.** RENDER pipelines on
  the Vk substrate; text path. First stage where real-app smoke
  is meaningful. Specific counter gates per workload.
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
- [ ] **Per-glyph queue_wait_idle in `GlyphAtlas::intern`** —
  v1-era TODO. Gets folded into v2's `RenderEngine` rewrite
  during Stage 3.
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
