# Recovering soft focus

Focus Recovery's Lens defocus model processes convolution taps across contiguous rows, retaining
the reference calculation's addition order, border reflection and quality settings. Repeated
input pictures can reuse the complete restoration: this helps slowed or held footage without
changing the timeline frame rate, retiming or the picture.

Reuse compares every input pixel bit (including alpha), frame dimensions and all evaluated
restoration parameters. Strength keyframes, a changed radius or quality, colour primaries,
preview scale and changed upstream pixels therefore cannot return an old result. Masks are
applied after the restored effect output. The cache stays in memory, capped at 128 MiB and four
entries per rendering process, counting both input and output floats. Frames too large to fit
are processed normally. A busy cache is bypassed; rendering workers never wait on another
worker's effect calculation. Failed or cancelled calculations are not stored.

Export and preview batches containing enabled Focus Recovery process their frames in order,
leaving the Rayon pool available for parallel work within each restoration. This lets later
copies of a picture reuse a completed result instead of simultaneously missing the cache.
Batches without active Focus Recovery keep their frame parallelism. Nested compositions are
conservatively ordered when their visible parent contains Focus Recovery; nesting checks have
depth and work limits.

`perf.stats` reports `effects.focusRecoveryCache` hits, misses, bypasses and memory usage for
the process answering the query. Native background rendering processes own separate caches;
the counters in the window's process do not aggregate those workers. While a cache is busy,
its entry and byte counts are `null` rather than blocking the query.

Select a video clip, search Effects for **Focus Recovery**, and apply it. It lives in
**Video Effects > Blur & Sharpen**. Start at the defaults and judge the face or other subject
at **Full** playback resolution and 100% monitor zoom. Lower-resolution playback cannot show
detail finer than its own pixels. Toggle the effect's **fx** switch to compare.
For a restoration-first stack, place it before the clip's creative grade or shared-look stage.

- **Recovery strength** blends the recovered picture with the original. Zero is an exact bypass.
- **Focus softness** describes the blur in original source pixels. Start around 2–3 px for
  mildly soft HD footage. Increase slowly; a wrong or excessive radius produces outlines.
- **Noise protection** stops small residual differences from being repeatedly amplified.
  Increase it if skin, shadows or compression noise become rough.
- **Halo protection** limits light/dark outlines against the original local tonal range.
  Increase it if hair, glasses or high-contrast edges acquire outlines.
- **Blur model** offers Gentle softness (Gaussian) and Lens defocus (circular). The Gaussian model uses
  half the softness value as its standard deviation; the circular model uses it as disc radius.
  Try the other model if one creates artifacts; neither is an automatic measurement of the lens.
- **Recovery quality** sets a fixed iteration count: Quick 4, Balanced 10, Detailed 20.
  More iterations take longer and may amplify artifacts; they are not always better.

The effect uses a clean-room, noise-damped **Richardson–Lucy deconvolution** of linear-light
luminance. A normalized blur model predicts the observed soft image, and successive bounded
corrections reduce the prediction error. Reflecting image boundaries avoid wraparound; alpha
weights prevent transparent black from contaminating edges. The same luminance multiplier is
applied to RGB, preserving chromatic ratios and alpha. HDR brightness is not clipped to SDR white.
Settings are fixed across frames and scale with playback resolution, without random or
frame-adaptive parameter changes. This is not temporal motion compensation or generative AI.

It can improve recoverable detail in mildly defocused footage. It cannot reconstruct missing
fine detail reliably, remove severe motion blur, or refocus foreground and background independently.
Mask it to the subject when appropriate. Retain the source files and judge the correction during
motion, including shadow noise and bright edges. It runs on CPU in previews and exports; use
preview rendering for demanding footage. Saved projects, presets, keyframes, masking, Undo and
ordinary effect enable/bypass work as they do for other video effects.
The workspace is bounded to frames through 8192 × 4320. Invalid geometry, non-finite pixels or
unavailable scratch memory preserve the input instead of partially applying restoration.

## Assistant and local MCP

Use the existing `effects.list`, `effects.apply` and `effects.setParam` commands. Apply the effect
in bulk with `effects.apply {effect:"focus_recovery",clips:[...]}`. Its returned `applied` entries
identify each instance. Set parameters through `effects.setParam {clip:...,effect:index,param:...,
value:...}`; use the editor's batch facility for multiple edits. Numeric parameters are `amount`
(0–100), `radius` (0.5–6 source px), `noise` and `halo` (0–100). `model` is a choice: 0 Gaussian,
1 circular; `quality`: 0 Quick, 1 Balanced, 2 Detailed. The assistant should inspect the rendered
subject, start conservatively, compare at Full resolution, and describe limitations honestly.

## Public references

These describe methods and product behavior, not proprietary implementations. No third-party
implementation code or artwork is included.

- [MathWorks: Lucy–Richardson deconvolution](https://www.mathworks.com/help/images/ref/deconvlucy.html):
  point-spread functions, iteration counts and damping against noise amplification.
- [RawTherapee: sharpening](https://rawpedia.pixls.us/sharpening/): an editing workflow that exposes
  Richardson–Lucy radius, amount, damping and iterations; incorrect blur assumptions cause halos.
- [Adobe: Smart Sharpen controls](https://helpx.adobe.com/photoshop/desktop/effects-filters/smart-filters/sharpen-controls-with-smart-sharpen.html):
  blur-model selection and noise control in a professional photo editor. This does not establish
  which internal algorithm that product uses.

Synthetic regression tests independently blur a known sharp pattern and require recovery to reduce
the error. They also exercise noise/halo guards, borders, alpha, HDR, bypass, reduced resolution,
malformed inputs, commands, project persistence and the effect's panel controls.
