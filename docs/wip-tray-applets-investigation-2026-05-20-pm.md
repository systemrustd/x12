# Tray applets investigation — 2026-05-20 PM session

Picks up where `wip-cow-shadow-hunt-2026-05-20.md` left off. The
COW-authoritative plan (`docs/superpowers/plans/2026-05-20-cow-authoritative-mode.md`)
landed in full this morning; this session was driven by hardware smoke
of that branch.

## Context: plan + what landed

The plan implemented across 8 commits:

- **Phase 1** (3 commits) — `build_scene` strips per-top-level draws when
  COW is registered. Scanout becomes `root + COW + cursor` only. Matches
  Xorg's compositor contract.
- **Phase 2** (5 commits) — `handle_reparent_window` reconciles
  inherited redirect state on reparent. Mirrors Xorg's
  `compUnredirectOneSubwindow` + `compRedirectOneSubwindow`.

Phase 1+2 alone were structurally correct but did not fix the
visible-tray-applets symptom. Hardware smoke after Phase 1+2 showed
the same MATE desktop bugs as before (tray empty, window disappears,
panel popups absent). That led to the rest of this session.

## Five additional correctness fixes shipped

In commit order on `cow-authoritative-mode`:

### `4519645` — diagnostic logging

Added `log::debug!` for the Phase 2 reconciliation branches (entry
condition + REVOKE/GRANT/FLIP/no-op tag) and `REDIRECT_FOUND` mirror
of the existing `NO_REDIRECT_FOUND` resolver trace. Both targets
already routed by `just yserver-mate-hw-trace` (`debug` default level +
`yserver::kms::v2::paint=trace`).

Made the next four fixes possible to diagnose from logs.

### `cad9dec` — CWA on descendant routed to redirected ancestor

`KmsBackendV2::change_subwindow_attributes` had a Stage 4d guard that
skipped the eager `clear_window_area_with_background` when the window
had its own redirected backing. The guard checked only
`self.store.redirected_target(leaf).is_some()` — i.e. the window has
its *own* backing.

That missed the case where a window has no own backing but its paints
route to an ancestor's backing via `resolve_paint_target`'s ancestor
walk. Live trigger: mate-panel tray applets, after reparenting out of
root into the panel's notification socket. marco churns
`bg_pixmap = None` CWA on every drag-induced configure; pre-fix
yserver landed a transparent fill into mate-panel's backing at the
applet's screen position, wiping the icon each time.

**Fix:** broaden the guard to use `resolve_paint_target` — if the
resolved target is anything other than the leaf, paints route through
a backing (own or ancestor) → skip the clear.

**Effect on the desktop:** bottom-left show-desktop applet now stays
visible. Was the first applet to render correctly.

Pinned by `cwa_on_descendant_routed_to_redirected_ancestor_does_not_clear`.

### `ebbdd5b` — protocol-layer ClipByChildren manual-redirect exemption

`copy_area_effective_dst_rects` at
`crates/yserver-core/src/core_loop/process_request.rs` was subtracting
every mapped `InputOutput` child from the destination rect under
`ClipByChildren`. For a window whose children are MANUALLY
redirected, the children's pixels are placed by the redirecting
compositor (frequently the parent's own client — XEMBED's
notification-area-applet model). Subtracting them strips the
compositor's own composite-target rect to empty.

Live trigger: notification-area-applet's `CopyArea`s from each tray
slot's pixmap into its own visible top-level were "fully clipped, no
backend call" in the log. Result: tray icons never landed in the
panel's backing.

**Fix:** per-child query of `effective_redirect_mode_for_window`. Skip
subtraction only when mode is Manual. Automatic-redirected children
still clip (regression guard preserves the existing marco/CC frame
test).

Pinned by:
- `copy_area_clip_by_children_ignores_manually_redirected_child`
- `copy_area_clip_by_children_still_subtracts_automatic_redirected_child`

### `6a00b99` — v2-layer ClipByChildren same exemption

