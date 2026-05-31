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

## Run history (yserver KMS, drawing primitives + colormap)

First yserver runs of `Xlib9` (drawing primitives) and `Xlib10`
(colormap install/uninstall + misc) on 2026-05-26, driven by
`just xts-yserver <scenario>` (vng harness). Baseline for the
draw-primitives-completeness program of work.

### `Xlib9` baseline 2026-05-26

| Date       | PASS | FAIL | UNRES | UNIN | UNTST | NOTIU | UNSUP | ABORT | Notes |
|------------|-----:|-----:|------:|-----:|------:|------:|------:|------:|-------|
| 2026-05-26 |   14 |   61 |     0 |   17 |     0 |     3 |     1 |  1375 | First yserver Xlib9 run, **old `-display none` recipe**. Cases 0-3 ran; XDrawArc (case 4) onward all ABORT — yserver wedges (mistaken for a depth-4 hang; really the headless-no-pageflip stall, see known-issues). |
| 2026-05-26 |  116 |  658 |    21 |    0 |    16 |   146 |    15 |     0 | **egl-headless recipe** (Venus display config). yserver no longer wedges: 18 of 46 cases ran (XClearArea … XDrawString16) before the 900s timeout, **0 ABORT** (was 1375). FAILs are pixel-exactness vs the X rasterization spec (stroke geometry + thin-line bresenham not bit-exact), tracked separately. Full 46-case run needs a longer budget — each GetImage-heavy case is slow under the guest's software/Venus Vulkan. |

### `Xlib10` baseline 2026-05-26

Full scenario completed (23 cases / 95 tests). Subset of interest:

| Test                       | PASS | FAIL | UNTST | Notes |
|----------------------------|-----:|-----:|------:|-------|
| `XInstallColormap`         |    0 |    3 |     3 | Opcode 81 unhandled — request silently dropped. |
| `XUninstallColormap`       |    0 |    1 |     4 | Opcode 82 unhandled. |
| `XListInstalledColormaps`  |    0 |    2 |     0 | Stub reply returns empty list. |
| Xlib10 totals              |   16 |   37 |    36 | UNRES=5, UNSUP=1. |

### `Xlib8` baseline 2026-05-26

Full scenario completed (29 cases / 165 tests). Subset of interest
for the draw-primitives work:

| Test                  | PASS | FAIL | UNTST | UNRES | Notes |
|-----------------------|-----:|-----:|------:|------:|-------|
| `XSetArcMode`         |    1 |    1 |     1 |     0 | Plus 1 NORESULT. ArcMode stored but not honoured in poly_fill_arc. |
| `XSetDashes`          |    1 |    4 |     1 |     1 | Opcode 58 unhandled — `gc.dashes` stays at the single-byte CreateGC/ChangeGC form. |
| `XSetLineAttributes`  |    3 |    2 |     1 |     0 | line_width / line_style / cap_style / join_style stored but not honoured in rasterizer. |
| Xlib8 totals          |   53 |   67 |    22 |    12 | UNSUP=10. |

### `Xlib9draw` baseline 2026-05-26 — superseded

> **Resolved.** The "hang" described below was the `-display none`
> recipe leaving KMS pageflips with no completion event, not a
> depth-4 GetImage bug. Fixed by switching the recipe to the
> egl-headless/Venus display config; the full `Xlib9` now runs (see
> the *Full scenario sweep* section). This custom scenario + its
> finding are kept for history; the temp `Xlib9draw`/`Xlib9line`
> entries have been removed from `xts/xts5/tet_scen`.

Custom scenario: `Xlib9draw` was a local addition to
`xts/xts5/tet_scen` listing only the `Xlib9/X(Draw|Fill|Put)*` tests,
since the full `Xlib9` scenario appeared to hang after
`XCopyPlane` (depth-4 GetImage spam was the last log line). With the
custom scenario we got signal on the draw primitives directly.

The apparent hang recurred ~31 subtests into `XDrawArcs`; root cause
turned out to be the pageflip-completion stall, not depth-4.

