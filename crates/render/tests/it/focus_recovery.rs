//! Independent forward-blur fixtures: restoring the known sharp picture must reduce error.
use filmcraft_project::{ParamValue, find_effect};
use filmcraft_render::{
    Image,
    effects::{FxCtx, apply},
};
use filmcraft_time::Tick;

fn context(scale: f32) -> FxCtx<'static> {
    FxCtx {
        t: Tick::ZERO,
        px_scale: scale,
        seconds: 0.0,
        timecode: "",
        clip_name: "",
        project: None,
        env: None,
        frame_rate: 24.0,
        working: filmcraft_color::WorkingSpace::Rec709,
        gamut: filmcraft_color::Gamut::Bt709,
    }
}
fn recover(image: &mut Image, params: &[(&str, f64)], scale: f32) {
    let mut effect = find_effect("focus_recovery").expect("Focus Recovery must be discoverable").instance();
    for (id, value) in params {
        effect.params.get_mut(*id).unwrap().value = ParamValue::Float(*value);
    }
    apply(image, &effect, &context(scale));
}

fn sharp() -> Image {
    let mut image = Image::filled(80, 48, [0.15, 0.15, 0.15, 1.0]);
    for y in 0..image.h {
        for x in 0..image.w {
            let v = 0.35 + 0.14 * ((x as f32 * 0.36).sin()) + 0.12 * ((y as f32 * 0.45).cos());
            image.px[(y * image.w + x) * 4..(y * image.w + x) * 4 + 3].fill(v);
        }
    }
    image
}
fn blurred(source: &Image, sigma: f32) -> Image {
    // A separately implemented 2D Gaussian forward model, not the restoration's convolution.
    let mut image = source.clone();
    for y in 0..source.h {
        for x in 0..source.w {
            let (mut sum, mut weights) = (0.0, 0.0);
            for dy in -5i32..=5 {
                for dx in -5i32..=5 {
                    let weight = (-((dx * dx + dy * dy) as f32) / (2.0 * sigma * sigma)).exp();
                    let xx = (x as i32 + dx).clamp(0, source.w as i32 - 1) as usize;
                    let yy = (y as i32 + dy).clamp(0, source.h as i32 - 1) as usize;
                    sum += source.get(xx, yy)[0] * weight;
                    weights += weight;
                }
            }
            image.px[(y * source.w + x) * 4..(y * source.w + x) * 4 + 3].fill(sum / weights);
        }
    }
    image
}
fn error(a: &Image, b: &Image) -> f64 {
    let mut sum = 0.0;
    for y in 8..a.h - 8 {
        for x in 8..a.w - 8 {
            sum += (a.get(x, y)[0] - b.get(x, y)[0]).powi(2) as f64;
        }
    }
    sum
}
#[test]
fn focus_recovery_reduces_known_optical_blur_instead_of_only_boosting_contrast() {
    let original = sharp();
    let soft = blurred(&original, 1.25);
    let mut restored = soft.clone();
    recover(&mut restored, &[("radius", 2.5), ("amount", 100.0), ("noise", 0.0), ("halo", 0.0)], 1.0);
    assert!(error(&original, &restored) < error(&original, &soft) * 0.35, "restored {}, blurred {}", error(&original, &restored), error(&original, &soft));
    assert!(restored.px.iter().all(|v| v.is_finite()));
}

#[test]
fn focus_recovery_zero_strength_and_disabled_are_exact_bypass() {
    let original = sharp();
    let mut image = original.clone();
    recover(&mut image, &[("amount", 0.0)], 1.0);
    assert_eq!(image, original);
    let mut effect = find_effect("focus_recovery").unwrap().instance();
    effect.enabled = false;
    apply(&mut image, &effect, &context(1.0));
    assert_eq!(image, original);
}

#[test]
fn focus_recovery_evaluates_strength_keyframes_at_media_time() {
    let mut effect = find_effect("focus_recovery").unwrap().instance();
    let strength = effect.params.get_mut("amount").unwrap();
    strength.put_keyframe(Tick::ZERO, ParamValue::Float(0.0));
    strength.put_keyframe(Tick(254_016_000_000), ParamValue::Float(100.0));
    let original = blurred(&sharp(), 1.25);
    let mut image = original.clone();
    apply(&mut image, &effect, &context(1.0));
    assert_eq!(image, original);
    let mut later = context(1.0);
    later.t = Tick(254_016_000_000);
    apply(&mut image, &effect, &later);
    assert_ne!(image, original);
}

