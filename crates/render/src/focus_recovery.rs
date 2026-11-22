//! Clean-room damped Richardson–Lucy luminance restoration. See docs/focus-recovery.md.
//! All kernels are normalized, symmetric PSFs. No proprietary/GPL implementation is used.
use rayon::prelude::*;
use std::sync::OnceLock;

use crate::effect_cache::{EffectCache, EffectCacheStats};
use crate::{
    Image,
    effects::{FxCtx, choice, f},
};
use filmcraft_project::EffectInstance;

// A bounded workspace supports up to 8K DCI without input-sized unbounded allocation.
const MAX_PIXELS: usize = 8192 * 4320;
const EPS: f32 = 1e-6;

// Counts both the original and restored floats. Oversized pictures bypass the cache;
// it must not turn 4K/8K restoration into unbounded persistent frame storage.
const CACHE_BYTES: usize = 128 * 1024 * 1024;

fn cache() -> &'static EffectCache<[u32; 7]> {
    static CACHE: OnceLock<EffectCache<[u32; 7]>> = OnceLock::new();
    CACHE.get_or_init(|| EffectCache::new(CACHE_BYTES))
}

pub(crate) fn cache_stats() -> EffectCacheStats {
    cache().stats()
}

#[derive(Clone, Copy)]
struct Parameters {
    amount: f32,
    radius: f32,
    noise: f32,
    halo: f32,
    iterations: u32,
    disc: bool,
    wide: bool,
}

impl Parameters {
    fn key(self) -> [u32; 7] {
        [self.amount.to_bits(), self.radius.to_bits(), self.noise.to_bits(), self.halo.to_bits(), self.iterations, u32::from(self.disc), u32::from(self.wide)]
    }
}

fn buffer(n: usize) -> Option<Vec<f32>> {
    let mut values = Vec::new();
    values.try_reserve_exact(n).ok()?;
    values.resize(n, 0.0);
    Some(values)
}

/// Half-sample reflection preserves constants without wrapping the other side into an edge.
fn reflect(p: isize, size: usize) -> usize {
    if p >= 0 && (p as usize) < size {
        return p as usize;
    }
    let period = (size * 2) as isize;
    let p = p.rem_euclid(period);
    if p < size as isize { p as usize } else { (period - 1 - p) as usize }
}

enum Psf {
    Gaussian(Vec<f32>),
    Disc(Vec<(isize, isize, f32)>),
}
impl Psf {
    fn new(radius: f32, disc: bool) -> Self {
        if !disc {
            let sigma = (radius * 0.5).max(0.15);
            let support = (sigma * 3.0).ceil() as isize;
            let mut weights: Vec<f32> = (-support..=support).map(|p| (-(p * p) as f32 / (2.0 * sigma * sigma)).exp()).collect();
            let sum: f32 = weights.iter().sum();
            for weight in &mut weights {
                *weight /= sum;
            }
            Self::Gaussian(weights)
        } else {
            let support = (radius + 0.5).ceil() as isize;
            let mut taps = Vec::new();
            for y in -support..=support {
                for x in -support..=support {
                    // Integrate each pixel's aperture with a symmetric 4x4 grid. Fractional source
                    // radii and reduced previews must not jump between integer-sized discs.
                    let mut coverage = 0.0;
                    for sy in 0..4 {
                        for sx in 0..4 {
                            let xx = x as f32 + (sx as f32 + 0.5) / 4.0 - 0.5;
                            let yy = y as f32 + (sy as f32 + 0.5) / 4.0 - 0.5;
                            if xx * xx + yy * yy <= radius * radius {
                                coverage += 1.0;
                            }
                        }
                    }
                    if coverage > 0.0 {
                        taps.push((x, y, coverage));
                    }
                }
            }
            let sum: f32 = taps.iter().map(|p| p.2).sum();
            for tap in &mut taps {
                tap.2 /= sum;
            }
            Self::Disc(taps)
        }
    }

