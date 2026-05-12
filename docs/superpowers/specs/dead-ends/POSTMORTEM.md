# Postmortem — 2026-05-12 paint-composite-sync rework (v1–v5)

**Date:** 2026-05-12
**Status:** Abandoned. Re-architecture chosen instead.

## What we tried

Replace per-op `vkQueueWaitIdle` in the KMS paint→composite→flip path with Vulkan-native sync (binary semaphores per (slot, output), `synchronization2` barriers, exported SYNC_FDs), preserving the existing API shape of the recorders, scratch types, and compositor.

5 spec versions, 3 plan revisions, 4 codex review rounds on the spec, 3 on the plan.

## Why it failed

The pattern across rounds was unambiguous:

- Spec round 2: 2 blockers (timeline portability, shared mirror sync).
- Spec round 3: 1 blocker (binary semaphore wait/signal cardinality — N consumers cannot wait on one signal).
- Spec round 4: 1 blocker (skipped-output `paint_done` leak), 2 highs (descriptor ring sizing, output-set hotplug).
- Plan round 2: 3 blockers (`screen_dirty` cleared unconditionally; `compose_catch_up_outputs` skipped retirement; `composite_fence` registered before submit succeeded), 3 highs.
- Plan round 3: 4 blockers (post-submit error fence orphan, `paint_done` orphan on composite preflight failure, mock signature regression, `OutputId` stability), 4 highs.

Blocker count *did not decrease* across iterations. Each fix surfaced new edge cases because the design tried to do async per-output work inside an API shape whose recorders, scratch types, and compositor were all built on a "GPU-is-idle-when-this-returns" contract. The contract was the bug. Surgically preserving it while adding async sync produced a system where every part interacted with every other part in subtle ways.

## What we learned

1. **The right unit of replacement is the renderer scheduling/lifetime layer, not the draw implementations.** Trying to fix sync inside `run_one_shot_op`'s shape was the wrong scope.

2. **"GPU-idle-on-return" is a load-bearing assumption across the codebase.** Scratch resizes, descriptor pool resets, scanout BO reuse, the "mirror is sampleable because previous op waited" contract — all depend on it implicitly. Async sync inside this contract is structurally fragile.

3. **Codex was correct on every blocker.** None of the findings were noise. The pattern of recurring blockers is itself the diagnostic signal that the design is wrong, not that the review is too strict.

4. **Recurring blockers across review rounds are not "diminishing returns."** They are a smell. We accepted the smell for one round too many before pivoting.

## What replaces this

Re-architecture in the style of wlroots / Mutter:

- `PaintBatch` accumulates paint into a per-frame command buffer.
- `OutputFrame` per dirty output owns command buffer + descriptors + sync primitives.
- In-flight list tracks frames; retirement driven by timeline/fence, not immediate reuse.
- Timeline semaphores internally; binary SYNC_FDs only at the KMS boundary.
- Per-output dirty generations replace global `screen_dirty`.
- KMS `FB_DAMAGE_CLIPS` and render damage clipping come *after* the sync hot path is correct.

Scope: weeks of focused work, not days. A KMS feature freeze is part of the cost.

## What to keep from this work

Reading the dead-end files for the **problem statements** is still useful:

- The "Current state — concrete observations" section in the spec catalogues every `vkQueueWaitIdle` site and its lifetime reason. Reusable as input for the re-architecture.
- The codex review chains in this session's transcript identified real correctness invariants (binary semaphore cardinality, SYNC_FD copy transference, same-queue ordering guarantees) that the new spec must respect.
- The phasing skeleton (P0 probe → scaffolding → migration → cleanup → flicker re-test) is a reasonable shape for any re-architecture plan.

Do not reuse:

- The (slot, output) semaphore pool design — preserves the wrong contract.
- Tier 1b compositor descriptor ring — symptom of trying to keep `CompositorPipeline` unchanged.
- Catch-up path / per-output dirty / preflight C(F) — all artefacts of the surgical approach.
- The legacy dispatch helper — would not exist in a re-architecture (no incremental rollout to defend against).
