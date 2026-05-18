# Stage 4d compose-debug checkpoint — 2026-05-18

Session checkpoint for resuming the mate-with-compositing "CC visible/
disappears" investigation. Branch: `rendering-model-v2`.

## What landed (committed)

- `c3bc948` — fix(kms-v2): format-aware `sample_view` on Storage; scene
  binds it. Fixed depth-24 α-leak in scene composition.
- `6bdfffc` — fix(damage): walk ancestor windows when fanning out damage.
  X11 DAMAGE spec compliance — descendant paints now notify ancestor
  damage objects. Test: `xfixes_subtract_region_partial_overlap_returns_remaining_bands`.

## What's uncommitted (working tree)

Three landed fixes (with passing tests) + a pile of diagnostic
instrumentation. Tests: `cargo test --workspace --lib --tests`
→ 300 + 312 + 209 + ... all green. Clippy clean.

### Fixes

1. **XFIXES `SUBTRACT_REGION` bug fix** (`process_request.rs`,
   `nested.rs`)
   - Pre-fix: dispatcher returned `Vec::new()` whenever a and b overlapped.
   - Post-fix: calls `nested::subtract_regions(&a, &b)` (made `pub(crate)`).
   - Test: `xfixes_subtract_region_partial_overlap_returns_remaining_bands`
     pins partial-overlap returning the remaining bands instead of empty.
   - **Not** the load-bearing cause for current mate symptoms — marco
     doesn't use SUBTRACT_REGION in this session (only 3 XFIXES calls,
     all `SET_CURSOR_NAME`). But a real latent bug.

2. **`SetPictureClipRectangles` empty-list semantics fix** (`v2/backend.rs:6630`)
   - Pre-fix: `*clip = if rects.is_empty() { None } else { Some(rects) };`
     — empty list collapsed to `None` (paint everywhere).
   - Post-fix: `*clip = Some(rects);` — empty list stays `Some(vec![])`
     (paint nothing, per X11 RENDER spec).
   - Test: `v2_set_picture_clip_rectangles_empty_list_is_empty_clip_not_no_clip`.
   - **Disputed**: matches X11 spec + Xorg source (`RegionFromRects(0,
     NULL)` → empty region → intersect → no paint). But marco appears to
     send `SetClip(n=0)` before composites and expects them to paint.
     Either marco's pattern is non-spec OR there's a different mechanism
     we haven't found. See "Open question" below.

3. **`ChangeWindowAttributes` skip-clear-for-redirected-windows fix**
   (`v2/backend.rs:3492-3527`)
   - Pre-fix: every CWA with CWBackPixmap/CWBackPixel triggered
     `clear_window_area_with_background`, which routed through
     `resolve_paint_target` into the redirected backing B and filled
     it with depth-24 default black. Bug exposed by marco re-asserting
     bg_pixmap=None on every drag-induced configure → "CC turns opaque
     black on drag".
   - Post-fix: when W has `redirected_target = Some(_)`, skip the eager
     clear entirely. Per X11 spec, CWA doesn't auto-clear anyway — the
     bg attribute only affects future ClearArea/Expose handling.
   - Test: `cwa_on_redirected_window_does_not_clear_backing`.
   - **Confirmed effective** on hardware: CC backing now preserves its
     content across drag instead of being wiped to black.

### Diagnostic instrumentation (temporary)

- `clear_window_area_calls: u32` field on `KmsBackendV2` (test-only
  observable for the CWA-skip regression test).
- `present_to_cow_sources: VecDeque<u32>` field on `KmsBackendV2` +
  `Backend::note_present_pixmap` trait hook + `process_request.rs`
  wiring. v2's `do_dump_drawables_v2` includes the 16 most-recent
  COW-targeted Present sources in the dump output as
  `yserver-v2-drawable-{run}-present-src-{i}-0x{xid}-...ppm`.
