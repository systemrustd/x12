# Phase 6.10 — Multi-monitor on KMS Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Drive every connected DRM connector as an independent X11 RANDR output, laid out side-by-side in a single virtual screen. End-to-end validation under `vng` with `virtio-gpu-pci,max_outputs=2`.

**Spec:** [`docs/superpowers/specs/2026-05-07-phase6-10-multi-monitor-design.md`](../specs/2026-05-07-phase6-10-multi-monitor-design.md). Read it first — every Step references a section.

**Architecture:** Five fix tracks, mostly mechanical, gated by an upfront environment-validation step:

1. *vng multi-scanout verification* — codex flagged the `-display sdl,gl=on -device virtio-gpu-pci,max_outputs=2` recipe as unproven in this repo. Step 0 confirms the validation environment works *before* writing backend code.
2. *Per-CRTC page-flip identification* — `drm/page_flip.rs::drain_events` currently discards `PageFlipEvent.crtc`. Plumbing-only refactor.
3. *Per-output `Output` lifecycle* — `discover_output` → `discover_outputs` returning `Vec<Output>`. Hard error on unplaceable connector (virtio-gpu scope).
4. *`KmsBackend` carrying `Vec<OutputLayout>`* — per-output swapchain, per-output composite + flip with bbox intersection pre-filter, partial-modeset rollback on bring-up failure.
5. *RANDR multi-output state + handler audit* — `RandrState` becomes vec-shaped with deduped modes, primary output, and screen-mm aggregation; every `RR…` arm in `core_loop/process_request.rs` updated in lockstep.

**Tech stack:** Rust 2024, drm 0.15, pixman, mio, nix. No new deps.

**Branch:** `phase6-10-multi-monitor`

**Worktree:** Create via `superpowers:using-git-worktrees` from current `master`.

---

## Status

Not started. Depends on Phase 6.9 (commits up to and including `b12d25f`).

## Strategy

Each numbered Step is one logical commit. `cargo build` + `cargo test --workspace` green at every commit. Manual smoke gates at Step 0 (vng env confirmed), Step 4 (single-output regression-free), Step 5 (multi-output bring-up), Step 6 (xrandr / xterm placement / cursor traversal).

After every commit:
```sh
cargo +nightly fmt
cargo clippy --workspace --all-targets -- -W clippy::pedantic
cargo test --workspace
```

Self-review (codex) at the end of Step 5 before Step 6 smoke. Fix any blocking findings before moving on.

## Reference points (read these once before starting)