After `ebbdd5b` the protocol-layer split correctly returned a
non-empty sub-rect, but the v2 backend's `copy_area` ran its OWN
ClipByChildren pass and re-clipped to empty. The two layers had been
added independently; v2 didn't know the protocol layer was now
authoritative.

**Fix:** apply the same Manual-redirect exemption in v2 using v2's
`scene_participating` flag (false implies Manual redirect for mapped
windows in `windows_v2`). Added a small `engine_copy_area_calls`
counter so the regression test can assert without a Vk-backed
fixture.

Pinned by:
- `copy_area_clip_by_children_skips_manually_redirected_child`
- `copy_area_clip_by_children_still_subtracts_automatic_child_in_v2`

After this fix, all `CopyArea` traces show successful dispatch (zero
"fully clipped, no backend call") and the icon paints visibly land in
the resolved backing per the log.

## Outstanding: nm-applet still invisible after all 5 fixes

User reports after `6a00b99`:
- Bottom-left show-desktop applet: visible ✓ (fixed by `cad9dec`)
- nm-applet on top panel: completely missing — panel renders
  continuous grey, no hole, no icon
- Window (mate-cc) still disappears after icons come
- Menu popups briefly appear on hover/click

The dump + log analysis of this state is what the rest of this doc
covers.

## Hardware smoke evidence after all 5 fixes

Log: `yserver-hw-mate.log` from the 14:12-14:13Z run.

### Protocol layer is healthy

```
$ grep -c "fully clipped" yserver-hw-mate.log
0
$ grep "reparent reconcile" yserver-hw-mate.log | grep -oE "[A-Z_-]+|no-op|skipped" | sort | uniq -c
   ... no-op (state already consistent) for every tray-applet reparent
```

### Icon CopyArea reaches mate-panel-top's backing per the log

For nm-applet's icon paint (line 10212-10217 in the log):

```
client 32 #449 CopyArea src=0x2100019 dst=0x2100003 gc=0x2100010 \
                                                  src=(0,0) dst=(0,0) 32x27
resolve_paint_target REDIRECT_FOUND xid=0x400080 leaf_id=DrawableId(173) \
                                    backing_id=DrawableId(71) offset=(2381,0)
copy_area src=0x4000f0->id=DrawableId(185) \
          dst=0x400080->id=DrawableId(71)+off=(2381,0) \
          src_xy=(0,0) dst_xy=(0,0) 32x27
damage_fanout: level_drawable=0x1100003 rect=(2381,0 32x27) \
               match_ids=1 fired_count=1
```

`DrawableId(71)` is mate-panel-top's backing (allocated via
`allocate_redirected_backing W=0x40002a: fresh B=0x400060 (2560x28)`).
The icon CopyArea reaches it at offset `(2381, 0)`. Damage subscriber
(marco) fires.

### marco IS reading mate-panel-top's backing

```
render_create_picture pic=0x400069 drawable=0x400060 \
                     ynest_format=0x4 value_mask=0x100 value_bytes=4
```

marco creates Picture 0x400069 wrapping drawable `0x400060` — the
*current* (post-resize) mate-panel-top backing. Then composites it
into its offscreen with various clip rects, ending with
`clip[(2361,0 52x27)]` covering the tray area.

### But the backing pixels show no icon

`yserver-v2-drawable-0-backing-W0x40002a-B0x400060-2560x28.ppm` cropped
to `(2350, 0, 250, 28)` shows the clock at the right side but
**no tray icons** in the (2361, 0)→(2412, 27) region where the
CopyArea reportedly landed.

The COW dump and scanout match — panel renders continuously across
the tray area, but with panel-grey pixels instead of icons.

### So somewhere between the icon CopyArea log line and the dump...

...the pixels either don't actually land in storage, or land then get
cleared by something we haven't traced.

## v2-vs-Xorg backing lifecycle (the broader gap)

mate-panel-top resizes from `2560x25` to `2560x28` shortly after marco
redirects it. Each tray-slot socket (`W=0x4000c4` etc.) reallocates 3-4
times as its applet's GtkPlug configure cascades fire. **Every
reallocation is a place v2 diverges from Xorg.**