- `Ctrl-Alt-F12` (and `SIGUSR2`) → `Message::DumpDrawables` →
  `dump_drawables` Backend trait method → `do_dump_drawables_v2`.
  Dumps root, COW, every redirected backing, and recent present-sources
  as **PAM (P7, RGBA)** so the α channel is preserved end-to-end (don't
  use the `.ppm` extension to assume RGB-only).
- `pict_format: u32` field on `PictureRecord::Drawable` capturing
  marco's requested PictFormat at CreatePicture time (currently
  instrumentation-only; engine still picks force-opaque via drawable
  depth).
- Tracing additions (all under `RUST_LOG=yserver::kms::v2::*=trace`):
  - `yserver::kms::v2::render` — `render_create_picture`,
    `render_composite` (with full clip rect detail + `<None>`/`<empty>`
    distinction), `render_composite stats`, `render_change_picture`
    (with post-call clip state), `set_picture_clip_rectangles`
  - `yserver::kms::v2::fill` — `fill_rect_batch` with target + color +
    n_rects + first_rect
  - `yserver::kms::v2::store` — `set_redirected_target` (old/new)
  - `yserver::kms::v2::paint` — `resolve_paint_target NO_REDIRECT_FOUND`
    (only when a window xid resolves to leaf id = no redirect found in
    ancestor chain)
  - debug-level: `allocate_redirected_backing` (idempotent + fresh
    paths), `seed_backing_from_window` entry, `DAMAGE::Create` /
    `DAMAGE::Destroy` enriched with drawable + damage_id
- `Justfile`: `yserver-mate-hw` recipe's default `log` arg now passes
  all five trace targets.

## Current debug state

### What worked

- **Sample-view fix** (committed): depth-24 windows / COW no longer
  show garbage α in scene composition.
- **Damage ancestor walk** (committed): marco's damage subscriptions
  on top-level windows now fire when descendants paint. matched-rate
  jumped 3.4× in smoke; behaviorally not the load-bearing visible-bug
  cause, but spec-correct.
- **CWA-skip-for-redirected** (uncommitted): B no longer wiped on drag.
- **Empty-clip = `Some(empty)` semantics** (uncommitted): correct per
  spec; helps the "wallpaper overwrites CC" failure mode.

### What's still broken

Latest mate-with-compositing smoke (2026-05-18 10:25-10:26 UTC):
- CC visible initially (the fixes do work for the open-window case).
- Drag: bits of CC disappear progressively.
- Caja's desktop draw: CC totally invisible.

Dump evidence at 10:26:33 (caja-drew-state):
- **CC backing `B = 0x4003e2`**: contains the full Control Center UI
  (PAM dump shows clean RGB + variable α; CWA-skip fix is doing its
  job).
- **COW + marco's offscreens**: NO CC content. Just wallpaper + desktop
  icons + top-panel gone.
- Marco IS issuing render_composite with src = CC's picture (`0x40048b`)
  onto its offscreens at 10:26:30-32. **All produce `recorded_draws=0`**
  because marco precedes every composite with `SetPictureClipRectangles
  pic=offscreen n=0 rects=[]`. With the empty-clip fix, v2 treats
  `clip = Some(empty)` as "paint nothing" — composites no-op, marco's
  offscreens stay stale.

### The dilemma