    fn convolve(&self, src: &[f32], dst: &mut [f32], scratch: &mut [f32], w: usize, h: usize) {
        match self {
            Self::Gaussian(weights) => {
                let support = (weights.len() / 2) as isize;
                scratch.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
                    for (x, value) in row.iter_mut().enumerate() {
                        *value = weights.iter().enumerate().map(|(k, weight)| src[y * w + reflect(x as isize + k as isize - support, w)] * weight).sum();
                    }
                });
                dst.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
                    for (x, value) in row.iter_mut().enumerate() {
                        *value = weights.iter().enumerate().map(|(k, weight)| scratch[reflect(y as isize + k as isize - support, h) * w + x] * weight).sum();
                    }
                });
            }
            Self::Disc(taps) => {
                dst.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
                    // Same tap/addition order as scalar Sum<f32>, but contiguous slices let
                    // the compiler vectorize the interior without per-sample reflection.
                    row.fill(-0.0);
                    for &(dx, dy, weight) in taps {
                        let sy = reflect(y as isize + dy, h);
                        let source = &src[sy * w..(sy + 1) * w];
                        let lo = (-dx).clamp(0, w as isize) as usize;
                        let hi = (w as isize - dx).clamp(0, w as isize) as usize;
                        for (x, value) in row[..lo].iter_mut().enumerate() {
                            *value += source[reflect(x as isize + dx, w)] * weight;
                        }
                        // Tiny images can have no interior at all. Do not form an empty
                        // slice at a source offset beyond the row in that case.
                        if lo < hi {
                            let start = (lo as isize + dx) as usize;
                            for (value, sample) in row[lo..hi].iter_mut().zip(&source[start..start + hi - lo]) {
                                *value += sample * weight;
                            }
                        }
                        for (i, value) in row[hi..].iter_mut().enumerate() {
                            *value += source[reflect((hi + i) as isize + dx, w)] * weight;
                        }
                    }
                });
            }
        }
    }
}

/// Fail closed before mutation if geometry, parameters or workspace allocation are invalid.
/// Like Image::new, unavailable workspace preserves the original image rather than panicking.
pub(crate) fn apply(image: &mut Image, effect: &EffectInstance, cx: &FxCtx) {
    apply_with_cache(image, effect, cx, cache());
}

fn apply_with_cache(image: &mut Image, effect: &EffectInstance, cx: &FxCtx, cache: &EffectCache<[u32; 7]>) {
    let Some(n) = image.w.checked_mul(image.h).filter(|n| *n > 0 && *n <= MAX_PIXELS) else { return };
    if n.checked_mul(4) != Some(image.px.len()) {
        return;
    }
    // Prevent hostile/non-finite radiance from overflowing convolution or squared residuals.
    if image.px.iter().any(|v| !v.is_finite() || v.abs() > 1e12) {
        return;
    }
    let (amount, radius, noise, halo) = (f(effect, "amount", cx), f(effect, "radius", cx), f(effect, "noise", cx), f(effect, "halo", cx));
    if ![amount, radius, noise, halo, cx.px_scale].iter().all(|v| v.is_finite()) || cx.px_scale <= 0.0 {
        return;
    }
    let amount = amount.clamp(0.0, 100.0) / 100.0;
    if amount == 0.0 {
        return;
    }
    let radius = (radius.clamp(0.5, 6.0) * cx.px_scale).clamp(0.3, 24.0);
    let noise = noise.clamp(0.0, 100.0) / 100.0;
    let halo = halo.clamp(0.0, 100.0) / 100.0;
    let iterations = match choice(effect, "quality") {
        0 => 4,
        1 => 10,
        _ => 20,
    };
    if filmcraft_media::cancel::cancelled() {
        return;
    }
    let parameters =
        Parameters { amount, radius, noise, halo, iterations, disc: choice(effect, "model") == 1, wide: cx.gamut == filmcraft_color::Gamut::Bt2020 };
    cache.apply(image, parameters.key(), |image| restore(image, parameters));
}

