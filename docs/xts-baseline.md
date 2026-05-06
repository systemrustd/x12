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

## Baseline (ynest, `Xproto` scenario)

```
       CASES TESTS  PASS UNSUP UNTST NOTIU  WARN   FIP  FAIL UNRES  UNIN ABORT
Xproto   122   389     1     0     0     0     0     0   210   160    11     0
```

The full scenario completed in ~4 minutes. **The headline is that
ynest stayed up through the entire battery** — no panics, no hangs,
no socket disconnects beyond what the tests themselves induced. From
a "does the server survive xts" angle, that's the win.

The sole PASS is `OpenDisplay 2`. Everything else is masked by two
structural bugs:

| Failure mode | REPORT lines | Cause |
|---|---|---|
| big-endian client connection rejected | 483 | xts opens a second connection in reversed byte sex to test byte-swap handling; ynest's setup handshake refuses (by design — see `nested.rs`). |
| `BadLength` not raised | 433 (250 + 183) | xts sends oversized / zero-length requests expecting a `BadLength` reply; we don't validate request length. |
| `Expose` not delivered | 131 | Specific Expose-generation gaps (~30-ish unique tests). |

The other ~200 individual FAILs are spread across grab semantics,
screen-saver state, error-code edge cases, etc.

## Quick-win path (in priority order)

1. **`BadLength` enforcement.** Single guard in
   `process_request` checking `header.length * 4 == 4 + body.len()`
   (or honouring the BIG-REQUESTS extended length when set). Fires
   `BadLength` errors when a request runs past its declared length.
   Lifts 250+183 REPORT lines — likely 50–80 tests jump to PASS.
2. **Big-endian client byte-order at the wire reader.** Swap-on-read
   for u16/u32 setup + request fields when the client signals
   big-endian. Larger surface but unblocks ~161 tests' second-
   connection probes; once those connections work, many of the same
   tests pass naturally on the wire path.
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