#[test]
fn focus_recovery_noise_and_halo_guards_bound_artifacts() {
    let mut noise = Image::filled(80, 48, [0.2, 0.2, 0.2, 1.0]);
    for (i, p) in noise.px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        // Correlated camera/compression noise in recoverable spatial frequencies. White noise
        // above the PSF cutoff need not be amplified even without damping.
        let value = 0.2 + ((i % 80) as f32 * 0.9).sin() * 0.004 + ((i / 80) as f32 * 0.7).sin() * 0.001;
        p[..3].fill(value);
    }
    let mut raw = noise.clone();
    let mut protected = noise.clone();
    recover(&mut raw, &[("amount", 100.0), ("noise", 0.0), ("halo", 0.0)], 1.0);
    recover(&mut protected, &[("amount", 100.0), ("noise", 100.0), ("halo", 0.0)], 1.0);
    let variance = |image: &Image| image.px.as_chunks::<4>().0.iter().map(|p| (p[0] - 0.2).powi(2) as f64).sum::<f64>();
    assert!(variance(&protected) < variance(&raw) * 0.6, "noise guard: protected {}, raw {}", variance(&protected), variance(&raw));
    let mut edge = Image::filled(80, 48, [0.2, 0.2, 0.2, 1.0]);
    for y in 0..edge.h {
        for x in 40..edge.w {
            edge.px[(y * 80 + x) * 4..(y * 80 + x) * 4 + 3].fill(0.7);
        }
    }
    let mut edge = blurred(&edge, 1.25);
    recover(&mut edge, &[("amount", 100.0), ("noise", 0.0), ("halo", 100.0)], 1.0);
    assert!(edge.px.as_chunks::<4>().0.iter().all(|p| p[0] >= 0.2 - 1e-6 && p[0] <= 0.7 + 1e-6));
}

#[test]
fn focus_recovery_preserves_coverage_color_ratios_flat_borders_and_hdr() {
    let mut image = Image::filled(31, 19, [0.0, 0.0, 0.0, 0.0]);
    for y in 3..16 {
        for x in 3..28 {
            let alpha = if x % 2 == 0 { 0.4 } else { 1.0 };
            image.px[(y * 31 + x) * 4..(y * 31 + x) * 4 + 4].copy_from_slice(&[2.0 * alpha, 0.8 * alpha, -0.1 * alpha, alpha]);
        }
    }
    let original = image.clone();
    recover(&mut image, &[], 1.0);
    for (a, b) in image.px.as_chunks::<4>().0.iter().zip(original.px.as_chunks::<4>().0.iter()) {
        assert_eq!(a[3].to_bits(), b[3].to_bits());
        for c in 0..3 {
            assert!((a[c] - b[c]).abs() < 1e-5, "flat alpha edge changed: {a:?} {b:?}");
        }
    }
    let mut image = blurred(&sharp(), 1.25);
    for p in image.px.as_chunks_mut::<4>().0.iter_mut() {
        p[0] *= 6.0;
        p[1] *= 3.0;
        p[2] *= 1.5;
    }
    let before = image.clone();
    recover(&mut image, &[], 1.0);
    assert!(image.px.as_chunks::<4>().0.iter().any(|p| p[0] > 1.0));
    for (a, b) in image.px.as_chunks::<4>().0.iter().zip(before.px.as_chunks::<4>().0.iter()) {
        assert_eq!(a[3], b[3]);
        assert!((a[0] / a[1] - b[0] / b[1]).abs() < 1e-5);
        assert!((a[1] / a[2] - b[1] / b[2]).abs() < 1e-5);
    }
    let mut tiny = Image::filled(1, 1, [0.4, 0.2, 0.1, 1.0]);
    let flat = tiny.clone();
    recover(&mut tiny, &[], 1.0);
    assert!(tiny.px.iter().zip(flat.px).all(|(a, b)| (a - b).abs() < 1e-5));
}

#[test]
fn focus_recovery_scales_to_preview_and_is_deterministic() {
    let original = sharp();
    for scale in [1.0, 0.5, 0.25] {
        let soft = blurred(&original, 1.25 * scale);
        let mut a = soft.clone();
        let mut b = soft.clone();
        recover(&mut a, &[("amount", 100.0), ("noise", 0.0), ("halo", 0.0)], scale);
        recover(&mut b, &[("amount", 100.0), ("noise", 0.0), ("halo", 0.0)], scale);
        assert_eq!(a, b, "random or frame-dependent recovery");
        assert!(error(&original, &a) < error(&original, &soft) * 0.5, "scale {scale}");
    }
}