### Xorg's rotate-on-resize

Reference: `/home/jos/Projects/xserver/composite/compalloc.c:680-712`
and `compwindow.c:376-388, 495+`:

1. `compReallocPixmap`: if new dimensions differ, allocate new pixmap
   via `compNewPixmap`. **Save old in `cw->pOldPixmap` — don't free
   it.** `compSetPixmap` repoints `pWin`'s pixmap to the new one.
2. `compCopyWindow` (the per-screen `CopyWindow` hook):
   `(*pGC->ops->CopyArea)(&cw->pOldPixmap->drawable,
   &pPixmap->drawable, pGC, ...)` — bits get copied from the old
   pixmap into the new one at the corresponding positions.
3. `compFreeOldPixmap`: `(*pScreen->DestroyPixmap)(cw->pOldPixmap)` —
   X server drops its reference. Pixmaps are refcounted; if a
   compositor's `NameWindowPixmap` alias still holds the pixmap, it
   stays alive until the compositor `FreePixmap`s its handle.

Net effect: even compositors that don't re-call `NameWindowPixmap` on
resize see *the carried-over content* on read. Compositors that do
re-name pick up the new backing on the next call.

### v2's current shape

- **Freeze-old via refcount** — works. `name_window_pixmap` does
  `alias_registry.incref(backing)`. `render_create_picture` does
  `store.incref(drawable_id)`. `release_redirected_backing` does
  `alias_registry.decref` and only calls `free_pixmap` when refcount
  hits 0; `free_pixmap` is alias-registry-aware and only drops the
  store entry when both registry and store refcounts hit 0. Tracing
  through the log, the OLD backing's storage entry stays alive when a
  Picture references it.

