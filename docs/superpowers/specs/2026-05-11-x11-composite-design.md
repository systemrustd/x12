# Full X11 composite support — design

Date: 2026-05-11
Status: v3.1 — plan-ready (codex round 3 sign-off)
Author: codex/claude session

## Background

The MATE session under yserver-hw reaches the point where marco starts,
claims `WM_S0` and `_NET_WM_CM_S0`, accepts MapRequests, reparents
top-levels, and decorates them. Two visible defects remain and one
hidden defect is now confirmed:

1. **Visible**: every framed window has a pure-black 10-pixel rim. PPM
   scanout dump at `x=594..603` (left rim of caja "Home") reads
   `(0,0,0)`; same on right (`x=1956..1965`), top, and bottom. The rim
   sits inside the frame mirror — frame size 1372×853, rim runs
   `594..1965` outer-to-outer = 1372 px exact. So the black pixels are
   in the painted mirror, not the root background.
2. **Hidden** (newly confirmed): marco *is* trying to engage compositor
   mode. `compositing-manager = true` in `org.mate.Marco.general`
   (user-confirmed). In the live log, marco issues
   `XCompositeRedirectSubwindows` (we accept the bookkeeping bit), then
   `XCompositeNameWindowPixmap` — which our handler at
   `process_request.rs:2440..2560` rejects with `BadAlloc` because the
   KMS backend's `composite_opcode()` returns `None`
   (`backend.rs:6905`) and `name_window_pixmap()` returns `Unsupported`
   (`backend.rs:7335`). Marco's compositor module then bails and falls
   back to the legacy WM path.
3. **Visible (consequence of #2)**: no shadows, no fade animations, no
   ARGB-blended titlebars. Marco's decoration pixmap is painted by
   cairo onto a depth-24 surface; only the *visible* border region is
   painted; the *invisible* border region (the strip cairo reserves
   for compositor shadows) stays at the pixmap's initial state. The
   pixmap is then CopyArea'd onto the frame mirror. The combination of
   our initial clear (`(0,0,0,0)` in `target.rs:initialize_clear`) and
   the composite frag shader's hard override (`α = 1.0` at
   `composite.frag.glsl:27` when `use_src_alpha == 0`, which is the
   case for window mirror draws at `backend.rs:5896`) produces opaque
   black at that rim.

The *core bug* is yserver's compositor: **we render depth-24 window
mirrors as if every pixel had been painted, by forcing `α = 1.0` at
sample time.** That violates the X11 invariant that the server never
contributes content to pixels the client (or its delegating WM)
didn't paint. Marco's invisible-border pattern is one trigger, but
not the only one — any client that partially paints a depth-24 window
hits the same shape.

A second, contributing class of bugs lives in our paint paths: text,
trapezoid, and Composite use standard premultiplied src-over and do
not guarantee α=255 on first paint into a freshly-allocated mirror.
For paths where the X11 op is semantically opaque (PutImage ZPixmap
on depth-24, FillRectangles in a TrueColor visual, ImageText, …) we
need an explicit alpha-write policy or those paths will look
*transparent* once we stop clamping at composite time.

Marco's compositor bail is independent of the rim but tightly
coupled to it: if compositor engagement worked, marco would paint
shadows over the redirected pixmap, ARGB-blend onto the overlay, and
the rim would (mostly) disappear via marco's compositor doing the
right thing with α=0. So *either* fixing our alpha invariant *or*
enabling marco's compositor mode would visibly remove the rim — by
two completely different paths, only one of which addresses the root
cause.

This spec describes the foundational fix (alpha invariant), the
compositor-backing implementation (XComposite redirect end-to-end on
KMS), and the question of whether the GLX TFP gate is a hard or soft
prerequisite for marco engaging compositor mode in this environment.

## Goal

A MATE session under yserver-hw renders identically (visually and
behaviourally) to MATE under xorg-server, including:

- No black rim around framed windows under any client / WM
  combination.
- Marco's compositor effects (shadows, fades, ARGB-blended titlebars)
  active when `compositing-manager = true`.
- Any X11 compositor (marco, picom, etc.) running on yserver-hw
  produces the same visual output it produces on xorg-server.

## Non-goals

- GLX direct-rendering performance parity beyond Phase 4.2 baselines.
- Marco's GLX compositor backend specifically. Marco has both XRender
  and GLX backends; XRender is sufficient if it engages. If only the
  GLX backend triggers `compositing-manager = true` in this
  environment, that gates L3 promotion, captured below.
- Backing-store (`WhenMapped`/`Always`).

## Constraints

- Must not regress wezterm, e16, fvwm3, gtk3-demo, xclock, xeyes,
  vkcube, control-center, file-manager interactions, panel + applets.
- Must not regress xts5 / rendercheck baselines beyond a one-time
  approved rebase per phase.
- Single-threaded core (Phase 6.8). No new locks; new state goes on
  `ServerState` or backend struct.
- KMS-only focus. ynest must keep working but receives no new
  compositor machinery in this design; if a primitive is shared, the
  ynest implementation may be a thin host-X passthrough.

## Current state — concrete observations

These are the load-bearing facts for the design. Sources are inline.

