# Verifying Color Grading

Run the grading audit with synthetic images and footage; personal projects are unnecessary:

```sh
cargo test -p filmcraft-golden --test golden grading_ -- --nocapture
cargo test -p filmcraft-engine --lib grading_ -- --nocapture
cargo test -p filmcraft-ui-egui grading_ -- --nocapture
cargo test -p filmcraft-render lumetri -- --nocapture
cargo test -p filmcraft-render color_match -- --nocapture
cargo test -p filmcraft-scopes
```

Set `FILMCRAFT_REQUIRE_GRADING_GPU=1` for a verification run that must obtain an actual GPU
device and read back its output. Without it, machines without a compatible GPU may skip that
test, reporting the reason. The matrix logs whether each grade ran in the grading shader or
on the CPU before GPU composition. Exposure must exercise the grading shader; an uploaded
CPU-rendered image cannot satisfy that assertion. HDR, LUTs, wheels, curves and HSL can use
the CPU fallback. Comparison uses the existing golden-suite tolerance: 99th percentile at
most six 8-bit code values and mean RGB error below 1.5.

The matrix requires a meaningful before/after case for **every declared grading parameter**,
including prerequisites such as enabling HSL, activating a look before testing its intensity,
and using a nonzero vignette before testing its shape. A spatial color chart includes qualifier
islands, fine detail, transparent/translucent samples and HDR values above SDR white. The tests
check finite output, unchanged alpha and a measurable effect. These are functional checks;
they do not establish an artistic preference or a standardized film look.

Separate analytic checks verify exposure in stops, frame-time evaluation of animated exposure,
neutral desaturation, isolated RGB curves, white-balance direction, and a manually specified
red/blue-swapping `.cube` lattice, including zero Creative LUT intensity. Engine tests compare
saved/reopened and exported PNG pixels with the graded Program render. Scope tests use known
color bars, ramps and HDR reference-white levels.

The wheel interaction test clicks the actual painted green/blue ring and checks the resulting
rendered tint, rather than setting an engine vector directly. The full-panel test finds Basic
controls by automation id, clicks Exposure, verifies changed pixels and checks single-step Undo.
Set `FILMCRAFT_UI_SNAPSHOT_DIR` to an output directory to capture the Color panel tests offscreen.

Shot matching is approximate, based on tonal/color statistics. Regression tests require refused
matches to preserve the project and undo/redo history, newly added grades to undo in one step,
and identical graded reference shots to retain their picture with imported LUTs, animated
exposure and Rec.709/PQ/HLG working spaces. The solver evaluates the project's LUT library and
the clip's frame time. HDR statistics convert BT.2020 primaries to linear sRGB and normalize to
nominal 1000-nit white; grading itself remains in the sequence's HDR working space. Skin-tone
protection weights a color range; it is not a face detector.

The HDR grading tests verify working-light values numerically. The current Program monitor and
these GPU comparison images use an SDR display transform; this audit does not certify a native
HDR display or a particular physical monitor's calibration.

## Shared groups and Vibes

See [Color by feeling](color-vibes.md) for the workflow and assistant commands. Group tests use
the same generated footage with independently different exposure and white balance, require
the rendered frames to agree after matching, and change the shared mood, strength, curves,
contrast and saturation without altering member corrections. They cover save/reopen, Undo,
copy/paste, removal, rebalance, hostile commands and corrupt group project data. HDR and
wide-gamut checks use the actual working primaries, including extended negative BT.709
components when converting colors for perceptual statistics. The Oklab conversion follows
[Ottosson's published matrices and test vectors](https://bottosson.github.io/posts/oklab/).

Vibes tests verify that returning a slider to zero restores its exact base and that adjusting
a bypassed section starts it from neutral without reviving old settings. UI tests choose a
mood by clicking a card, change a real slider, check single-step Undo, and switch to Advanced
and back without changing pixels. Native backend tests check separate-process calculation,
atomic completion, cancellation and stale-project protection. Run these with:

```sh
cargo test -p filmcraft-engine grading_group
cargo test -p filmcraft-render grading_vibes
cargo test -p filmcraft-ui-egui grading_vibes
cargo test -p filmcraft --test backend_process grading_group_backend
```
