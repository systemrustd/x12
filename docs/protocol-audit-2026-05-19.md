# Protocol audit — RENDER / COMPOSITE / XFIXES / DAMAGE vs Xorg

Audit date: 2026-05-19. Four sub-agents each cross-referenced yserver v2 against
`/home/jos/Projects/xserver` (render/, composite/, xfixes/, damageext/ + miext/damage/).
Focus: clipping, redirect routing, damage delivery — the compositor-WM battleground.

---

## Tier 1 — likely causing visible bugs now

### 1. Paint into backing B never fires DamageNotify on window W
**Severity: Bug**
- yserver: `damage_fanout.rs:167-275`
- Xorg: `damageext.c` + `composite/compwindow.c`

Damage fanout matches by `drawable_id == B`, not by `redirected_target` indirection.
Compositors that `XDamageCreate(window=W)` see zero events for auto-redirected paints →
marco/xfwm4 sit silent after the initial composite. Matches "marco emits 0 DamageNotify".

### 2. `render_composite` drops src/mask client clips entirely
**Severity: Bug**
- yserver: `kms/v2/backend.rs:5780-5879`
- Xorg: `render/mipict.c:316-389` (`miComputeCompositeRegion`)

`resolve_picture_for_render` returns no clip for src or mask; engine only sees `dst_clip`.
xfwm4/muffin shadow blits that scope a source picture with `SetPictureClipRectangles`
paint over the whole dst. Active hypothesis in `status.md:1973-1979`.

### 3. Scene Manual-redirect subtree prune drops Automatic-redirected descendants
**Severity: Likely-bug**
- yserver: `kms/v2/scene.rs:1296-1556`

If any ancestor has `scene_participating=false`, entire subtree pruned regardless of
per-descendant mode. Xorg only stops normal-scene compositing at the Manual ancestor —
Automatic descendants still get auto-composited into the Manual ancestor's backing.
Symptom: `RedirectWindow(frame, Manual)` + `RedirectSubwindows(frame, Automatic)`
(GTK/marco CSD pattern) makes Automatic widgets vanish. Matches Control Center
missing-menu/widget reports.

### 4. `pict_format` stored but ignored — xRGB32 on depth-32 storage treated as ARGB32
**Severity: Spec-deviation (active bug)**
- yserver: `kms/core.rs:488` (comment says "Currently instrumentation-only")

Compositor offscreens with alpha=0-padding leak transparent black onto wallpaper.
Active 4d shadow/CSD ARGB-vs-xRGB mismatch. Real fix is Stage 4e PictFormat tracking.

### 5. `render_composite_glyphs` silently drops everything except `Over` + SolidFill source
**Severity: Bug**
- yserver: `kms/v2/backend.rs:5944-5947`
- Xorg: supports full op table for CompositeGlyphs

No client error, just counter bump. Cairo gradient-source glyph paint (mate Control
Center yellow headers, GTK header-bar text on a gradient background) renders nothing.

### 6. Seed copy on redirect activation reads from W, not from parent
**Severity: Likely-bug**
- yserver: `kms/v2/backend.rs:849-953`
- Xorg: `composite/compalloc.c:541-606` (`compNewPixmap` copies parent w/ IncludeInferiors)

Manual-redirected freshly-created W seeds B with default-init opaque black even when
parent shows real pixels. Matches recurring "black band on map".

### 7. CreateRegionFromGC translates by clip_origin; Xorg does not
**Severity: Bug**
- yserver: `process_request.rs:2622-2626`
- Xorg: `xfixes/region.c:204-232` — returns raw clip-coordinate rects, origin only
  applied when GC is *used*

A WM that builds a region from a GC then re-installs as that GC's clip gets
double-translation → wrong area shadowed/repainted.

### 8. `drawable_origin` field defined but never written — mid-WIP
**Severity: Bug (incomplete)**
- yserver: `kms/core.rs:487`, `process_request.rs:1153`
- Backend method `set_picture_drawable_origin` missing entirely (E0599 diagnostic)
- Also: `picture_client_clip_rects` missing (E0599 at `process_request.rs:2670`)

Window-backed pictures whose drawable is at non-(0,0) (CSD frame children) have clips
in dst-coords, get clamped by `dst_extent.max(0)`, lose negative-shifted rects.
**This is the in-flight work referenced by the current diagnostics.**

---

## Tier 2 — will hang or visibly break clients

