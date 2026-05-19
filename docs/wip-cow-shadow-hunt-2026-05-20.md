# WIP — COW lazy-register + audit #11 (2026-05-20)

**Status**: WIP, in tree but **does not work**. Picking up tomorrow.

## What's in the branch

Two related changes, both load-bearing for the unresolved question
"how does v2 show compositor shadows without losing per-window content."

### 1. Lazy `register_cow` on first paint into COW (scene.rs + backend.rs)

`SceneCompositor::register_cow` was defined but no live backend ever
called it — the live `inner.cow` stayed `None` forever, so `build_scene`
never emitted the Composite Overlay Window as a scene layer, and any
compositor that paints via `Present::Pixmap → COW` (picom-style,
several marco builds, etc.) had its full-screen composited image
land in COW storage but never reach the scanout.

The change:

- Move `cow: Option<DrawableId>` from `SceneCompositorInner` to the
  outer `SceneCompositor` so the stub fixture (no Vk) can also track
  registration state.
- Thread `cow` through `tick` → `tick_one_output` → `build_scene` as
  a parameter (was read from `inner.cow` previously).
- Add `SceneCompositor::is_cow_registered`.
- New `KmsBackendV2::maybe_register_cow_on_paint(target_id)` helper.
  Called from `copy_area`, `put_image`, `render_composite`,
  `render_composite_glyphs`, `render_fill_rectangles`,
  `render_trapezoids`, `render_triangles_op` — every paint vector
  whose `resolve_paint_target` could resolve to the COW storage.
- `release_overlay_window` final-release calls
  `self.scene.unregister_cow()` before dropping storage.
- New test `v2_paint_into_cow_registers_scene_entry` pins both the
  pre-paint negative invariant (allocation alone must not register)
  and the post-paint registration.

**Design intent** of "lazy on first paint": xfwm4's compositor
allocates COW (so `XCompositeGetOverlayWindow` returns a valid xid)
but paints into a child compositor window — never into COW. An
eager registration would emit the zero-filled depth-24 COW as a
scene-top force-opaque layer (sample-side swizzle forces α=1.0 on
depth-24) and cover the actual output with solid black. The
lazy hook only registers when the compositor actually writes
into COW.

### 2. Audit #11 — `MapWindow` doesn't emit damage (process_request.rs)

`handle_map_window` + `handle_map_subwindows` now call
`accumulate_damage_full_to_state(state, window)` after
`backend.map_subwindow`. Xorg's `miPaintWindow` fires damage on the
window's extent when it becomes viewable (the server-background
fill is itself a paint, and DAMAGE hooks every paint via
`miext/damage/damage.c`). yserver was filling the background but
not firing the damage notify, so compositors subscribed via
`XDamageCreate(window)` missed the first frame and the window
stayed invisible on COW until the next paint into it.

New test `map_window_emits_damage_on_window_extent` pins the
invariant: a `DamageObject` subscribed to a window has non-empty
rects after `handle_map_window` returns.

**Status**: passes unit + Vk-backed integration tests. Hardware
smoke: shows the regression described below — so this fix alone
is not sufficient (and is not the proximate cause of the
regression, but landing it in the same commit because reverting
the COW fix without it would re-open audit #11 once we revisit
shadows).

## Confirmed compositor-dependent: XFCE is unaffected

User reports the regression does NOT reproduce under XFCE. xfwm4's
built-in compositor paints via RENDER into a child compositor
window of root (its own depth-32 stage), not via `Present::Pixmap`
onto COW. So `maybe_register_cow_on_paint` never fires under XFCE,
`inner.cow` stays `None`, v2's per-window scene walk wins, all
working.

The MATE case has a separate compositor (client 015 in the
`mate.xtrace` from this run: `RedirectSubwindows(root, Manual)` +
`GetOverlayWindow` + `Present::Pixmap` onto COW) that's the
trigger. Likely picom or a marco build with Present output —
verify by `ps` next session.

So the lazy-register design correctly *avoids* the xfwm4 trap. The
remaining failure is specifically "Present-Pixmap-onto-COW
compositor + incomplete-COW + force-opaque depth-24". Three things
to address; if only one is the proximate fix, it's likely the
"COW must be alpha-aware so non-painted regions are transparent
through to v2's walk" angle.

## The regression (hardware smoke, 2026-05-20 ~01:20 yoga / mate)

User observation, paraphrased:

> initially all applets were showing and the menu worked. after caja
> draws the icons everything breaks.

State transition pinned: it's the moment the compositor first
`Present::Pixmap`s onto COW, which happens when caja's first
desktop-icon paint generates damage the compositor can read.