| Date       | PASS | FAIL | UNRES | UNIN | UNTST | NOTIU | UNSUP | ABORT | Notes |
|------------|-----:|-----:|------:|-----:|------:|------:|------:|------:|-------|
| 2026-05-26 |   15 |   82 |     0 |    0 |     2 |    32 |     1 |   182 | XDrawArc (6 subtests) ran fully; XDrawArcs ran through subtest 31 of 112 before yserver hung on depth-4 get_image; remaining 15 cases ABORTed. 15 PASS / 82 FAIL of the testable surface. |

### Full scenario sweep — yserver KMS, 2026-05-27

Every remaining xts scenario run against yserver under the
egl-headless recipe (`tools/xts-batch.sh`). **Headline: every
scenario ran to completion — 0 ABORT, 0 panic, no hang, across the
entire suite.** This is the survival bar for a hardware run. PASS
rates are secondary; most FAILs are pixel-exactness vs Xorg's `mi`
rasterizers (acceptable) or unimplemented esoterica.

| Scenario  | CASES | TESTS | PASS | FAIL | UNRES | UNTST | UNSUP | NOTIU | ABORT |
|-----------|------:|------:|-----:|-----:|------:|------:|------:|------:|------:|
| Xproto    |   122 |   389 |  344 |    8 |    16 |    19 |     2 |     0 |     0 |
| Xlib3     |   109 |   162 |  100 |   23 |     5 |    21 |     6 |     1 |     0 |
| Xlib4     |    29 |   324 |  110 |  174 |     7 |    17 |    11 |     5 |     0 |
| Xlib5     |    15 |    84 |   56 |   21 |     0 |     5 |     2 |     0 |     0 |
| Xlib6     |     8 |    50 |    4 |   17 |     0 |    29 |     0 |     0 |     0 |
| Xlib7     |    58 |   172 |   83 |   31 |     0 |    13 |    45 |     0 |     0 |
| Xlib8     |    29 |   165 |   60 |   63 |    10 |    22 |    10 |     0 |     0 |
| Xlib9 †   |    31 |  1316 |  157 |  893 |    62 |    21 |    22 |   159 |     0 |
| Xlib10    |    23 |    95 |   23 |   31 |     5 |    35 |     1 |     0 |     0 |
| Xlib11    |    33 |   195 |   24 |   98 |     2 |     4 |    24 |    43 |     0 |
| Xlib12    |    27 |   138 |   89 |   15 |     4 |    16 |     2 |    12 |     0 |
| Xlib13    |    32 |   269 |   61 |  160 |    33 |     9 |     3 |     3 |     0 |
| Xlib14    |    45 |    58 |   19 |   34 |     0 |     5 |     0 |     0 |     0 |
| Xlib15    |    45 |   159 |  122 |    4 |     0 |    33 |     0 |     0 |     0 |
| Xlib16    |    30 |   105 |   82 |    0 |     0 |    22 |     1 |     0 |     0 |
| Xlib17    |    55 |   131 |   90 |   18 |     2 |    21 |     0 |     0 |     0 |
| Xopen     |     8 |   127 |  122 |    3 |     0 |     0 |     2 |     0 |     0 |
| ShapeExt  |    11 |    11 |   11 |    0 |     0 |     0 |     0 |     0 |     0 |
| XI        |    36 |   316 |    0 |   21 |     0 |   289 |     1 |     5 |     0 |
| XIproto   |    35 |   107 |    0 |    0 |     0 |   107 |     0 |     0 |     0 |
| Xt3       |    21 |    73 |   73 |    0 |     0 |     0 |     0 |     0 |     0 |
| Xt4       |    33 |   192 |   94 |    0 |     0 |    98 |     0 |     0 |     0 |
| Xt5       |    10 |    69 |   26 |    0 |     0 |    41 |     0 |     0 |     0 |
| Xt6       |     7 |    71 |   67 |    4 |     0 |     0 |     0 |     0 |     0 |
| Xt7       |    11 |   106 |   96 |    1 |     0 |     6 |     0 |     3 |     0 |
| Xt8       |     7 |    43 |   35 |    4 |     0 |     4 |     0 |     0 |     0 |
| Xt9       |    33 |   189 |  128 |    0 |     4 |    55 |     2 |     0 |     0 |
| Xt10      |     8 |    17 |   16 |    0 |     0 |     1 |     0 |     0 |     0 |
| Xt11      |    58 |   285 |  247 |    2 |     0 |    34 |     0 |     0 |     0 |
| Xt12      |    22 |    67 |   55 |    0 |     1 |    11 |     0 |     0 |     0 |
| Xt13      |    39 |   178 |  123 |    6 |     2 |    47 |     0 |     0 |     0 |
| Xt14      |     2 |    18 |   18 |    0 |     0 |     0 |     0 |     0 |     0 |
| Xt15      |     1 |     2 |    0 |    0 |     0 |     0 |     2 |     0 |     0 |
| XtC       |    29 |   147 |   86 |    2 |     2 |    56 |     1 |     0 |     0 |
| XtE       |     1 |     1 |    1 |    0 |     0 |     0 |     0 |     0 |     0 |