#[test]
fn focus_recovery_circular_model_restores_a_known_defocus_disc() {
    let original = sharp();
    let mut soft = original.clone();
    for y in 0..original.h {
        for x in 0..original.w {
            let (mut sum, mut count) = (0.0, 0.0);
            for dy in -3i32..=3 {
                for dx in -3i32..=3 {
                    if dx * dx + dy * dy <= 6 {
                        let xx = (x as i32 + dx).clamp(0, 79) as usize;
                        let yy = (y as i32 + dy).clamp(0, 47) as usize;
                        sum += original.get(xx, yy)[0];
                        count += 1.0;
                    }
                }
            }
            soft.px[(y * 80 + x) * 4..(y * 80 + x) * 4 + 3].fill(sum / count);
        }
    }
    let mut restored = soft.clone();
    let mut effect = find_effect("focus_recovery").unwrap().instance();
    for (id, value) in [("amount", 100.0), ("radius", 2.5), ("noise", 0.0), ("halo", 0.0)] {
        effect.params.get_mut(id).unwrap().value = ParamValue::Float(value);
    }
    effect.params.get_mut("model").unwrap().value = ParamValue::Choice(1);
    apply(&mut restored, &effect, &context(1.0));
    assert!(error(&original, &restored) < error(&original, &soft) * 0.4);
}

#[test]
fn focus_recovery_hostile_images_and_parameters_never_panic_or_partially_edit() {
    for (name, value) in [("radius", f64::NAN), ("amount", f64::INFINITY), ("noise", f64::NEG_INFINITY)] {
        let mut image = sharp();
        let before = image.clone();
        recover(&mut image, &[(name, value)], 1.0);
        assert_eq!(image, before);
    }
    for mut image in [Image { w: usize::MAX, h: 2, px: vec![] }, Image { w: 3, h: 2, px: vec![0.2; 4] }, Image::new(0, 0)] {
        let before = image.clone();
        recover(&mut image, &[], 1.0);
        assert_eq!(image, before);
    }
    let mut image = sharp();
    image.px[11] = f32::NAN;
    let before: Vec<_> = image.px.iter().map(|p| p.to_bits()).collect();
    recover(&mut image, &[], 1.0);
    assert_eq!(before, image.px.iter().map(|p| p.to_bits()).collect::<Vec<_>>());
}

#[test]
#[ignore = "local private reference frames; set FILMCRAFT_FOCUS_FIXTURE_DIR and FILMCRAFT_FOCUS_OUTPUT_DIR"]
fn focus_recovery_local_reference_frames() {
    let input = std::path::PathBuf::from(std::env::var_os("FILMCRAFT_FOCUS_FIXTURE_DIR").expect("fixture directory"));
    let output = std::path::PathBuf::from(std::env::var_os("FILMCRAFT_FOCUS_OUTPUT_DIR").expect("output directory"));
    std::fs::create_dir_all(&output).unwrap();
    let mut files: Vec<_> = std::fs::read_dir(input).unwrap().map(|p| p.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "png")).collect();
    files.sort();
    assert!(!files.is_empty() && files.len() <= 24);
    for file in files {
        let frame = image::open(&file).unwrap().to_rgba8();
        let mut image = Image::new(frame.width() as usize, frame.height() as usize);
        for (p, source) in image.px.as_chunks_mut::<4>().0.iter_mut().zip(frame.pixels()) {
            for c in 0..3 {
                p[c] = filmcraft_color::srgb_to_linear(source.0[c] as f32 / 255.0);
            }
            p[3] = 1.0;
        }
        let started = std::time::Instant::now();
        recover(&mut image, &[], 1.0);
        assert!(image.px.iter().all(|v| v.is_finite()));
        let bytes: Vec<u8> = image
            .px
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| {
                [
                    (filmcraft_color::linear_to_srgb(p[0]).clamp(0.0, 1.0) * 255.0).round() as u8,
                    (filmcraft_color::linear_to_srgb(p[1]).clamp(0.0, 1.0) * 255.0).round() as u8,
                    (filmcraft_color::linear_to_srgb(p[2]).clamp(0.0, 1.0) * 255.0).round() as u8,
                    255,
                ]
            })
            .collect();
        let path = output.join(file.file_name().unwrap());
        image::save_buffer(&path, &bytes, image.w as u32, image.h as u32, image::ColorType::Rgba8).unwrap();
        eprintln!("{}: {:?}, {}", file.file_name().unwrap().to_string_lossy(), started.elapsed(), path.display());
    }
}