### 9. `SelectSelectionInput` never emits `XFixesSelectionNotify`
**Severity: Bug**
- yserver: `process_request.rs:2555-2568`; event emit site missing entirely
- Xorg: `xfixes/select.c:53-94`

Every clipboard manager (klipper, copyq, gpaste, gnome-shell clipboard indicator) wedges.

### 10. `GetClientDisconnectMode` (XFIXES minor 34) has a reply but is silently dropped
**Severity: Bug**
- yserver: `_` arm in XFIXES dispatch
- Xorg: `xfixes/disconnect.c:89-109`

docstring claimed "no block" — wrong. gnome-session probe hangs to read timeout.

### 11. Map-window doesn't emit damage (server background paint not hooked)
**Severity: Bug**
- yserver: `process_request.rs:8610` — calls `backend.map_subwindow` but no damage bump
- Xorg: `miext/damage/damage.c` hooks `miPaintWindow`

Tooltip/menu first-frame missing on marco compositing.

### 12. `DamageSubtract` doesn't re-fire when damage still non-empty after subtract
**Severity: Bug**
- yserver: `process_request.rs:4168-4170`
- Xorg: `damageext.c:422-424` — calls `DamageExtReport` again if still non-empty

RepeatNotify mechanism dead. Compositor stalls if another client adds damage while
the focused client is between Subtracts.

### 13. Raw/Delta DamageNotify throttled to one event per Subtract cycle
**Severity: Bug**
- yserver: `damage_fanout.rs:241,271` — `if !fired { … fired = true }`
- Xorg: `damage.c:1941-1953` — emits every paint for Raw; emits on new area for Delta

picom (uses DeltaRegion) misses incremental damage between two Subtracts → smearing.

### 14. `SelectCursorInput` never emits `XFixesCursorNotify`; `GetCursorImage` returns 0×0
**Severity: Bug**
- yserver: `process_request.rs:2569-2578` (storage only); `xfixes.rs:329-345` (empty reply)
- Xorg: `xfixes/cursor.c:360-413`

Screencast, magnifier, accessibility tools see no cursor. Cursor-follow features broken.
`xfixes_change_cursor_by_name` also a documented no-op in v2 backend.

### 15. `ExpandRegion` (XFIXES 3.0, minor 28) stubbed
**Severity: Stub**
- yserver: falls through `other` arm
- Xorg: `xfixes/region.c:732-770`

xfwm4 uses ExpandRegion for shadow extents → shadows visually undersized.

### 16. `CreateRegionFromBitmap` returns empty rects
**Severity: Bug**
- yserver: `process_request.rs:2598-2643` — inserts `Vec::new()`
- `bitmap_to_yx_banded_rects` exists at `nested.rs:736` but not wired up

xfwm4/picom pixel-precise shadow shapes break; SetWindowShape with bitmap clears shape.

---

## Tier 3 — spec correctness, lower priority

### COMPOSITE

- **NameWindowPixmap missing `viewable` check** — should be BadMatch for unmapped W
  (`process_request.rs:3185-3220` vs `composite/compext.c:241-242`)
- **NameWindowPixmap accepts InputOnly windows** — should be BadMatch
  (`process_request.rs:3175-3280` vs `compext.c:250-252`)
- **NameWindowPixmap missing `LEGAL_NEW_RESOURCE` precedence** — wrong error class on
  duplicate XID
- **Mode-flip Manual→Automatic emits no synthetic damage** — compositor misses repaint
  trigger (`process_request.rs:615-662` vs `compalloc.c:299-307`)
- **`compReallocPixmap` not short-circuiting on size-unchanged moves** — every drag
  invalidates all NameWindowPixmap aliases, causing flicker
  (`process_request.rs:781-874` vs `compalloc.c:698-708`)
- **COW born without `MapNotify` fanout** — marco's StructureNotify subscription never
  fires (`kms/v2/backend.rs:3925-3983`)