Marco's per-frame idiom is `SetClip(n=0) → composite → SetClip(n=0) →
composite → ...`. Both interpretations of `SetClip(n=0)` produce
failure:

- **Empty = `None` (v1 + pre-fix v2):** composites fire → CC painted
  → next full-screen composite overwrites CC ("wallpaper bleeds
  through" / "shadow only" failure mode).
- **Empty = `Some([])` (X11 spec + my fix):** composites no-op → CC
  not painted at all (only the rare `n=N>0` clips actually paint;
  long n=0 storms during drag/caja-draw leave the offscreen stale →
  "CC disappears" failure mode).

X11 RENDER spec + Xorg source agree with `Some([])` semantics
(`RegionFromRects(0, NULL)` → empty region → intersect → no paint).
Yet marco works on Xorg. Mechanism unknown.

### Key xids from the last dump

| xid | role | drawable id |
|---|---|---|
| `0x100` | root | varies |
| `0x103` | COW | varies |
| `0x40002e` / `0x400036` | marco offscreens (depth-24 5120×1440) | 43 / 47 |
| `0x40002f` / `0x400037` | marco offscreen pictures | — |
| `0x40038d` | CC marco frame W (depth-24 997×652) | varies |
| `0x4003e2` | CC backing B (depth-24 997×652) | 538-range |
| `0x40048b` | Picture wrapping CC's B (`pict_format=0x3`) | — |

## Open questions

1. **How does marco actually work on Xorg?** X11 spec + Xorg source
   both say `SetClip(n=0)` = empty region = no paint. But marco's
   idiom on Xorg is the same `n=0`-before-every-composite pattern.
   Either: (a) marco doesn't actually send `n=0` on Xorg (different
   code path), (b) Xorg has a non-spec early-out we haven't found,
   (c) marco's idiom relies on later non-n=0 SetClips covering for
   the no-op composites.

2. **Why does marco enter `n=0`-only state during drag / after caja?**
   At 10:26:32 we see 506 `n=0` SetClips in one second on marco's
   offscreens. Some kind of compositor state where marco believes
   "nothing to repaint" → uses empty clip on every composite. What
   would normally take marco out of this state? Damage events?
   Expose events? Configure events?

3. **Is the empty-clip fix correct?** Spec-compliant, but breaks
   marco's idiom. Reverting matches v1 + Xorg de-facto behavior but
   reintroduces wallpaper-overwrites-CC.

## Decision points for next session

1. **Commit policy.** Three uncommitted fixes (subtract-region, CWA-
   skip, empty-clip-semantics) + ~12 trace additions + 1 hotkey +
   PAM dump format change. Probably ship as 3 commits (one per fix)
   + a 4th squashing the diagnostic instrumentation that should land
   too (or revert post-investigation).

2. **Empty-clip interpretation.** Keep, revert, or replace with a
   smarter heuristic (e.g. "empty + dst is a redirected backing →
   treat as None" — pragmatic compromise).

3. **Next investigation direction:**
   - Audit marco's clip-set/composite pattern in detail (look at the
     specific frames where CC IS visible vs IS NOT visible — what
     differs?)
   - Trace damage events end-to-end during the workload (verify the
     ancestor-walk fix is actually firing for CC; check if marco's
     damage subscriptions on the right xids are alive)
   - Try `picom` instead of marco (different compositor, see if
     the same n=0 idiom appears)
   - Look at Xorg's render dispatcher in detail for a non-spec
     interpretation

## How to resume

```bash
cd /home/jos/Projects/yserver
git status                    # see uncommitted changes
cargo test --workspace --lib --tests  # all green
# Re-smoke: just yserver-mate-hw (recipe includes all trace targets)
# Ctrl-Alt-F12 to dump drawables; PAM format, viewable with imagemagick.
```

Memory files updated:
- `~/.claude/projects/-home-jos-Projects-yserver/memory/MEMORY.md`
  has a new pointer to this checkpoint.

## File pointers

- This checkpoint: `docs/superpowers/2026-05-18-stage-4d-compose-debug-checkpoint.md`
- v2 spec: `docs/superpowers/specs/2026-05-15-rendering-model-v2.md`
- v2 status: `docs/status.md`
- Diagnostic-trace recipe: `Justfile :: yserver-mate-hw`
- Hotkey: `crates/yserver/src/input_thread.rs` (Ctrl-Alt-F12)
- Drawable-dump: `crates/yserver/src/kms/v2/backend.rs :: do_dump_drawables_v2`