- **Frag shader contract today**: when `use_src_alpha == 0`, the
  shader emits `vec4(c.rgb, 1.0)` regardless of the sampled α. The
  comment on the shader explicitly says "the pixman X8R8G8B8 mirror's
  alpha pad isn't guaranteed to be 0xFF" — i.e. the override exists
  because *the mirror alpha is not trusted*
  (`composite.frag.glsl:8..15`).
- **Composite emission contract today**: three sites in
  `walk_subtree_into_draws` + `build_composite_scene` —
  `use_src_alpha: false` for window mirror draws (`backend.rs:5896`),
  `use_src_alpha: true` for cursor (`backend.rs:5810`),
  `use_src_alpha: false` for bg pixmap (`backend.rs:5782`).
- **Mirror initial state**: both window mirrors
  (`allocate_window_mirror`, `backend.rs:5507`) and pixmap mirrors
  (`allocate_pixmap_mirror`, `backend.rs:5541`) call
  `target.initialize_clear`, which writes `(0,0,0,0)` to all pixels
  (`vk/target.rs:665..710`). Mirror format is `B8G8R8A8_UNORM` for
  depth 24 *and* depth 32 (`vk/target.rs:241..245`). So depth-24
  mirrors physically carry an alpha channel; today nothing relies on
  its content because the shader clamps.
- **Vk paint pipeline blend state**: `pipeline.rs:229` (and equivalent
  for the text pipeline at `vk/text_pipeline.rs:217..221`) configures
  src-over on both colour and alpha channels:
  `src_color_blend_factor = ONE`,
  `dst_color_blend_factor = ONE_MINUS_SRC_ALPHA`,
  `src_alpha_blend_factor = ONE`,
  `dst_alpha_blend_factor = ONE_MINUS_SRC_ALPHA`. Premultiplied src-
  over. This means *first paint into a transparent destination* writes
  whatever α the fragment emits — which may or may not be 255.