- `crates/yserver/src/drm/modeset.rs:82-198` — `discover_output`, `Output`, `pick_primary_plane`, `commit_modeset`, `disable_output`. Single-output assumption concentrated here.
- `crates/yserver/src/drm/page_flip.rs:41-49` — `drain_events`. The `Event::PageFlip(_)` discard is the load-bearing single-output assumption.
- `crates/yserver/src/drm/swapchain.rs:53` — `submitted_idx` invariant ("at most one buffer Submitted at a time"). Becomes per-swapchain.
- `crates/yserver/src/kms/backend.rs:500-505` — `KmsBackend` field declarations.
- `crates/yserver/src/kms/backend.rs:935-1000` — `KmsBackend::open` (DRM init, modeset, swapchain construction).
- `crates/yserver/src/kms/backend.rs:1664-1749` — `composite_and_flip`. Per-output paint loop refactor lands here.
- `crates/yserver/src/kms/backend.rs:2073-2103` — `fb_dimensions` + `drain_page_flips_and_composite`.
- `crates/yserver/src/lib.rs:36-40` — `KmsBackend::open` → `fb_dimensions` → `ServerState::with_geometry`. Virtual-screen extent flows through here.
- `crates/yserver-core/src/randr.rs` — full `RandrState` definition. Hard-coded const IDs at top.
- `crates/yserver-core/src/server.rs:165, 228` — `RandrState` field on `ServerState` + the `nested` constructor that wires it.
- `crates/yserver-core/src/core_loop/process_request.rs:~1274` onward — RANDR request handlers (`RRGetScreenResources`, `RRGetOutputInfo`, `RRGetCrtcInfo`, `RRGetOutputPrimary`, `RRGetMonitors`, etc.). Codex flagged these as still serving singletons.
- `crates/yserver-core/src/core_loop/run.rs:380-382` — **second** singleton-ID call site: `handle_host_container_resize` references `CRTC_ID` / `MODE_ID` / `OUTPUT_ID` directly. Must be updated in lockstep.
- `crates/yserver-protocol/src/x11/randr.rs:316-540` — wire encoders (already iterate `Vec`s; spec is right that protocol side doesn't need restructuring).
- `crates/yserver-core/src/backend/trait_def.rs` — the `Backend` trait definition.
- `crates/yserver-core/src/backend/recording.rs` — `RecordingBackend` test double (must implement any new trait method).
- `crates/yserver/src/kms/backend.rs:5108` — `make_test_backend()` constructor. Updates to the field shape break this; must land in the same commit as the field refactor.
- `crates/yserver/src/kms/backend.rs:1873` — `KmsBackend::absolute_origin(window)` is the helper `window_intersects` should call (not `window_absolute_position`, which lives on `ResourceTable`).
- `crates/yserver/src/bin/ynest.rs:62-79` — actual ynest entry point; calls `yserver_core::nested::run(display, width, height)`.
- `crates/yserver-core/src/nested.rs:209-247` — `pub fn run(display, width, height)` constructs `ServerState::with_geometry(width, height)`. Width/height are explicit args — `HostX11Backend` itself does **not** store screen dimensions, so a hypothetical `Backend::randr_outputs()` cannot derive them from the backend alone.
- `crates/yserver-core/src/server.rs:212-228` — `ServerState::with_geometry` calls `RandrState::nested(0, width, height)`. The plan adds a sibling `with_randr_outputs(width, height, outputs: Vec<RandrOutput>)` and wires entry points to call it directly.
- `crates/yserver/src/input_thread.rs` — libinput cursor accumulator. Operates in virtual-screen coords already.
- `Justfile:3-44` — existing vng recipes (`yserver`, `yserver-headless`, `yserver-ssh`).

---

## Step 0 — vng multi-scanout environment verification

**Goal:** Confirm `virtio-gpu-pci,max_outputs=2` exposes two connectors to the guest kernel and that QEMU's display backend renders both scanouts visibly. Per spec §2.8: this is the validation hypothesis, not a verified path. Confirm before writing any backend code.

**No code changes in this step.** Output is a working note recording the verified recipe (or a documented escalation if neither SDL nor GTK works).

- [ ] **Step 0.1: Build the existing single-output yserver** (`cargo build --bin yserver`) so we have a known-good binary to run in the multi-output env.

- [ ] **Step 0.2: Boot under `max_outputs=2 -display sdl,gl=on` and check connector count.**

```sh
vng -r {{KERNEL}} --disable-microvm --rw \
    --qemu-opts="-display sdl,gl=on -vga none -device virtio-gpu-pci,max_outputs=2 \
                 -device virtio-tablet-pci -device virtio-keyboard-pci" \
    -- bash -c 'for c in /sys/class/drm/card0-*/status; do echo "$c: $(cat $c)"; done; sleep 5'
```

Expected: two `Virtual-N: connected` lines. If only one connector shows, the kernel is collapsing scanouts (bug or QEMU version skew); fall through to Step 0.3.

- [ ] **Step 0.3: If SDL collapses scanouts, try GTK with tabs.**

```sh
vng -r {{KERNEL}} --disable-microvm --rw \
    --qemu-opts="-display gtk -vga none -device virtio-gpu-pci,max_outputs=2 \
                 -device virtio-tablet-pci -device virtio-keyboard-pci" \
    -- bash -c 'sleep 30'
```

Visually confirm the QEMU window shows two display tabs.

- [ ] **Step 0.4: Run yserver under the chosen recipe and confirm bring-up still succeeds.**

It will only modeset the first connector (since the backend is single-output today), but it should not crash, and the second scanout should remain blank-but-present in the QEMU display.

- [ ] **Step 0.5: Record the verified recipe** in `docs/superpowers/notes/2026-05-07-phase6-10-vng-recipe.md`. Include exact `vng` command line, the kernel-reported connector list, and any QEMU/host-version caveats. This file replaces the spec's tentative recipe and is referenced by Step 5.

If neither SDL nor GTK works on this host, **stop and escalate** — the entire phase's validation gate depends on multi-scanout vng. Possible alternatives: real bare-metal validation only, or running two QEMU instances (loses the shared-process model).

## Step 1 — Per-CRTC page-flip identifier (plumbing only)

**Files:**
- Modify: `crates/yserver/src/drm/page_flip.rs`
- Modify: `crates/yserver/src/kms/backend.rs::drain_page_flips_and_composite` (callsite)

**Goal:** Plumb `crtc::Handle` through the page-flip drain closure. Single-output behavior preserved — the existing single-CRTC callsite ignores the new parameter. No swapchain logic changes yet.

- [ ] **Step 1.1: TDD — failing test for the new closure signature.**

  Add a unit test in `drm/page_flip.rs` that exercises a synthetic event drain. The drm crate's `Event` enum can be constructed in tests via the public fields on `PageFlipEvent`. Test asserts the closure receives the CRTC handle.

- [ ] **Step 1.2: Refactor `drain_events` to take `FnMut(crtc::Handle)`.**

  ```rust
  pub fn drain_events<F: FnMut(crtc::Handle)>(
      device: &Device,
      mut on_page_flip: F,
  ) -> io::Result<()> {
      for event in device.receive_events()? {
          if let Event::PageFlip(ev) = event {
              on_page_flip(ev.crtc);
          }
      }
      Ok(())
  }
  ```

- [ ] **Step 1.3: Update `drain_page_flips_and_composite` callsite.**

  Closure currently does `handled += 1`. Now does `handled += 1` and discards the CRTC for the moment (Step 4 will use it). Single-output behavior unchanged.

- [ ] **Step 1.4: `cargo test --workspace` green; `cargo clippy` clean.** Commit.

## Step 2 — `discover_outputs` returning `Vec<Output>`

**Files:**
- Modify: `crates/yserver/src/drm/modeset.rs`
- Modify: callsites in `crates/yserver/src/kms/backend.rs::open` and `crates/yserver/src/lib.rs` (one-liner that delegates to backend)

**Goal:** Multi-output enumeration with hard-error on unplaceable connector (spec §2.1, virtio-gpu scope).

- [ ] **Step 2.1: Carve a pure CRTC/plane assignment helper, then TDD it.**

  The codex review flagged that `Device::for_tests()` is just `/dev/null` (`drm/device.rs:22`) and can't drive `discover_outputs` end-to-end. The plan extracts the **pure** part of the assignment policy — the part that takes already-fetched connector/encoder/CRTC/plane handles and decides which to claim — into a free function:

  ```rust
  /// Inputs:
  ///   - `connectors`: ordered list of (connector handle, connector_name, encoder_handle, candidate_crtcs, candidate_planes)
  /// Output:
  ///   - `Vec<Assignment { connector, encoder, crtc, plane }>`, or `Err(stranded_name)` if any connector lacks an unclaimed (CRTC, plane).
  fn assign_outputs(connectors: &[ConnectorCandidate]) -> Result<Vec<Assignment>, String>;
  ```

  This is what gets unit tests: feed in synthetic candidate lists, assert the assignments. `discover_outputs` becomes a thin shell that pulls real handles out of the device, calls `assign_outputs`, and finishes property lookups for the assigned set. Tests cover: (a) two connectors → two distinct CRTCs picked; (b) one connector with an empty candidate-CRTC list → returns `Err` with the connector name; (c) two connectors competing for the same single CRTC → second connector returns `Err`.

  Note: greedy is still the policy (virtio-gpu scope per spec §2.1) — we're just making it testable. The follow-up phase that adds bipartite matching swaps `assign_outputs` internals, leaves the contract.

- [ ] **Step 2.2: Implement `discover_outputs(device: &Device) -> io::Result<Vec<Output>>`.**

  Walk every `info.state() == Connected && !info.modes().is_empty()` connector. Maintain `claimed_crtcs: HashSet<crtc::Handle>` and `claimed_planes: HashSet<plane::Handle>`. If `build_output` cannot place a connector, **return `Err`** — do not silently skip. Add the TODO marker for real-hardware matching from spec §2.1.

- [ ] **Step 2.3: Keep `discover_output(device) -> io::Result<Output>` as a thin wrapper** that returns `outs.into_iter().next().ok_or(...)?`. Callsites in tests and any single-output code keep working.

- [ ] **Step 2.4: `cargo test --workspace` green; `cargo clippy` clean.** Commit.

## Step 3 — `OutputLayout` + per-output swapchain on `KmsBackend`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (struct fields, `open`, `disable_output`)
- Modify: `crates/yserver/src/lib.rs:127-130` (`backend.disable_output()` callsite — no signature change)

**Goal:** Replace the singleton `output: Output` and `swapchain: Swapchain` fields with `outputs: Vec<OutputLayout>`. Bring-up loops with rollback. `composite_and_flip` and `drain_page_flips_and_composite` still touch only the first output (Step 4 makes the painting per-output).

- [ ] **Step 3.1: Define `OutputLayout`.**

  ```rust
  pub(crate) struct OutputLayout {
      pub output:    drm::modeset::Output,
      pub swapchain: drm::Swapchain,
      pub x:         i32,
      pub y:         i32,
      pub width:     u16,
      pub height:    u16,
  }

  impl OutputLayout {
      pub fn rect(&self) -> Rect { Rect { x: self.x, y: self.y, w: self.width.into(), h: self.height.into() } }
  }
  ```

- [ ] **Step 3.2: Replace `output` + `swapchain` fields with `outputs: Vec<OutputLayout>` on `KmsBackend`.** Keep `fb_w` and `fb_h` (they now hold the virtual-screen extent: `fb_w = max(layout.x + layout.width)`, `fb_h = max(layout.y + layout.height)`).

- [ ] **Step 3.3: Update `KmsBackend::open` per spec §2.1 rollback pattern.**

  ```rust
  let outputs = drm::modeset::discover_outputs(&device)?;

  // Horizontal layout, in connector order.
  let mut layouts = Vec::with_capacity(outputs.len());
  let mut next_x: i32 = 0;
  for output in outputs {
      let w = output.picked.width;
      let h = output.picked.height;
      let buffers = build_swapchain_buffers(&device, w.into(), h.into())?;
      let initial_fb = buffers[0].fb_id();
      if let Err(err) = drm::modeset::commit_modeset(&device, &output, initial_fb) {
          for done in layouts.iter().rev() {
              let _ = drm::modeset::disable_output(&device, &done.output);
          }
          return Err(err);
      }
      let swapchain = drm::Swapchain::with_initial_scanout(buffers, 0);
      layouts.push(OutputLayout { output, swapchain, x: next_x, y: 0, width: w, height: h });
      next_x += i32::from(w);
  }
  ```

- [ ] **Step 3.4: Update `disable_output` to loop.**

  ```rust
  pub fn disable_output(&self) -> io::Result<()> {
      let mut last_err = None;
      for layout in &self.outputs {
          if let Err(e) = drm::modeset::disable_output(&self.device, &layout.output) {
              log::warn!("disable_output failed for {}: {e}", layout.output.connector_name);
              last_err = Some(e);
          }
      }
      last_err.map_or(Ok(()), Err)
  }
  ```

- [ ] **Step 3.5: `composite_and_flip` and `drain_page_flips_and_composite` temporarily operate on `outputs[0]` only.** This keeps Step 3 compilation green; Step 4 generalizes them.

- [ ] **Step 3.6: Update `make_test_backend` in the same commit.**

  `crates/yserver/src/kms/backend.rs:5108` constructs a synthetic `KmsBackend` for the existing tests. With `output` + `swapchain` replaced by `outputs: Vec<OutputLayout>`, `make_test_backend` must build a single `OutputLayout` (default `(0, 0, 800, 600)`, dummy `Output`/`Swapchain` from existing `Swapchain::empty_for_tests()` and an existing test-output helper if one exists or a new `Output::empty_for_tests()` if not). Otherwise the workspace build breaks. **Land this in the same commit as Step 3.2.**

- [ ] **Step 3.7: Carve an injectable modeset commit, then TDD rollback.**

  Codex flagged that a `commit_modeset_with_hook` only tests rollback if `KmsBackend::open` actually calls an injectable commit. Refactor `open` to accept a `ModesetCommit` function pointer (or `&dyn Fn`):

  ```rust
  type ModesetCommit = fn(&Device, &Output, framebuffer::Handle) -> io::Result<()>;

  impl KmsBackend {
      pub fn open(path: &str) -> io::Result<Self> {
          Self::open_with_commit(path, drm::modeset::commit_modeset)
      }

      fn open_with_commit(path: &str, commit: ModesetCommit) -> io::Result<Self> {
          // ... build outputs, loop calling `commit(...)` instead of `commit_modeset(...)` ...
      }
  }
  ```

  The rollback unit test calls `open_with_commit` with a closure that succeeds for the first call and returns `Err` on the second, then asserts that the first output's `disable_output` was invoked. (Use a `Cell<usize>` counter inside the closure; track `disable_output` calls via a parallel test seam — likely an injected `disable_output` fn pointer too, or by inspecting the device's atomic-commit history if a recording-style `Device` test seam exists.) If injecting `disable_output` proves messy, settle for asserting that `open_with_commit` returns the right error and trust manual smoke for the disable calls.

