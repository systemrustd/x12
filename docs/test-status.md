# Test status — latest numbers

Snapshot of the current xts5 (X Test Suite) and rendercheck (RENDER
smoke) pass rates against the ynest backend on master.

For run-by-run history, debugging notes, and the breakdown of *why*
each scenario sits where it does, see
[`xts-baseline.md`](xts-baseline.md). This file is the headline only.

How to reproduce: see the "How to reproduce" section of
`xts-baseline.md`.

## xts5 — last full run 2026-05-07 (master, post-bucket3)

| scenario  | cases | tests | PASS | FAIL | UNRES | UNTST | UNSUP |
|-----------|------:|------:|-----:|-----:|------:|------:|------:|
| Xproto    |   122 |   389 |  337 |   25 |     7 |     0 |     0 |
| Xlib3     |   109 |   162 |  110 |   17 |     3 |    25 |     6 |
| Xlib4     |    29 |   324 |   89 |  197 |     5 |    17 |    11 |
| Xlib5     |    15 |    84 |   52 |   25 |     0 |     5 |     2 |
| Xlib6     |     8 |    50 |    4 |   17 |     0 |    29 |     0 |
| Xlib7     |    58 |   172 |   82 |   31 |     1 |    13 |    45 |
| Xlib8     |    29 |   165 |   46 |   73 |    14 |    22 |    10 |
| Xlib9     |    46 |  1472 |  219 |  606 |   388 |    33 |    23 |
| Xlib10    |    23 |    95 |   14 |   39 |     5 |    36 |     1 |
| Xlib11    |    33 |   195 |   23 |   99 |     2 |     4 |    24 |
| Xlib12    |    25 |   130 |   81 |   14 |     4 |    13 |     2 |
| Xlib13    |    32 |   269 |   62 |  158 |    34 |     9 |     3 |
| Xlib14    |    45 |    58 |   19 |   34 |     0 |     5 |     0 |
| Xlib15    |    45 |   159 |  122 |    4 |     0 |    33 |     0 |
| Xlib16    |    30 |   105 |   82 |    0 |     0 |    22 |     1 |
| Xlib17    |    55 |   131 |   85 |   12 |     9 |    19 |     0 |
| ShapeExt  |    11 |    11 |   11 |    0 |     0 |     0 |     0 |
| **total** |       |       | **1438** | 1351 |   472 |   285 |   128 |

Coverage breakdown:
- Xproto: 337
- Xlib3: 110
- Xlib4–17: 980
- ShapeExt: 11
- **= 1438 PASS** out of ~3674 tests across 715 cases.

XI / XIproto suites complete cleanly with all UNTST due to 0-device
advertisement; not counted above.

yserver/KMS: not yet measured (xts requires a host X server; would
need a `vng`-internal xts harness or building xts inside the vng
guest).

## yserver / KMS — first full sweep (bare-metal, 2026-05-10)

First end-to-end run on real hardware (Venus-capable GPU, 600 s
per-scenario budget). Captured against master + the
`composite_and_flip` dirty gate (Cause 1 perf fix). Cause 2 (per-RENDER
op `vkQueueWaitIdle`) is still in place; it dominates the throughput on
the heavy scenarios — Xlib9 truncates at 600 s, composite/cacomposite
in rendercheck both INCOMPLETE for the same reason.

