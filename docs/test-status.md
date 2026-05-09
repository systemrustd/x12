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

## yserver / KMS — Phase 4.1 parity baseline (master, 2026-05-08)

Captured on a separate machine ahead of the Phase 4.1 Vulkan rewrite,
to anchor the parity bar (design §4: ±5 PASS on xts5,
match-or-beat on rendercheck) for sub-phase 4.1.5's final gate.

| scenario  | PASS | total | source |
|-----------|-----:|------:|--------|
| Xproto    |  358 |   389 | external run, 2026-05-08 |

Additional scenarios + rendercheck numbers will land here as they
arrive.

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