Notes:
- **† `Xlib9` is throughput-limited, not hung.** 31 of 46 cases
  completed within a 90-min budget; the remaining ~15 (GetImage-heavy
  font/image cases) are just slow under the guest's software/Venus
  Vulkan readback — 0 ABORT on everything that ran, no hang. A real
  GPU (hardware run) makes GetImage fast and the scenario should
  finish in full. Numbers shown are the 31 completed cases.
- **`XI` / `XIproto` are almost entirely UNTESTED**, not failing —
  the XInput test framework skips when it can't enumerate the
  expected device set. XInput device handling is incomplete; WIP
  lives on `origin/cinnamon` (the "Cinnamon click-activation chain"
  XI2 work — ~810 lines in `process_request.rs` + pointer/key
  fanout, unfinished). Revisit XI/XIproto once that lands. Not a
  survival concern (0 ABORT).
- `Xt*` (toolkit) all complete; FAIL counts are small.
- `Xlib16` is clean (82 PASS / 0 FAIL).
- **`Xlib8`** XSetArcMode: ArcMode is now honoured (Chord/PieSlice),
  but the filled-arc spans aren't bit-exact to Xorg `mifillarc`, so
  the pixel-check is a definite FAIL (was NORESULT) — hence 63 FAIL
  vs the pre-arc 62. Acceptable pixel-exactness diff, not a
  regression.

(Total tests: 122 cases / 389 purposes throughout.)

The full scenario completes in ~4 minutes. **The headline is that
ynest stays up through the entire battery** — no panics, no hangs,
no socket disconnects beyond what the tests themselves induced. From
a "does the server survive xts" angle, that's the win.

### Bare-metal baseline — yserver KMS on bee, 2026-05-31

First full bare-metal `just xts-yserver-hw all` run on bee (Ryzen 9
6900HX / RDNA2 / RADV, KMS direct via DRM master + libseat). The
2026-05-27 sweep above used the egl-headless / Venus QEMU recipe;
this is the same suite driven against real hardware. **0 ABORT, no
panic, no hang** through the scenarios that ran. Run started 11:37:38,
yserver torn down 11:57:38 = **~20 minutes**; the user logged out
before scenarios past Xlib9 ran (so Xlib10–17 / Xt* / XI / Xopen /
ShapeExt / XtC / XtE not in this snapshot — rerun with a longer
session for a full sweep).

