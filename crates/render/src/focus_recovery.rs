//! Clean-room damped Richardson–Lucy luminance restoration. See docs/focus-recovery.md.
//! All kernels are normalized, symmetric PSFs. No proprietary/GPL implementation is used.
use rayon::prelude::*;

use crate::{
    Image,
    effects::{FxCtx, choice, f},
};
use filmcraft_project::EffectInstance;

// A bounded workspace supports up to 8K DCI without input-sized unbounded allocation.
const MAX_PIXELS: usize = 8192 * 4320;
const EPS: f32 = 1e-6;

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
                    for (x, value) in row.iter_mut().enumerate() {
                        *value = taps.iter().map(|&(dx, dy, weight)| src[reflect(y as isize + dy, h) * w + reflect(x as isize + dx, w)] * weight).sum();
                    }
                });
            }
        }
    }
}

/// Fail closed before mutation if geometry, parameters or workspace allocation are invalid.
/// Like Image::new, unavailable workspace preserves the original image rather than panicking.
pub(crate) fn apply(image: &mut Image, effect: &EffectInstance, cx: &FxCtx) {
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
    let psf = Psf::new(radius, choice(effect, "model") == 1);
    let (Some(mut observed), Some(mut alpha), Some(mut norm), Some(mut estimate), Some(mut work), Some(mut scratch), Some(mut correction)) =
        (buffer(n), buffer(n), buffer(n), buffer(n), buffer(n), buffer(n), buffer(n))
    else {
        return;
    };
    let luma = if cx.gamut == filmcraft_color::Gamut::Bt2020 { [0.2627, 0.6780, 0.0593] } else { [0.2126, 0.7152, 0.0722] };
    for (i, pixel) in image.px.as_chunks::<4>().0.iter().enumerate() {
        if pixel.iter().all(|p| p.is_finite()) && pixel[3] > EPS && pixel[3] <= 1.0 {
            alpha[i] = pixel[3];
            observed[i] = ((pixel[0] * luma[0] + pixel[1] * luma[1] + pixel[2] * luma[2]) / pixel[3]).max(0.0);
        }
    }
    psf.convolve(&alpha, &mut norm, &mut scratch, image.w, image.h);
    estimate.copy_from_slice(&observed);
    for _ in 0..iterations {
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
}
