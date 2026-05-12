# Full X11 composite support — implementation plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Spec:** `docs/superpowers/specs/2026-05-11-x11-composite-design.md` (v3.1).
**Revision:** v4 (post codex review round 3).

**Goal:** Land L1 (mirror alpha contract), L2 (XComposite redirect end-to-end on KMS), and conditionally L3 (`GLX_EXT_texture_from_pixmap`) so a MATE session under `yserver-hw` renders identically to MATE under `xorg-server`: no black rim, marco compositor effects (shadows, fades, ARGB-blended titlebars) active.

**Architecture:** Three independently shippable layers. **L1** removes the `α=1.0` clamp at composite time. Per-paint-path α-write policy uses **three mechanisms** depending on how the paint reaches Vulkan — see "L1 mechanism matrix" below. **L2** turns the existing COMPOSITE bookkeeping into real off-screen backings: the backing is allocated by Redirect activation (B.6); `NameWindowPixmap` *aliases* the current backing (B.5); a refcounted alias registry tracks lifetime; redirect-aware drawable resolution routes paint to the backing; overlay-window promotion/demotion gates scanout. **L3** advertises `GLX_EXT_texture_from_pixmap` and exports redirected backings via DRI3 — *only* if empirical evidence after L2 lands shows marco's compositor needs it.

**Tech stack:** Rust (workspace: `yserver`, `yserver-core`, `yserver-protocol`), Vulkan (`ash`), GLSL frag/vert shaders compiled to SPIR-V via `build.rs`, X11/XCB on the client side, just recipes for `rendercheck` and `xts5`.

## Verified citations

- KMS backend: `crates/yserver/src/kms/backend.rs`
  - `composite_opcode()` → `None` at `:6905`
  - `name_window_pixmap()` → `Unsupported` at `:7335`
  - Composite scene builder `walk_subtree_into_draws`: window mirror draws emit `use_src_alpha: false` at `:5896`; bg-pixmap draw at `:5782`; cursor at `:5810`
  - `try_vk_fill_with_function` at `:2603` → `vk/ops/fill::record_fill_rectangles`; `Copy` GcFunction goes to `try_vk_solid_fill` at `:2720`
- Op recorders (each owns its own Vulkan mechanism):
  - `crates/yserver/src/kms/vk/ops/fill.rs::record_fill_rectangles` — **`cmd_clear_attachments`** (no shader)
  - `crates/yserver/src/kms/vk/ops/copy.rs::record_copy_area_*` — **`cmd_copy_image`** (no shader, α auto-preserved if formats match)
  - `crates/yserver/src/kms/vk/ops/image.rs::record_put_image`/`record_get_image` — **`cmd_copy_buffer_to_image`** / `cmd_copy_image_to_buffer` (no shader; α set by caller in staging buffer)
  - `crates/yserver/src/kms/vk/ops/render.rs` — **pipelined frag shader** (`cmd_bind_pipeline` + `cmd_draw`)
  - `crates/yserver/src/kms/vk/ops/text.rs` — pipelined frag shader
  - `crates/yserver/src/kms/vk/ops/traps.rs` — **CPU rasterisation** into an R8 coverage buffer; the actual paint reuses the RENDER Composite pipeline
  - `crates/yserver/src/kms/vk/logic_fill_pipeline.rs` — non-Copy GcFunction (XOR/AND/OR/…), pipelined frag shader
- Composite shaders: `crates/yserver/src/kms/vk/shaders/composite.{frag,vert}.glsl`
- Pipeline blend state: `crates/yserver/src/kms/vk/pipeline.rs:229..243` (premultiplied src-over)
- Spec-constant precedent: `crates/yserver/src/kms/vk/render_pipeline.rs:737..774` (4 specialization constants today)
- Mirror initial state: `crates/yserver/src/kms/vk/target.rs::initialize_clear` (α=0 on all pixels)
- COMPOSITE handlers: `crates/yserver-core/src/core_loop/process_request.rs:2417..2576`
- Resize-time alias invalidation: `crates/yserver-core/src/core_loop/process_request.rs:482 invalidate_composite_named_pixmaps_to_state`, called from `:6102` in ConfigureWindow when `(width, height)` change. **Currently unconditionally drops all aliases on resize** — this contradicts the COMPOSITE-spec rule that named pixmaps freeze with pre-resize content. L2 must change this.
- `composite_redirects: HashMap<(ResourceId, bool), u8>` at `crates/yserver-core/src/server.rs:212`
- `Window.composite_named_pixmaps: Vec<NamedCompositePixmap>` at `crates/yserver-core/src/resources.rs:2125`
- Drawable resolution: `crates/yserver-core/src/resources.rs:1242 host_drawable_target`
- DRI3 `BufferFromPixmap` handler reads `state.resources.pixmap(...).host_xid` *directly* at `crates/yserver-core/src/core_loop/process_request.rs:4337..4380` — NOT via `host_drawable_target`. Implication: once a named pixmap's `host_xid` is set (via `set_pixmap_host_xid` at `:2551`) to point at the backing, DRI3 export "just works".
- Disconnect cleanup: `crates/yserver-core/src/core_loop/process_disconnect.rs` currently retains `composite_redirects` only by *dead window*, not by *owning client* (e.g., line in the area of `:164`). Once `RedirectRecord.owner` exists (B.1), the cleanup needs to also drop redirects whose owner just disconnected.
- Pixmap host-XID registration: `set_pixmap_host_xid` in `resources.rs:1210`, called from process_request.rs at `:2551` (NAME_WINDOW_PIXMAP), `:2924`, `:4204`, `:4310`, `:8576`.
- Damage fanout: `crates/yserver-core/src/core_loop/damage_fanout.rs:32 accumulate_damage_to_state` (keyed by public XID, no change needed beyond audit)
- `COMPOSITE_MAJOR_OPCODE = 144` (`yserver-core/src/nested.rs:57`)
- Integration test pattern: `crates/yserver/tests/dri3_fd_leak.rs` (`#[test] #[ignore = "needs live Vulkan ICD"]`, run with `cargo test --test <name> -- --ignored`)
- Recipes: `just rendercheck-yserver`, `just xts-yserver` (per `memory/reference_test_recipes.md`)

## L1 mechanism matrix

