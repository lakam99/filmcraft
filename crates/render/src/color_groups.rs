//! A clip's automatic balance followed by a project-owned creative look. The two are separate:
//! changing the shared look never overwrites or re-solves the member's calibration.
use filmcraft_color::{Gamut, WorkingSpace};
use filmcraft_project::EffectInstance;
use filmcraft_time::Tick;

use crate::Image;
use crate::effects::{FxCtx, apply};

/// Working images are linear light with the same reference-white units. Only their primaries
/// need conversion; PQ/HLG encoding belongs to grading/display/output, not this image transform.
pub fn convert_working(img: &mut Image, from: WorkingSpace, to: WorkingSpace) {
    let gamut = |working: WorkingSpace| if working.is_hdr() { Gamut::Bt2020 } else { Gamut::Bt709 };
    convert_gamut(img, gamut(from), gamut(to));
}

pub fn convert_gamut(img: &mut Image, from: Gamut, to: Gamut) {
    if from == to {
        return;
    }
    use filmcraft_color::spaces::{apply3, gamut_matrix, to_f32};
    let matrix = to_f32(&gamut_matrix(from, to));
    for p in img.px.as_chunks_mut::<4>().0 {
        let rgb = apply3(&matrix, [p[0], p[1], p[2]]);
        p[..3].copy_from_slice(&rgb);
    }
}

pub fn apply_gain(img: &mut Image, gain: [f32; 3]) {
    for p in img.px.as_chunks_mut::<4>().0 {
        for k in 0..3 {
            p[k] *= gain[k];
        }
    }
}

/// Average exposure/color in stops rather than averaging unrelated picture positions. Ignore
/// transparent/zero channels; no amount of multiplication recovers a channel with no signal.
pub fn log_rgb(img: &Image) -> Option<[f64; 3]> {
    let mut sums = [0.0f64; 3];
    let mut counts = [0u64; 3];
    for p in img.px.as_chunks::<4>().0 {
        if !p[3].is_finite() || p[3] <= 1e-3 {
            continue;
        }
        for k in 0..3 {
            let value = p[k] / p[3];
            if value.is_finite() && value > 1e-5 {
                sums[k] += f64::from(value.min(1e6)).ln();
                counts[k] = counts[k].saturating_add(1);
            }
        }
    }
    if counts.contains(&0) {
        return None;
    }
    Some(std::array::from_fn(|k| sums[k] / counts[k] as f64))
}

pub fn gains(current: [f64; 3], reference: [f64; 3]) -> Option<[f32; 3]> {
    if !current.iter().chain(&reference).all(|v| v.is_finite()) {
        return None;
    }
    let limit = 32.0f64.ln();
    Some(std::array::from_fn(|k| (reference[k] - current[k]).clamp(-limit, limit).exp() as f32))
}

pub(crate) fn apply_group(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let Some(link) = &e.color_group else { return };
    let Some(group) = cx.project.and_then(|p| p.color_groups.get(&link.group)).filter(|g| g.enabled) else { return };
    // Shared looks are always plain Color Grading. This also prevents cycles in hostile files
    // from recursively resolving group references during rendering.
    if group.look.effect != "lumetri" || group.look.color_group.is_some() {
        return;
    }
    let Some(balance) = link.balance_effect() else { return };
    convert_gamut(img, cx.gamut, group.gamut);
    apply_gain(img, link.gain_rgb);
    let group_cx = FxCtx { t: Tick::ZERO, working: group.working, gamut: group.gamut, ..*cx };
    apply(img, &balance, &group_cx);
    let amount = group.look_strength;
    if amount.is_finite() && amount > 0.0 {
        if amount >= 1.0 {
            apply(img, &group.look, &group_cx);
        } else {
            // Blend with the balanced input, so even a subtle look retains member calibration.
            let normalized = img.px.clone();
            apply(img, &group.look, &group_cx);
            for (p, before) in img.px.as_chunks_mut::<4>().0.iter_mut().zip(normalized.as_chunks::<4>().0) {
                for k in 0..3 {
                    p[k] = before[k] + (p[k] - before[k]) * amount;
                }
                p[3] = before[3];
            }
        }
    }
    convert_gamut(img, group.gamut, cx.gamut);
}
