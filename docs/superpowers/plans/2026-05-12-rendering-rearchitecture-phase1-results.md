# Phase 1 — rendering re-architecture — validation results

Date: 2026-05-12
Plan: `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase1.md`
Branch: `graphics-followups`

## Preflight checks

All ran clean at the end of T10 (commit `87e887c`):

- `cargo +nightly fmt --check` — no diff.
- `cargo test` — all crates green:
  - `yserver` unit tests: 121 passed, 0 failed.
  - `yserver-core`: 284 passed.
  - `yserver-protocol`: 208 passed.
  - Plus integration test crates.
- `cargo clippy` — 5 pre-existing doc-list-indentation warnings in `kms/backend.rs` and `kms/vk/pipeline.rs`. Zero new warnings introduced by phase 1.

## Cutover greps

- `rg 'screen_dirty' crates/yserver/` — **zero hits**. Field, writers, reader, and tests all removed in T7.
- `rg -n 'queue_wait_idle' crates/yserver/src/kms/vk/ops/mod.rs` — **4 hits** (`OpsCommandPool::drop` at 59, `run_one_shot_op` at 100, `OpsStaging::ensure` at 168, `OpsStaging::drop` at 184). Hot-path waitIdle is **still in place** as required — phase 4 removes it.

## Hardware smoke (bee)

User-driven validation on hardware host (`bee`):

- Desktop session (mate via `just yserver-mate-hw-release` or equivalent) comes up.
- **Observed: slightly more responsive** than the pre-phase-1 baseline.

The responsiveness improvement is not expected from the canonical phase-1 changes alone (paint recorders and per-op `vkQueueWaitIdle` are unchanged). The most plausible contributor is the **per-output dirty narrowing in T8**: geometry-change handlers no longer bump every output's dirty generation, so multi-output setups skip redundant composite cycles on outputs unaffected by a given change. The previous `mark_all_outputs_dirty()` blast-radius is now confined to producers that genuinely require it (paint, cursor motion, the catch-all `mark_dirty` trait method).

## Skipped at user discretion

- **XTS scenarios** (`just xts-ynest`, `just xts-yserver`) — user judgment, no regression expected at this scope.
- **rendercheck** (`just rendercheck-yserver`) — user judgment, no regression expected.

## Done conditions

Per the plan's "Done conditions" section, all 9 conditions hold:

1. ✅ All tasks committed; tree is green (fmt/clippy/test).
2. ✅ `screen_dirty: bool` no longer exists.
3. ✅ Five new scheduler types exist (`OutputDamageState`, `InFlight`+`InFlightFrame`, `PaintBatch`, `OutputFrame`, `RenderScheduler`) with unit tests; wired into `KmsBackend`.
4. ✅ Geometry-change paths use `mark_window_dirty_with_old_rect` (Configure/Restack/Map/Unmap/Destroy/Reparent — 6 sites, zero fallback `mark_all_outputs_dirty` TODOs).
5. ✅ Every **successful Vulkan composite submit** pushes an `OutputFrame` into `self.scheduler.in_flight`.
6. ✅ In-flight queue polled non-blocking at every quiescent point; fully-retired frames drained.
7. ✅ Hardware smoke (bee) passes; no regression; responsiveness improvement observed.
8. ✅ `vkQueueWaitIdle` catalogue at `docs/superpowers/specs/2026-05-12-waitidle-catalogue.md` — 22 sites classified.
9. ✅ Hot-path `vkQueueWaitIdle` in `vk/ops/mod.rs::run_one_shot_op` still present — phase 4 removes it.

## Commit summary (graphics-followups, 2026-05-12)

| Task | Commit(s) | Notes |
|---|---|---|
| Setup | `200b8de` | HLD, codex parallel, phase-1 plan, dead-end POSTMORTEM |
| T1 | `2491f48`, `94845cb`, `58dcf75` | waitIdle catalogue + 2 review rounds |
| T2 | `fe82a56`, `0317a29` | OutputDamageState + debug_assert hardening |
| T3 | `4b8399f`, `e882398` | InFlight queue + get_mut for borrow-split |
| Plan fix | `2924ef9` | T9 poll_in_flight rewritten to index-based two-pass |
| T4 | `425a9fd` | PaintBatch + OutputFrame shells |
| T5 | `2a5aa81` | RenderScheduler shell |
| T6 | `4eea3bb` | Wire damage + scheduler into backend |
| T7 | `6044dac`, `5cb37dd` | screen_dirty cutover + test reframe |
| T8 | `7210821`, `3184fd9`, `223b1bb`, `fbc42f0` | helper + Configure + Map/Unmap/Destroy + Reparent/Restack |
| T9 | `2a85c1d`, `29a996e` | Composite → InFlight routing + SAFETY comments |
| T10 | `e7418ca`, `87e887c` | Invariant asserts + logs (vacuous-assert fix) |

22 commits total on top of `9237cdc` (master branch tip at start).

## Known deferred items

- **T8 C2 (`3184fd9`)** accidentally bundled the user's pre-existing `docs/status.md` change (102 lines) because the implementer used `git commit -am`. Content is correct; commit title is slightly misleading. Subsequent commits used explicit `git add`. Not worth rewriting history.
- **Unmap/Map of already-(un)mapped windows** spuriously bump dirty generations. Asymmetric with `DestroyWindow` which correctly guards on `mapped`. Not a correctness bug (composite walks skip unmapped windows); cleanup candidate for a follow-up handler-symmetry pass.
- **Double-bump in combined ConfigureWindow + restack** (geometry-only + stack-only paths both fire) — wasted work, not a correctness bug.
- **No-VK + non-null fence fallback** in `poll_in_flight` returns `gpu_done` (i.e. `false`), which would stall a frame indefinitely. Unreachable today (no VK ⇒ no `try_vulkan_composite_flip` success ⇒ no frame pushed with non-null fence). Worth a comment if VK teardown is ever added mid-session.

## What's next

Phase 2 (frame-owned composite descriptor pools) is the natural next step. Phase 3 (recorder migration to `PaintBatch`) and phase 4 (sync rework with timeline semaphores + binary SYNC_FD at the KMS boundary) follow. Per the HLD's status section: write each phase's plan *after* the previous phase lands, against the real shape of the code.