fn restore(image: &mut Image, parameters: Parameters) -> bool {
    let Parameters { amount, radius, noise, halo, iterations, disc, wide } = parameters;
    let n = image.px.len() / 4;
    let psf = Psf::new(radius, disc);
    let (Some(mut observed), Some(mut alpha), Some(mut norm), Some(mut estimate), Some(mut work), Some(mut scratch), Some(mut correction)) =
        (buffer(n), buffer(n), buffer(n), buffer(n), buffer(n), buffer(n), buffer(n))
    else {
        return false;
    };
    let luma = if wide { [0.2627, 0.6780, 0.0593] } else { [0.2126, 0.7152, 0.0722] };
    for (i, pixel) in image.px.as_chunks::<4>().0.iter().enumerate() {
        if pixel.iter().all(|p| p.is_finite()) && pixel[3] > EPS && pixel[3] <= 1.0 {
            alpha[i] = pixel[3];
            observed[i] = ((pixel[0] * luma[0] + pixel[1] * luma[1] + pixel[2] * luma[2]) / pixel[3]).max(0.0);
        }
    }
    psf.convolve(&alpha, &mut norm, &mut scratch, image.w, image.h);
    estimate.copy_from_slice(&observed);
    for _ in 0..iterations {
        if filmcraft_media::cancel::cancelled() {
            return false;
        }
        work.par_iter_mut().enumerate().for_each(|(i, value)| *value = estimate[i] * alpha[i]);
        psf.convolve(&work, &mut correction, &mut scratch, image.w, image.h);
        correction.par_iter_mut().enumerate().for_each(|(i, value)| {
            let prediction = *value / norm[i].max(EPS);
            let residual = observed[i] - prediction;
            // Suppress sub-noise residuals rather than amplifying camera/compression noise at
            // every iteration. Luminance-dependent threshold preserves bright/dark balance.
            let threshold = noise * 0.025 * observed[i].sqrt().max(0.05);
            let damping = residual * residual / (residual * residual + threshold * threshold + 1e-16);
            *value = (1.0 + damping * residual / prediction.max(EPS)).clamp(0.25, 4.0) * alpha[i];
        });
        psf.convolve(&correction, &mut work, &mut scratch, image.w, image.h);
        estimate.par_iter_mut().enumerate().for_each(|(i, value)| {
            *value = (*value * (work[i] / norm[i].max(EPS)).clamp(0.25, 4.0)).clamp(observed[i] * 0.25, observed[i] * 4.0);
        });
    }
    let (w, h) = (image.w, image.h);
    let neighborhood = radius.ceil().max(1.0) as isize;
    image.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let i = y * w + x;
            if alpha[i] <= EPS || observed[i] <= EPS {
                continue;
            }
            let mut recovered = estimate[i];
            if halo > 0.0 {
                let (mut lo, mut hi) = (observed[i], observed[i]);
                for dy in -neighborhood..=neighborhood {
                    for dx in -neighborhood..=neighborhood {
                        let j = reflect(y as isize + dy, h) * w + reflect(x as isize + dx, w);
                        if alpha[j] > EPS {
                            lo = lo.min(observed[j]);
                            hi = hi.max(observed[j]);
                        }
                    }
                }
                let margin = (hi - lo) * (1.0 - halo) * 0.5;
                recovered = recovered.clamp((lo - margin).max(0.0), hi + margin);
            }
            // One multiplier preserves RGB ratios, including extended negative wide-gamut RGB.
            // Never clamp HDR highlights to [0,1], and leave coverage exactly unchanged.
            let gain = 1.0 + amount * (recovered / observed[i] - 1.0);
            for channel in &mut pixel[..3] {
                *channel *= gain;
            }
        }
    });
    !filmcraft_media::cancel::cancelled()
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_project::{ParamValue, find_effect};
    use filmcraft_time::Tick;

    fn context() -> FxCtx<'static> {
        FxCtx {
            t: Tick::ZERO,
            px_scale: 1.0,
            seconds: 0.0,
            timecode: "",
            clip_name: "",
            project: None,
            env: None,
            frame_rate: 60.0,
            working: filmcraft_color::WorkingSpace::Rec709,
            gamut: filmcraft_color::Gamut::Bt709,
        }
    }

    fn image() -> Image {
        let mut image = Image::new(31, 17);
        for (i, pixel) in image.px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let v = 0.1 + (i * 73 % 997) as f32 / 1500.0;
            let alpha = if i % 7 == 0 { 0.4 } else { 1.0 };
            pixel.copy_from_slice(&[v * alpha, v * 0.8 * alpha, v * 0.6 * alpha, alpha]);
        }
        image
    }

    fn bits(image: &Image) -> Vec<u32> {
        image.px.iter().map(|p| p.to_bits()).collect()
    }

    #[test]
    fn disc_convolution_is_bit_exact_at_small_borders_and_fractional_radii() {
        for w in [1, 2, 7, 31] {
            for h in [1, 3, 8, 17] {
                for radius in [0.3, 0.5, 2.0975, 2.1, 6.0, 24.0] {
                    let src: Vec<f32> = (0..w * h).map(|i| ((i * 73 % 997) as f32 - 400.0) / 997.0).collect();
                    let psf = Psf::new(radius, true);
                    let Psf::Disc(taps) = &psf else { panic!("disc PSF") };
                    let mut expected = vec![0.0; w * h];
                    for (y, row) in expected.chunks_mut(w).enumerate() {
                        for (x, value) in row.iter_mut().enumerate() {
                            *value = taps.iter().map(|&(dx, dy, weight)| src[reflect(y as isize + dy, h) * w + reflect(x as isize + dx, w)] * weight).sum();
                        }
                    }
                    let mut actual = vec![0.0; w * h];
                    psf.convolve(&src, &mut actual, &mut vec![0.0; w * h], w, h);
                    assert!(expected.iter().zip(&actual).all(|(a, b)| a.to_bits() == b.to_bits()), "{w}x{h}, radius {radius}");
                }
            }
        }
    }

    #[test]
    fn repeated_frame_reuses_complete_recovery_exactly() {
        let cache = EffectCache::new(1 << 20);
        let effect = find_effect("focus_recovery").unwrap().instance();
        let original = image();
        let mut first = original.clone();
        apply_with_cache(&mut first, &effect, &context(), &cache);
        let mut repeated = original;
        let mut later = context();
        later.t = Tick(254_016_000_000);
        later.seconds = 1.0;
        later.timecode = "00:00:01:00";
        apply_with_cache(&mut repeated, &effect, &later, &cache);
        assert_eq!(bits(&first), bits(&repeated));
        assert_eq!((cache.stats().hits, cache.stats().misses), (1, 1), "a held picture with static parameters needs one restoration");
    }

    #[test]
    fn every_restoration_parameter_and_colour_context_invalidates_reuse() {
        let cache = EffectCache::new(1 << 20);
        let uncached = EffectCache::new(0);
        let original = image();
        let effect = find_effect("focus_recovery").unwrap().instance();
        apply_with_cache(&mut original.clone(), &effect, &context(), &cache);
        for (id, value) in [
            ("amount", ParamValue::Float(13.0)),
            ("radius", ParamValue::Float(3.7)),
            ("noise", ParamValue::Float(17.0)),
            ("halo", ParamValue::Float(11.0)),
            ("quality", ParamValue::Choice(2)),
            ("model", ParamValue::Choice(1)),
        ] {
            let mut changed = effect.clone();
            changed.params.get_mut(id).unwrap().value = value;
            let mut expected = original.clone();
            apply_with_cache(&mut expected, &changed, &context(), &uncached);
            let mut actual = original.clone();
            let misses = cache.stats().misses;
            apply_with_cache(&mut actual, &changed, &context(), &cache);
            assert_eq!(cache.stats().misses, misses + 1, "{id}");
            assert_eq!(bits(&actual), bits(&expected), "{id}");
        }
        for (scale, gamut) in [(0.5, filmcraft_color::Gamut::Bt709), (1.0, filmcraft_color::Gamut::Bt2020)] {
            let mut cx = context();
            cx.px_scale = scale;
            cx.gamut = gamut;
            let misses = cache.stats().misses;
            let mut actual = original.clone();
            let mut expected = original.clone();
            apply_with_cache(&mut actual, &effect, &cx, &cache);
            apply_with_cache(&mut expected, &effect, &cx, &uncached);
            assert_eq!(cache.stats().misses, misses + 1);
            assert_eq!(bits(&actual), bits(&expected));
        }
    }

    #[test]
    fn animated_strength_cannot_reuse_another_times_output() {
        let cache = EffectCache::new(1 << 20);
        let mut effect = find_effect("focus_recovery").unwrap().instance();
        let strength = effect.params.get_mut("amount").unwrap();
        strength.put_keyframe(Tick::ZERO, ParamValue::Float(20.0));
        strength.put_keyframe(Tick(254_016_000_000), ParamValue::Float(100.0));
        let mut first = image();
        apply_with_cache(&mut first, &effect, &context(), &cache);
        let mut later = context();
        later.t = Tick(254_016_000_000);
        let mut second = image();
        apply_with_cache(&mut second, &effect, &later, &cache);
        assert_ne!(bits(&first), bits(&second));
        assert_eq!((cache.stats().hits, cache.stats().misses), (0, 2));
    }

    #[test]
    fn cancelled_and_hostile_frames_do_not_enter_the_cache() {
        let cache = EffectCache::new(1 << 20);
        let effect = find_effect("focus_recovery").unwrap().instance();
        let mut actual = image();
        let original = actual.clone();
        filmcraft_media::cancel::with_cancel(&std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)), || {
            apply_with_cache(&mut actual, &effect, &context(), &cache);
        });
        assert_eq!(bits(&actual), bits(&original));
        actual.px[3] = f32::NAN;
        apply_with_cache(&mut actual, &effect, &context(), &cache);
        assert_eq!(cache.stats().entries, Some(0));
    }
}