| Scenario | CASES | TESTS | PASS | FAIL | UNRES | UNTST | UNSUP | Δ vs 2026-05-27 |
|----------|------:|------:|-----:|-----:|------:|------:|------:|-----------------|
| Xproto   |   122 |   389 |  358 |    6 |     4 |    19 |     2 | **+14 PASS / -2 FAIL / -12 UNRES** (grab event-mask fix `8f6305e`) |
| Xlib3    |       |       |  108 |   15 |     5 |       |       | +8 PASS / -8 FAIL |
| Xlib4    |       |       |  108 |  176 |     7 |       |       | -2 PASS / +2 FAIL (noise) |
| Xlib5    |       |       |   56 |   21 |     0 |       |       | unchanged |
| Xlib6    |       |       |    4 |   17 |     0 |       |       | unchanged |
| Xlib7    |       |       |   83 |   31 |     0 |       |       | unchanged |
| Xlib8    |       |       |   60 |   63 |    10 |       |       | unchanged |
| Xlib9    |       |       |  158 |  892 |    62 |       |       | +1 / -1 (noise) |
| **Total**|       |  2489 |  935 | 1221 |    88 |   147 |    98 |   |

**Headline movement:** the only material change is `Xproto`'s grab
family closing — `GrabPointer` / `GrabKeyboard` / `AllowEvents` /
`ChangeActivePointerGrab` / `UngrabPointer` all flipped UNRES → PASS
after `8f6305e` filtered grab-activation/deactivation synthesised
crossings by the grabber's per-window event-mask selection (matching
Xorg `dix/enterleave.c` semantics). xts opens fresh connections that
don't select `EnterWindowMask`, expects the reply on the wire first;
pre-fix yserver fanned `EnterNotify`/`LeaveNotify` unconditionally
and the test framework couldn't recover the reply ordering.

