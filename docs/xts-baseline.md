# X Test Suite (xts5) baseline

First run of the X.Org X Test Suite against `ynest`, captured 2026-05-06
right after XTEST landed.

## How to reproduce

1. Build xts5 against the local checkout at `/home/jos/Projects/xts`
   (the existing meson build under `build/`).
2. From the yserver checkout: `just xts-ynest scenario=Xproto` — this
   builds release `ynest`, boots it on `:99` with a 1024×768 host
   container, runs `tools/xts-run.sh` (which invokes `xts/check.sh`),
   tears down ynest on exit.
3. Result tree lands in `/home/jos/Projects/xts/results/<timestamp>/`
   with a `summary` file alongside the raw `journal`.
4. Re-run with a different scenario by overriding the recipe arg:
   `just xts-ynest scenario=Xlib3`.

## Result columns

The xts-report layout: `CASES | TESTS | PASS | UNSUP | UNTST | NOTIU |
WARN | FIP | FAIL | UNRES | UNIN | ABORT`. CASES is test cases; TESTS
is "test purposes" (each case has 1+ purposes — closer to the unit
count we care about). Stable success is PASS; everything else is some
flavour of not-passing.

## Run history (ynest, `Xproto` scenario)

| Date       | PASS | FAIL | UNRES | UNIN | NORES | Notes |
|------------|-----:|-----:|------:|-----:|------:|-------|
| 2026-05-06 |    1 |  210 |   160 |   11 |     7 | First run after XTEST landed. |
| 2026-05-06 |    1 |   74 |   296 |   11 |     7 | After `BadLength` enforcement at the top of `process_request` (per-opcode length table for opcodes 1–127). 136 tests moved FAIL → UNRES: each AllocColor-style probe runs 2 native + 2 BE sub-checks; previously the native sub-checks FAILed (BadLength not raised) so the test result was FAIL; now those sub-checks pass but the BE sub-checks still UNRES on connection rejection, leaving the test as UNRES. Real BadLength progress; PASS count gated on big-endian client support. |
| 2026-05-06 |   26 |   91 |   252 |    0 |     0 | After Phases 0+A+B+C+D+D1 of BE client support (request reader, setup, errors, replies, events, shared `wire_swap` module). BE clients now complete setup, receive replies/errors/events in their byte order. Phase D2 (raw event templates / SendEvent re-encoding) and Phase E (inbound request body swap) still pending — explains the remaining 252 UNRES. |
| 2026-05-06 |  195 |   78 |    97 |    0 |     0 | Phase E lands — per-opcode `request_swap_table` covers ~70 core opcodes; inbound BE request bodies are byte-swapped in place at the reader thread before dispatch. **PASS 26 → 195** (+169). UNRES 252 → 97 (-155). The remaining 97 UNRES are mostly tests of opcodes not yet covered by the swap table or of BE behaviour blocked by other gaps (Phase D2 / variable-length BadLength). |
| 2026-05-06 |  229 |   40 |    99 |    0 |     0 | Phases D2 (raw event templates re-encode per recipient) + F (content-aware `BadLength` for variable-length opcodes). **PASS 195 → 229** (+34); FAIL 78 → 40 (-38). End-to-end **PASS 1 → 229** for the full BE branch. |
| 2026-05-06 |  337 |   25 |     7 |    0 |     0 | `xproto` branch — residual fixes on top of BE: 6 missing reply implementations (GetMotionEvents/GetFontPath/GetKeyboardControl/GetPointerControl/GetScreenSaver/ListInstalledColormaps), MappingNotify fanout on Set{Pointer,Modifier}Mapping (event before reply per spec), AllocColorCells/AllocColorPlanes BadAlloc on TrueColor, StoreColors/StoreNamedColor BadAccess on read-only colormap, BadValue mask validation (CW/GC/configure/keyboard), BadIDChoice on duplicate or out-of-range XIDs (CreateColormap/CopyColormapAndFree/CreateCursor/OpenFont), MapWindow self-Expose + parent-tracked Viewable/Unviewable, ClearArea Expose, CopyArea/CopyPlane GraphicsExpose, PolyPoint/Line/Segment/Rectangle/Arc content-shape BadLength, ChangeProperty swap-table fix (format byte at body[12] is u8, not u32), max_request_length enforcement (256K units BIG-REQUESTS, u16::MAX otherwise), error-resilience on backend draw failures. **PASS 229 → 337** (+108). |