- **`Window.composite_named_pixmaps`** already exists at
  `resources.rs:2123` as `Vec<NamedCompositePixmap>`. Designed for
  XComposite alias semantics (multiple `NameWindowPixmap` over a
  window's lifetime). Not yet wired to any backend allocation path.
- **`ServerState.composite_redirects`** stores the (window,
  subwindows) → mode bit per client request (`server.rs:211..`,
  `process_request.rs:2440..2454`). Bookkeeping only — no actual
  routing of paint to off-screen pixmaps.
- **KMS backend COMPOSITE wiring**: `composite_opcode() -> None`
  (`backend.rs:6905`) → handler rejects `NameWindowPixmap` with
  `BadAlloc` before reaching the backend. `name_window_pixmap` itself
  returns `Unsupported` (`backend.rs:7335`).
- **Outbound DRI3 fd export**: `BUFFER_FROM_PIXMAP` works
  (`process_request.rs:4337..`, `dri3_export_pixmap` on the backend).
  Inbound `PixmapFromBuffer` also works. So the DMA-buf side of TFP
  is already there.
- **GLX**: phase 4.2 complete. We answer `GetDrawableAttributes` with
  the `GLX_TEXTURE_TARGET_EXT` / `GLX_Y_INVERTED_EXT` tags
  (`process_request.rs:4670..`), but we do not implement
  `glXBindTexImageEXT` / `ReleaseTexImageEXT`, and `glxext.h`'s
  `GLX_EXT_texture_from_pixmap` is not in our advertised extension
  string.

## Architecture overview

Three layers, each independently shippable. Visible result of each:

```
┌─────────────────────────────────────────────────────────────┐
│ L3: GLX_EXT_texture_from_pixmap                             │
│  → satisfies the mate-session GL-helper gate IF marco's     │
│    compositor refuses to engage without it                  │
│  → enables marco's GLX backend if user prefers it           │
├─────────────────────────────────────────────────────────────┤
│ L2: real COMPOSITE redirect on KMS backend                  │
│  → composite_opcode() returns Some, name_window_pixmap()    │
│    allocates an off-screen pixmap mirror, paint of          │
│    redirected windows is routed to the pixmap rather than   │
│    the scanout, overlay window is real and is what we       │
│    scan out from when redirect is active                    │
│  → marco's XRender compositor draws shadows / fades         │
├─────────────────────────────────────────────────────────────┤
│ L1: mirror alpha contract                                   │
│  → painting paths explicitly write α=255 on opaque ops      │
│  → composite shader stops forcing α=1.0 for window mirrors  │
│  → untouched mirror pixels (α=0) are transparent at         │
│    composite time, underlying content shows through         │
└─────────────────────────────────────────────────────────────┘
```

L1 is the prerequisite for both L2 and L3. L2 is the prerequisite
for marco compositor effects. L3 is conditionally a prerequisite for
marco engaging compositor mode *at all* in this environment — to be
resolved empirically (see "L3 gating" below).

## L1 — mirror alpha contract

### Two kinds of α

The contract distinguishes the α channel of a mirror by *semantic
ownership*, not by the wire-format depth:

- **Server-owned opaque α** — the α channel is internal compositor
  bookkeeping. The X11 wire never observes it. A pixel either was
  painted by some opaque X11 op (α=255, plus AA coverage where the
  op produced it) or it was never touched (α=0).
- **Client-meaningful α** — the α channel was driven by the client
  via an ARGB-aware op (ARGB32 PutImage, RENDER on an ARGB
  destination, …). The server transports α verbatim; the compositor
  consumes it.

In our current codebase, "server-owned opaque α" corresponds
exactly to drawables of depth 24, and "client-meaningful α" to
drawables of depth 32 ARGB. We use depth as the proxy below, but
the underlying invariant is semantic ownership — if we ever
introduce additional pixmap formats, the depth-as-proxy mapping
re-derives from the semantic split, not the other way around.

### The contract

For every mirror M the server owns, M's α channel obeys at all times:

1. **Initial state** — `M.alpha[p] == 0` for every pixel `p` after
   `initialize_clear`. (No change to today's behaviour.)
2. **Opaque-paint rule** (server-owned α): any X11 paint op whose
   semantics make the destination *fully opaque* writes `α = 255`
   to every pixel the op covers. The covered pixels are determined
   by the op itself (rect bounds, shape mask, glyph coverage, etc.).
   Examples: FillRectangles in a TrueColor RGB visual; PutImage
   ZPixmap into a depth-24 drawable; ClearArea; ImageText.
3. **Coverage-paint rule** (server-owned α): antialiased ops
   (CompositeGlyphs with an alpha-mask source, RENDER
   Trapezoids/Triangles, polygon AA) write the *coverage* α the op
   computed, premultiplied with whatever source α the op uses. This
   is the only path where painted-but-not-fully-opaque is valid for
   server-owned α.
4. **CopyArea α-preserve rule** — when copying between two mirrors
   of the same internal format, the α channel is copied verbatim. A
   server-owned mirror with `α[p] == 0` (the "rim" case) propagates
   as `α[p] == 0` to the destination. This is an internal
   implementation rule, not a wire-protocol claim.
5. **Client-meaningful-α ops** — when the destination is a
   depth-32 ARGB drawable, α follows client semantics directly. The
   mirror's α is neither forced nor synthesised by the server.
6. **At composite time** — window mirror draws emit
   `use_src_alpha: true` (the push constant becomes a
   specialization constant — see "mechanism" below). The frag shader
   samples α straight from the mirror and the blend stage treats
   `α=0` as transparent, so underlying lower layers show through any
   pixel the server hasn't painted.

### How each paint path satisfies the contract

This is the audit codex flagged as missing. Source line numbers refer
to `crates/yserver/src/kms/backend.rs` unless noted.

| X11 op (entry point) | Backend entry / Vk recorder | Current α behaviour | Required change |
|----------------------|-----------------------------|---------------------|-----------------|
| FillRectangles (70 PolyFillRectangle / 53 FillRectangles) | `poly_fill_rectangle` (`backend.rs:8289`) / `fill_rectangle` (`backend.rs:8398`) → `try_vk_fill_with_function` (2603) | src-over blend; α from solid fill | Force fragment α=255 for opaque visuals (rule 2) |
| PolyRectangle (67) | core graphics → fill path | stroked variant; same blend | Force α=255 (rule 2) |
| PolyLine (65) | core graphics | same | Force α=255 (rule 2) |
| PolySegment (66) | core graphics | same | Force α=255 (rule 2) |
| PolyPoint (64) | core graphics | same | Force α=255 (rule 2) |
| PolyArc (68) | `poly_arc` (`backend.rs:8171`) | partial-angle arc → full-ellipse approximation; same blend | Force α=255 (rule 2) |
| PolyFillArc (71) | `poly_fill_arc` (`backend.rs:8309`) | same | Force α=255 (rule 2) |
| FillPoly (69) | core graphics | same | Force α=255 (rule 2) |
| ImageText8 (76), ImageText16 (77) | text path | text-aware blit | Force α=255 — image text is fully opaque (rule 2) |
| PolyText8 (74), PolyText16 (75) | text path | text-aware blit | Force α=255 (rule 2) |
| CopyArea (62) | `try_vk_copy_area` (2309) | image copy; preserves whatever src has | No change (rule 4 — preserve src α) |
| CopyPlane (63) | `try_vk_copy_plane` | same | No change (rule 4) |
| PutImage (72) ZPixmap depth-24 | `try_vk_put_image` (2828) | Vk PutImage path | Force α=255 (rule 2) |
| PutImage (72) ZPixmap depth-32 | same | same | Keep client α (rule 5) |
| PutImage XYBitmap / XYPixmap | same | rasterised 1-bit blit | Force α=255 on set bits, leave unset pixels untouched (rule 2) |
| ClearArea (61) | bg_pixel or bg_pixmap fill | solid or tiled | Force α=255 (rule 2) |
| RENDER FillRectangles | `render_fill_rectangles` (9018) | RENDER blend op | If op is `Src` or destination is depth-24, force α=255; else (PictOp blend on ARGB) follow op (rule 5) |
| RENDER Composite | `try_vk_render_composite` (4441) | src-over | If dst depth-24, force α=255 on touched pixels; if ARGB, follow op (rules 2 vs 5) |
| RENDER CompositeGlyphs | `try_vk_render_composite_glyphs` (4039) | glyph coverage | Coverage-paint rule (3): result α is glyph coverage × source α |
| RENDER Trapezoids / Triangles | `try_vk_trapezoids` (3430) | coverage-AA blend | Coverage-paint rule (3) |
| Tiled / stippled fill | `try_vk_tiled_fill` | varies | Force α=255 on opaque visual; coverage rule on stipple-AA paths |
| Cursor allocation | `render_create_cursor` (9342) | already alpha-true | No change |

This list covers every paint-side entry point currently in
`crates/yserver/src/kms/backend.rs`. Any paint path added later must
declare a rule under this taxonomy before merging — there is a
checklist item in the L1 plan for "audit any new paint-side trait
method before landing".

### Implementation strategy

The literal mechanism for "write α=255" is per-path. Preferred
mechanism, with fallbacks:

1. **Fragment specialization constant (preferred).** Each paint
   pipeline that has a static α policy (every entry in the table
   above that needs a single rule) is compiled with a Vulkan
   specialization constant — `ALPHA_MODE = Opaque (1) | PassThrough
   (0) | Coverage (2)`. The frag shader switches on it at compile
   time; no per-draw push constant. This matches the existing
   precedent in `crates/yserver/src/kms/vk/render_pipeline.rs:737..`,
   where RENDER pipelines already use spec-constants for the
   `MODE`/`OP`/`A8_DST`/`COMPONENT_ALPHA` switches.
2. **Per-draw push constant.** Fallback for paths whose α policy is
   dynamic (e.g. RENDER FillRectangles where the destination depth
   determines the rule per call). Minimal pipeline-count churn but
   slightly worse SPIR-V quality.
3. **Color-write-mask + α-only draw.** For simple opaque ops if a
   pipeline split isn't viable. Two-pass.
4. **vkCmdClearAttachments pre-pass.** Last resort; not on the
   critical path.

The plan-level work prefers #1, falls back to #2 where the policy
is dynamic, and avoids #3/#4 unless they buy real simplicity in a
specific path.

### Composite shader change

`composite.frag.glsl` becomes either:

```glsl
void main() {
    out_color = texture(tex, v_uv);
}
```

— with the `use_src_alpha` field stripped from the vertex shader and
push-constant layout — OR (during rollout) keep the conditional
behind a specialization constant so opaque-path bring-up doesn't
have to land all at once:

```glsl
layout(constant_id = 0) const int SRC_ALPHA_MODE = 1;
void main() {
    vec4 c = texture(tex, v_uv);
    out_color = (SRC_ALPHA_MODE != 0) ? c : vec4(c.rgb, 1.0);
}
```

with the existing window-mirror compositor variant compiled twice
(modes 0 and 1). New opaque paths switch their composite-time
draw to mode-1 as their α-write policy lands. Final state: only
mode-1 remains; the spec-constant is removed.

Pipeline blend state stays at premultiplied src-over (`ONE`,
`ONE_MINUS_SRC_ALPHA`).

Pre-condition for *flipping every window-mirror draw to mode-1*:
every opaque-paint pipeline has been audited and either writes
α=255 or carries a coverage value the compositor is expected to
respect. Per-path rollout under spec-constant keeps individual flips
small and reversible.

### Risks specific to L1

- **Anti-aliased text edges and trapezoid coverage.** These paint
  paths legitimately produce α<255 at the AA edge. After the shader
  change, those pixels will composite as partially transparent —
  visually correct for AA glyphs over a known background, but new
  behaviour for our renderer. rendercheck baselines may shift on the
  alpha-coverage tests. Accept a one-time rebase.
- **Opaque paths whose alpha output is not yet auditable.** Until
  every path declares its rule, flipping the shader is a regression
  risk. Mitigation: keep the `use_src_alpha` flag as an opt-in during
  the rollout; flip per-path; remove the flag only when every paint
  recorder has been migrated.
- **Pixmap-as-source CopyArea between mirrors of mismatched
  formats.** Today all mirrors are `B8G8R8A8_UNORM`. If we ever
  introduce additional formats (e.g. for ARGB32 vs depth-24
  distinction), the α-preserve rule needs format conversion.
- **ParentRelative bg_pixmap.** When a window's
  `background_pixmap = ParentRelative` and a region needs auto-fill
  (e.g. on Map or ClearArea), the X server walks up to the parent's
  bg_pixmap and tiles it into the child at the appropriate offset.
  That auto-fill is an opaque paint; it must write α=255. Specifically
  in our code today the bg fill happens in `fill_mirror_solid` and
  the bg_pixmap path in the ClearArea handler — both must respect
  the contract.

### Test strategy for L1

1. **Synthetic invariant test.** Allocate a window, fill only half via
   FillRectangle, map it over a known root colour, scanout-dump. The
   unfilled half must show the root colour. Add as a `cargo test`
   that drives a minimal client against a headless yserver.
2. **rendercheck.** Existing baseline; expect AA-related deltas; rebase
   once.
3. **xts5.** No expected delta from L1; any new failure indicates a
   missed paint path.
4. **Visual smoke under MATE.** Specifically: file manager, control
   panel, gtk3-demo, wezterm. The framed-rim should be gone.
5. **PPM regression check.** Per-frame scanout dump under a scripted
   MATE session; assert no `(0,0,0)` pixels in known-rim regions.

## L2 — XComposite redirect end-to-end

### Scope

Implement on the KMS backend:

- `composite_opcode()` returns `Some(<COMPOSITE major>)`.
- `name_window_pixmap()` allocates an off-screen pixmap mirror tied
  to the redirected window's current backing.
- Paint dispatch for any drawable that resolves to a redirected window
  goes to the off-screen pixmap mirror, not to the screen mirror.
- Overlay window is real (has a host_xid + mirror), and is what KMS
  scans out from when any redirect is active.
- Damage events are attached to the redirected window so the
  compositor (marco) gets the recompose triggers it subscribes to.

### State model

Three pieces of state, plus a refcount story for the backing
resource.

- **`Window.composite_named_pixmaps: Vec<NamedCompositePixmap>`** —
  *keep as the alias history*. XComposite spec lets a client take
  multiple named pixmaps over a window's lifetime; each one is a
  stable alias to the redirected backing at the time of the call.
  The Vec already matches that. Each entry carries `client_pixmap`,
  `host_pixmap`, `width`, `height`.

- **`Window.redirected_backing: Option<RedirectedBacking>`** (new
  field). When the window is redirected, this is the *current*
  off-screen backing: host-side pixmap XID + size + format +
  refcount. Re-allocated on window resize; the old `RedirectedBacking`
  doesn't go away when this happens — see refcount story below.

- **`ServerState.composite_redirects`** — change from
  `HashMap<(ResourceId, bool), u8>` to a richer value type:

  ```rust
  struct RedirectRecord {
      mode: CompositeRedirectMode,    // Manual (1) | Automatic (2)
      owner: ClientId,                // which client took the redirect
  }
  HashMap<(ResourceId, bool), RedirectRecord>
  ```

  Lookup is by `(window, subwindows)`. Conflict policy: a second
  `Redirect*` on the same `(window, subwindows)` from a *different*
  client returns `BadAccess`. A second call from the same client is
  a no-op (mode and owner unchanged). Marco is the only compositor
  in our environment; the single-owner model is sufficient.
  Multi-compositor refcounting (xorg-server style) is an explicit
  out-of-scope follow-up.

#### Backing pixmap refcount and lifetime

The backing resource is a host-side pixmap *plus* its associated
`Window.redirected_backing` entry *plus* any `NamedCompositePixmap`
alias that points at it. The lifetime is the *union* of:

1. The window being redirected (i.e. there is an entry in
   `composite_redirects` whose key includes this window or a
   `subwindows=true` ancestor).
2. Any `NamedCompositePixmap` alias still alive in any window's
   `composite_named_pixmaps` Vec OR addressable via the public
   pixmap XID in `ResourceTable.pixmaps`.

When a backing pixmap is allocated, it is assigned a refcount of 1
for reason (1). Each `NameWindowPixmap` call increments the refcount
and registers a `NamedCompositePixmap` entry. `FreePixmap` on a
named-pixmap XID decrements. Releasing the redirect (`Unredirect*`
or window destroy + no remaining aliases) decrements reason (1)'s
hold.

On window *resize*, a fresh backing is allocated and reason (1)'s
hold moves to the new backing. The old backing's reason (1) refcount
goes to zero; it survives only if some `NamedCompositePixmap` alias
holds it (the "frozen pre-resize content" required by the spec).
When all aliases on the old backing free, the old backing is
destroyed.

On window *destroy*, reason (1)'s hold is released. Surviving
`NamedCompositePixmap` aliases keep the backing alive. The window's
resource is destroyed; the backing is independent.

This refcount lives on the backing itself (a backend-private
`PixmapState`-like entry keyed by host pixmap XID, augmented with a
refcount). The protocol-facing `ResourceTable.pixmaps` entries for
named pixmaps act as additional resource handles into the same
underlying backing — `FreePixmap` of a named pixmap drops the
client's reference.

After a window is destroyed, surviving `NamedCompositePixmap`
aliases need a home. Two reasonable places:

- **A backend-owned alias registry**, keyed by host pixmap XID and
  carrying `width`, `height`, `format`, and `refcount`. The
  protocol-facing pixmap resource (`ResourceTable.pixmaps`) carries
  a reference into this registry. When the window owning the alias
  goes away, the alias entry already lives in the backend registry;
  the `Window.composite_named_pixmaps` Vec disappears with the
  window but the alias keeps the backing alive through the registry
  entry. *This is the preferred location* — keeps lifetime
  management on the backend side where the Vk image lives.
- *Alternative*: a `ServerState`-level alias map indexed by the
  protocol-facing pixmap XID. Same effect, just a different home;
  less coupling to the backend. Chosen *only* if the backend-owned
  approach introduces protocol/backend layering issues during
  implementation.

The plan picks the backend-owned registry; the alternative is
documented for reviewers.

### Paint dispatch routing

Every draw-targeting drawable resolution path (currently
`host_drawable_target` on `ResourceTable`) needs to consult redirect
state. If the drawable is a window whose effective redirect (own or
inherited from an ancestor with `subwindows=true`) is active, the
paint goes to `redirected_backing.host_pixmap`; otherwise it goes to
today's `host_xid`.

This is the centre of the implementation. Every existing dispatcher
(core graphics, RENDER, MIT-SHM PutImage, …) goes through that
resolution. Done correctly, no individual handler needs to know about
redirect.

### Subtree redirect semantics

XComposite spec, section 3 ("Per-window redirection") and section 4
("Subtree redirection") together pin down:

- A window with `RedirectSubwindows` redirects all *direct and
  indirect descendants* (the entire subtree rooted at the window's
  children — not the window itself).
- Each redirected window has its own off-screen pixmap. The *parent*
  doesn't aggregate its children's drawing.
- Paints into the redirected window go into that window's own off-
  screen pixmap; clipping by parent visibility is the compositor's
  job, not the server's.

So the answer to codex's blocker question: **each redirected window
gets its own off-screen pixmap.** That matches the
`composite_named_pixmaps: Vec<…>` model (each window has its own).

For `RedirectWindow` (single-window), the window itself is the
redirected one. For `RedirectSubwindows(W)`, every child of W that
exists now or in the future is redirected. The redirect bit on the
parent is *inherited*, not flattened.

### Damage interaction

Today, the damage fanout in `damage_fanout.rs:40` keys by the
*drawable* of the paint op. After L2 we split two concerns
explicitly:

- **Public damage target** (the XID emitted in the DamageNotify
  event): the *window* XID. The compositor subscribes to
  DamageNotify on the window, not the backing pixmap. Backings are
  internal and have no public XID until a `NamedCompositePixmap`
  alias gives them one — and even then, the named pixmap is a
  snapshot the compositor reads, not a target it watches.
- **Internal redraw key** (the identifier the backend uses to mark
  which Vk image is dirty and needs re-blit): the *backing host
  pixmap XID* when redirect is active, or the *window host XID*
  when redirect is not active. This is the same kind of identifier
  the existing dirty-tracking machinery uses; the change is what
  identifier it gets for a redirected window.

Concretely: paint op resolves draw target through
`host_drawable_target`; that resolution is *backing-aware* under
L2. The backend marks the backing mirror dirty (internal key =
backing XID). The damage fanout layer, given a window XID and an
"internal target was dirtied" signal, then emits DamageNotify on
the *window* XID to clients subscribed there. Separating the two
keeps the existing internal dirty-tracking semantics
unchanged while routing the public event to the right XID.

### Overlay window lifecycle

Today: `COMPOSITE_OVERLAY_WINDOW = ResourceId(0x103)`, parent=root,
mapped Viewable, `host_xid: None`. Logical-only, not in
`top_level_order`.

After L2, two states:

- **No active compositor**: overlay stays logical-only. Top-levels
  are drawn directly from their mirrors as today. The overlay XID
  exists so marco's compositor probe doesn't break root's event
  mask (the bug fixed in commit `6b5b02e`).
- **Promoted**: overlay has a real backend mirror (full screen
  extent, depth 32 ARGB). KMS scans out *only* the overlay; the
  top-level chain still has mirrors, but they're consumed by the
  compositor's XRender ops (which read the redirected backings) and
  not drawn directly to scanout.

**Promotion trigger**: the first successful `NameWindowPixmap` on
any window. `Redirect*` alone is *not* a sufficient trigger — a
client can redirect without ever taking a named pixmap (a
misbehaving probe, or a compositor that bails before its first
frame). Promotion on first `NameWindowPixmap` proves the
compositor intends to use the overlay as its canvas.

**Demotion trigger**: when all of the following hold simultaneously:
- No entry in `composite_redirects` references any window.
- No `NamedCompositePixmap` alias remains (every named pixmap was
  freed).
- No backend-internal state still tags the overlay as the active
  scanout canvas — concretely, no scanout BO in any output's
  `scanout_pools` is in `Submitted`/`Pending` phase backed by the
  overlay's mirror. The KMS backend already tracks BO phases per
  output (`backend.rs::try_vulkan_composite_flip`); the demotion
  check polls those phases for "overlay-backed in flight" and
  defers demotion until the next `pageflip-complete` clears them.

In short: demote when there is no remaining redirect *and* no
remaining named pixmap *and* no in-flight overlay-backed scanout
flip. Both transitions are pure backend state; no protocol-surface
change.

**Input**: per XComposite spec, the overlay window is
input-transparent. Our implementation: when promoted, the overlay
participates in scanout but not in pointer/keyboard hit-test. The
hit-test walks `top_level_order` minus overlay (overlay is excluded
even though it is, technically, the visually-topmost mapped
window). Event-mask selection on the overlay still works — the
compositor selects `Exposure` on it today; we keep that path
functional through promotion.

**Stacking interaction**: when promoted, the overlay is always
drawn last in the compositor pass (it is the destination of the
compositor's blends; nothing should occlude it visually).
Override-redirect popups still draw to scanout on their own — they
are not redirected by `RedirectSubwindows(root)` — so when both an
OR popup and a compositor are active, the OR popup draws after the
overlay. This is handled by keeping OR top-levels in
`top_level_order` and the overlay as a separate "draw after
everything" slot.

### Multi-compositor semantics

XComposite spec allows multiple clients to all hold redirects on the
same window (refcounted). xorg-server's behaviour: the first redirect
sets the mode; subsequent matching redirects refcount; an
`Unredirect` decrements; the redirect ends when refcount hits zero.

We do *not* implement this in L2 v1. One owner per (window,
subwindows). A second redirect attempt by a different client returns
`BadAccess`. Marco is the only compositor in our target environment;
this is enough. A follow-up phase can add refcounting if a real
second compositor emerges.

### ParentRelative + BackgroundPixmap interactions

When a redirected window is mapped with `background_pixmap =
ParentRelative`, the server's auto-fill of the window walks up to
the parent's bg_pixmap and tiles it. Under L2, the auto-fill must
target the redirected backing, not the screen mirror, and must
obey L1's α=255 rule. The dispatch routing change above covers this
naturally (auto-fill goes through `host_drawable_target`).

### Risks specific to L2

- **Window resize during redirect.** `NameWindowPixmap` returns the
  *current* backing; on resize, a new backing is allocated; the old
  named pixmap aliases the *old* contents (frozen). Stale-pixmap
  retention isn't optional — the compositor reads it for the next
  frame's cross-fade.
- **Window destruction during redirect.** Named pixmaps outlive the
  window (XComposite spec). Backing lifetime is tied to the named-
  pixmap alias, not the window. Refcounting on the host-pixmap XID.
- **Off-screen pixmap format vs window depth.** Backing pixmap depth
  must match the redirected window's depth. If the window is depth-32
  ARGB, the backing is ARGB; the compositor wraps it in a matching
  Picture format and gets correct alpha. If the window is depth-24,
  the backing is depth-24, and L1's α-padding-as-bookkeeping applies.
- **Override-redirect windows.** OR windows aren't redirected by
  `RedirectSubwindows(root)`. Marco doesn't compose them; our hit-
  test for input still finds them. They draw directly to scanout
  alongside the overlay if any. Stacking order must keep them above
  the overlay where appropriate (e.g. tooltips).
- **GLX/DRI3 + redirect.** A GLX direct-rendering client whose
  drawable is a redirected window currently writes via DRI3 dma-buf
  into the host window's backing. After L2 the dma-buf needs to be
  bound to the redirected backing instead. Phase 4.2 plumbing needs
  re-targeting.
- **Existing PRESENT + redirect.** PRESENT::PresentPixmap on a
  redirected window currently copies to the host window. Under L2
  it should copy to the redirected backing.

### Test strategy for L2

1. **Marco compositor smoke**. `compositing-manager = true`, restart
   marco, expect `RedirectSubwindows` + `NameWindowPixmap` in log
   without errors, expect shadows visible in PPM scanout.
2. **picom**. Run picom on a lighter WM (fvwm3) to verify second
   compositor.
3. **Redirect-toggle**. Enable/disable `compositing-manager` at
   runtime; expect clean transition both directions, no leaked
   pixmaps.
4. **Resize under redirect**. Resize a window; verify new
   `NameWindowPixmap` returns a fresh XID, old XID still returns
   pre-resize content.
5. **Destroy under redirect**. Destroy a redirected window after
   `NameWindowPixmap`; verify the named pixmap stays valid until
   freed.

## L3 — GLX_EXT_texture_from_pixmap gating

### The empirical question

mate-session's `is-accelerated` check fails because we don't advertise
`GLX_EXT_texture_from_pixmap`. The check's exit code 256 maps to "GL
not viable". Marco's compositor module reads this *or* checks
directly. Two scenarios:

- **Scenario A**: marco's compositor probes the GLX extension string
  and disables itself entirely if TFP is missing. In this case L3 is
  a *hard prerequisite* for L2 producing any visible effect, even
  though the compositor module would otherwise use the XRender
  backend.
- **Scenario B**: marco's compositor uses TFP only for the GLX
  backend and falls back to XRender when TFP is missing. In this
  case L3 is *soft* — the XRender backend works as long as L2 is
  done. L3 only matters for users who specifically want the GLX
  compositor backend's performance.

The live log on our environment shows: **marco does engage
RedirectSubwindows and NameWindowPixmap** — confirming scenario B.
Marco's compositor module proceeds past the TFP gate into XRender-
based composition; it bails not because of TFP but because we
BadAlloc on NameWindowPixmap.

So **L3 is soft in this environment**. We can defer it. The plan-
level work prioritises L2 backend support; L3 follows only if a
user-visible gap remains after L1+L2 are in.

We need to confirm this empirically once L2 lands — if marco's
compositor engages and shadows appear, scenario B is confirmed and
L3 is genuinely optional. If it bails for a different reason after
NameWindowPixmap succeeds, we revisit.

### Scope if/when promoted

- Add `GLX_EXT_texture_from_pixmap` to the advertised extension
  string returned by `QueryServerString` (already partial — we
  return some of the canonical attribute tags).
- Implement `glXBindTexImageEXT` and `glXReleaseTexImageEXT` as GLX
  minor opcodes (proxy or stub depending on whether direct rendering
  uses them).
- Plumb redirected pixmap → DRI3 fd export. Already works at the
  protocol layer (`BufferFromPixmap`); needs to handle the redirected
  backing pixmap specifically.

## Phasing

- **Phase A (L1)** — alpha invariant + composite shader switch + per-
  path audit. ~1 calendar week of focused work. Visible result: no
  black rim under any client, regardless of compositor mode.
- **Phase B (L2)** — KMS backend composite plumbing (overlay window
  promotion, off-screen backing, redirect routing, damage mapping).
  ~2 calendar weeks. Visible result: marco's compositor effects work.
- **Phase C (L3)** — only if empirical evidence after Phase B says
  it's needed. ~1 week if/when.

Phase A is the *foundation* and must land first: without L1, L2's
output still shows the rim through marco's compositor blends when
marco wraps a depth-24 redirected pixmap in an ARGB32 Picture (which
it does — see `compositor-xrender.c` in marco source).

L2 is not optional for the spec's goal (full desktop). L3's promotion
is conditional.

## Risks (consolidated)

- **L1 frag-shader flip without complete paint-path audit** —
  windows go transparent at first paint. Mitigation: per-path roll-
  out gated by opt-in flag; flip the shader last.
- **rendercheck baseline shift on AA-coverage tests after L1.** One-
  time rebase, signed off.
- **xts5 regressions on partial-paint scenarios.** Audit-driven.
- **L2 resource lifetime bugs** — named pixmaps outliving windows,
  resize-frozen aliases, host XID refcounting. Test the lifecycle
  cases explicitly.
- **L2 + Phase 4.2 DRI3 interaction** — redirected windows need
  dma-bufs bound to the redirected backing.
- **L2 + PRESENT** — PresentPixmap path needs redirect-aware
  routing.
- **Overlay-as-real-window promotion** — input hit-test must skip
  it; stacking must be deterministic; demotion must clean up.
- **Multi-compositor / re-redirect** — out of scope for L2 v1;
  single-owner with BadAccess on conflict.
- **L3 turns out to be a hard prerequisite after L2 lands** — adds
  ~1 week and ships before MATE works.
- **ParentRelative bg_pixmap chains** — L1's α=255 rule must cover
  the recursive parent-walk in auto-fill.

## Open questions

1. **L1 contract — semantic vs depth as proxy.** ✅ **Resolved
   (A.18b, 2026-05-11).** Audit of `crates/yserver-core/src/resources.rs`
   confirms the only advertised TrueColor visuals are
   `ROOT_VISUAL` (depth=24, `alpha_mask=0`) and `ARGB_VISUAL`
   (depth=32, `alpha_mask=0xff00_0000`). The canonical signal is
   `Visual.alpha_mask`; "depth-24 vs depth-32" remains a reliable
   proxy for "server-owned α vs client-meaningful α" so long as
   no new Visual breaks the mapping. If a future depth-30 deep-
   colour or ARGB-shaped depth-24 visual is added, the per-path
   α policy should re-key on `alpha_mask != 0` directly. The
   backend currently reads `WindowState.depth` /
   `PixmapState.depth` for the decision, which matches the proxy.
2. **CopyArea α-preserve scope.** The internal rule preserves src α
   when copying between server-owned mirrors. We expect this to be
   semantically equivalent to xorg-server's behaviour (which has no
   internal α to preserve at all, but treats unwritten pixels as
   unwritten). Audit during planning whether any X11 client behaves
   differently against a destination whose α was 0 (server-owned)
   vs 255 (also server-owned). If a real client diverges, we add
   an opt-in "force opaque on copy" per-call flag for the affected
   path. Default: preserve.
3. **L2 redirect modes.** Implement Manual only initially.
   Automatic (server composites on the compositor's behalf) is rare
   and not needed for marco. Defer to a follow-up; document the
   decision in the plan.
4. **Overlay participation, precisely.** When promoted:
   - In scanout: yes — overlay is the canvas the compositor draws
     onto, and KMS scans out from its mirror.
   - In pointer hit-test: no — overlay is input-transparent;
     hit-test walks `top_level_order` minus overlay.
   - In event masks: yes (existing) — compositor selects `Exposure`
     on the overlay XID; we preserve that path. Other event masks
     on the overlay are honoured but unusual.
   - In `top_level_order`: no — overlay is a separate "draw after
     everything else" slot, not part of the normal stacking list.
     OR popups that draw to scanout sit *above* the overlay
     visually (drawn after).
5. **L3 empirical confirmation.** Once L2 lands, verify marco's
   compositor engages without TFP. The live log on this environment
   already shows marco reaching `RedirectSubwindows` and
   `NameWindowPixmap` without TFP, so the prediction is "yes". If
   it doesn't, L3 promotes from "optional" to "blocking before MATE
   compositor works".

## Glossary

- **Mirror** — backend-owned Vulkan image representing a drawable's
  current contents. Window mirrors live in `WindowState.vk_mirror`;
  pixmap mirrors in `PixmapState.vk_mirror`. Both are
  `B8G8R8A8_UNORM`.
- **Redirected backing** — under L2, an off-screen pixmap mirror
  that receives paint targeting a redirected window. The window
  itself is not on screen; the compositor reads the backing.
- **Named pixmap** — under L2, an X11 Pixmap resource that aliases a
  redirected backing at a moment in time. Read-only from the
  compositor's perspective; survives the window's destruction.
- **Overlay window** — `COMPOSITE_OVERLAY_WINDOW`, currently
  `ResourceId(0x103)`. Logical-only today; promoted to a real
  scanout target when redirect is active.
