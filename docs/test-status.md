# Test status — latest numbers

Snapshot of the current xts5 (X Test Suite) and rendercheck (RENDER
smoke) pass rates. This file is the headline only; run-by-run history
and debugging notes live in [`xts-baseline.md`](xts-baseline.md) and
`status.md`.

## xts5 — first COMPLETE full run, yserver/KMS bare-metal (M2 MBP, 2026-06-04)

`just xts-yserver-hw all 21600` — **all 1078 test cases finished in
53 minutes with zero crashes and zero hangs** 
Journal + summary: `xts/results/2026-06-04-15:48:44/`.


| scenario  | cases | tests | PASS | FAIL | UNRES | UNTST | UNSUP | NOTIU |
|-----------|------:|------:|-----:|-----:|------:|------:|------:|------:|
| Xproto    |   122 |   389 |  358 |    7 |     3 |    19 |     2 |     0 |
| Xlib3     |   109 |   162 |  105 |   18 |     5 |    27 |     6 |     1 |
| Xlib4     |    29 |   324 |  108 |  176 |     7 |    17 |    11 |     5 |
| Xlib5     |    15 |    84 |   59 |   18 |     0 |     5 |     2 |     0 |
| Xlib6     |     8 |    50 |    4 |   17 |     0 |    29 |     0 |     0 |
| Xlib7     |    58 |   172 |   83 |   31 |     0 |    13 |    45 |     0 |
| Xlib8     |    29 |   165 |   60 |   63 |    10 |    22 |    10 |     0 |
| Xlib9     |    46 |  1472 |  176 |  974 |    63 |    33 |    23 |   201 |
| Xlib10    |    23 |    95 |   25 |   29 |     5 |    35 |     1 |     0 |
| Xlib11    |    33 |   195 |   49 |   73 |     2 |     4 |    24 |    43 |
| Xlib12    |    27 |   138 |   91 |   14 |     4 |    15 |     2 |    12 |
| Xlib13    |    32 |   269 |   79 |  140 |    35 |     9 |     3 |     3 |
| Xlib14    |    45 |    58 |   19 |   34 |     0 |     5 |     0 |     0 |
| Xlib15    |    45 |   159 |  121 |    5 |     0 |    33 |     0 |     0 |
| Xlib16    |    30 |   105 |   82 |    0 |     0 |    22 |     1 |     0 |
| Xlib17    |    55 |   131 |   94 |   16 |     0 |    21 |     0 |     0 |
| Xopen     |     8 |   127 |  122 |    3 |     0 |     0 |     2 |     0 |
| Xt3       |    21 |    73 |   73 |    0 |     0 |     0 |     0 |     0 |
| Xt4       |    33 |   192 |   94 |    0 |     0 |    98 |     0 |     0 |
| Xt5       |    10 |    69 |   26 |    0 |     0 |    41 |     0 |     0 |
| Xt6       |     7 |    71 |   67 |    4 |     0 |     0 |     0 |     0 |
| Xt7       |    11 |   106 |   96 |    1 |     0 |     6 |     0 |     3 |
| Xt8       |     7 |    43 |   35 |    4 |     0 |     4 |     0 |     0 |
| Xt9       |    33 |   189 |  125 |    0 |     7 |    55 |     2 |     0 |
| Xt10      |     8 |    17 |   16 |    0 |     0 |     1 |     0 |     0 |
| Xt11      |    58 |   285 |  245 |    4 |     0 |    34 |     0 |     0 |
| Xt12      |    22 |    67 |   55 |    0 |     1 |    11 |     0 |     0 |
| Xt13      |    39 |   178 |  124 |    7 |     0 |    47 |     0 |     0 |
| Xt14      |     2 |    18 |   18 |    0 |     0 |     0 |     0 |     0 |
| Xt15      |     1 |     2 |    0 |    0 |     0 |     0 |     2 |     0 |
| XtC       |    29 |   147 |   86 |    3 |     1 |    56 |     1 |     0 |
| XtE       |     1 |     1 |    1 |    0 |     0 |     0 |     0 |     0 |
| ShapeExt  |    11 |    11 |   10 |    1 |     0 |     0 |     0 |     0 |
| XI        |    36 |   316 |   37 |  200 |    18 |    53 |     2 |     5 |
| XIproto † |    35 |   107 |   41 |   29 |     7 |    15 |     0 |     0 |
| **total** | **1078** | **5987** | **2784** | **1871** | **168** | **730** | **139** | **273** |

**2784 / 5987 test purposes PASS (46.5%).** WARN: 4 (Xlib9 ×2,
Xt11 ×2); FIP/UNIN: 0.

† plus 15 ABORTs at the run tail: client resource-ID bases are never
recycled and the 32-bit base overflows at client 4096, so the last
XIproto TCMs couldn't connect ("Could not open display"). Tracked in
status.md.

Largest FAIL buckets / next targets:
1. **Xlib9 (974)** — drawing/GetImage content: depth-4/15/16 pixmap
   formats are advertised in the setup but the v2 engine can't read
   them back (zeroed fallback data); plane-mask content semantics.
2. **XI (200)** — X Input extension device semantics.
3. **Xlib4 (176)** — window attributes.
4. **Xlib13 (140)** — WM/visibility semantics.
5. **Xlib11 (73)** — residual grab semantics (root-down passive-grab
   ancestor search + NotifyGrab crossings; one over-delivery).

Comparison points: the 2026-05-10 bare-metal sweep (Xlib-only, Xlib9
truncated at 600 s) totalled ~1343 PASS; the 2026-05-07 ynest run
totalled 1438 PASS over a smaller scenario set. This run is the first
with the Xt/Xopen/XI sections included and the first to complete
end-to-end.

## rendercheck

A fresh rendercheck run against yserver/KMS is underway on another
machine (2026-06-04); numbers land here when it finishes.

Last known (bare-metal 2026-05-10, rendercheck 1.6, 600 s/test):
4470/4470 with composite + cacomposite INCOMPLETE on the 600 s
budget; ynest reference 4478/4478 (100%).

> Use rendercheck ≥ 1.6. 1.5 has a bug in
> `gradients::render_to_gradient_test` that trips even against the
> host X server.