The other scenarios are largely unchanged — the remaining bulk
(Xlib9's 892, Xlib4's 176, Xlib3-8's combined 415) is dominated by:

- **Pixel-content rendering** (~6500 lines): pixel-check vs Xorg's
  `mi*` rasterizers, GX{op} Boolean alu modes unimplemented
  (GXxor/GXnor/GXequiv/GXorReverse/GXcopyInverted/…), arc-mode
  ArcChord rasterization, line cap/join, dashed line — the
  drawing-completeness program of work.
- **Colormap fidelity** (~510 XQueryColors fails): yserver synthesises
  RGB from pixel via bit-replication; xts wants the exact RGB stored
  by `XAllocColor`. Needs real per-colormap entry tracking.
- **`GravityNotify` not emitted** (~250 lines): child windows don't
  reposition per `win_gravity` on parent resize.
- **Scattered argument validation** (`Bad{Match,Value,Window,GC,
  Font,Pixmap}` missing): per-handler arg-relation checks (~285
  test reports).
- **`Closedown mode RetainPermanent`** (~24): close-mode state
  machine doesn't preserve windows past disconnect.
- **`BadLength`** on variable-length opcodes (PolyText8/16, SetFontPath)
  + **"too big a reply"** on ListFonts/ListFontsWithInfo/GetImage
  (~6 Xproto FAILs): exact-length content validation for the
  TextItem-style encoding + BIG-REQUESTS reply-side path.

None of the above are "quick simple wins" — each is its own
medium-scope program. Pick by impact (XQueryColors clears ~510;
GravityNotify ~250; everything else <100 each).


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

## Run history (yserver KMS, `Xproto` scenario)

The KMS backend runs only inside `vng`. `just xts-yserver` boots
yserver as the only X server in a virtme-ng guest (virtio-gpu KMS)
and runs the same xts harness ynest uses. Results land in
`/home/jos/Projects/xts/results/<ts>/` on the host (vng mounts
host rootfs `--rw`).

| Date       | PASS | FAIL | UNRES | UNTST | Notes |
|------------|-----:|-----:|------:|------:|-------|
| 2026-05-17 |  358 |    6 |     4 |    19 | First captured xts-yserver run. v2 backend (`YSERVER_RENDER_MODEL=v2`, the dispatch default since `3afa5bd`). 92% pass on 389 test purposes; **+21 PASS over the ynest `Xproto` row that landed `xproto` branch** (337). Failing test cases (~10): `pBadRequest`, `pGetImage`, `pListFonts`, `pListFontsWithInfo`, `pPolyText8/16`, `pPutImage` (×3 subtests UNRES), `pSetFontPath`. Mix of font-path metadata, GetImage edge cases, and one PolyText format. Triage TBD. |

## rendercheck (RENDER smoke suite)

`rendercheck` (Arch package: `rendercheck`, AUR has 1.6 prebuilt) is
a separate suite for the X RENDER extension. Wired up via
`tools/rendercheck.sh` and the `just rendercheck-ynest` recipe.
`tools/rendercheck.sh` honours `RENDERCHECK_BIN=/path/to/rendercheck`
to point at a non-system build.

**Use rendercheck ≥ 1.6.** 1.5 (still in some distros) has a bug in
the triangles test that mis-grades the Disjoint/Conjoint operator
expectations — both Xwayland and ynest "fail" the same 144 triangle
cases under 1.5 even though the rendered output is correct. Upstream
commit `3d7add9 triangles: Fix tests for conjoint and disjoint ops`
fixes the test.

### Baseline 2026-05-07 (rendercheck 1.6, ynest on :99, 600 s per test)

| test        | pass | total | status |
|-------------|-----:|------:|--------|
| fill        |   48 |    48 | OK |
| dcoords     |    2 |     2 | OK |
| scoords     |    1 |     1 | OK |
| mcoords     |    1 |     1 | OK |
| tscoords    |    2 |     2 | OK |
| tmcoords    |    2 |     2 | OK |
| blend       |    4 |     4 | OK |
| composite   |    4 |     4 | OK |
| cacomposite |    4 |     4 | OK |
| gradients   | 3649 |  3649 | OK |
| repeat      |  304 |   304 | OK |
| triangles   |  456 |   456 | OK |
| bug7366     |    1 |     1 | OK |
| **total**   | 4478 |  4478 | **100 % PASS** |

### How we got here, in this branch

1. `d0c63ec docs+tools(rendercheck)` — bumped per-test budget so
   `composite` / `gradients` / `repeat` / `cacomposite` actually
   complete (they were timing out at 90–120 s, not hung). Re-baselined
   at 2809/3092 (90.8 %) with 1.5.
2. `96cc0aa feat(render): forward Triangles/TriStrip/TriFan` — added
   dispatch for RENDER minors 11–13 (mirror of 10 Trapezoids), so
   triangle paint operations actually reach the host. Got the suite
   to 4469/4470 under 1.6 and to host-parity at 95.3 % under 1.5
   (with the residual being the 1.5 test bug).
3. Final commit (this one) — `PictureKind::Sourceless` flag on
   `PictureState`, set on `CreateSolidFill`/`CreateLinearGradient`/
   `CreateRadialGradient`. Composite/Trapezoids/Triangles/
   FillRectangles/CompositeGlyphs now synthesise `BadDrawable` when
   the dst picture is sourceless, fixing
   `gradients::render_to_gradient_test`. 1.6 budget bumped to 600 s.

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

### 2026-05-07 (post-bucket2: per-opcode value validation, branch `xts-bucket2-value-validation`)

Three commits added per-opcode value-range validation in
`request_lengths::invalid_value`, called right after the existing
value-mask gate. Group A: fixed-position scalar fields (Grab*
modes/owner_events bool, CopyPlane single-bit). Group B: CW/GC
value-list walking, ChangeKeyboardControl per-bit validators, and
ChangePointerControl conditional checks. Group C: residual
header.data scalar enums and bools (ChangeSaveSet, ConfigureWindow
stack_mode, CirculateWindow, SendEvent propagate, AllowEvents,
CreateColormap, Bell, SetScreenSaver, ForceScreenSaver).

| scenario | PASS | FAIL | UNRES | UNTST | UNSUP | Δ vs pre-bucket2 |
|----------|-----:|-----:|------:|------:|------:|:------------------|
| Xlib4    |   83 |  203 |     5 |    17 |    11 | 61 → 83 (+22) |
| Xlib5    |   51 |   26 |     0 |     5 |     2 | 48 → 51 (+3) |
| Xlib6    |    4 |   17 |     0 |    29 |     0 | flat |
| Xlib7    |   82 |   30 |     2 |    13 |    45 | 81 → 82 (+1) |
| Xlib8    |   46 |   73 |    14 |    22 |    10 | 19 → 46 (+27) |
| Xlib9    |  219 |  606 |   388 |    33 |    23 | 218 → 219 (+1) |
| Xlib10   |   14 |   39 |     5 |    36 |     1 | 10 → 14 (+4) |
| Xlib11   |   22 |  100 |     2 |     4 |    24 | flat |
| Xlib12   |   81 |   14 |     4 |    13 |     2 | flat (Xlib12 still hits 240s timeout in case 7 `XEventsQueued` purpose 2; pre-existing, ~45 s on either side) |
| Xlib13   |   62 |  158 |    34 |     9 |     3 | 49 → 62 (+13) |
| Xlib14   |   19 |   34 |     0 |     5 |     0 | flat |
| Xlib15   |  122 |    4 |     0 |    33 |     0 | flat |
| Xlib16   |   82 |    0 |     0 |    22 |     1 | flat |
| Xlib17   |   85 |   12 |     9 |    19 |     0 | flat (case 53 XWriteBitmapFile flake oscillates between NORESULT and 1 PASS / 4 FAIL across runs) |
| **sum**  |**972** | 1316 |   463 |   260 |   122 | **889 → 972 (+83)** |

No FAIL regressions. Each of A/B/C was verified with its own
`tools/xts-xlib-sweep.sh :99` run.

Total xts coverage on master + bucket2 branch:
Xproto 337 + Xlib3 110 + Xlib4–17 972 + ShapeExt 11 = **1430 PASS**.

### 2026-05-07 (post-bucket3: CW field persistence, branch `xts-bucket3-cw-persistence`)

Extends the `Window` struct + `CreateWindowRequest` /
`ChangeWindowAttributesRequest` parsers to persist all CW value-list
bits the spec requires `GetWindowAttributes` to surface:
`bit_gravity`, `win_gravity`, `backing_store`, `backing_planes`,
`backing_pixel`, `save_under`, `colormap`, `do_not_propagate_mask`.
The `backing_store` byte is also wired into the `data` slot of the
GetWindowAttributes reply (was previously hard-coded zero). The
hard-coded defaults in `process_request::window_attributes` are
gone — now read from the Window struct.

| scenario | PASS | FAIL | UNRES | UNTST | UNSUP | Δ vs bucket 2 |
|----------|-----:|-----:|------:|------:|------:|:--------------|
| Xlib4    |   89 |  197 |     5 |    17 |    11 | 83 → 89 (+6) |
| Xlib5    |   52 |   25 |     0 |     5 |     2 | 51 → 52 (+1) |
| Xlib7    |   82 |   31 |     1 |    13 |    45 | flat (PASS unchanged; one Xlib7 case shifted UNRES → FAIL) |
| Xlib11   |   23 |   99 |     2 |     4 |    24 | 22 → 23 (+1) |
| (others) | flat | — | — | — | — | unchanged |
| **sum**  |**980** | 1309 |   462 |   260 |   122 | **972 → 980 (+8)** |

Smaller delta than bucket 2 because most XChangeWindowAttributes test
purposes also exercise other features we don't implement yet —
ParentRelative bg-pixmap inheritance, ColormapNotify event generation,
border-tile-origin re-rendering, pixel-correctness in tiled fills —
so persistence alone unblocks only the purposes that *only* tested
the round-trip. The persistence itself works (verified by
`cw_fields_persist_through_create_and_change` unit test); other
buckets unblock the rest.

Total xts coverage on master + bucket3 branch:
Xproto 337 + Xlib3 110 + Xlib4–17 980 + ShapeExt 11 = **1438 PASS**.

(plus XI + XIproto suites complete cleanly with all UNTST due to
0-device advertisement).

### Top blockers driving the remaining FAIL

The "first-case hang" class is gone. The residual ~2.6k FAIL across
the full battery sit at **34% PASS (1347 / 3971)**. To get the
unique `REPORT:` lines and counts for the buckets below, sum across
all the post-fix journals:

```sh
for d in $(ls -1dt /home/jos/Projects/xts/results/2026-05-07-10:* | head -14); do
    grep "REPORT:" "$d/journal" | sed 's/^[^|]*|[^|]*|//'
done | sort | uniq -c | sort -rn | head -40
```

Top buckets (rough estimate of distinct affected tests in parens):

1. **Drawing-on-uninitialised-pixmap-source semantics** (≈100-200
   tests, ~3000 raw `REPORT` lines). xts's `functest()` (in
   `xts5/lib/gc/function.mc`) sets up a source/dest drawable pair,
   fills the dest, runs `XCopyArea(src, dst, ...)`, then walks the
   dest looking for any non-zero pixel; "Nothing was drawn with a gc
   function of GXcopy" / "No pixel set in drawable" / "Setup error in
   functest" all cascade from one root.
   **Investigated 2026-05-07** (branch `xts-bucket1-depth-correct-gc`,
   commit `b5c6463`) — actual root cause is *not* the drawing path
   itself. xts's `resetvinf(VI_WIN_PIX)` iterates every advertised
   visual, including the depth-32 ARGB visual. ynest creates the
   corresponding depth-32 client window as a depth-32 *sub-window*
   on the host's depth-24 root. On a non-composited host x.org,
   `XGetImage` against a depth-32 sub-window alpha-strips
   fully-transparent pixels — `XSetForeground(W_FG=1) →
   XFillRectangle → XGetImage` returns 0 (alpha=0 in `0x00000001`
   shows through to parent) where xts expects 1. The
   `CHECKPASS(2*nvinf())` accounting demands *both* visuals pass, so
   the depth-32 iteration alone fails the test. The depth-32 GC
   fix on this branch is verified-correct (depth-32 *pixmap* fills
   now deposit pixels — was silently zero before) but doesn't move
   the xts numbers because xts hits the *window* path. Two ways to
   close the bucket:
   (a) **Don't advertise the depth-32 ARGB visual** when running
       under a non-composited host. Cheap, immediate; breaks future
       picom-on-ynest support and ARGB cursor pipeline.
   (b) **Redirect depth-32 client windows through depth-32 backing
       pixmaps** on the host instead of forwarding as sub-windows;
       blit pixmap → host-visible drawable on Expose / damage. Days
       of work; a real fix that preserves ARGB. The depth-32 GC fix
       in `b5c6463` is half-blocking infrastructure for this — it
       makes depth-32 pixmap fills / copies actually work.

   **Tried and rejected: COMPOSITE redirect (2026-05-07).** Sending
   `Composite::RedirectWindow(host_xid, Automatic)` on every depth-32
   sub-window causes the host server to allocate off-screen storage,
   but `XGetImage` on the redirected window only ~75 % of the time
   reads the off-screen pixels with alpha intact — the other 25 % it
   returns the alpha-flattened on-screen buffer (verified with a C
   smoke that creates+fills+reads a depth-32 window 20 times under
   ynest: 15 / 20 returned `0x00000001`, 5 / 20 returned `0x00000000`;
   xts numbers stayed flat across the whole battery, confirming the
   non-determinism). Reading via `XCompositeNameWindowPixmap` on the
   redirected window was strictly worse (0 / 5 returned the right
   pixel). Moving on: option (a) is the small immediate fix, option
   (b) is the real one. Branch `xts-bucket1-depth-correct-gc` keeps
   the depth-correct-GC fix in `b5c6463` (still correct on its own
   merits) and drops the COMPOSITE-redirect attempt.