- **Copy old→new on rotate — MISSING.** `rotate_redirected_backing_on_resize`
  (`crates/yserver-core/src/core_loop/process_request.rs:789-884`)
  calls `release_redirected_backing(old)` then
  `allocate_redirected_backing(new size)`. The new backing starts
  empty (the allocate's seed-copy is *parent→B*, not *old-B→new-B*).

  Consequence: a compositor (or notification-area-applet) that doesn't
  rebind to the new backing reads stale content. A compositor that
  does rebind reads an empty new backing missing all carryover bits.
  Xorg gets this right via the `compCopyWindow` step.

- **Compositor responsibility — partly working.** marco re-calls
  `NameWindowPixmap` for some windows (`0x1100105` is re-named 8 times
  in this run) but not all (`0x1100003`/mate-panel-top is named once
  before the resize). The current break may be a combination of (a)
  marco not re-naming + (b) v2 not carrying bits + (c) the
  notification-area-applet nested case below.

### Nested case: notification-area-applet's intermediate Pictures

notification-area-applet composites each tray applet's backing into a
temporary pixmap, then `CopyArea`s the temporary onto its own
visible top-level. Specifically (lines 10193-10214 of the log):

1. `CreatePixmap pid=0x2100019 32x27` — temporary, depth 32.
2. `render_composite src=0x4000f2 dst=0x4000f1 (=0x2100019) 16x27` at
   `(16,0)` — composite right-half from one applet's backing.
3. `render_composite src=0x4000f3 dst=0x4000f1 16x27` at `(0,0)` —
   composite left-half from another applet's backing.
4. `CopyArea src=0x2100019 dst=0x2100003 32x27` — copy assembled
   temporary into the panel applet's visible window.

The source Pictures (0x4000f2 and 0x4000f3) wrap drawables that are
the *initial* backings of the tray-applet sockets — allocated when
the socket was 16x27. The sockets later resized to 18x27 and then
26x27, but the source Pictures are never recreated.

If the source Picture wraps a freed backing, the
`render_composite` reads garbage. If the source Picture wraps a
frozen-but-stale backing (alias_registry kept it alive), the
`render_composite` reads pre-resize content — which may itself be
empty if the applet only painted after the resize.

This nested-stale-Picture case is the most suspect single cause of
the visible breakage. Confirming it would need:

- A counter or log per `engine.copy_area` invocation showing
  successful vs failed writes per dst DrawableId.
- A dump trigger immediately after notification-area-applet's
  `CopyArea` to see whether mate-panel-top's backing actually receives
  pixels.
- A check on the source Picture's underlying storage at the moment of
  composite — does the freed/frozen 16x27 backing have icon content?

## Next steps (proposed, not done in this session)

Ordered by tractability:

1. **Implement compCopyWindow analog in `rotate_redirected_backing_on_resize`.**
   Before the release-then-allocate, save the old backing's content;
   after the allocate, `CopyArea` from old→new at the corresponding
   positions. This matches Xorg directly and is the smallest concrete
   gap with a clear spec reference.

2. **Add per-dst `engine_copy_area_success` counter.** Mirror
   `engine_copy_area_calls` but only increment on `Ok(())` return
   from `self.engine.copy_area`. Lets tests + logs distinguish
   "dispatched the call" from "the call completed". May expose
   silent drops.

3. **Trace the nested-Picture case.** When
   `release_redirected_backing` decrefs and the alias_registry entry
   is removed, also `set_scene_participating(false)` is called on the
   old drawable. Verify this doesn't tank the source-Picture reads
   (e.g. by transitioning Vulkan layout incorrectly). Verify the
   alias-frozen storage IS readable by `render_composite`.

4. **Audit marco's re-name behavior across mate-panel-top vs
   `0x1100105`.** Why does marco re-name one but not the other? May
   be a yserver event-emission gap (e.g. missing
   `CompositeRedirectNotify`-like signal, or a ConfigureNotify shape
   marco doesn't react to).

## Branch state at session end

```
6a00b99 fix(v2 copy_area): apply manual-redirect exemption to v2 ClipByChildren
ebbdd5b fix(copy_area): ClipByChildren must exempt manually-redirected children
cad9dec fix(v2): CWA on descendant routed to redirected ancestor must skip clear
4519645 chore(diag): log reparent reconciliation branches + resolver routing
e9fe71a feat(composite): reconcile redirect on reparent (Phase 2)
74f7333 test(reparent): pin Xorg-style redirect reconciliation invariants
cd30336 feat(recording): opt-in supports_redirect_activation flag
3d5be5f refactor(composite): thread origin through teardown_redirect_for_window
c0c60f1 feat(scene): cow-authoritative mode strips top-level draws (Phase 1, 2a)
77f7178 style: nightly fmt — collapse AddTraps if-let onto one line
63951bc test(scene): cow=Some must strip top-level draws (Phase 1, 2a)
a269cce update nvidia doc                                                  (carry-over)
77c370b test(scene): rename existing COW tests to cow=None form for 2a
7e980bf chore(v2 backend): bundle scanout + COW state into drawable dump   (pre-Task-1)
dceac42 fix(render): emit damage for AddTraps stub                         (pre-Task-1)
0cc129b chore(damage_fanout): trace level_drawable + match/fired counts    (pre-Task-1)
c6f9e9c chore(justfile): add damage_fanout=trace to yserver-mate-hw-trace  (pre-Task-1)
```

**The user-visible bug is not fixed.** nm-applet still invisible,
mate-cc still disappears, panel popups still flicker. The five
commits each fix a real correctness layer with a unit test, but the
tests pin the layer's invariant — not the end-to-end "tray applet
renders" outcome. That end-to-end outcome remains broken and is what
the next session has to address (likely starting from the
`compCopyWindow` analog).

Process hygiene below the user-visible result:

- 940+ workspace tests green
- `cargo +nightly fmt` clean
- `cargo clippy --all-targets` exit 0 with no new warnings from any
  of the new commits

Squash-mergeable into `rendering-model-v2` if you want to land the
five correctness fixes and the diagnostic logging as a batch and
tackle the v2-lifecycle gaps in a follow-up session — but recognise
that landing this batch alone does not restore the tray.