- **COW has no input-passthrough region by default** — pointer events may be absorbed
  by COW before WM input proxy (Xorg's COW born input-transparent)
- **COW `host_xid: None` pre-GetOverlayWindow** — any client request against 0x103
  before compositor issues GetOverlayWindow bypasses normal BadWindow/BadDrawable
- **ConfigureWindow on Manual-redirected child before first map emits no damage**
  (`process_request.rs:7508-7512` — gated on `redirected_backing.is_some()`)
- **`composite_named_pixmaps` aliases never disposed** — memory growth across
  compositor restarts; see status.md Task 4 (resources.rs vs process_disconnect.rs)

### RENDER

- **`clip_mask = Pixmap` on ChangePicture is a logged no-op** (`backend.rs:2205-2218`)
  — Cairo subpixel-mask path and shaped-window paths broken
- **`subwindow_mode` (IncludeInferiors vs ClipByChildren) stored but never read**
  — window-backed pictures always behave as IncludeInferiors; child windows
  aren't clipped out from source sampling
- **Alpha-map (`alpha_map`/`alpha_x`/`alpha_y`) stored but never sampled**
  (`core.rs:507-509`) — alpha-modulated source pictures silently ignore it
- **`SetPictureClipRectangles` v2 site doesn't canonicalize** (`backend.rs:6807-6821`)
  — status.md claims this landed, but code doesn't show it; needs verification (?)
- **Mask picture clip not passed to engine for direct Composite-with-mask-picture path**
  (`backend.rs:5771-5779`)

### XFIXES

- **QueryVersion hard-codes (5,0) instead of `min(client, server)`** — a client
  requesting V1 sees V5 back and may issue V2+ opcodes
  (`process_request.rs:2542-2548`)
- **Combine/Invert/RegionExtents silently ignore missing source** — should return
  BadRegion; instead treats missing as empty
- **CopyRegion/Subtract/Combine don't validate dest exists** — should return BadRegion
  for unknown dest XID
- **CreateRegionFromWindow accepts `kind=Input`** — Xorg returns BadValue
- **FetchRegion silently returns empty for missing region** — should return BadRegion
- **ChangeSaveSet target+map ignored** (`process_request.rs:2849-2851`) — windows that
  should re-root on client disconnect vanish instead

### DAMAGE

- **Ancestor walk ignores `border_width`** — damage coords off by border width in
  parent space for any WM using non-zero borders (`damage_fanout.rs:93,105`)
- **DamageNotify NonEmpty fires even when damage was already non-empty** (should be
  empty→non-empty transition only) (`damage_fanout.rs:252-261`)
- **Configure damage only fires at new position, not old** — old-position trails on
  drag until full repaint (`process_request.rs:7508-7514`)
- **`DamageSubtract` doesn't re-clip to drawable extent post-subtract**
  (`process_request.rs:4141-4170` vs `damage.c:1854-1873`)
- **`DamageAdd` doesn't translate by `pDrawable->x/y`** (`damageext.c:454-456`)
- **No damage cleanup on DestroyWindow / FreePixmap / client disconnect** — dangling
  entries accumulate across compositor restart; phantom notifies on recycled XIDs

---

## Tier 4 — stubs unlikely to trip current clients

- RENDER: `AddGlyphsFromPicture` (minor 21), `Scale` (9), `ColorTrapezoids/Triangles`
  (14/15), `AnimCursor` (31), `AddTraps` (32), `ConicalGradient` (36)
- XFIXES: `GetCursorName`/`GetCursorImageAndName`/`SetCursorName`/`ChangeCursor`
  (minors 23-26), `CreatePointerBarrier`/`DestroyPointerBarrier`/`SetClientDisconnectMode`
  (31-33)
- DAMAGE: `DamageNoteCritical`; `DamageAdd` origin translation; `DamageQueryVersion`
  dispatch gate; all Poly*/FillPoly/PolyText over-damage by full drawable (correct but
  wasteful)
- RENDER `render_create_cursor` allocates xid but never rasterises (themed cursors)

---

## Recommended fix order

For maximum compositor-WM impact with minimum churn:

1. **#1** Damage delivery via `redirected_target` — unblocks marco/xfwm4 compositor event loop
2. **#2** RENDER src/mask clip propagation — active status.md hypothesis
3. **#8** Finish `set_picture_drawable_origin` / `picture_client_clip_rects` WIP — already in-flight
4. **#3** Scene subtree prune — explains class of missing-widget reports
5. **#5** CompositeGlyphs drop non-SolidFill source silently — yellow headers etc.
6. **#9** XFixesSelectionNotify — clipboard managers wedge in every DE
7. **#11 + #12** Map damage + Subtract RepeatNotify — compositor loop completeness
8. **#4** PictFormat tracking (xRGB vs ARGB) — Stage 4e substrate

---

*Generated from parallel agent audit 2026-05-19. Xorg reference: `/home/jos/Projects/xserver`.*
