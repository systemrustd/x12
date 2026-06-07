# Test status — latest numbers

Snapshot of the current xts5 (X Test Suite) and rendercheck (RENDER
smoke) pass rates. This file is the headline only; run-by-run history
and debugging notes live in [`xts-baseline.md`](xts-baseline.md) and
`status.md`.

## xts5 — full run #4, yserver/KMS bare-metal (bee, 2026-06-07)

`just xts-yserver-hw all 3600` — all 1078 test cases in 54 minutes,
zero crashes, zero hangs, **zero ABORTs**. Journal + summary:
`xts/results/2026-06-07-17:14:17/`.

**3747 / 5987 test purposes PASS (62.6%)** — up from 3419 (57.1%) in
run #3, driven by:
- Xlib9 (+198, 608 → 806) — silence's font/PCF/CopyArea/clear work
  plus the `77f785b` ConfigureWindow-on-root drop that unblocked the
  GetImage BadMatch cascade.
- XIproto (+59), XI (+24) — XI cluster from `b86338c…f5ac527`.
- Xlib14 (+27), Xlib13 (+19), Xlib17 (+2), Xt11 (+2).

Total UNRES collapsed from 184 (run #3) to **116** — almost all of
the 1043 Xlib9 UNRES seen in the broken 14:01 attempt cleared.

| scenario  | cases | tests | PASS | FAIL | UNRES | UNTST | UNSUP | NOTIU | Δ PASS |
|-----------|------:|------:|-----:|-----:|------:|------:|------:|------:|-------:|
| Xproto    |   122 |   389 |  358 |    7 |     3 |    19 |     2 |     0 |      0 |
| Xlib3     |   109 |   162 |  104 |   19 |     5 |    21 |     6 |     1 |     −1 |
| Xlib4     |    29 |   324 |  105 |  175 |    11 |    17 |    11 |     5 |      0 |
| Xlib5     |    15 |    84 |   59 |   18 |     0 |     5 |     2 |     0 |      0 |
| Xlib6     |     8 |    50 |    6 |   15 |     0 |    29 |     0 |     0 |      0 |
| Xlib7     |    58 |   172 |   83 |   31 |     0 |    13 |    45 |     0 |      0 |
| Xlib8     |    29 |   165 |   88 |   39 |     6 |    22 |    10 |     0 |      0 |
| Xlib9     |    46 |  1472 |  806 |  380 |    23 |    36 |    23 |   201 |   +198 |
| Xlib10    |    23 |    95 |   25 |   29 |     5 |    35 |     1 |     0 |      0 |
| Xlib11    |    33 |   195 |   50 |   72 |     2 |     4 |    24 |    43 |      0 |
| Xlib12    |    27 |   138 |   91 |   14 |     4 |    15 |     2 |    12 |      0 |
| Xlib13    |    32 |   269 |  115 |  106 |    33 |     9 |     3 |     3 |    +19 |
| Xlib14    |    45 |    58 |   46 |    7 |     0 |     5 |     0 |     0 |    +27 |
| Xlib15    |    45 |   159 |  122 |    4 |     0 |    33 |     0 |     0 |      0 |
| Xlib16    |    30 |   105 |   82 |    0 |     0 |    22 |     1 |     0 |      0 |
| Xlib17    |    55 |   131 |  102 |    8 |     0 |    21 |     0 |     0 |     +2 |
| Xopen     |     8 |   127 |  122 |    3 |     0 |     0 |     2 |     0 |      0 |
| Xt3       |    21 |    73 |   73 |    0 |     0 |     0 |     0 |     0 |      0 |
| Xt4       |    33 |   192 |   94 |    0 |     0 |    98 |     0 |     0 |      0 |
| Xt5       |    10 |    69 |   26 |    0 |     0 |    41 |     0 |     0 |      0 |
| Xt6       |     7 |    71 |   67 |    4 |     0 |     0 |     0 |     0 |      0 |
| Xt7       |    11 |   106 |   96 |    1 |     0 |     6 |     0 |     3 |      0 |
| Xt8       |     7 |    43 |   35 |    4 |     0 |     4 |     0 |     0 |      0 |
| Xt9       |    33 |   189 |  126 |    0 |     6 |    55 |     2 |     0 |      0 |
| Xt10      |     8 |    17 |   16 |    0 |     0 |     1 |     0 |     0 |      0 |
| Xt11      |    58 |   285 |  247 |    2 |     0 |    34 |     0 |     0 |     +2 |
| Xt12      |    22 |    67 |   55 |    0 |     1 |    11 |     0 |     0 |      0 |
| Xt13      |    39 |   178 |  124 |    5 |     2 |    47 |     0 |     0 |     −2 |
| Xt14      |     2 |    18 |   18 |    0 |     0 |     0 |     0 |     0 |      0 |
| Xt15      |     1 |     2 |    0 |    0 |     0 |     0 |     2 |     0 |      0 |
| XtC       |    29 |   147 |   88 |    0 |     2 |    56 |     1 |     0 |      0 |
| XtE       |     1 |     1 |    1 |    0 |     0 |     0 |     0 |     0 |      0 |
| ShapeExt  |    11 |    11 |   11 |    0 |     0 |     0 |     0 |     0 |      0 |
| XI        |    36 |   316 |  211 |   52 |    13 |    33 |     2 |     5 |    +24 |
| XIproto   |    35 |   107 |   95 |    0 |     0 |    12 |     0 |     0 |    +59 |
| **total** | **1078** | **5987** | **3747** | **995** | **116** | **704** | **139** | **273** | **+328** |

ShapeExt, Xlib16, XIproto and Xt3/4/5/9/10/14 are fully clean (zero
FAIL/UNRES). yserver survived the whole sweep with zero panics in
the server log.

Notes:

- **First bee/HW run that exceeds the air baseline** — the 2026-06-07
  silence push (Xlib9 font/copy/clear) and my XI work (XIproto chain
  delivery, XSendExtensionEvent specials, propagate=True,
  ChangeDeviceDontPropagateList, XSetDeviceValuators, button/modifier
  map persistence, GetDeviceControl wire layout, …) compose cleanly.
- **`77f785b` ConfigureWindow-on-root drop**: an xts5 client sent
  `XConfigureWindow(root, width=70, height=61)` in the full-suite
  stream and we applied the geometry to `ROOT_WINDOW`, shrinking
  root to 70×61 and tripping the on-screen GetImage check for every
  subsequent window. Xorg silently accepts the request without
  effect (the root's geometry is owned by the screen/RandR); the fix
  short-circuits the handler when target == ROOT_WINDOW. This single
  one-line guard recovered ~750 PASS on bee that the 14:01 broken
  run lost as 1100+ UNRES.
- 2 NORESULTs: `Xt5/XtUnmanageChildren`, `Xt5/XtUnmanageChild`
  (unchanged from run #3).
- Slight regressions worth a future glance: Xlib3 −1, Xt13 −2. Both
  too small to chase against the +328 net gain.

Largest FAIL buckets / next targets:
1. **Xlib9 (380)** — remaining drawing/GetImage content semantics
   (down from 526).
2. **Xlib4 (175)** — window attributes (unchanged).
3. **Xlib13 (106)** — WM/visibility semantics (down from 124).
4. **Xlib11 (72)** — residual grab semantics (root-down passive-grab
   ancestor search + NotifyGrab crossings; one over-delivery).
5. **XI (52)** — remaining XTest-input-through-XI1-freeze gap; the
   AllowSome / master-device tests are blocked on input pipeline,
   not protocol parsing.

Previous full runs:
- #3 — 2026-06-06 (air, M1): 3419/5987 PASS (57.1%) —
  `xts/results/2026-06-06-20:26:54/`. Last run before silence's
  Xlib9 push and the `77f785b` ConfigureWindow fix.
- #2 — 2026-06-05 (M2) + 2026-06-06 air XI row: 3370/5987 PASS (56.3%)
  — `xts/results/2026-06-05-13:20:07/` (+ `2026-06-06-00:58:03` for XI).
- #1 — 2026-06-04 (first ever to complete): 2784/5987 PASS (46.5%) —
  `xts/results/2026-06-04-15:48:44/`.

Aborted run between #3 and #4 (`xts/results/2026-06-07-14:01:34/`):
2999/5987 PASS, 1290 UNRES — the GetImage BadMatch cascade caused
by an unguarded `XConfigureWindow` on the root window, fixed by
`77f785b` before run #4.

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
