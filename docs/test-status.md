# Test status — latest numbers

Snapshot of the current xts5 (X Test Suite) and rendercheck (RENDER
smoke) pass rates. This file is the headline only; run-by-run history
and debugging notes live in [`xts-baseline.md`](xts-baseline.md) and
`status.md`.

## xts5 — full run #2, yserver/KMS bare-metal (M2 MBP, 2026-06-05)

`just xts-yserver-hw all 21600` — all 1078 test cases in 54 minutes,
zero crashes, zero hangs. Journal + summary:
`xts/results/2026-06-05-13:20:07/`.

**3224 / 5987 test purposes PASS (53.9%)** — up from 2784 (46.5%) on
2026-06-04, driven by the Xlib9 drawing/GetImage fixes
(176 → 602 PASS, +426).

| scenario  | cases | tests | PASS | FAIL | UNRES | UNTST | UNSUP | NOTIU | Δ PASS |
|-----------|------:|------:|-----:|-----:|------:|------:|------:|------:|-------:|
| Xproto    |   122 |   389 |  358 |    7 |     3 |    19 |     2 |     0 |      0 |
| Xlib3     |   109 |   162 |  105 |   18 |     5 |    27 |     6 |     1 |      0 |
| Xlib4     |    29 |   324 |  101 |  183 |     7 |    17 |    11 |     5 |     −7 |
| Xlib5     |    15 |    84 |   59 |   18 |     0 |     5 |     2 |     0 |      0 |
| Xlib6     |     8 |    50 |    6 |   15 |     0 |    29 |     0 |     0 |     +2 |
| Xlib7     |    58 |   172 |   83 |   31 |     0 |    13 |    45 |     0 |      0 |
| Xlib8     |    29 |   165 |   69 |   58 |     6 |    22 |    10 |     0 |     +9 |
| Xlib9     |    46 |  1472 |  602 |  548 |    62 |    33 |    23 |   201 |   +426 |
| Xlib10    |    23 |    95 |   25 |   29 |     5 |    35 |     1 |     0 |      0 |
| Xlib11    |    33 |   195 |   50 |   72 |     2 |     4 |    24 |    43 |     +1 |
| Xlib12    |    27 |   138 |   91 |   14 |     4 |    15 |     2 |    12 |      0 |
| Xlib13    |    32 |   269 |   79 |  139 |    36 |     9 |     3 |     3 |      0 |
| Xlib14    |    45 |    58 |   19 |   34 |     0 |     5 |     0 |     0 |      0 |
| Xlib15    |    45 |   159 |  121 |    5 |     0 |    33 |     0 |     0 |      0 |
| Xlib16    |    30 |   105 |   82 |    0 |     0 |    22 |     1 |     0 |      0 |
| Xlib17    |    55 |   131 |   99 |   11 |     0 |    21 |     0 |     0 |     +5 |
| Xopen     |     8 |   127 |  122 |    3 |     0 |     0 |     2 |     0 |      0 |
| Xt3       |    21 |    73 |   73 |    0 |     0 |     0 |     0 |     0 |      0 |
| Xt4       |    33 |   192 |   94 |    0 |     0 |    98 |     0 |     0 |      0 |
| Xt5       |    10 |    69 |   26 |    0 |     0 |    41 |     0 |     0 |      0 |
| Xt6       |     7 |    71 |   67 |    4 |     0 |     0 |     0 |     0 |      0 |
| Xt7       |    11 |   106 |   96 |    1 |     0 |     6 |     0 |     3 |      0 |
| Xt8       |     7 |    43 |   35 |    4 |     0 |     4 |     0 |     0 |      0 |
| Xt9       |    33 |   189 |  125 |    0 |     7 |    55 |     2 |     0 |      0 |
| Xt10      |     8 |    17 |   16 |    0 |     0 |     1 |     0 |     0 |      0 |
| Xt11      |    58 |   285 |  245 |    4 |     0 |    34 |     0 |     0 |      0 |
| Xt12      |    22 |    67 |   55 |    0 |     1 |    11 |     0 |     0 |      0 |
| Xt13      |    39 |   178 |  125 |    6 |     0 |    47 |     0 |     0 |     +1 |
| Xt14      |     2 |    18 |   18 |    0 |     0 |     0 |     0 |     0 |      0 |
| Xt15      |     1 |     2 |    0 |    0 |     0 |     0 |     2 |     0 |      0 |
| XtC       |    29 |   147 |   88 |    1 |     1 |    56 |     1 |     0 |     +2 |
| XtE       |     1 |     1 |    1 |    0 |     0 |     0 |     0 |     0 |      0 |
| ShapeExt  |    11 |    11 |   11 |    0 |     0 |     0 |     0 |     0 |     +1 |
| XI        |    36 |   316 |   37 |  200 |    18 |    53 |     2 |     5 |      0 |
| XIproto † |    35 |   107 |   41 |   29 |     7 |    15 |     0 |     0 |      0 |
| **total** | **1078** | **5987** | **3224** | **1434** | **164** | **730** | **139** | **273** | **+440** |

ShapeExt is now fully clean (11/11).

† plus 15 ABORTs at the run tail: client resource-ID bases are never
recycled and the 32-bit base overflows at client 4096, so the last
XIproto TCMs couldn't connect ("Could not open display"). Tracked in
status.md.

**Regression to investigate:** Xlib4 dropped 108 → 101 PASS
(FAIL 176 → 183) — likely fallout from the new paint-op spec-error
emission (`12446c5`) or the XFillRectangle Vulkan-path change
(`a2d5480`).

Largest FAIL buckets / next targets:
1. **Xlib9 (548)** — remaining drawing/GetImage content semantics.
2. **XI (200)** — X Input extension device semantics (untouched).
3. **Xlib4 (183)** — window attributes (incl. the −7 regression).
4. **Xlib13 (139)** — WM/visibility semantics.
5. **Xlib11 (72)** — residual grab semantics (root-down passive-grab
   ancestor search + NotifyGrab crossings; one over-delivery).

Previous full run (2026-06-04, first ever to complete):
2784/5987 PASS (46.5%) — `xts/results/2026-06-04-15:48:44/`.

## rendercheck — bare-metal 2026-06-04, rendercheck 1.6, 900 s/test

| category    |  PASS | TOTAL |
|-------------|------:|------:|
| fill        |    64 |    64 |
| dcoords     |     2 |     2 |
| scoords     |     1 |     1 |
| mcoords     |     1 |     1 |
| tscoords    |     2 |     2 |
| tmcoords    |     2 |     2 |
| blend       |     5 |     5 |
| composite   |     5 |     5 |
| cacomposite |     5 |     5 |
| gradients   |  6081 |  6081 |
| repeat      |   380 |   380 |
| triangles   |   570 |   570 |
| bug7366     |     1 |     1 |
| **total**   | **7119** | **7119** |

**100% pass.**

> Use rendercheck ≥ 1.6. Version 1.5 has a bug in
> `gradients::render_to_gradient_test` that trips even against the
> host X server.