| scenario  | cases | tests | PASS | FAIL | UNRES | UNTST | UNSUP | Δ vs ynest |
|-----------|------:|------:|-----:|-----:|------:|------:|------:|-----------:|
| Xproto    |   122 |   389 |  306 |    4 |    58 |    19 |     2 |        -31 |
| Xlib3 †   |  ~109 |  ~162 | ~100 |   25 |     5 |    19 |     6 |        -10 |
| Xlib4     |    29 |   324 |  102 |  184 |     5 |    17 |    11 |        +13 |
| Xlib5     |    15 |    84 |   52 |   25 |     0 |     5 |     2 |          0 |
| Xlib6     |     8 |    50 |    4 |   17 |     0 |    29 |     0 |          0 |
| Xlib7     |    58 |   172 |   82 |   31 |     1 |    13 |    45 |          0 |
| Xlib8     |    29 |   165 |   51 |   67 |    14 |    22 |    10 |         +5 |
| Xlib9 ‡   |    31 |  1333 |  135 |  915 |    62 |    21 |    22 |        -84 |
| Xlib10    |    23 |    95 |   15 |   38 |     5 |    36 |     1 |         +1 |
| Xlib11    |    33 |   195 |   30 |   92 |     2 |     4 |    24 |         +7 |
| Xlib12    |    27 |   138 |   89 |   15 |     4 |    16 |     2 |         +8 |
| Xlib13    |    32 |   269 |   64 |  155 |    35 |     9 |     3 |         +2 |
| Xlib14    |    45 |    58 |    9 |   40 |     4 |     5 |     0 |        -10 |
| Xlib15    |    45 |   159 |  121 |    5 |     0 |    33 |     0 |         -1 |
| Xlib16    |    30 |   105 |   82 |    0 |     0 |    22 |     1 |          0 |
| Xlib17    |    55 |   131 |   91 |   18 |     2 |    20 |     0 |         +6 |
| ShapeExt  |    11 |    11 |   10 |    1 |     0 |     0 |     0 |         -1 |
| **total** |       |       | **~1343** |   |       |       |       |        -95 |

† `xts-report` does not produce a summary table for this journal;
counts are from `awk -F'|' /PASS|FAIL|.../` on the raw outcomes. ynest
column is per-purpose, so the totals are roughly comparable.
‡ TIMEOUT at 600 s — 31 of 46 cases ran (vs ynest 46/46). At a longer
budget the projected PASS count is ≈ 200 (from the per-case rate).

Sweep wall time end-to-end: 39 minutes (12:04 → 12:43).

## rendercheck — yserver / KMS, bare-metal 2026-05-10 (rendercheck 1.6, 600 s/test)

| test        | pass | total | status |
|-------------|-----:|------:|--------|
| fill        |   48 |    48 | OK |
| dcoords     |    2 |     2 | OK |
| scoords     |    1 |     1 | OK |
| mcoords     |    1 |     1 | OK |
| tscoords    |    2 |     2 | OK |
| tmcoords    |    2 |     2 | OK |
| blend       |    4 |     4 | OK |
| composite   |    ? |     ? | INCOMPLETE (rc=124, 600 s budget) |
| cacomposite |    ? |     ? | INCOMPLETE (rc=124, 600 s budget) |
| gradients   | 3649 |  3649 | finished at the cap (rc=124 fired right at exit) |
| repeat      |  304 |   304 | OK |
| triangles   |  456 |   456 | OK |
| bug7366     |    1 |     1 | OK |
| **total**   | **4470** | **4470** | 2 incomplete |

vs ynest 4478/4478. The 8-case gap is composite + cacomposite
TIMEOUTing — Cause 2 batching would close this.

## rendercheck — last full run 2026-05-07 (rendercheck 1.6, ynest, 600 s/test)

| test        | pass | total |
|-------------|-----:|------:|
| fill        |   48 |    48 |
| dcoords     |    2 |     2 |
| scoords     |    1 |     1 |
| mcoords     |    1 |     1 |
| tscoords    |    2 |     2 |
| tmcoords    |    2 |     2 |
| blend       |    4 |     4 |
| composite   |    4 |     4 |
| cacomposite |    4 |     4 |
| gradients   | 3649 |  3649 |
| repeat      |  304 |   304 |
| triangles   |  456 |   456 |
| bug7366     |    1 |     1 |
| **total**   | **4478** | **4478** |

100 % PASS on rendercheck 1.6 default suite.

> Use rendercheck ≥ 1.6. 1.5 has a bug in `gradients::render_to_gradient_test` that
> trips even against the host X server.