- [ ] **Step 3.8: `cargo test --workspace` green; `cargo clippy` clean.** Single-output regression smoke: `just yserver-headless-shutdown` produces 100+ flips and clean shutdown. Commit.

## Step 4 — Per-output composite + flip + page-flip dispatch

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs::composite_and_flip` (~1664)
- Modify: `crates/yserver/src/kms/backend.rs::drain_page_flips_and_composite` (~2090)
- Modify: `crates/yserver/src/kms/backend.rs` — add `window_intersects(window_id, rect) -> bool` helper

**Goal:** Per-output paint loop with origin translation by `(-layout.x, -layout.y)` and bbox intersection pre-filter (spec §2.5). Page-flip events dispatch to the correct swapchain via `crtc::Handle`.

- [ ] **Step 4.1: TDD — failing test for `window_intersects` pre-filter.**

  Test creates a synthetic `KmsBackend` with one window at virtual-screen `(2000, 100, 100, 100)` and queries intersection against an output rect at `(0, 0, 1024, 768)` (no overlap → false) and an output rect at `(1900, 0, 1024, 768)` (overlap → true). Test exists in `kms/backend.rs` colocated with the helper.

- [ ] **Step 4.2: Implement `window_intersects(window_id: u32, rect: Rect) -> bool`.**

  Use `KmsBackend::absolute_origin(window)` (`backend.rs:1873`) — it already returns `(f64, f64)` window origin in screen coords, and is cheaper than a `ResourceTable` walk because the backend tracks subwindow placement internally. Combine with the window's stored width/height for the bbox. Standard rect-overlap test.

- [ ] **Step 4.3: Refactor `composite_and_flip` to per-output loop per spec §2.5.**

  ```rust
  pub fn composite_and_flip(&mut self) -> io::Result<()> {
      let top_levels: Vec<u32> = self.top_level_order.clone();

      // Pre-filter per output (spec §2.5: avoid descending whole off-screen subtrees).
      let visible_per_output: Vec<Vec<u32>> = self.outputs.iter()
          .map(|layout| top_levels.iter()
              .copied()
              .filter(|&id| self.window_intersects(id, layout.rect()))
              .collect())
          .collect();

      for (layout_idx, layout) in self.outputs.iter_mut().enumerate() {
          let visible = &visible_per_output[layout_idx];
          let Some(buf_idx) = layout.swapchain.acquire_idx() else { continue };
          // ... wrap buffer in PixmanImage, call paint_output(scanout, layout, visible) ...
          // ... drop scanout, submit_flip(layout.output, fb_id), swapchain.submit ...
      }
      Ok(())
  }
  ```

  Lift the existing body into a `paint_output(&self, scanout: &mut PixmanImage, layout: &OutputLayout, visible: &[u32])` method that:
  - Fills root background (translated: `(0,0)` in scanout = `(layout.x, layout.y)` in screen space).
  - Overlays `bg_pixmap` if present.
  - Composites each `window_id` in `visible` via a new `composite_window_into_offset` that takes `(-layout.x, -layout.y)` translation.
  - Draws the cursor at `(cursor_x - layout.x, cursor_y - layout.y)`, clipped to scanout bounds.

- [ ] **Step 4.4: Refactor `drain_page_flips_and_composite` per spec §2.4.**

  ```rust
  pub fn drain_page_flips_and_composite(&mut self) -> io::Result<()> {
      let mut flipped: Vec<crtc::Handle> = Vec::new();
      drm::page_flip::drain_events(&self.device, |crtc| flipped.push(crtc))?;

      for crtc in flipped {
          if let Some(layout) = self.outputs.iter_mut().find(|o| o.output.crtc == crtc) {
              if let Some(idx) = layout.swapchain.submitted_idx() {
                  layout.swapchain.complete(idx)
                      .map_err(|e| io::Error::other(format!("swapchain.complete: {e}")))?;
              }
          } else {
              log::warn!("page-flip event for unknown CRTC {crtc:?}");
          }
      }

      self.composite_and_flip()
  }
  ```

- [ ] **Step 4.5: TDD — composite-and-flip test with two synthetic outputs.**

  `make_test_backend` (already updated in Step 3.6 to take a single `OutputLayout`) gets a sibling `make_test_backend_two_outputs(positions: &[(i32, i32, u16, u16)])` that builds an in-memory backend with two `OutputLayout`s side-by-side, each backed by a `PixmanImage`-only scanout (no real DRM Buffer). Test creates a window at virtual-screen `(2000, 100)`, calls a test-only `paint_output_for_test(layout_idx)` that runs the same paint loop body as `composite_and_flip` but skips the `submit_flip` step, asserts the second output's scanout received the window and the first did not. Drop scaffolding once the per-output painter is verified.

- [ ] **Step 4.6: `cargo test --workspace` green; `cargo clippy` clean.** Single-output regression: `just yserver-headless-shutdown` still flips 100+/sec, clean shutdown. Commit.

## Step 5 — `RandrState` multi-output + RANDR handler audit

**Files:**
- Modify: `crates/yserver-core/src/randr.rs` (rewrite)
- Modify: `crates/yserver-core/src/server.rs` (`RandrState` field type, `with_geometry` constructor + new `with_randr_outputs` constructor)
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (every `RR…` handler ~line 1274 onward)
- Modify: `crates/yserver-core/src/core_loop/run.rs` (singleton-ID call site at L380-L382 in `handle_host_container_resize`)
- Modify: `crates/yserver-protocol/src/x11/randr.rs::CrtcInfoData` (add `x: i16`, `y: i16` fields + encoder)
- Modify: `crates/yserver-core/src/nested.rs` (`pub fn run` constructs the synthetic single-output `RandrOutput` for ynest and seeds `with_randr_outputs`)
- Modify: `crates/yserver/src/lib.rs:36-40` (after `KmsBackend::open`, ask backend for its layout list and seed `ServerState::with_randr_outputs`)
- Add to `KmsBackend` only: a `pub fn randr_outputs(&self) -> Vec<RandrOutput>` *inherent* method (not a trait method — see §below) that builds records from `self.outputs`.

**Why no `Backend::randr_outputs()` trait method:** the original plan put `randr_outputs` on the `Backend` trait. Codex's review showed that's the wrong shape: `HostX11Backend` doesn't store screen dimensions, and the actual ynest entry (`crates/yserver/src/bin/ynest.rs:62-79` → `nested::run(display, width, height)`) gets dimensions as explicit args. Putting `randr_outputs` on the trait would force `HostX11Backend` to grow width/height fields just to answer the trait. Cleaner: each entry point constructs the `Vec<RandrOutput>` from sources it already has — KMS pulls from `OutputLayout`s, ynest builds a literal `ynest-0` single-output record from its `width`/`height` args. `RecordingBackend` (`crates/yserver-core/src/backend/recording.rs`) and `make_test_backend` need no new method to implement.

**Goal:** `RandrState` carries `Vec<RandrOutput>` + `Vec<RandrMode>` (deduped) + `primary_output` + `screen_*_mm`. Every RANDR request handler reads from the new collections instead of singleton const literals. ynest behavior unchanged (one output → identical wire bytes).

- [ ] **Step 5.1: TDD — unit tests for `RandrState::from_outputs`.**

  Tests in `crates/yserver-core/src/randr.rs`:
  - Two outputs at `(0,0,1024,768)` and `(1024,0,1280,1024)` produce `screen_width = 2304`, `screen_height = 1024`, `screen_width_mm = sum`, `screen_height_mm = max`.
  - Two outputs at the same `(w,h,vrefresh)` share a single `RandrMode` entry in `state.modes`.
  - Two outputs at different resolutions produce two entries.
  - `primary_output == outputs[0].output_id`.

- [ ] **Step 5.2: Define `RandrOutput`, `RandrMode`, rewrite `RandrState`.**

  Drop `OUTPUT_ID` / `CRTC_ID` / `MODE_ID` consts. Per spec §2.6.1. `RandrState::from_outputs(timestamp, outputs: Vec<RandrOutput>) -> Self` computes screen extent, mm aggregation, mode dedup. Keep `RandrState::nested(timestamp, w, h)` as a thin wrapper that builds a single `RandrOutput` and calls `from_outputs` (preserves test fixtures and ynest behavior).

- [ ] **Step 5.3: Add `(x, y)` to `CrtcInfoData` and encoder.**

  In `crates/yserver-protocol/src/x11/randr.rs::encode_get_crtc_info_reply`: bytes 8-11 already reserved for x/y per spec; today they're zero-fill. Two `put(byte_order, &mut out, …)` calls to land the new fields. Update `CrtcInfoData` struct definition + every callsite that constructs one.

- [ ] **Step 5.4: Update RANDR request handlers in `core_loop/process_request.rs` AND `core_loop/run.rs`.**

  Codex's review identified a second singleton-ID call site in `core_loop/run.rs:380-382` (`handle_host_container_resize`) that hard-codes `CRTC_ID` / `MODE_ID` / `OUTPUT_ID`. Update **both files** in this step — if the consts go away from `randr.rs` without touching `run.rs`, the build breaks.

  - [ ] `core_loop/run.rs::handle_host_container_resize`: when the host container resizes (ynest only), update `state.randr.outputs[0]` (and the matching `RandrMode` if dedup makes resolution unique) instead of the const triple. Re-emit `RRScreenChangeNotify` / `RRCrtcChangeNotify` / `RROutputChangeNotify` per current behavior, but read IDs from the (now-mutable) `RandrOutput`.

  Walk the `RR…` arm dispatch table starting around line 1274 of `process_request.rs`. For each handler, replace singleton-const reads with iteration over `state.randr.outputs` / `state.randr.modes`:

  - [ ] `RRGetScreenSizeRange`: returns `(state.randr.screen_width, screen_height, screen_width, screen_height)` (no min/max growth in this phase).
  - [ ] `RRGetScreenResources` / `RRGetScreenResourcesCurrent`: build `crtcs: Vec<u32>`, `outputs: Vec<u32>`, `modes: Vec<ModeInfo>` from state.
  - [ ] `RRGetOutputInfo`: look up the matching `RandrOutput`, return `(timestamp, crtc, width_mm, height_mm, name)`.
  - [ ] `RRGetCrtcInfo`: look up by CRTC, return `(timestamp, x, y, width, height, mode_id, [output_id])`.
  - [ ] `RRGetCrtcGamma` / `RRGetCrtcGammaSize`: return size=0 per output.
  - [ ] `RRGetOutputPrimary`: return `state.randr.primary_output`.
  - [ ] `RRGetMonitors`: per spec §2.6.2 — one monitor entry per output, first marked primary, name atom is interned connector name.
  - [ ] `RRSelectInput`: unchanged (per-client mask storage already works).
  - [ ] `RRGetOutputProperty`: continue returning empty.
  - [ ] All mutation paths (`RRSetCrtcConfig`, `RRSetOutputPrimary`, etc.): continue returning `BadValue`.

- [ ] **Step 5.5: Construct `Vec<RandrOutput>` at each entry point.**

  No trait method. Each entry point owns its own construction:

  - **ynest** (`crates/yserver-core/src/nested.rs::run` — called from `crates/yserver/src/bin/ynest.rs:62-79`): builds one synthetic `RandrOutput` literally matching today's `RandrState::nested` semantics — `name = "ynest-0"`, `output_id = 1`, `crtc_id = 2`, `mode_id = 3`, `(x, y) = (0, 0)`, `width`/`height` from the `width`/`height` args, mm derived via the existing 96-DPI heuristic. **The exact integer IDs and the `ynest-0` name are preserved** so the existing xts Xrandr pass and any wire-byte-equivalence tests don't observe a delta. Codex flagged that "host connector name" is *not* a stable source for ynest — there is no host connector concept exposed to the nested server.
  - **yserver** (`crates/yserver/src/lib.rs:36-40`): after `KmsBackend::open`, call a new inherent `KmsBackend::randr_outputs(&self) -> Vec<RandrOutput>` that walks `self.outputs` and emits one `RandrOutput` per layout. Output ID / CRTC ID / mode ID allocation follows the deduped scheme from §2.6.1: outputs 1..=N, CRTCs (N+1)..=2N, modes 2N+1.. with dedup by `(w, h, vrefresh)`. Connector name from `output.connector_name`.

- [ ] **Step 5.6: Wire callers.**

  - In `nested::run`, replace `ServerState::with_geometry(width, height)` with `ServerState::with_randr_outputs(width, height, build_ynest_randr_outputs(width, height))`.
  - In `yserver::run` (`crates/yserver/src/lib.rs:36-40`), call `backend.randr_outputs()` after `KmsBackend::open`, and pass to `ServerState::with_randr_outputs(fb_w, fb_h, outputs)`.
  - `ServerState::with_geometry` keeps working as a thin wrapper (delegates to `with_randr_outputs` with the ynest-style synthetic single-output record so any other callers — tests, recording backend — keep passing).

- [ ] **Step 5.7: `cargo test --workspace` green; `cargo clippy` clean.** Critical regression check: ynest's xts Xrandr scenario (and Xproto / Xlib3 baselines) must not regress. Run `just xts-ynest scenario=Xrandr` if the scenario exists; if not, run a basic `xrandr -q` against ynest and confirm the output looks identical to pre-Step-5 (one output, same name, same dimensions).

- [ ] **Step 5.8: Self-review via codex** before Step 6 visual smoke. Use `codex exec --sandbox read-only` with a prompt file referencing the spec, the new `randr.rs`, and the updated `process_request.rs`. Address any blocking findings.

  Commit.

## Step 6 — Multi-monitor smoke under vng

**Files:**
- Modify: `Justfile` (add `yserver-multihead` target)
- Create: `docs/superpowers/notes/2026-05-07-phase6-10-validation.md` (record the smoke results)

**Goal:** End-to-end visual confirmation that two outputs work as a unified virtual screen.

- [ ] **Step 6.1: Add Justfile target.**

  Use the recipe verified in Step 0 (likely `-display sdl,gl=on -device virtio-gpu-pci,max_outputs=2`). **Pin `YSERVER_MODE=1024x768`** so the geometry arithmetic in Step 6.2 is predictable — without this, virtio-gpu's xres/yres hint and SDL's window sizing can produce per-output widths that don't match what the test assertions assume.

  ```just
  yserver-multihead:
      cargo build --bin yserver
      vng -r {{KERNEL}} --disable-microvm --rw \
          --qemu-opts="-display sdl,gl=on -vga none -device virtio-gpu-pci,max_outputs=2 \
                       -device virtio-tablet-pci -device virtio-keyboard-pci" \
          -- env YSERVER_MODE=1024x768 target/debug/yserver
  ```

- [ ] **Step 6.2: Smoke run.**

  With `YSERVER_MODE=1024x768` pinned in Step 6.1, both outputs run 1024×768. Virtual screen is 2048×768; the seam is at x=1024.

  ```sh
  just yserver-multihead
  # In the guest, via ssh or in-window terminal:
  DISPLAY=:7 xrandr -q
  DISPLAY=:7 xterm -geometry +1200+100 &     # past the seam at x=1024 → second output
  DISPLAY=:7 xterm -geometry +100+100 &      # first output
  DISPLAY=:7 xclock -geometry +1500+200 &    # second output
  DISPLAY=:7 fvwm3 &
  ```

  Visually verify each item from the spec's validation gate (§5):

  - [ ] Two SDL/GTK windows show distinct portions of the virtual screen.
  - [ ] Cursor crosses the seam smoothly.
  - [ ] `xrandr -q` reports two `Virtual-N` outputs at expected `(x, y)` positions and modes.
  - [ ] `xrandr -q` reports `primary` flag on the first output.
  - [ ] `xrandr -q` shows two outputs sharing one mode line (dedup verification — both outputs should run the same QEMU default mode).
  - [ ] `xterm +1200+100` lands on the second output; `+100+100` lands on the first.
  - [ ] `fvwm3` starts; panels and chrome render across both outputs.
  - [ ] `xdpyinfo` reports root `dimensions` matching virtual-screen extent and `width_mm`/`height_mm` ≈ sum-of-w / max-of-h.

  Capture screenshots into the validation note.

- [ ] **Step 6.3: ynest regression smoke.**

  Confirm Phase 6.9 xts matrix unchanged (Xproto, Xlib3, ShapeExt, XI, XIproto). Diff PASS counts against the run-history table in `docs/xts-baseline.md`. Single-output behavior preserved means zero deltas.

- [ ] **Step 6.4: Bare-metal validation (optional, if hardware available).**

  On the CachyOS host with two physical displays, run `sudo target/release/yserver` per the Phase 6.1 follow-ups recipe (stop conflicting kmscon first). Re-run the §5 checks. Record outcome in the validation note. **Failure is informational, not blocking** — Phase 6.10 is virtio-gpu-scoped per §2.1; bare-metal multi-output may surface the encoder/CRTC matching gap that's deferred to Phase 6.10.x.

- [ ] **Step 6.5: Update `docs/status.md`** with a new "Phase 6.10 — Multi-monitor on KMS" section recording landed work, validation outcomes, and follow-ups (real-hardware matching, hotplug, runtime mode switch). Mirror the existing Phase 6.x section style.

  Commit.

## Step 7 — Merge

- [ ] **Step 7.1: Final clippy + test sweep.** `cargo clippy --workspace --all-targets -- -W clippy::pedantic` clean. `cargo test --workspace` green.
- [ ] **Step 7.2: Squash branch onto `master`** per project convention. PR description summarizes the architecture changes (per-CRTC swapchain, virtual-screen layout, RANDR multi-output, deduped modes, primary-output, mm aggregation), validation outcomes, and the explicit virtio-gpu scope.
- [ ] **Step 7.3: Address PR comments** via new commits (no amend). Resolve threads on GH per project convention.

---

## Out of scope (deferred to Phase 6.10.x — track separately, not in this plan)

- Real-hardware encoder/CRTC matching (Intel/AMD shared encoder pools).
- Hotplug (connector add/remove at runtime, KMS uevent drain, `RRScreenChangeNotify` fanout).
- Runtime mode switching (`RRSetCrtcConfig`).
- Mirror/clone mode.
- Overlay / cursor planes.
- Per-output EDID-derived physical mm.
- `YSERVER_LAYOUT` env override (default horizontal-by-enumeration is enough).
- xrandr-driven layout reconfigure.