## Run history (ynest, `ShapeExt` scenario)

The SHAPE extension scenario is small (11 tests / 11 cases — one purpose
per X function exposed by `libXext`'s SHAPE binding).

| Date       | PASS | FAIL | Notes |
|------------|-----:|-----:|-------|
| 2026-05-07 |    5 |    6 | First ShapeExt run. 5 of 6 FAILs are `XShape{OffsetShape,CombineMask,CombineRectangles,CombineRegion,GetRectangles}` returning `ordering=Unsorted` instead of `YXBanded` from `GetRectangles`. The 6th is `XShapeQueryExtents` ignoring `border_width` in the default unshaped bounding region. |
| 2026-05-07 |   11 |    0 | `GetRectangles` reply now reports `ORDERING_YX_BANDED`; `normalize_region_rects` sorts by `(y, x)` so the YXBanded claim is honest for non-overlapping bands; `default_shape_rect` is kind-aware (`KIND_BOUNDING` → `(-bw, -bw, w+2bw, h+2bw)`, `KIND_CLIP`/`KIND_INPUT` → `(0, 0, w, h)`). **PASS 5 → 11**. |

## Run history (ynest, `Xlib3` scenario)

| Date       | PASS | FAIL | UNRES | UNTST | UNSUP | Notes |
|------------|-----:|-----:|------:|------:|------:|-------|
| 2026-05-06 |   96 |   31 |     3 |    25 |     6 | First Xlib3 run (162 tests / 109 cases) on top of all the Xproto fixes. |
| 2026-05-06 |  110 |   17 |     3 |    25 |     6 | `xts-xlib3` branch — vendor string ("The X.Org Foundation"), release_number (12_401_011), 7 pixmap formats (added depth=15, depth=16), screen mm dimensions (677×381 reference), SetCloseDownMode validates header.data ∈ {0,1,2}. **PASS 96 → 110** (+14). Remaining FAILs are mostly XCloseDisplay subtests requiring full resource-lifecycle accounting and a couple of colormap-install limit tests. |

(Total tests: 122 cases / 389 purposes throughout.)

The full scenario completes in ~4 minutes. **The headline is that
ynest stays up through the entire battery** — no panics, no hangs,
no socket disconnects beyond what the tests themselves induced. From
a "does the server survive xts" angle, that's the win.

The sole PASS is `OpenDisplay 2`. The remaining outcome is masked by
the structural bugs below. The first row was the baseline cause;
struck-through rows are fixed but their tests still UNRES because of
a remaining gate further up.

| Failure mode | REPORT lines | Cause |
|---|---|---|
| big-endian client connection rejected | 483 | xts opens a second connection in reversed byte sex to test byte-swap handling; ynest's setup handshake refuses (by design — see `setup_thread.rs`). Until this is fixed, every test that runs both native+BE sub-checks UNRES'es regardless of native correctness. |
| ~~`BadLength` not raised~~ | ~~433 (250 + 183)~~ | **Fixed 2026-05-06.** Per-opcode length contract enforced for opcodes 1–127 in `process_request`; under-length and (for fixed opcodes) over-length headers now reply `BadLength`. |
| `Expose` not delivered | 131 | Specific Expose-generation gaps (~30-ish unique tests). |

The other ~200 individual FAILs are spread across grab semantics,
screen-saver state, error-code edge cases, etc.

## Quick-win path (in priority order)

1. ~~**`BadLength` enforcement.**~~ Done 2026-05-06; behaviour
   verified against all per-opcode under/over-length probes. The
   PASS count did not move because the same tests also probe
   reversed byte sex; see #2.
2. **Big-endian client byte-order at the wire reader.** Now the
   gating issue: with `BadLength` correct, ~136 tests UNRES purely
   because their second connection (BE) is refused. Implementation
   would need swap-tables for request bodies, replies, events, and
   the setup-success encoder. Larger surface but unblocks a clear
   set of tests.
3. **`Expose` correctness pass.** Smaller bucket; specific bugs
   rather than a single missing primitive.

## Known caveats

- The xts results dir lives outside the yserver tree
  (`/home/jos/Projects/xts/results/`). The `summary` from this
  baseline is checked in at `docs/xts-baseline-summary.txt` for
  reproducibility.
- `Xproto` is the most "protocol-shaped" scenario. Other categories
  (Xlib*, Xt*, XI, SHAPE) will have different failure profiles —
  Xt-suite tests will mostly UNRES because the toolkit's font /
  resource expectations diverge sharply from ynest's stubs.

## Not yet measured

- **`yserver` (KMS) baseline.** The KMS backend runs only inside
  `vng`, so running xts against it requires either building xts
  inside the guest's rootfs or tunnelling a guest DISPLAY to the
  host. Deferred — once the structural quick-wins land we expect
  KMS numbers to be lower than ynest (no `RENDER`-via-host fallback,
  fewer extension stubs), and the comparison is only interesting
  after those are fixed.

## rendercheck (RENDER smoke suite)

`rendercheck` (Arch package: `rendercheck`) is a separate suite for
the X RENDER extension. Wired up via `tools/rendercheck.sh` and the
`just rendercheck-ynest` recipe.

Default test list excludes `repeat` and `cacomposite` — both run every
operator against every format and exceed the per-test cap on ynest
because some operator paths hang (suspected: ynest's RENDER-via-host
forwarding doesn't terminate on a few op/format combinations, needs
a proper investigation).

Baseline 2026-05-07 (xts-followups merged to master, ynest on :99,
60s per-test timeout):

| test       | pass | total | status |
|------------|-----:|------:|--------|
| fill       |   30 |    30 | OK     |
| dcoords    |    2 |     2 | OK     |
| scoords    |    1 |     1 | OK     |
| mcoords    |    1 |     1 | OK     |
| tscoords   |    2 |     2 | OK     |
| tmcoords   |    2 |     2 | OK     |
| blend      |    4 |     4 | OK     |
| triangles  |  174 |   456 | FAIL — 282 ops produce dst=white where xts expects dst=black; suspect `Composite` operator dispatch on triangle paths |
| bug7366    |    1 |     1 | OK     |
| composite  |    — |     — | TIMEOUT @ 120s — investigate |
| gradients  |    — |     — | TIMEOUT @ 120s — investigate |
| repeat     |    — |     — | TIMEOUT (excluded by default) |
| cacomposite|    — |     — | TIMEOUT (excluded by default) |
| **total**  |  217 |   499 | (excludes timeouts) |

Host (`:0`, X.Org Foundation): every test passes — e.g. `fill`
160/160 vs ynest 30/30 because we advertise 3 picture formats and
the host advertises 15.

## Xlib4 – Xlib17 baselines

### 2026-05-07 (post-`9bcb9b0`, ROOT_WINDOW protected from DestroyWindow)

The original "first case hangs" pattern in seven of fourteen scenarios
turned out to be a single root cause: xts Xlib4 sends
`XDestroyWindow(root)` and our `destroy_window_inner` deleted root
from the resources table — so every subsequent client got
`BadWindow` on root queries. Fix is one early-return in
`destroy_window_inner`. Single-ynest sweep below uses the same
`tools/xts-xlib-sweep.sh` runner with no per-scenario restart.

| scenario | cases | tests | PASS | FAIL | UNRES | UNTST | UNSUP | Δ vs pre-fix PASS |
|----------|------:|------:|-----:|-----:|------:|------:|------:|:------------------|
| Xlib4    |    29 |   324 |   61 |  225 |     5 |    17 |    11 | 34 → 61 (and now runs all 29 cases) |
| Xlib5    |    15 |    84 |   48 |   26 |     3 |     5 |     2 | 0 → 48 |
| Xlib6    |     8 |    50 |    4 |   17 |     0 |    29 |     0 | 4 → 4 |
| Xlib7    |    58 |   172 |   81 |   31 |     2 |    13 |    45 | 6 → 81 |
| Xlib8    |    29 |   165 |   19 |  100 |    14 |    22 |    10 | 0 → 19 |
| Xlib9    |    46 |  1472 |  218 |  607 |   388 |    33 |    23 | 0 → 218 |
| Xlib10   |    23 |    95 |   10 |   43 |     5 |    36 |     1 | 0 → 10 |
| Xlib11   |    33 |   195 |   22 |  100 |     2 |     4 |    24 | 0 → 22 |
| Xlib12   |    25 |   130 |   81 |   14 |     4 |    13 |     2 | 59 → 81 |
| Xlib13   |    32 |   269 |   37 |  183 |    34 |     9 |     3 | 0 → 37 |
| Xlib14   |    45 |    58 |   19 |   34 |     0 |     5 |     0 | 1 → 19 |
| Xlib15   |    45 |   159 |  122 |    4 |     0 |    33 |     0 | 5 → 122 |
| Xlib16   |    30 |   105 |   82 |    0 |     0 |    22 |     1 | 82 → 82 |
| Xlib17   |    55 |   131 |   85 |   12 |     9 |    19 |     0 | 0 → 85 |
| **sum**  |       |       |**889** | 1396 |   466 |   260 |   122 | **191 → 889** |

Total xts coverage on master:
Xproto 337 + Xlib3 110 + Xlib4–17 889 + ShapeExt 11 = **1347 PASS**
(plus XI + XIproto suites complete cleanly with all UNTST due to
0-device advertisement).

### Top blockers driving the remaining FAIL

The "first-case hang" class is gone. The residual ~1.4k FAIL across
Xlib4-17 falls into a few buckets:

1. **`ChangeWindowAttributes` doesn't persist most fields.** The
   `Window` struct stores background-pixel / background-pixmap /
   override-redirect / cursor; CW value-mask bits for bit-gravity,
   win-gravity, backing-store, backing-planes, backing-pixel,
   save-under, colormap, do-not-propagate-mask are accepted but
   discarded. Tests that set then read fail with "got default,
   expected <set value>".
2. **Missing protocol errors on bad inputs.** `did not generate
   BadAccess / BadColor / BadCursor / BadMatch / BadPixmap /
   BadValue / BadWindow` is a recurring `REPORT:` line. Many
   handlers blindly succeed. Adding spec-required error returns
   would lift FAIL → PASS without functional changes.
3. **Pixel-mismatch in image / fill / area tests.** "A total of
   N out of 9000 pixels were bad" appears across the geometry
   tests — likely backing-pixel default propagation, plus Expose
   mishandling, plus border-pixel defaults.
4. **Expose-region tracking gaps.** Xlib9's 388 UNRES is mostly
   tests that issue a structural change (Destroy/Resize/Map/
   Configure) and time-out waiting for an `Expose` covering the
   newly-uncovered area. Need a proper expose-region tracker that
   fires on those transitions.

### Pre-fix snapshot (kept for context — single-ynest, no root
protection, no XKB minor 15)