**Before that first Present**: `inner.cow = None`, v2's per-window
scene walk emits every redirected top-level's backing → user sees
everything (panels, applets, popups, CC frame, drag preview).

**After that first Present**: `inner.cow = Some(cow_id)`,
`build_scene` appends COW as a top-of-z force-opaque depth-24
draw → user sees ONLY what the compositor put in COW.

The compositor's COW image is incomplete in practice:
- nm-applet missing
- mate-panel popup menus missing
- Window content disappeared during drag (but visible in pager —
  proves the backing has content, just not on scanout)
- Several applets missing

The drawable dump (compositor's pre-Present offscreen) at the same
moment as the broken scanout DOES include the Control Center, but
the scanout doesn't. Either COW isn't being drawn (despite
`register_cow` firing) or it's being drawn but doesn't match the
Present source. **Diagnostic not yet conclusive** — needs the
trace-level COW probe line that was temporarily added and removed.

## Why this design is wrong (architectural)

Even with audit #11 + #12 + #13 + every other damage-delivery gap
closed, the floor is fragile. **Force-opaque depth-24 COW means
ANY scene layer below it is invisible.** Mixing "compositor's COW"
+ "v2's per-window scene walk" is a hard cutover, not a graceful
blend.

Two viable end-states:

1. **COW-only mode**: when a compositor is active, the v2 scene
   strips top-levels entirely and ONLY emits root + COW + cursor.
   The compositor's COW is fully authoritative. Requires all
   damage-delivery gaps closed (#11 done, #12, #13, possibly
   more) and probably proper ARGB COW so partial-frame state
   doesn't show as opaque black.

2. **No-COW mode**: never show COW (current pre-fix floor). v2's
   per-window scene walk wins. Shadows are missing because the
   compositor's shadow blits live in COW. This is the regression
   the user originally complained about ("we still don't have
   working shadows").

The lazy-register-on-first-paint approach mixes the two and
inherits the worst of both: COW kicks in late, hides what was
working, and the compositor's COW is not complete enough to
replace the v2 walk.

## Picking back up tomorrow

Choices ranked by safety + likely user satisfaction:

1. **Revert just the COW registration (back to no-shadows floor).**
   Keep audit #11. Track shadow-recovery as a real Stage 4e
   substrate task (PictFormat alpha + complete damage + ARGB
   COW). Most pragmatic; restores everything to working.

2. **Gate `register_cow` behind `YSERVER_V2_COW_ENABLE=1` env var.**
   Lets the user opt in to "shadows but breakage" or stay on
   the working floor by default. Cheap.

3. **Dig into why COW-on-scanout doesn't match the Present source.**
   Add diagnostic, re-run smoke, find the gap. Even if we fix it,
   we still hit the "compositor's COW is incomplete in practice"
   tax until #12, #13, and the broader damage-completeness work
   land.

If continuing with the shadow fix: at minimum needs #12 (DamageSubtract
RepeatNotify), #13 (Raw/Delta DamageNotify throttle), and probably a
PictFormat-aware ARGB COW. And the static evidence that the
compositor's offscreen really does contain everything visible — the
drawable dump suggests yes, but the scanout disagrees, which is
the proximate mystery to solve before any of the above pays off.

## Files touched

- `crates/yserver-core/src/core_loop/process_request.rs` — audit #11
  fix + `map_window_emits_damage_on_window_extent` regression test.
- `crates/yserver/src/kms/v2/backend.rs` — `maybe_register_cow_on_paint`
  helper, hook calls in seven paint methods, `unregister_cow` on
  release-overlay-window final, `test_scene_cow_registered` doc-hidden
  accessor.
- `crates/yserver/src/kms/v2/scene.rs` — `cow` field moved from
  `SceneCompositorInner` to `SceneCompositor`, threaded through
  `tick` / `tick_one_output` / `build_scene`, `is_cow_registered`
  accessor.
- `crates/yserver/tests/v2_acceptance.rs` —
  `v2_paint_into_cow_registers_scene_entry` regression test.

## Test state at WIP commit

- `cargo test --workspace` — 932 pass, 0 fail, 79 ignored.
- `cargo test --workspace -- --include-ignored` — all pass except
  pre-existing `dri3_fd_leak::dri3_import_loop_does_not_leak_fds`
  (fixture limitation, fails on `8859492` baseline too — not from
  these changes).
- `cargo +nightly fmt`, `cargo clippy --all-targets` clean for the
  touched lines; pre-existing warnings unchanged.