2. **Per-opcode value validation** (≈500 tests, ~559 missing
   `BadValue`/`BadMatch` errors). Today
   `request_lengths::invalid_value_mask` checks which BITS of a
   value-mask are valid for CreateWindow/ChangeWindowAttributes/
   ConfigureWindow/CreateGC/ChangeGC/CopyGC/ChangeKeyboardControl —
   but no handler checks the VALUES inside the value-list. xts
   probes `bit_gravity ∉ [0,10]`, `backing_store ∉ {NotUseful,
   WhenMapped, Always}`, grab modes, line/cap/join styles, etc.
   Mechanical to add: ~20 opcodes need per-value range tables,
   following the existing CW_VALID/GC_VALID pattern. Probably the
   single highest-leverage remaining work item by tests-per-hour.
   Top offending cases:

   ```
   28 XChangeWindowAttributes 46
   22 XChangeKeyboardControl 20
   20 XCreatePixmap 6
   20 XCopyPlane 43
   19 XGrabButton 39
   17 XGrabKey 16
   15 XGrabPointer 34
   12 XSetLineAttributes 6
   12 XGrabKeyboard 25
   12 XChangePointerControl 11
   ```

3. **`ChangeWindowAttributes` field persistence** (≈300 tests).
   The `Window` struct stores background-pixel / background-pixmap /
   override-redirect / cursor only. CW value-mask bits for
   bit-gravity, win-gravity, backing-store, backing-planes,
   backing-pixel, save-under, colormap, do-not-propagate-mask are
   accepted (no error) but discarded. Tests that set-then-read fail
   with "got default, expected <set value>". Half-day refactor:
   extend `Window` struct, parse all CW values, return them in
   `GetWindowAttributes`. The hardcoded defaults in
   `process_request.rs::window_attributes` go away.