| scenario | cases-ran | tests | PASS | note |
|----------|----------:|------:|-----:|------|
| Xlib4    |        10 |   175 |   34 | XDestroyWindow case 6 (Expose-after-destroy) hangs the rest |
| Xlib5    |         1 |    13 |    0 | first case hangs |
| Xlib6    |         5 |    42 |    4 | |
| Xlib7    |         6 |    45 |    6 | |
| Xlib8    |         1 |    25 |    0 | first case hangs |
| Xlib9    |         1 |    11 |    0 | first case hangs |
| Xlib10   |         4 |    16 |    0 | |
| Xlib11   |         1 |    12 |    0 | first case hangs |
| Xlib12   |        21 |   106 |   59 | mostly client-side event-queue helpers |
| Xlib13   |         1 |    29 |    0 | first case hangs |
| Xlib14   |         5 |     5 |    1 | |
| Xlib15   |         6 |    11 |    5 | |
| Xlib16   |        30 |   105 |   82 | Xrm — almost entirely client-side string mgmt |
| Xlib17   |         2 |     2 |    0 | |
| **sum**  |           |       |  191 | |

### How to reproduce

```sh
just xts-ynest scenario=Xlib4   # any Xlib<N>
```

For a multi-scenario sweep, the throwaway `tools/` script
`tools/xts-xlib-sweep.sh` (used to generate this baseline) drives each
scenario with a 240s overall budget