| Mechanism | Paint paths | How α=opaque lands | How α=client-driven lands |
|-----------|-------------|--------------------|---------------------------|
| **(M1) Caller-side α byte** | FillRectangles, PolyFillRectangle, ClearArea, PolyRectangle/Line/Segment/Point (Copy GcFunction), Poly[Fill]Arc (Copy), FillPoly (Copy), tiled fill, PutImage ZPixmap, PutImage XYBitmap/XYPixmap, ImageText/PolyText background fills if they go through fill.rs | Caller sets the `[f32;4]` `color[3] = 1.0` (or the staging-buffer α byte = `0xFF`) when the destination is depth-24 | Caller forwards the source α verbatim when the destination is depth-32 ARGB |
| **(M2) Fragment specialization constant** | RENDER FillRectangles/Composite/CompositeGlyphs, RENDER Trapezoids/Triangles (via the R8 coverage path's downstream Composite), text pipeline (foreground glyph rasterisation), non-Copy GcFunction (logic_fill_pipeline) | Pipeline variant compiled with `ALPHA_MODE = Opaque (1)` emits α=1.0 from frag stage | Variant with `ALPHA_MODE = PassThrough (0)` or `Coverage (2)` |
| **(M3) Auto-preserve** | CopyArea (same-format mirror→mirror), GetImage→PutImage round-trips | No code change — `cmd_copy_image` between same-format mirrors preserves α verbatim; rule 4 is automatic | Same |

> **CopyPlane is NOT M3.** `crates/yserver/src/kms/backend.rs:7896 copy_plane` rasterises the bit-plane into foreground/background rect lists and dispatches both through `try_vk_fill_with_function` (`:7955..7956`). Its α policy is M1 (caller-side α byte), same as FillRectangles.

The composite scene shader (composite.frag.glsl) is its own pipeline with its own `SRC_ALPHA_MODE` spec-constant (A.2) — that's the final dial flipped in A.16.

## Project conventions

- Before committing: `cargo fmt`, `cargo clippy -- -W clippy::pedantic`, `cargo test`. Fix all warnings.
- Don't amend commits unless asked. Each step's commit stands alone.
- Use `codex` for spec/plan reviews.
- Visual smoke required only for interactive checks (`memory/feedback_commit_after_testing.md`); unit-test/rendercheck passes are commit-worthy on their own.
- All work on the `mate` branch. PR-time squash to `master`.

---

# Phase A — L1: mirror alpha contract

**Visible result:** No black rim around framed windows under any client / WM combination, regardless of whether marco's compositor mode engages.

**Pattern:** for each path, (a) write the failing α-invariant test, (b) confirm red, (c) apply the path's mechanism from the matrix above, (d) confirm green, (e) commit.

The composite-scene window-mirror draws stay at `SRC_ALPHA_MODE = 0` (force opaque) through A.1–A.15. The flip happens last in A.16.

---

### Task A.1a: Test harness — in-process server fixture

**Goal:** Boot a `ServerState` and KMS backend (or a headless variant if a real KMS backend won't init off-display) inside the test process. No XCB yet — just expose a programmatic interface for creating resources and dispatching X11 requests through the same `process_request` codepath the real socket loop uses.

**Files:**
- Create: `crates/yserver/tests/common/server_fixture.rs` (and re-export from `crates/yserver/tests/common/mod.rs`)
- Reference: `crates/yserver/tests/dri3_fd_leak.rs` for the `VkContext::new()` + `#[ignore]` pattern
- Reference for state construction: `grep -n "ServerState::new\|fn dispatch" crates/yserver-core/src/` to find the existing entry points

**Step 1 — failing test:**
```rust
#[test]
#[ignore = "needs live Vulkan ICD"]
fn fixture_starts_and_creates_root_resources() {
    let fix = ServerFixture::start();
    assert!(fix.root_window().0 != 0);
    assert!(fix.has_default_visuals());
}
```

**Step 2:** Run; expect compile fail.

**Step 3:** Implement `ServerFixture` — owns a `ServerState`, the KMS backend trait object (try the real backend first; fall back to a minimal in-test backend implementing the same trait if KMS device acquisition fails in CI), a `next_client_id` counter, and `dispatch_request(client, buf)` that calls into `process_request`. Provide `root_window()`.

**Step 4:** Pass.

**Step 5:** Commit `test(L1): server fixture for in-process integration tests`.

---

### Task A.1b: Test harness — request helpers

**Goal:** Add the X11 request helpers needed by L1 paint tests: `create_window`, `create_pixmap`, `map_window`, `create_gc`, `fill_rectangle`, `clear_area`, `put_image`, `copy_area`, `set_root_pixel`.

**Files:**
- Modify: `crates/yserver/tests/common/server_fixture.rs`

**Step 1:** Failing test:
```rust
#[test] #[ignore]
fn fixture_can_fill_rectangle() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(100, 100, 24);
    fix.map_window(win);
    let gc = fix.create_gc(win, /*fg=*/0x00_00_ff_00);
    fix.fill_rectangle(win, gc, 10, 10, 30, 30);
    // No assertion on contents yet — that lands in A.1c when we add capture.
    assert!(fix.dispatched_without_error());
}
```

**Step 2:** Run; expect compile fail / missing helpers.

**Step 3:** Implement each helper as a tiny wrapper that encodes the X11 request bytes and calls `dispatch_request`. Use the wire encoders from `yserver_protocol::x11::*` (already used throughout the codebase).

**Step 4:** Pass.

**Step 5:** Commit `test(L1): paint-request helpers on the server fixture`.

---

### Task A.1c: Test harness — mirror readback

**Goal:** Expose per-pixel access to a window's mirror image (for L1 invariant assertions) and to the full composited scanout (for A.16's pass-through test).

**Files:**
- Modify: `crates/yserver/tests/common/server_fixture.rs`
- Reference: `crates/yserver/src/kms/vk/dst_readback.rs` (existing GPU→host readback machinery), `crates/yserver/src/kms/vk/ops/image::record_get_image` for the staging-buffer pattern.

**Step 1:** Failing test:
```rust
#[test] #[ignore]
fn fixture_captures_window_mirror_pixels() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    let gc = fix.create_gc(win, 0x00_00_ff_00);
    fix.fill_rectangle(win, gc, 0, 0, 64, 64);
    let img = fix.capture_window_mirror(win);
    // Green channel set, alpha = whatever the path wrote (test assertion comes in A.3).
    assert_eq!(img.pixel(32, 32).g, 0xff);
    assert_eq!(img.dimensions(), (64, 64));
}
```

**Step 2:** Run; expect missing `capture_window_mirror`.

**Step 3:** Implement `capture_window_mirror(win) -> ImageRgba8` and `capture_scanout() -> ImageRgba8` by allocating a CPU staging buffer, recording a `cmd_copy_image_to_buffer` from the mirror's `vk_image`, submitting and waiting via `with_ops_cb`, and reading the staging memory.

**Step 4:** Pass.

**Step 5:** Commit `test(L1): mirror + scanout readback on server fixture`.

---

### Task A.2: Composite shader `SRC_ALPHA_MODE` spec-constant

**Goal:** Move the per-draw `use_src_alpha` push constant in `composite.{frag,vert}.glsl` to a fragment specialization constant. Compile both variants (mode-0 = force opaque, mode-1 = pass-through). The Rust-side `CompositeDraw.use_src_alpha: bool` becomes a *pipeline selector* — semantics unchanged, observable behaviour identical.

**Files:**
- Modify: `crates/yserver/src/kms/vk/shaders/composite.frag.glsl`
- Modify: `crates/yserver/src/kms/vk/shaders/composite.vert.glsl` (strip `v_use_alpha` and the `use_src_alpha`/`_pad` push-constant fields)
- Modify: `crates/yserver/src/kms/vk/compositor.rs` (build two pipelines, select by `CompositeDraw.use_src_alpha`)
- Modify: `crates/yserver/src/kms/backend.rs` (push-constant struct shrinks)

**Step 1:** Add a unit test in `compositor.rs` (or new test module) asserting two pipeline variants exist:
```rust
#[test]
fn composite_pipeline_has_two_alpha_mode_variants() {
    let vk = test_vk_context();
    let c = Compositor::new(&vk, /* ... */).unwrap();
    assert!(c.pipeline_for(/*alpha=*/ false).as_raw() != c.pipeline_for(true).as_raw());
}
```

**Step 2:** Run; expect fail (currently one pipeline).

**Step 3:** Implement frag shader:
```glsl
#version 450
layout(constant_id = 0) const int SRC_ALPHA_MODE = 0;
layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;
layout(set = 0, binding = 0) uniform sampler2D tex;
void main() {
    vec4 c = texture(tex, v_uv);
    out_color = (SRC_ALPHA_MODE != 0) ? c : vec4(c.rgb, 1.0);
}
```
Vertex shader drops `v_use_alpha` and the `use_src_alpha/_pad` floats; push-constant Rust struct shrinks accordingly. The compositor caches both pipelines.

**Step 4:** Run unit test + `just rendercheck-yserver` — confirm no observable change.

**Step 5:** Commit `refactor(compositor): convert use_src_alpha to fragment spec-constant`.

---

### Task A.3: FillRectangles + family — caller-side opaque α byte

**Mechanism:** M1 (caller-side α byte in `cmd_clear_attachments`'s `[f32;4]` color value).

**Goal:** Every call site that constructs the colour array passed to `record_fill_rectangles` (and `try_vk_solid_fill`'s equivalent path) must set `color[3] = 1.0` when the destination is depth-24, and pass through client α when the destination is depth-32.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs:2603 try_vk_fill_with_function` and `:2720 try_vk_solid_fill` (and any `pixel_to_rgba` / `argb_from_pixel` helper they share)
- Test: `crates/yserver/tests/alpha_invariant.rs` (new)

**Step 1: Tests**
```rust
#[test] #[ignore]
fn fill_rectangle_writes_alpha_255_on_depth24() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.map_window(win);
    fix.fill_rectangle_simple(win, 10, 10, 20, 20, 0x00_00_ff_00);
    let m = fix.capture_window_mirror(win);
    assert_eq!(m.pixel(15, 15).a, 0xff);
    assert_eq!(m.pixel(50, 50).a, 0x00); // untouched
}

#[test] #[ignore]
fn fill_rectangle_argb32_preserves_client_alpha() {
    let mut fix = ServerFixture::start();
    let pix = fix.create_pixmap(64, 64, 32);
    fix.fill_rectangle_pixmap(pix, 0, 0, 64, 64, 0x80_00_00_ff);
    let m = fix.capture_pixmap_mirror(pix);
    let p = m.pixel(32, 32);
    assert_eq!((p.b, p.g, p.r, p.a), (0xff, 0, 0, 0x80));
}
```

**Step 2:** Run; expect failure on the depth-24 alpha assertion (today α stays 0; the composite shader's force-opaque hides it from scanout).

**Step 3:** Find the helper that converts a 32-bit X11 pixel to `[f32;4]` for the fill recorder. Modify so that when the destination is depth-24 the alpha component is forced to `1.0`; when depth-32, preserve the pixel's alpha byte verbatim. The depth is already available in the call frame (`drawable.depth()` via `host_drawable_target`).

**Step 4:** Run tests + `just rendercheck-yserver` (expect no delta since composite still force-opaque).

**Step 5:** Commit `feat(L1): FillRectangles writes opaque alpha on depth-24, preserves on ARGB32`.

---

### Task A.4: PolyRectangle / PolyLine / PolySegment / PolyPoint — verify routing

**Mechanism:** M1 (same call site as A.3 for the Copy GcFunction case).

**Files:**
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1:** Add tests — one per opcode, depth-24 destination, assert α=255 over the stroked pixels.

**Step 2:** Run.

**Step 3:** If A.3 wired the conversion helper, these pass already. Otherwise, find any local path that bypasses the helper and route through.

**Step 4:** Pass.

**Step 5:** Commit `test(L1): PolyRectangle/Line/Segment/Point opaque alpha`.

---

### Task A.5: PolyArc / PolyFillArc — verify routing

Same shape as A.4. One depth-24 test per opcode covering a fully-filled arc and a stroked arc.

**Files:** test only unless a separate fill helper exists.

**Step 5:** Commit `test(L1): Poly[Fill]Arc opaque alpha`.

---

### Task A.6a: FillPoly — verify routing

Same as A.5. One depth-24 test asserting α=255 inside the polygon interior.

**Step 5:** Commit `test(L1): FillPoly opaque alpha`.

---

### Task A.6b: Non-Copy GcFunction (logic_fill_pipeline) — Opaque spec-constant

**Mechanism:** M2 (frag shader spec-constant). When `GcFunction != Copy`, `try_vk_fill_with_function` skips the fast path and falls through to the logic-fill pipeline.

**Files:**
- Modify: `crates/yserver/src/kms/vk/logic_fill_pipeline.rs` + its frag shader (locate via `grep -n "FRAGMENT_SPV\|logic_fill" crates/yserver/src/kms/vk/logic_fill_pipeline.rs`)
- Modify: `crates/yserver/src/kms/backend.rs` fill-with-function dispatch
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1:** Test — depth-24 destination filled with `GcFunction = Xor`; assert α=255 over the touched pixels (XOR semantics affect the RGB but α is server-owned and must end up opaque).

**Step 2:** Run; expect fail.

**Step 3:** Add `ALPHA_MODE` spec-constant (0/1/2 for PassThrough/Opaque/Coverage) to the logic-fill frag shader, mirroring the precedent in `render_pipeline.rs:737..774`. Build two pipeline variants. Dispatch picks Opaque for depth-24, PassThrough for depth-32.

**Step 4:** Run.

**Step 5:** Commit `feat(L1): logic-fill pipeline honours destination-depth alpha invariant`.

---

### Task A.7: ImageText / PolyText — text-pipeline spec-constant

**Mechanism:** M2 (text frag shader).

**Files:**
- Modify: `crates/yserver/src/kms/vk/shaders/text.frag.glsl`
- Modify: `crates/yserver/src/kms/vk/text_pipeline.rs`
- Modify: `crates/yserver/src/kms/vk/ops/text.rs` (pipeline selection)
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1: Tests**
- `ImageText8` on depth-24 → α=255 across the full glyph bounding rect (background + foreground; ImageText paints both).
- `PolyText8` on depth-24 → α=255 *only* on foreground pixels; background pixels left untouched (α stays at mirror's prior value).

**Step 2:** Run; fail.

**Step 3:** Add `ALPHA_MODE` spec-constant; compile Opaque + PassThrough variants (Coverage variant is built later in A.13 — defer if it adds churn). The text op selects Opaque for depth-24. ImageText vs PolyText difference is in how rows are filled (background sweep vs glyph-only coverage), not in α handling.

**Step 4:** Run + rendercheck.

**Step 5:** Commit `feat(L1): ImageText/PolyText alpha invariant`.

---

### Task A.8: PutImage ZPixmap — caller-side α byte in staging buffer

**Mechanism:** M1 (PutImage uses `cmd_copy_buffer_to_image`; α is whatever the caller memcpy'd into the staging buffer).

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs::try_vk_put_image` (the row-pack loop that fills the staging buffer)
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1: Tests**
- Depth-24 PutImage of a known RGB pattern → α=255 across the destination rect (regardless of what client sent in the alpha-byte slot of an X8R8G8B8 row).
- Depth-32 ARGB32 PutImage with α-gradient → α matches client bytes verbatim.

**Step 2:** Fail.

**Step 3:** In the row-packing loop, when the destination depth is 24, overwrite the alpha byte to `0xFF`. When depth is 32, keep the client byte.

**Step 4:** Run + rendercheck.

**Step 5:** Commit `feat(L1): PutImage ZPixmap depth-aware alpha invariant`.

---

### Task A.9: PutImage XYBitmap / XYPixmap — caller-side α byte on set bits

**Mechanism:** M1.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs::try_vk_put_image` (XY-format branch — the bit-expansion path)
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1:** Test — XYBitmap with a checkerboard pattern; α=255 on "1" pixels; α unchanged on "0" pixels (PutImage XY paints only set bits — semantically equivalent to a stencil).

**Step 2:** Fail.

**Step 3:** Set α=0xFF in the staging row only for bits the XY expansion writes. Unset bits → no write to staging (the existing path already skips them; verify).

**Step 4:** Run.

**Step 5:** Commit `feat(L1): PutImage XYBitmap/XYPixmap opaque alpha on set bits`.

---

### Task A.10a: ClearArea (solid bg_pixel) — caller-side α byte

**Mechanism:** M1 (same fill path as A.3).

**Files:**
- Modify: caller side of `fill_mirror_solid` (the ClearArea handler — locate via `grep -n "fn.*clear_area\|fill_mirror_solid" crates/yserver-core/src/core_loop/process_request.rs crates/yserver/src/kms/backend.rs`)
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1:** Test — `ClearArea(win, 10, 10, 30, 30)` on a depth-24 window with `bg_pixel = 0xff_80_80_80`; α=255 across the cleared rect.

**Step 2:** Fail unless A.3's helper change already covers it.

**Step 3:** Ensure the colour-converter helper is shared with ClearArea's fill path; if not, plumb depth in.

**Step 4:** Run.

**Step 5:** Commit `feat(L1): ClearArea opaque alpha`.

---

### Task A.10b: ClearArea (ParentRelative bg_pixmap) — recursive auto-fill respects depth

**Mechanism:** M1 (auto-fill ultimately routes through the same fill path).

**Files:**
- Modify: ParentRelative auto-fill walker (locate via `grep -n "ParentRelative\|background_pixmap" crates/yserver-core/src/core_loop/process_request.rs`)
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1:** Test — parent with `bg_pixmap = solid_gray`, child with `bg_pixmap = ParentRelative` mapped at `(50,50)`; on map, capture child mirror; assert α=255 across the entire child rect *and* RGB matches parent's tiled content at the child-relative offset.

**Step 2:** Fail.

**Step 3:** Ensure the recursive parent-walk passes through the same depth-aware α helper at each level. Mixed-depth parents in the chain: deferred to A.18b unless a real client triggers it.

**Step 4:** Run.

**Step 5:** Commit `feat(L1): ParentRelative auto-fill honours opaque alpha through the chain`.

---

### Task A.10c: CopyArea α-preserve verification

**Mechanism:** M3 (auto-preserve — `cmd_copy_image` between same-format mirrors preserves α).

**Files:**
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1: Two tests** — one per server-owned vs client-meaningful α:
- **Depth-24 (server-owned α):** paint half of `src_win` (depth-24) so its mirror has `[some pixels: α=255, others: α=0]`. `CopyArea(src_win → dst_win)` of the full extent. Assert dst mirror's α distribution matches src.
- **Depth-32 ARGB (client-meaningful α):** paint `src_pix` (depth-32) with a known α gradient via `PutImage` ARGB32. `CopyArea(src_pix → dst_pix)` between two ARGB32 pixmaps. Assert each sampled α value in dst exactly matches src (rule 5 round-trip).

**Step 2:** Run. Should **pass already** post-A.3/A.8 (cmd_copy_image is α-faithful across both depths).

**Step 3:** If a wrapper anywhere clamps α, route it through the same image-copy primitive.

**Step 4:** Run.

**Step 5:** Commit `test(L1): CopyArea preserves mirror alpha (rule 4)`.

---

### Task A.10d: CopyPlane — caller-side opaque α byte

**Mechanism:** M1 (NOT M3). `crates/yserver/src/kms/backend.rs:7896 copy_plane` reads the source plane bit-by-bit, partitions pixels into foreground/background rect lists, then dispatches both through `try_vk_fill_with_function` (`:7955..7956`) — same fill mechanism as A.3.

**Files:**
- Test: `crates/yserver/tests/alpha_invariant.rs` (likely pass-through after A.3 because the dispatch funnels into the same shared helper)
- Audit: confirm `try_vk_fill_with_function`'s α policy applies regardless of whether the rects came from CopyPlane or PolyFillRectangle.

**Step 1:** Test — `CopyPlane(plane_mask = 1, src_win → dst_win)` of a known bit pattern on a depth-24 destination; assert α=255 across the destination rect (both foreground and background fills land at α=255).

**Step 2:** Run. Should pass if A.3 wired the α-byte logic at the shared call site.

**Step 3:** Fix if a CopyPlane-specific code path bypassed the helper.

**Step 4:** Run.

**Step 5:** Commit `test(L1): CopyPlane writes opaque alpha via M1`.

**Mechanism:** M2 (RENDER pipelines, already use spec-constant cache).

**Files:**
- Modify: `crates/yserver/src/kms/vk/ops/render.rs` and/or `vk/render_pipeline.rs` to add an `ALPHA_MODE` spec-constant alongside the existing `MODE/OP/A8_DST/COMPONENT_ALPHA`
- Modify: `crates/yserver/src/kms/backend.rs:9018 render_fill_rectangles` (pipeline selection)
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1: Tests**
- `RENDER FillRectangles(PictOpSrc, dst_depth=24)` → α=255.
- `RENDER FillRectangles(PictOpOver, dst_format=ARGB32, src_α=128)` → α=128.
- `RENDER FillRectangles(PictOpSrc, dst_format=ARGB32, src_α=128)` → α=128 (Src copies α).

**Step 2:** Fail.

**Step 3:** Extend the spec-constant cache key with `ALPHA_MODE`. For server-owned destinations with opaque ops, select Opaque; for ARGB destinations or non-opaque ops, select PassThrough.

**Step 4:** Run + rendercheck (one-time AA-coverage rebase candidate — defer to A.18).

**Step 5:** Commit `feat(L1): RENDER FillRectangles op-aware alpha invariant`.

---

### Task A.12: RENDER Composite — depth-aware spec-constant

**Mechanism:** M2.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs:4441 try_vk_render_composite` + RENDER pipeline cache
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1:** Tests covering depth-24 dst, ARGB32 dst, Src/Over ops.

**Step 2:** Fail.

**Step 3:** Same mechanism as A.11.

**Step 4:** Run.

**Step 5:** Commit `feat(L1): RENDER Composite depth-aware alpha invariant`.

---

### Task A.13: RENDER CompositeGlyphs — Coverage spec-constant

**Mechanism:** M2 (Coverage variant: α = glyph_coverage × source_α).

**Files:**
- Modify: text pipeline / render pipeline (whichever serves CompositeGlyphs)
- Modify: `crates/yserver/src/kms/backend.rs:4039 try_vk_render_composite_glyphs`
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1:** Test — render an AA glyph at known coordinates; sample α along an AA edge; assert α varies smoothly between 0 and 255 per the glyph's coverage atlas.

**Step 2:** Fail.

**Step 3:** Build the Coverage variant; pipeline selection for CompositeGlyphs uses it.

**Step 4:** Run + rendercheck (more AA delta; flag for A.18).

**Step 5:** Commit `feat(L1): RENDER CompositeGlyphs Coverage alpha`.

---

### Task A.14: RENDER Trapezoids / Triangles — Coverage at the consumed-mask stage

**Mechanism:** M2 (the R8 coverage buffer from `traps.rs` is the source of the downstream Composite; Coverage spec-constant on that Composite).

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs:3430 try_vk_trapezoids` (pipeline selection of the Composite that consumes the R8 mask)
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1:** Test — submit a trapezoid with a sloped edge; assert α along the edge falls between 0 and 255.

**Step 2:** Fail.

**Step 3:** Wire the consuming-Composite path to pick the Coverage variant when the source is the R8 mask scratch.

**Step 4:** Run + rendercheck.

**Step 5:** Commit `feat(L1): RENDER Trapezoids/Triangles Coverage alpha`.

---

### Task A.15: Tiled / Stippled fill — split by mechanism

**Mechanism:**
- Tiled fill of a depth-24 destination → M1 if the implementation is `cmd_copy_image`-based, M2 if pipeline-based. Inspect `try_vk_tiled_fill` first.
- Stippled fill (1-bit mask on a TrueColor visual) → M2 with Coverage on the stipple's mask.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs::try_vk_tiled_fill` + supporting pipeline/op
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1:** Two tests — tiled (α=255 across filled region) and stippled (α=255 on set stipple bits).

**Step 2:** Fail.

**Step 3:** Apply the relevant mechanism per branch.

**Step 4:** Run.

**Step 5:** Commit `feat(L1): tiled/stippled fill alpha invariant`.

---

### Task A.15b: GCfunction crossover audit

**Goal:** Same drawable, two paint ops in sequence, one with `GcFunction::Copy` and one with `GcFunction::Xor`. Confirm both write α=255 on depth-24 (M1 covers the Copy fast path; A.6b covered the logic_fill_pipeline path). The audit is a single test that exercises both branches against the same destination.

**Files:**
- Test: `crates/yserver/tests/alpha_invariant.rs`

**Step 1:** Test as above; sample α at points painted by each branch.

**Step 2:** Run. If A.3 + A.6b are both correct, this passes.

**Step 3:** Fix any gap.

**Step 4:** Run.

**Step 5:** Commit `test(L1): GCfunction Copy and non-Copy branches both honour opaque alpha`.

---

### Task A.16: Composite-scene window-mirror draws → SRC_ALPHA_MODE = 1

**Goal:** Flip the dial. Now that A.3–A.15 declare α-policy on every paint path, the composite scene can stop forcing opaque and pass through whatever the mirror α actually carries.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs:5896` (window-mirror composite draw) — flip `use_src_alpha: false` to `true`.
- **Leave `:5782` (bg-pixmap root draw) at `use_src_alpha: false`.** The spec mandates the flip only for window-mirror draws (`docs/superpowers/specs/2026-05-11-x11-composite-design.md` rule 6, "window mirror draws emit `use_src_alpha: true`"). The root background mirror is always fully covered with painted pixels (ClearArea + bg_pixel fills cover the entire screen extent), so forcing α=1.0 there is correct and observationally identical to passing α through. Broadening the flip to bg-pixmap is an unforced change with no justification in the spec.

**Step 1: Scanout pass-through test (in `tests/alpha_invariant.rs`):**
```rust
#[test] #[ignore]
fn composite_passes_unpainted_pixels_through_to_root() {
    let mut fix = ServerFixture::start();
    fix.set_root_pixel(0xff_ff_ff_ff); // white
    let win = fix.create_window(100, 100, 24);
    fix.map_window(win);
    let gc = fix.create_gc(win, 0x00_00_ff_00);
    fix.fill_rectangle(win, gc, 0, 0, 50, 100); // green left half
    let scanout = fix.capture_scanout();
    // Painted half: green.
    assert_eq!((scanout.pixel(25, 50).r, scanout.pixel(25, 50).g, scanout.pixel(25, 50).b),
               (0x00, 0xff, 0x00));
    // Unpainted half: white root through, NOT black.
    assert_eq!((scanout.pixel(75, 50).r, scanout.pixel(75, 50).g, scanout.pixel(75, 50).b),
               (0xff, 0xff, 0xff));
}
```

**Step 2:** Fail (today the right half is black per the rim bug).

**Step 3:** Flip only the `use_src_alpha` literal at `backend.rs:5896` (window-mirror push) to `true`. Leave `:5782` (bg-pixmap root push) unchanged.

**Step 4:** Run + `just rendercheck-yserver` + visual smoke under MATE.

**Step 5:** Commit `feat(L1): composite scene passes window-mirror alpha through (rim fix)`.

---

### Task A.17: Cleanup — name the pipeline selector clearly

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` `CompositeDraw.use_src_alpha` rename to `alpha_passthrough` (or similar)
- Modify: `crates/yserver/src/kms/vk/compositor.rs` comments

Trivial polish. No tests beyond a `cargo test` pass.

**Step 5:** Commit `refactor(compositor): clarify pipeline-selector naming after L1 flip`.

---

### Task A.18: rendercheck baseline rebase

**Files:** baseline files under whichever path `tools/rendercheck.sh` consumes (per `memory/reference_rendercheck_logs.md`).

**Step 1:** `just rendercheck-yserver`; capture deltas.

**Step 2:** Inspect:
- AA-edge coverage deltas → expected, accept.
- Anything else → investigate the offending path's α policy before rebasing.

**Step 3:** Update baselines.

**Step 4:** Re-run; confirm clean.

**Step 5:** Commit `test(L1): rebase rendercheck baselines for alpha-coverage shifts`.

---

### Task A.18b: Visual α-mask audit (resolve spec open question #1)

**Goal:** Spec open question #1 asks whether the depth-24/32 split holds as a proxy for server-owned vs client-meaningful α. The canonical signal is `Visual.alpha_mask` (`crates/yserver-core/src/resources.rs:53..63`): the root visual has `alpha_mask = 0` (server-owned), the ARGB visual has `alpha_mask = 0xff00_0000` (client-meaningful). The audit verifies the depth-as-proxy mapping by inspecting every advertised Visual.

**Files:**
- Modify: `docs/superpowers/specs/2026-05-11-x11-composite-design.md` (resolve open question #1 with the audit outcome)

**Step 1:** Inspect every Visual seeded in `crates/yserver-core/src/resources.rs:272..` (ROOT_VISUAL, ARGB_VISUAL, and any others), reading the `alpha_mask` field on each.

**Step 2:** Cross-reference visual ↔ depth ↔ alpha_mask. The mapping is sound iff:
- Every advertised depth-24 (or any non-32) Visual has `alpha_mask == 0`.
- Every advertised depth-32 Visual has `alpha_mask != 0`.
- No Visual exists where the depth-24-with-alpha or depth-32-without-alpha combination breaks the proxy.

**Step 3:** Update the spec:
- If the mapping is sound (expected outcome): document that `Visual.alpha_mask` is the canonical signal, and "depth-24 vs depth-32" remains a working proxy. The α-write rules in L1's contract apply by `alpha_mask != 0`, not by depth.
- If a non-conforming Visual exists: file a follow-up task to either drop the Visual from advertisement or split the α rule per-Visual.

**Step 4:** Re-read the modified spec to confirm.

**Step 5:** Commit `docs(spec): resolve depth-as-proxy open question via Visual.alpha_mask audit`.

---

### Task A.19: Visual smoke under MATE

**Manual.** Boot MATE under `yserver-hw` (`memory/reference_virtme_ng_drm_harness.md`). SIGUSR1 to dump scanout PPM. Inspect rim coords (`x=594..603` left, `x=1956..1965` right, top, bottom). Assert no `(0,0,0)` pixels in those regions. Exercise file-manager, control panel, gtk3-demo, wezterm — no regressions.

**Phase A done when:** the rim is visibly gone in MATE PPM dumps regardless of marco compositor state.

---

# Phase B — L2: XComposite redirect end-to-end on KMS

**Visible result:** Marco's compositor effects work under MATE with `compositing-manager = true` — shadows, fades, ARGB-blended titlebars.

---

### Task B.1: `RedirectRecord` value type + conflict policy

**Files:**
- Modify: `crates/yserver-core/src/server.rs:212` (value type)
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:2440..2454` (conflict check, parse Manual vs Automatic byte)
- Test: `crates/yserver-core/src/core_loop/process_request.rs` test module or new `crates/yserver/tests/composite_redirect.rs`

**Step 1: Tests** — three active tests, none deferred:
```rust
#[test]
fn redirect_window_conflict_from_different_client_returns_bad_access() {
    // Client A redirects W; client B's REDIRECT_WINDOW on W → BadAccess.
}
#[test]
fn redirect_window_same_client_idempotent() {
    // Client A REDIRECT_WINDOW twice on W → both succeed; map state unchanged.
}
#[test]
fn redirect_automatic_mode_returns_bad_value() {
    // REDIRECT_WINDOW with mode byte = 2 (Automatic) → BadValue.
    // The spec resolves Manual-only initially (open question #3 → resolved).
}
```

**Step 2:** Fail.

**Step 3:** Add:
```rust
#[derive(Debug, Clone, Copy)]
pub struct RedirectRecord {
    pub mode: CompositeRedirectMode,
    pub owner: ClientId,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeRedirectMode { Manual, Automatic }
pub composite_redirects: HashMap<(ResourceId, bool), RedirectRecord>;
```
Reject Automatic with `BadValue`. On conflict from a different client, `BadAccess`.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): RedirectRecord value type with owner + Manual-only policy`.

---

### Task B.1b: Disconnect cleanup respects redirect owner — and tears down backings

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_disconnect.rs` (~line 164, the existing `composite_redirects.retain` call)
- Test: extend the redirect test file

**Step 1: Tests** — three cases:
- Two clients, client A redirects window W (Manual), client B disconnects → redirect intact, W.redirected_backing intact.
- Client A disconnects with W still alive → redirect entry removed AND `Window.redirected_backing` cleared on W AND the backing's reason-1 refcount decremented in the backend's `alias_registry`.
- Client A disconnects after also taking a `NameWindowPixmap` (alias α on W) → redirect entry removed, W.redirected_backing cleared, but the backing **survives** with refcount=1 (held by α). α stays valid until FreePixmap.

**Step 2:** Fail (today the filter is by dead-window only, so client A's disconnect *with W still alive* would leave the redirect stranded AND keep `Window.redirected_backing` populated; the backing's reason-1 hold never decrements).

**Step 3:** Walk the redirects belonging to the disconnecting client *before* filtering them out, so the backings get the proper decref + Window.redirected_backing teardown. Pseudocode:

```rust
let owned: Vec<(ResourceId, bool)> = state.composite_redirects
    .iter()
    .filter(|(_, rec)| rec.owner == client_id)
    .map(|((win, sub), _)| (*win, *sub))
    .collect();
for (win, _sub) in &owned {
    // Same teardown path B.6c calls for explicit Unredirect.
    teardown_redirect_for_window(state, backend, origin, *win);
}
state.composite_redirects.retain(|(window, _), rec| {
    rec.owner != client_id && !dead_windows.contains(window)
});
```

`teardown_redirect_for_window` (introduced in B.6c) handles: `Window.redirected_backing.take()` + `alias_registry.decref` on the backing's reason-1 hold. Until B.6c lands, the helper is a stub; B.1b lands the call site so the wiring is in place.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): disconnect tears down redirects + backings for departing owner`.

---

### Task B.2: `Window.redirected_backing` field

**Files:**
- Modify: `crates/yserver-core/src/resources.rs:2125 Window` struct + initialisers at `:217`, `:261`, `:454`, `:2160`
- Test: existing `resources.rs` test module

**Step 1:** Test asserts `Window::placeholder(..).redirected_backing.is_none()`.

**Step 2:** Compile fail.

**Step 3:** Add:
```rust
pub struct RedirectedBacking {
    pub host_pixmap: crate::backend::PixmapHandle,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
}
// in Window:
pub redirected_backing: Option<RedirectedBacking>,
```
Initialise to `None` everywhere.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): Window.redirected_backing field`.

---

### Task B.3: Backend named-pixmap alias registry

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (add `alias_registry: AliasRegistry` field on the backend struct)
- Test: unit test in `backend.rs` covering insert / get / incref / decref

**Step 1:** Test:
```rust
#[test]
fn alias_registry_refcount_lifecycle() {
    let mut reg = AliasRegistry::default();
    let h = PixmapHandle::from_raw_for_test(0x77);
    reg.insert(h, AliasEntry { refcount: 1, width: 100, height: 50, depth: 24 });
    reg.incref(h);
    assert_eq!(reg.get(h).map(|e| e.refcount), Some(2));
    assert!(!reg.decref(h));
    assert!(reg.decref(h));
    assert!(reg.get(h).is_none());
}
```

**Step 2:** Compile fail.

**Step 3:** Implement `AliasRegistry` + `AliasEntry`.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): named-pixmap alias registry on KMS backend`.

---

### Task B.4: `composite_opcode() -> Some(144)`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs:6905`
- Test: unit test asserting the value.

> ⚠ Must land *paired* with B.5 (and B.6a). Without an actual `name_window_pixmap` implementation, the protocol handler at `process_request.rs:2518..2540` will now reach the backend call and error there, producing `BadAlloc`. To avoid a broken intermediate state on master, push B.4 + B.5 + B.6a together in the same PR.

**Step 5:** Commit `feat(L2): KMS backend advertises composite opcode 144` (squash with B.5/B.6a at PR-time if landing as separate commits causes a broken intermediate).

---

### Task B.5: `name_window_pixmap` — alias the existing backing (refcount only, no allocation)

**Goal:** `NameWindowPixmap` does **not** allocate a backing. It increments the refcount on the existing backing for the redirected window and returns its handle.

This corrects the v1-draft inversion: per spec §"Backing pixmap refcount and lifetime", *Redirect activation* allocates the backing with refcount=1 (reason 1); *each NameWindowPixmap* increments the refcount and registers a `NamedCompositePixmap` alias.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs:7335`
- Test: `crates/yserver/tests/composite_named_pixmap.rs` (new)

**Step 1: Tests**
```rust
#[test] #[ignore]
fn name_window_pixmap_on_unredirected_window_errors() {
    // No backing yet → name_window_pixmap returns an error.
    // (Note: at the protocol layer, the redirected check at process_request.rs:2498
    // already gates this; but the backend method should also fail-safe.)
}

#[test] #[ignore]
fn name_window_pixmap_returns_existing_backing_handle_and_increments_refcount() {
    let mut backend = KmsBackend::for_test_with_vk();
    let win = backend.test_create_redirected_window(100, 50, 24); // allocates backing in setup
    let pre = backend.alias_registry().get(backend.backing_handle_for_test(win))
        .map(|e| e.refcount);
    let aliased = backend.name_window_pixmap(None, win).unwrap();
    assert_eq!(aliased, backend.backing_handle_for_test(win)); // same handle
    let post = backend.alias_registry().get(aliased).map(|e| e.refcount);
    assert_eq!(post, pre.map(|r| r + 1));
}
```

**Step 2:** Fail.

**Step 3: Implement**
```rust
fn name_window_pixmap(
    &mut self,
    _origin: Option<OriginContext>,
    host_window: WindowHandle,
) -> io::Result<PixmapHandle> {
    let backing = self.redirected_backing_for_host_window(host_window)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "window not redirected"))?;
    self.alias_registry.incref(backing);
    Ok(backing)
}
```
`redirected_backing_for_host_window` is a lookup on a backend-private map `host_window → backing_pixmap_handle` that B.6a populates.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): name_window_pixmap aliases the existing backing (refcount)`.

---

### Task B.6a: Redirect-Window activation — allocate backing

**Goal:** On `REDIRECT_WINDOW` for a single window, allocate an off-screen pixmap mirror sized to the window's current geometry+depth and store the handle on `Window.redirected_backing` *and* in the backend's `host_window → backing` map. Refcount = 1 (reason 1: window is redirected).

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:2440..2454` (REDIRECT_WINDOW branch)
- Add: a backend trait method like `allocate_redirected_backing(host_window, w, h, depth) -> io::Result<PixmapHandle>` (`crates/yserver/src/kms/backend.rs`)
- Test: `crates/yserver/tests/composite_redirect.rs`

**Step 1:** Test — redirect a 200×150 depth-24 window; assert `Window.redirected_backing.is_some()`, dimensions match, alias_registry refcount=1.

**Step 2:** Fail.

**Step 3:** Implement the backend allocation method (uses `allocate_pixmap_mirror` + `next_host_xid_pixmap`). Hook into the REDIRECT_WINDOW handler: after recording the `RedirectRecord`, call into the backend, set `Window.redirected_backing`, register in the backend's host_window→backing map.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): RedirectWindow allocates per-window off-screen backing`.

---

### Task B.6b: Redirect-Subwindows activation — enumerate children + future-child wiring

**Goal:** `REDIRECT_SUBWINDOWS(W)` redirects every child of W (current and future).

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` REDIRECT_SUBWINDOWS branch
- Modify: CreateWindow handler (locate via `grep -n "CreateWindow\|create_window_to_state" crates/yserver-core/src/core_loop/process_request.rs`) — on create-as-child-of-W, check if W has a subtree-redirect and allocate a backing on the new child
- Test: `crates/yserver/tests/composite_redirect.rs`

**Step 1: Tests**
- Existing children: REDIRECT_SUBWINDOWS(W) with two existing children → both get backings allocated.
- Future child: REDIRECT_SUBWINDOWS(W), then CreateWindow(child of W) → the new child gets a backing automatically.

**Step 2:** Fail.

**Step 3:** Implement subtree enumeration on Redirect, and a hook in CreateWindow that checks for an ancestor with `subwindows=true` redirect.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): RedirectSubwindows enumerates children and wires future creations`.

---

### Task B.6c: Unredirect teardown

**Goal:** `UNREDIRECT_WINDOW` / `UNREDIRECT_SUBWINDOWS` decrements the backing's reason-1 refcount on each affected window; backing drops if no NamedCompositePixmap alias holds it; clears `Window.redirected_backing` on each.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:2448..2454`
- Test: `crates/yserver/tests/composite_redirect.rs`

**Step 1: Tests**
- Redirect then Unredirect with no NameWindowPixmap → backing freed; alias_registry empty.
- Redirect → NameWindowPixmap → Unredirect → backing *survives* until FreePixmap on the alias.

**Step 2:** Fail.

**Step 3:** Implement; share the decref helper with B.1b's disconnect cleanup.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): Unredirect decrements backing refcount; backing survives on alias`.

---

### Task B.6d: Resize-time alias invalidation rewrite

**Goal:** Replace the unconditional alias-drop in `invalidate_composite_named_pixmaps_to_state` (`process_request.rs:482`) with the spec's frozen-content rule. On window resize: allocate a new backing for the new size, move `Window.redirected_backing` to the new backing, decref the old backing's reason-1 hold. Existing aliases keep refcount on the old backing → its content is frozen at pre-resize.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:482..504 invalidate_composite_named_pixmaps_to_state` (rename to e.g. `freeze_aliases_and_rotate_backing_on_resize`; semantics change entirely)
- Modify: callsite at `:6102` (ConfigureWindow on resize) — pass new size/depth so the helper can allocate the new backing
- Test: `crates/yserver/tests/composite_redirect.rs`

**Step 1: Tests**
- Redirect window; paint green; NameWindowPixmap → alias α. Resize 100→200. NameWindowPixmap again → new alias β. Read α (via DRI3 or fixture's pixmap-read): green, 100×100. Read β: post-resize, 200×200.
- Same scenario but FreePixmap α before resize → resize drops the old backing entirely (no surviving alias).

**Step 2:** Fail (today the function calls `free_pixmap` on each alias and the backing is hard-released).

**Step 3:** Rewrite the function to:
1. Allocate a new backing matching the new (width, height, depth).
2. Set `Window.redirected_backing` to the new backing.
3. Decref the old backing's reason-1 hold. If aliases hold the old backing, its refcount stays > 0 — it survives, frozen.
4. Do *not* call `state.resources.free_pixmap` on the aliases — they remain valid X protocol resources pointing at the old (now-frozen) backing.
5. Do *not* call `backend.free_pixmap` on the alias host_pixmap handles — they share the backing handle.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): resize under redirect freezes aliases, rotates backing per spec`.

---

### Task B.7: Redirect-aware `host_drawable_target`

**Files:**
- Modify: `crates/yserver-core/src/resources.rs:1242 host_drawable_target`
- Test: extend the `host_drawable_target_*` test module at `resources.rs:2583..`

**Step 1: Test**
```rust
#[test]
fn host_drawable_target_redirected_window_returns_backing_xid() {
    let mut table = ResourceTable::new();
    make_top_level_with_host_xid(&mut table, 0x100_0002, 0x42);
    let backing = crate::backend::PixmapHandle::from_raw_for_test(0x99);
    table.windows.get_mut(&0x100_0002).unwrap().redirected_backing =
        Some(crate::resources::RedirectedBacking { host_pixmap: backing, width: 100, height: 50, depth: 24 });
    match table.host_drawable_target(ResourceId(0x100_0002)).unwrap() {
        HostDrawableTarget::Window { host_xid, depth, .. } => {
            assert_eq!(host_xid, backing);
            assert_eq!(depth, 24);
        }
        _ => panic!("expected Window variant routing to backing"),
    }
}
```

**Step 2:** Fail.

**Step 3: Implement**
```rust
if let Some(window) = self.windows.get(&id.0) {
    if let Some(b) = window.redirected_backing.as_ref() {
        return Some(HostDrawableTarget::Window {
            nested: id,
            host_xid: b.host_pixmap,
            depth: b.depth,
        });
    }
    let host_xid = window.host_xid?;
    return Some(HostDrawableTarget::Window { nested: id, host_xid, depth: window.depth });
}
```

**Step 4:** Run.

**Step 5:** Commit `feat(L2): host_drawable_target routes redirected windows to backing XID`.

---

### Task B.8: Audit non-host_drawable_target lookups for redirect awareness

**Goal:** Some paths read `window.host_xid` directly (bypassing `host_drawable_target`). DRI3 `BufferFromPixmap` (`process_request.rs:4337..`) reads `pixmap(...).host_xid` — that's fine because named pixmaps already store the backing's host XID via `set_pixmap_host_xid` at NAME_WINDOW_PIXMAP time. But other paths may use `window.host_xid` for paint or hit-test purposes; they need redirect-awareness.

**Files:**
- Audit-only: `grep -rn "\.host_xid\b" crates/yserver-core/src/ | grep -v test`. Classify each: (a) paint-time → reroute via `host_drawable_target`; (b) hit-test / event delivery → stay on window XID; (c) DRI3 export of a *pixmap* → unchanged.
- Modify: any paint path that currently reads `window.host_xid` directly. Suspect callers (from the codex review's grep): `process_request.rs:2924`, `:4204`, `:4310`, `:5950..5970` (ChangeWindowAttributes background-pixmap propagation), `:8576`.
- Test: covered indirectly by B.16/B.17; add a focused test once a specific bypass is identified.

**Step 1:** Audit list (document in commit message).

**Step 2..4:** Per bypass, switch to `host_drawable_target` if paint-time; document why if it's correctly host-window-direct.

**Step 5:** Commit `refactor(L2): audit host_xid bypasses for redirect awareness`.

---

### Task B.9a: Damage routing — verify public XID stays on window after redirect

**Files:**
- Audit: `crates/yserver-core/src/core_loop/damage_fanout.rs` — verify `accumulate_damage_*` is called with the *window* XID, not the backing pixmap XID, by every paint handler.
- Test: `crates/yserver/tests/composite_damage.rs` (new)

**Step 1:** Test:
```rust
#[test] #[ignore]
fn paint_into_redirected_window_emits_damage_on_window_xid() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(100, 100, 24);
    fix.composite_redirect_window(win, Mode::Manual);
    let dmg = fix.damage_create(win, ReportLevel::RawRectangles);
    fix.fill_rectangle(win, fix.create_gc(win, 0xffffffff), 10, 10, 30, 30);
    let evts = fix.collect_damage_events(dmg);
    assert_eq!(evts.len(), 1);
    assert_eq!(evts[0].drawable, win); // PUBLIC xid = window, not backing
}
```

**Step 2:** Run; may already pass since `accumulate_damage_to_state` keys on the public ResourceId. If it fails, a paint handler is passing the resolved (backing) XID downstream — fix.

**Step 3:** If needed, adjust the offending handler to pass the original window ResourceId to damage.

**Step 4:** Run.

**Step 5:** Commit `test(L2): damage stays on window XID under redirect`.

---

### Task B.9b: Internal dirty key on backing mirror

**Goal:** Backing mirror's dirty bit is what drives the compositor's re-blit. Confirm paint into a redirected window marks the *backing* pixmap_mirror dirty (not the window mirror). This is automatic post-B.7 because `host_drawable_target` routes paint to the backing's mirror.

**Files:**
- Test: `crates/yserver/tests/composite_damage.rs`

**Step 1:** Test asserts `backend.pixmap_mirror_is_dirty(backing_handle)` after a paint.

**Step 2:** Run.

**Step 3:** Fix if needed (likely not).

**Step 4:** Run.

**Step 5:** Commit `test(L2): backing mirror dirty-tracking is keyed correctly`.

---

### Task B.10a: Overlay promotion state machine

**Goal:** On first successful `NameWindowPixmap` after redirect activation, transition the overlay from `Logical` to `Promoted`. Allocate a full-screen ARGB32 mirror; assign it a `host_xid` for the overlay window (`COMPOSITE_OVERLAY_WINDOW`) so `host_drawable_target` resolves overlay-targeted draws to that mirror.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` — add `OverlayState` field + `promote_overlay`/`demote_overlay` helpers
- Modify: `process_request.rs:2468..2561` NAME_WINDOW_PIXMAP handler to call into the promotion helper after the first successful alias
- Modify: overlay window's resource record to receive a `host_xid` on promotion
- Test: `crates/yserver/tests/composite_overlay.rs` (new)

**Step 1:** Test:
```rust
#[test] #[ignore]
fn first_name_window_pixmap_promotes_overlay() {
    let mut fix = ServerFixture::start();
    assert!(!fix.overlay_is_promoted());
    let win = fix.create_window(100, 100, 24);
    fix.composite_redirect_window(win, Mode::Manual);
    fix.name_window_pixmap(win);
    assert!(fix.overlay_is_promoted());
}
```

**Step 2:** Fail.

**Step 3:** Implement state machine; promotion allocates the mirror + sets overlay host_xid.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): overlay promotes on first NameWindowPixmap`.

---

### Task B.10b: Scanout source switch to overlay when promoted

**Goal:** When the overlay is promoted, the scanout pass consumes the overlay mirror as its sole top-level source. The direct top-level chain is skipped (top-levels' mirrors still receive paint via their backings — but they're no longer drawn directly to scanout; the compositor's marco-issued ops paint them into the overlay).

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs::build_composite_scene` and/or `walk_subtree_into_draws` — branch on overlay state.
- Test: `crates/yserver/tests/composite_overlay.rs`

**Step 1:** Test — promote overlay; paint into overlay mirror via the host_xid resolved path; capture scanout; assert overlay's content is what's on screen.

**Step 2:** Fail.

**Step 3:** Implement scanout-source switch.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): scanout draws overlay mirror when promoted`.

---

### Task B.10c: Overlay demotion on full-quiescence

**Goal:** After scanout pageflip, check: `composite_redirects.is_empty() && alias_registry.is_empty() && !overlay_backed_scanout_in_flight()`. If all true, drop the overlay mirror, clear its host_xid, restore direct top-level scanout.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` — hook into the pageflip-complete callback (`try_vulkan_composite_flip` machinery)
- Test: `crates/yserver/tests/composite_overlay.rs`

**Step 1:** Test:
```rust
#[test] #[ignore]
fn overlay_demotes_when_all_state_quiesces() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(100, 100, 24);
    fix.composite_redirect_window(win, Mode::Manual);
    let pix = fix.name_window_pixmap(win);
    assert!(fix.overlay_is_promoted());
    fix.free_pixmap(pix);
    fix.composite_unredirect_window(win);
    fix.wait_for_pageflip();
    assert!(!fix.overlay_is_promoted());
}
```

**Step 2:** Fail.

**Step 3:** Implement.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): overlay demotes when redirects + aliases + BO in-flight all drain`.

---

### Task B.10d: Promote → demote → promote with in-flight scanout BO

**Goal:** Exercise the edge case where a redirect is re-activated while a previous overlay-backed BO is still in `Submitted` phase.

**Files:**
- Test: `crates/yserver/tests/composite_overlay.rs`

**Step 1:** Test:
```rust
#[test] #[ignore]
fn promote_demote_promote_handles_in_flight_overlay_bo() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(100, 100, 24);
    fix.composite_redirect_window(win, Mode::Manual);
    let p1 = fix.name_window_pixmap(win); // promote
    fix.fill_rectangle(win, fix.create_gc(win, 0x00_00_ff_00), 0, 0, 100, 100);
    fix.submit_scanout_but_dont_wait_for_flip();
    fix.free_pixmap(p1);
    fix.composite_unredirect_window(win);
    // Pageflip hasn't completed yet; demotion is deferred.
    assert!(fix.overlay_is_promoted(), "demotion deferred while BO in-flight");
    // Re-activate before pageflip.
    fix.composite_redirect_window(win, Mode::Manual);
    let p2 = fix.name_window_pixmap(win);
    fix.wait_for_pageflip();
    // Overlay remains promoted (new lifecycle); old BO drains cleanly.
    assert!(fix.overlay_is_promoted());
    fix.free_pixmap(p2);
}
```

**Step 2:** Run.

**Step 3:** If demotion races a re-promotion, harden the state machine. Likely the existing phase-tracking in `try_vulkan_composite_flip` is sufficient; verify and fix if not.

**Step 4:** Run.

**Step 5:** Commit `test(L2): overlay promote/demote/promote with in-flight BO`.

---

### Task B.11: Overlay input transparency

**Files:**
- Modify: pointer hit-test path. Locate via `grep -rn "pointer\|hit_test\|window_under" crates/yserver-core/src/`.
- Test: `crates/yserver/tests/composite_overlay.rs`

**Step 1:** Test — promote overlay; pointer warp to centre; assert hit-test resolves to the underlying redirected window, not the overlay.

**Step 2:** Fail.

**Step 3:** Skip `COMPOSITE_OVERLAY_WINDOW` in the hit-test walk regardless of promotion state.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): promoted overlay excluded from pointer hit-test`.

---

### Task B.12: Overlay + OR-popup stacking

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs::build_composite_scene` — draw overlay, then OR popups, then cursor.
- Test: `crates/yserver/tests/composite_overlay.rs`

**Step 1:** Test — promote overlay; create an OR popup; assert its pixels in the scanout, not the overlay's at the popup's coords.

**Step 2:** Fail.

**Step 3:** Implement draw order.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): OR popups draw above promoted overlay`.

---

### Task B.13: ParentRelative + redirect verification

**Files:**
- Test: extend `composite_redirect.rs`

**Step 1:** Test — parent with bg_pixmap, child with `ParentRelative`, RedirectSubwindows(parent), map both; assert child's backing receives the auto-fill content (RGB + α=255).

**Step 2:** Run. Should pass given A.10b + B.6b + B.7.

**Step 3:** Fix any leak.

**Step 4:** Run.

**Step 5:** Commit `test(L2): ParentRelative auto-fill routes to redirected backing`.

---

### Task B.14: Resize under redirect — alias survival edges

**Files:**
- Test: extend `composite_redirect.rs`. Covers cases beyond B.6d's basic resize test.

**Step 1: Tests**
- Resize with multiple aliases → all survive on the old backing; new aliases via NameWindowPixmap point at the new backing.
- Resize *before* any NameWindowPixmap → no aliases held the old backing; old backing freed; new backing has refcount=1 (reason 1).
- Resize twice without freeing aliases → 3 backings live simultaneously (oldest with α, mid with β, newest as Window.redirected_backing); FreePixmap(α) drops the oldest, FreePixmap(β) drops the middle.
- FreePixmap *before* DestroyWindow vs DestroyWindow before FreePixmap — both must keep aliases valid until both have happened (alias holds refcount; destroying the window doesn't free aliases).

**Step 2:** Run.

**Step 3:** Fix lifecycle.

**Step 4:** Run.

**Step 5:** Commit `test(L2): resize-under-redirect alias survival edges`.

---

### Task B.15: Destroy under redirect — multi-alias survival

**Files:**
- Modify: DestroyWindow handler — release reason-1 hold; do not free aliases.
- Test: `composite_redirect.rs`

**Step 1: Tests**
- Destroy a redirected window with two aliases → both aliases stay valid; their pixmap XIDs are still present in `ResourceTable.pixmaps`.
- Destroy → FreePixmap(α) → FreePixmap(β) → alias_registry empty; backing freed.
- Destroy → FreePixmap(β) → FreePixmap(α) (reverse order) → same end state.

**Step 2:** Fail.

**Step 3:** In DestroyWindow: if `redirected_backing.is_some()`, decref reason-1 hold. Do not touch `composite_named_pixmaps`. The named-pixmap resources stay alive via their own host_xid on the backing.

**Step 4:** Run.

**Step 5:** Commit `feat(L2): destroyed redirected window keeps aliases valid until freed`.

---

### Task B.16: PRESENT + redirect — verification only

**Goal:** Confirm `PresentPixmap` and `PresentPixmapSynced` both route to the redirected backing automatically. Both handlers already resolve `dst` via `host_drawable_target` (`crates/yserver-core/src/core_loop/process_request.rs:3576` PresentPixmap, `:3762` PresentPixmapSynced), so post-B.7 they become redirect-aware with zero additional code. This task is a verification test; if it fails, a downstream consumer of the resolved target bypassed the routing.

**Files:**
- Test: `crates/yserver/tests/composite_present.rs` (new)
- Audit: confirm both Present opcodes (locate via `rg -n "PresentPixmap|present_scheduler" crates/yserver-core/src/`) use the resolved `dst` end-to-end.

**Step 1:** Two tests — one per opcode:
```rust
#[test] #[ignore]
fn present_pixmap_on_redirected_window_blits_to_backing() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(100, 100, 24);
    fix.composite_redirect_window(win, Mode::Manual);
    let src = fix.create_pixmap_with_fill(100, 100, 24, 0x00_aa_bb_cc);
    fix.present_pixmap(win, src);
    fix.wait_for_present();
    let pix = fix.name_window_pixmap(win);
    let img = fix.read_pixmap(pix);
    assert_eq!((img.pixel(50, 50).r, img.pixel(50, 50).g, img.pixel(50, 50).b),
               (0xcc, 0xbb, 0xaa));
}

#[test] #[ignore]
fn present_pixmap_synced_on_redirected_window_blits_to_backing() {
    // Same shape using PresentPixmapSynced (handler at :3761..); also asserts the sync-fence path triggers.
}
```

**Step 2:** Run. Expected to **pass after B.7**.

**Step 3:** If either fails, find the path that bypasses `host_drawable_target` (likely a stash of the original window XID downstream of `dst` resolution) and reroute through the resolved value.

**Step 4:** Run.

**Step 5:** Commit `test(L2): PRESENT routes to redirected backing via B.7 (no handler change)`.

---

### Task B.16b: GL direct-rendering client on a redirected window

**Goal:** Spec L2 risk "GLX/DRI3 + redirect" calls out that a direct-rendering GL client's dma-buf binding must reach the redirected backing, not the original window's host XID, after L2. In the modern Mesa flow this happens automatically: the client allocates a buffer, calls `dri3_create_pixmap` to register it as a Pixmap resource, then `PresentPixmap(window, pixmap)` on each swap — and `PresentPixmap`'s `dst` resolves via `host_drawable_target` (B.7), so the present blit lands on the backing. This task is a **verification smoke test** that confirms the chain end-to-end with a real GL client.

If the verification fails, investigate whether any code path lets a client bind a dma-buf *directly to a window XID* (bypassing the Pixmap-resource indirection). If found, route it through `host_drawable_target` too.

**Files:**
- Test: `crates/yserver/tests/composite_dri3_redirected.rs` (new) — `#[ignore = "needs live Vulkan ICD + Mesa zink"]`
- Smoke (manual): vkcube / glxgears under MATE with `compositing-manager = true`, per `memory/reference_vng_use_zink.md`.

**Step 1: Test scaffolding**
```rust
#[test] #[ignore]
fn dri3_present_on_redirected_window_lands_in_backing() {
    let mut fix = ServerFixture::start();
    let win = fix.create_window(64, 64, 24);
    fix.composite_redirect_window(win, Mode::Manual);

    // Allocate a host dma-buf via the existing fixture helper (mirrors what
    // `dri3_fd_leak.rs` does), wrap it in a pixmap via PixmapFromBuffer,
    // fill its content with a known pattern, and PresentPixmap onto the window.
    let buf = fix.allocate_dmabuf(64, 64, /*format=*/ BGRA8);
    fix.fill_dmabuf(&buf, 0x00_aa_bb_cc);
    let pix = fix.dri3_pixmap_from_buffer(buf, win);
    fix.present_pixmap(win, pix);
    fix.wait_for_present();

    // Verify via the backing-side alias.
    let alias = fix.name_window_pixmap(win);
    let img = fix.read_pixmap(alias);
    assert_eq!((img.pixel(32, 32).r, img.pixel(32, 32).g, img.pixel(32, 32).b),
               (0xcc, 0xbb, 0xaa));
}
```

**Step 2:** Run; expect **pass** post-B.7+B.16.

**Step 3:** If it fails, the failure points at a direct-host-XID dma-buf binding path. Find it and route via `host_drawable_target`.

**Step 4:** Run.

**Step 5:** Commit `test(L2): DRI3 PixmapFromBuffer + Present on redirected window lands in backing`.

---

### Task B.17: Marco compositor smoke (manual)

`gsettings set org.mate.Marco.general compositing-manager true`; restart marco; tail logs for `RedirectSubwindows` + per-top-level `NameWindowPixmap` without errors; SIGUSR1 → PPM; visual check shadows + no rim. If marco still bails, dig into the failure log.

---

### Task B.18: picom on fvwm3 (manual)

Sanity for a second compositor under a lighter WM.

---

### Task B.19: Rendercheck + xts5 baseline refresh

If A.18 already rebased AA-coverage and B.* didn't shift more, this is a no-op verification. Otherwise rebase with reason.

**Step 5:** Commit `test(L2): refresh rendercheck/xts baselines (only if needed)`.

---

# Phase C — L3: GLX_EXT_texture_from_pixmap (conditional)

**Visible result:** marco's GLX compositor backend works if a user enables it.

**Trigger:** B.17 outcome. Marco engages the XRender compositor (scenario B) → C is **deferred**. Marco fails to engage → C proceeds.

---

### Task C.0: Empirical confirmation

Manual. Run B.17 again on the merged L2 master. Update the spec's open question #5 with the result. If marco engages, stop.

---

### Task C.1 / C.2 / C.3: TFP extension surface — ship together

**Goal:** A client must not observe an extension advertised by `QueryServerString` without the matching opcodes implemented. Ship C.1 (extension string), C.2 (`glXBindTexImageEXT` opcode 38), and C.3 (`glXReleaseTexImageEXT` opcode 39) as a single PR — even if each is its own commit during development.

**Files:**
- Modify: GLX QueryServerString reply construction (locate via `grep -rn "GLX_EXT_\|glx_extensions_string" crates/yserver-core/src/`)
- Modify: GLX dispatch table (locate via `grep -rn "glx_render\|glx_vendor\|GLXVendorPrivateOpcode\|VendorPrivate" crates/yserver-core/src/`)
- Test: `crates/yserver/tests/glx_tfp.rs` (new) — single test file covering all three.

**Step 1:** Test — XCB client `GetExtension(GLX)` then `QueryServerString` includes `GLX_EXT_texture_from_pixmap`; `BindTexImageEXT` + `ReleaseTexImageEXT` round-trip without protocol error.

**Step 2:** Fail.

**Step 3:** Implement. Both opcodes are server-side no-ops in our deployment — direct clients use DRI3 already; indirect clients are not a target. Return success without touching backing state. Watch out for `memory/reference_glxproto_struct_layouts.md` (pad-before-meaningful-field traps).

**Step 4:** Run.

**Step 5:** Commit either as one (`feat(L3): GLX_EXT_texture_from_pixmap stubs (extension + Bind + Release)`) or three small commits squashed at PR-time. **Do not push the extension string without the opcode handlers.**

---

### Task C.4: TFP path samples redirected backing — end-to-end

**Files:**
- Test: `crates/yserver/tests/glx_tfp.rs`

**Step 1:** Test — client `glXCreatePixmap` on a NamedCompositePixmap from a redirected window, `glXBindTexImageEXT`, draw a textured quad sampling that texture, read back, assert pixel matches the painted source. May require an actual GL test harness; if too heavy, defer and document.

**Step 2..5:** Run / fix / pass / commit.

---

### Task C.5: Marco GLX backend visual smoke (manual)

Force marco's GLX backend (if user-toggleable); confirm shadows + no regressions.

---

# Phasing recap

| Phase | Scope | Tasks | Duration | Visible |
|-------|-------|-------|----------|---------|
| A | L1 alpha invariant | A.1a..A.19 (≈22) | ~1 wk | No black rim |
| B | L2 redirect end-to-end | B.1..B.19 (≈22) | ~2 wk | Marco shadows work |
| C | L3 TFP | C.0..C.5 (≤6) | ~1 wk if triggered | GLX TFP available |

**Critical path:** A.1a → A.1c → A.3 → A.7..A.15 → A.16 → B.4 + B.5 + B.6a (together) → B.6b → B.6c → B.6d → B.7 → B.10a → B.10b → B.17 (proves L2; gates C.0).

---

# Risks (consolidated)

- A.3 mechanism rests on a single colour-converter helper covering every fill call site. If the helper is bypassed anywhere, the path stays α=0. Audit via grep before A.18.
- A.16 flip before A.3..A.15 + A.6b complete → regression. Lockstep ordering enforced by listing every paint path before A.16.
- B.4/B.5/B.6a as separate commits → broken intermediate. Land together at PR-time.
- B.6d (alias-freeze on resize) reverses today's drop-on-resize behaviour. Any consumer that assumes "post-resize alias is dead" breaks. xorg-server semantics are alias-survives; our code's drop is the bug.
- B.10c demotion racing a re-promotion → covered by B.10d explicit test.
- B.16 routing audit (and B.8): easy to miss a paint path that reads `window.host_xid` directly. The grep audit is the safety net.
- C.1 alone (extension string without opcodes) → broken external surface. Enforce co-ship.
- ParentRelative + mixed-depth (e.g., depth-32 child under depth-24 parent) → A.18b audit pending; defer until a real client trips it.

---

# Skills / references

- `@superpowers:executing-plans` — overall flow
- `@superpowers:test-driven-development` — TDD per task
- `@superpowers:verification-before-completion` — never claim "done" without green tests
- `@superpowers:systematic-debugging` — when red persists
- Memory: `reference_rendercheck_logs.md`, `reference_yserver_diag_toggles.md`, `reference_xephyr_source.md`, `feedback_visible_smoke_testing.md`, `feedback_test_vectors_must_be_external.md` (do **not** write tautological tests for COMPOSITE behaviour — ground expected values against xorg-server captures or the spec's wire layout).