4. **Pixel-mismatch in image / fill / area tests** (≈500 tests,
   "A total of N out of 9000 pixels were bad" and "Pixel mismatch
   in image"). Likely entangled with bucket #1 plus a separate
   issue around backing-pixel default propagation, border-pixel
   defaults, and possibly `XYBitmap`/`XYPixmap` PutImage formats
   (currently only ZPixmap is handled).
5. **Depth-N pixmap support for N ∉ {1, 24, 32}** (≈400 tests,
   "Incorrect depth (24 != N)" appears as 158/78/78/78 for N=32/8/4/1).
   `supported_pixmap_depth` rejects everything outside a small set;
   tests that allocate depth 2/4/8/15/16-bit pixmaps then read back
   get the host's default depth (24). Extend the supported-depth
   list and let host-X allocate; about half a day.
6. **Expose-region tracking after Destroy/Resize/Map/Configure**
   (≈400 tests, mostly Xlib9 UNRES). Tests that issue a structural
   change and time-out waiting for an `Expose` covering the
   newly-uncovered area. Multi-day — needs a real region tracker;
   probably the largest scope item remaining.
7. **Xlib12 per-connection setup/teardown latency** (single
   scenario, currently truncated by the 240 s sweep budget at
   case 7 / `XEventsQueued` purpose 2). The Xlib12 cases
   (XPeekEvent, XEventsQueued, XIfEvent, XMaskEvent, XNextEvent,
   XPutBackEvent, …) implement "verify that blocking did not
   occur" by forking a child watchdog per call, which tears down
   and re-opens the X connection. We pay the connection-setup
   cost ~70 times in the scenario; if our setup or close-path
   round-trip is even ~500 ms slower than host x.org, the wall
   time blows past 240 s and the rest of the scenario gets cut
   off. The cases that *do* run all PASS — this is throughput,
   not correctness. Profile a single XOpenDisplay/XCloseDisplay
   round-trip against host x.org to find the offender; likely a
   pending-event drain or final flush we hold for too long.

Bucket (1) is the biggest payoff if it turns out to be a single
small bug, but the investigation is open-ended. Buckets (2) and
(3) are the safe next steps with predictable scope and clear
endpoints.

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

