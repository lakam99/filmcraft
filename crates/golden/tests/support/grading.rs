//! Grading verification independent of saved golden images: declared-control coverage, analytic
//! light/colour invariants, and real GPU readback (not the scalar implementation of a GPU op).
use super::{
    Builder, ColorInfo, ColorSpace, DemoScene, ParamValue, Primaries, Project, RATE, RenderOptions, Rgba8, TICKS_PER_SECOND, Tick, Transfer, Vec2, device,
    diff, find_effect, render_cpu, set,
};
use filmcraft_color::WorkingSpace;
use filmcraft_render::Image;
use filmcraft_render::effects::{FxCtx, apply};

struct Case {
    id: &'static str,
    before: Vec<(&'static str, ParamValue)>,
    value: ParamValue,
    hdr: bool,
}

fn cases() -> Vec<Case> {
    use ParamValue::{Bool as B, Choice as C, Color, Curve, Float as F, Text, Vec2 as V};
    let mut out = Vec::new();
    let mut add = |id, before: &[(&'static str, ParamValue)], value, hdr| out.push(Case { id, before: before.to_vec(), value, hdr });
    for (id, value) in [
        ("temperature", 65.0),
        ("tint", 50.0),
        ("exposure", 1.0),
        ("contrast", 40.0),
        ("highlights", -60.0),
        ("shadows", 60.0),
        ("whites", -40.0),
        ("blacks", 30.0),
        ("saturation", 0.0),
        ("faded_film", 60.0),
        ("sharpen", 70.0),
        ("vibrance", -70.0),
        ("creative_sat", 0.0),
        ("wheel_shadows_l", 35.0),
        ("wheel_midtones_l", 35.0),
        ("wheel_highlights_l", -35.0),
        ("vignette_amount", -3.0),
    ] {
        add(id, &[], F(value), false);
    }
    for (id, active) in [
        ("basic_on", ("exposure", F(1.0))),
        ("creative_on", ("faded_film", F(60.0))),
        ("curves_on", ("curve_red", Curve(vec![[0.0, 0.0], [0.5, 0.8], [1.0, 1.0]]))),
        ("wheels_on", ("wheel_midtones_l", F(35.0))),
        ("vignette_on", ("vignette_amount", F(-3.0))),
    ] {
        add(id, &[active], B(false), false);
    }
    for id in ["input_lut", "look_lut"] {
        add(id, &[], Text("builtin:look-monochrome".into()), false);
    }
    add("look", &[], C(1), false);
    add("look_intensity", &[("look", C(1))], F(0.0), false);
    for id in ["shadow_tint", "highlight_tint"] {
        add(id, &[], Color([0.8, 0.35, 0.2, 1.0]), false);
    }
    for id in ["curve_luma", "curve_red", "curve_green", "curve_blue"] {
        add(id, &[], Curve(vec![[0.0, 0.0], [0.5, 0.75], [1.0, 1.0]]), false);
    }
    for id in ["hue_vs_sat", "hue_vs_hue", "hue_vs_luma", "luma_vs_sat", "sat_vs_sat"] {
        add(id, &[], Curve(vec![[0.0, 0.3], [1.0, 0.3]]), false);
    }
    for id in ["wheel_shadows", "wheel_midtones", "wheel_highlights"] {
        add(id, &[], V(Vec2::new(0.2, 0.3)), false);
    }
    let key = [("hsl_on", B(true)), ("hsl_show_mask", C(3))];
    add("hsl_on", &[("hsl_show_mask", C(3))], B(true), false);
    for (id, v) in [
        ("hsl_hue", 140.0),
        ("hsl_hue_range", 130.0),
        ("hsl_sat_min", 90.0),
        ("hsl_luma_min", 65.0),
        ("hsl_luma_max", 20.0),
        ("hsl_soft", 90.0),
        ("hsl_denoise", 80.0),
        ("hsl_blur", 80.0),
    ] {
        add(id, &key, F(v), false);
    }
    add("hsl_show_mask", &[("hsl_on", B(true))], C(3), false);
    for (id, v) in [("hsl_temp", 80.0), ("hsl_tint", 80.0), ("hsl_sat", 0.0), ("hsl_hue_shift", 90.0)] {
        add(id, &[("hsl_on", B(true))], F(v), false);
    }
    for (id, v) in [("vignette_midpoint", 20.0), ("vignette_roundness", 70.0), ("vignette_feather", 10.0)] {
        add(id, &[("vignette_amount", F(-3.0))], F(v), false);
    }
    add("hdr_white", &[("contrast", F(50.0))], F(4000.0), true);
    add("hdr_specular", &[], F(-75.0), true);
    add("curves_hdr_range", &[("curve_luma", Curve(vec![[0.0, 0.0], [0.5, 0.8], [1.0, 1.0]]))], F(4000.0), true);
    out
}

fn context(working: WorkingSpace) -> FxCtx<'static> {
    FxCtx {
        t: Tick::ZERO,
        px_scale: 1.0,
        seconds: 0.0,
        timecode: "",
        clip_name: "",
        project: None,
        env: None,
        frame_rate: 24.0,
        gamut: working.output_space().gamut(),
        working,
    }
}

/// Spatial colour/noise chart, with hard edges and red qualifier islands, in premultiplied linear
/// light. Include transparent and translucent samples as well as opaque highlights.
fn chart(hdr: bool) -> Image {
    let mut img = Image::new(96, 48);
    for (i, p) in img.px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (i % 96, i / 96);
        let a = [0.0, 0.25, 0.75, 1.0][x / 24];
        let c = if (x + y * 3) % 7 == 0 {
            [0.7, 0.025, 0.015]
        } else {
            [((x * 13 + y * 3) % 97) as f32 / 96.0, ((y * 5 + x) % 49) as f32 / 48.0, ((x * 3 + y * 7) % 97) as f32 / 96.0]
        };
        let gain = if hdr { 16.0 } else { 0.85 };
        *p = [c[0] * a * gain, c[1] * a * gain, c[2] * a * gain, a];
    }
    img
}

#[test]
fn grading_every_declared_control_changes_its_intended_image_and_preserves_alpha() {
    let all = cases();
    let tested: std::collections::BTreeSet<_> = all.iter().map(|c| c.id).collect();
    let declared: std::collections::BTreeSet<_> = find_effect("lumetri").unwrap().params.iter().map(|p| p.id).collect();
    assert_eq!(tested, declared, "New grading controls need an observable-behaviour case");
    let mut failures = Vec::new();
    for c in all {
        let original = chart(c.hdr);
        let mut e = find_effect("lumetri").unwrap().instance();
        set(&mut e, &c.before);
        let mut before = original.clone();
        let cx = context(if c.hdr { WorkingSpace::Rec2100Pq } else { WorkingSpace::Rec709 });
        apply(&mut before, &e, &cx);
        set(&mut e, &[(c.id, c.value)]);
        let mut after = original.clone();
        apply(&mut after, &e, &cx);
        assert!(after.px.iter().all(|v| v.is_finite()), "{}: non-finite output", c.id);
        for (p, src) in after.px.as_chunks::<4>().0.iter().zip(original.px.as_chunks::<4>().0) {
            assert_eq!(p[3], src[3], "{} changed alpha", c.id);
        }
        let delta = before.px.iter().zip(&after.px).map(|(a, b)| (a - b).abs()).sum::<f32>();
        eprintln!("grading {}: aggregate delta {delta:.5}", c.id);
        if delta < 0.01 {
            failures.push(c.id);
        }
    }
    assert!(failures.is_empty(), "Controls with no measurable effect: {failures:?}");
}

#[test]
fn grading_known_linear_light_and_colour_invariants() {
    let original = chart(false);
    let mut identity = original.clone();
    let mut e = find_effect("lumetri").unwrap().instance();
    apply(&mut identity, &e, &context(WorkingSpace::Rec709));
    assert!(identity.px.iter().zip(&original.px).all(|(a, b)| (a - b).abs() < 2e-5), "Default grade must be identity");
    for stops in [-1.0, 0.5, 1.0] {
        let mut img = Image::new(1, 1);
        img.px.copy_from_slice(&[0.02, 0.05, 0.1, 0.5]);
        set(&mut e, &[("exposure", ParamValue::Float(stops))]);
        apply(&mut img, &e, &context(WorkingSpace::Rec709));
        for (got, source) in img.px[..3].iter().zip([0.02, 0.05, 0.1]) {
            assert!((got - source * 2.0f32.powf(stops as f32)).abs() < 2e-5, "Exposure {stops}: {:?}", img.px);
        }
        assert_eq!(img.px[3], 0.5);
    }
    for id in ["saturation", "creative_sat"] {
        let mut e = find_effect("lumetri").unwrap().instance();
        set(&mut e, &[(id, ParamValue::Float(0.0))]);
        let mut img = original.clone();
        apply(&mut img, &e, &context(WorkingSpace::Rec709));
        for p in img.px.as_chunks::<4>().0 {
            assert!((p[0] - p[1]).abs() < 1e-5 && (p[1] - p[2]).abs() < 1e-5, "{id}: {p:?}");
        }
    }
    for (id, less) in [("temperature", 2), ("tint", 1)] {
        let mut e = find_effect("lumetri").unwrap().instance();
        set(&mut e, &[(id, ParamValue::Float(60.0))]);
        let mut img = Image::new(1, 1);
        img.px.copy_from_slice(&[0.18, 0.18, 0.18, 1.0]);
        apply(&mut img, &e, &context(WorkingSpace::Rec709));
        assert!(img.px[0] > img.px[less] + 0.02, "{id} must move grey towards its painted warm/magenta end: {:?}", img.px);
    }
    // One RGB channel curve must leave the other two untouched.
    for (id, channel) in [("curve_red", 0), ("curve_green", 1), ("curve_blue", 2)] {
        let mut e = find_effect("lumetri").unwrap().instance();
        set(&mut e, &[(id, ParamValue::Curve(vec![[0.0, 0.0], [0.5, 0.8], [1.0, 1.0]]))]);
        let mut img = original.clone();
        apply(&mut img, &e, &context(WorkingSpace::Rec709));
        for (p, src) in img.px.as_chunks::<4>().0.iter().zip(original.px.as_chunks::<4>().0) {
            for k in 0..3 {
                if k != channel {
                    assert!((p[k] - src[k]).abs() < 2e-5, "{id} changed channel {k}");
                }
            }
        }
    }
}

#[test]
fn grading_imported_lut_and_look_intensity_have_known_pixel_values() {
    let mut project = Project::new("LUT verification");
    // Red-fast .cube lattice, specified directly rather than generated by the LUT implementation.
    project.luts.push(filmcraft_project::ProjectLut {
        id: "swap".into(),
        name: "Swap red and blue".into(),
        source_path: None,
        format: "cube".into(),
        text: "LUT_3D_SIZE 2\n0 0 0\n0 0 1\n0 1 0\n0 1 1\n1 0 0\n1 0 1\n1 1 0\n1 1 1\n".into(),
    });
    for id in ["input_lut", "look_lut"] {
        for intensity in [0.0, 100.0] {
            let mut e = find_effect("lumetri").unwrap().instance();
            set(&mut e, &[(id, ParamValue::Text("lib:swap".into())), ("look_intensity", ParamValue::Float(intensity))]);
            let mut img = Image::new(1, 1);
            img.px.copy_from_slice(&[0.05, 0.1, 0.2, 0.5]);
            let cx = FxCtx { project: Some(&project), ..context(WorkingSpace::Rec709) };
            apply(&mut img, &e, &cx);
            let want = if id == "input_lut" || intensity == 100.0 { [0.2, 0.1, 0.05, 0.5] } else { [0.05, 0.1, 0.2, 0.5] };
            assert!(img.px.iter().zip(want).all(|(a, b)| (a - b).abs() < 2e-5), "{id}/{intensity}: {:?}, wanted {want:?}", img.px);
        }
    }
}

#[test]
fn grading_animated_exposure_uses_the_requested_frame_time() {
    let mut e = find_effect("lumetri").unwrap().instance();
    let exposure = e.params.get_mut("exposure").unwrap();
    exposure.put_keyframe(Tick::ZERO, ParamValue::Float(0.0));
    exposure.put_keyframe(Tick(TICKS_PER_SECOND), ParamValue::Float(1.0));
    for fraction in [0.0, 0.25, 0.5, 1.0] {
        let mut img = Image::new(1, 1);
        img.px.copy_from_slice(&[0.1, 0.1, 0.1, 1.0]);
        let cx = FxCtx { t: Tick((TICKS_PER_SECOND as f64 * fraction) as i64), ..context(WorkingSpace::Rec709) };
        apply(&mut img, &e, &cx);
        assert!((img.px[0] - 0.1 * 2.0f32.powf(fraction as f32)).abs() < 2e-5, "Frame {fraction}: {:?}", img.px);
    }
}

#[test]
fn grading_gpu_readback_matches_cpu_for_every_control() {
    // CI without a GPU may skip, but a local verification run must explicitly require readback.
    let Some((dev, queue)) = device("grading matrix", true) else {
        assert!(std::env::var_os("FILMCRAFT_REQUIRE_GRADING_GPU").is_none(), "Grading verification requires a GPU");
        return;
    };
    let mut gpu = filmcraft_gpu::GpuCompositor::new(&dev, &queue);
    let mut failures = Vec::new();
    for c in cases() {
        let mut b = Builder::new(1);
        let item = if c.hdr {
            b.p.sequence_mut(b.seq).unwrap().settings.color.working = WorkingSpace::Rec2100Pq;
            let pq = ColorInfo { transfer: Transfer::Pq, primaries: Primaries::Bt2020, ..ColorInfo::SRGB_FULL };
            b.encoded(DemoScene::OceanSunset, ColorSpace::Rec2100Pq, 8.0, Some(pq), None)
        } else {
            b.demo(DemoScene::OceanSunset)
        };
        let clip = b.place(0, item, 0, 48);
        b.effect(clip, "lumetri", &c.before);
        let effect = b.clip(clip).effects.iter_mut().find(|e| e.effect == "lumetri").unwrap();
        set(effect, &[(c.id, c.value)]);
        let scene = b.at(24);
        let cpu = render_cpu(&scene);
        let plan =
            filmcraft_render::plan::plan_frame(&scene.project, scene.seq, RATE.tick_of(scene.frame), RenderOptions::default(), gpu.caps(), &scene.sources);
        let shader = match &plan {
            filmcraft_render::plan::FramePlan::Layers { layers, .. } => {
                layers.iter().any(|l| l.fx.as_ref().is_some_and(|f| f.ops.iter().any(|op| matches!(op, filmcraft_render::gpufx::FxOp::Lumetri { .. }))))
            }
            _ => false,
        };
        if c.id == "exposure" {
            assert!(shader, "The GPU comparison must exercise the grading shader, not just upload a CPU-rendered frame");
        }
        gpu.composite(&plan);
        let (w, h, mut px) = gpu.read_output().expect("Actual GPU readback");
        for p in px.as_chunks_mut::<4>().0 {
            p[3] = 255;
        }
        let result = Rgba8::new(w, h, px);
        let d = diff(&cpu, &result).unwrap();
        eprintln!("grading {} [{}]: {d}", c.id, if shader { "grading shader" } else { "CPU grade / GPU composite" });
        if d.p99 > 6 || d.mean_abs * 4.0 / 3.0 >= 1.5 {
            failures.push(format!("{}: {d}", c.id));
        }
    }
    assert!(failures.is_empty(), "GPU grading mismatch:\n{}", failures.join("\n"));
}

#[test]
fn grading_group_gpu_fallback_matches_cpu_and_preserves_transparency() {
    use filmcraft_project::{ColorGradeGroup, GradeGroupLink, GradeReference};
    let Some((device, queue)) = device("shared grading", true) else {
        assert!(std::env::var_os("FILMCRAFT_REQUIRE_GRADING_GPU").is_none());
        return;
    };
    let mut gpu = filmcraft_gpu::GpuCompositor::new(&device, &queue);
    for strength in [0.0, 0.4, 1.0] {
        let mut b = Builder::new(1);
        let media = b.demo(DemoScene::OceanSunset);
        let clip = b.place(0, media, 0, 48);
        let group = ColorGradeGroup {
            name: "Shared verification".into(),
            enabled: true,
            working: WorkingSpace::Rec709,
            gamut: filmcraft_color::Gamut::Bt709,
            protect_skin: true,
            reference: GradeReference { log_rgb: [0.0; 3], bands: [[0.0; 4]; 3], chroma: 0.0, skin: [0.0; 3] },
            look: filmcraft_render::vibes::style("dark-gritty").unwrap(),
            look_strength: strength,
            vibe: None,
        };
        let id = b.p.alloc_id();
        b.p.color_groups.insert(id, std::sync::Arc::new(group));
        let mut effect = find_effect("grading_group").unwrap().instance();
        effect.color_group =
            Some(Box::new(GradeGroupLink { group: id, gain_rgb: [0.9, 1.1, 1.2], balance: Default::default(), distance_before: 0.0, distance_after: 0.0 }));
        b.clip(clip).effects.push(effect.clone());
        b.p.check_color_groups().unwrap();
        let original = chart(false);
        let mut working = original.clone();
        let cx = FxCtx { project: Some(&b.p), ..context(WorkingSpace::Rec709) };
        apply(&mut working, &effect, &cx);
        for (after, before) in working.px.as_chunks::<4>().0.iter().zip(original.px.as_chunks::<4>().0) {
            assert!(after.iter().all(|v| v.is_finite()));
            assert_eq!(after[3], before[3]);
        }
        let scene = b.at(24);
        let cpu = render_cpu(&scene);
        let plan =
            filmcraft_render::plan::plan_frame(&scene.project, scene.seq, RATE.tick_of(scene.frame), RenderOptions::default(), gpu.caps(), &scene.sources);
        gpu.composite(&plan);
        let (w, h, mut pixels) = gpu.read_output().unwrap();
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        let difference = diff(&cpu, &Rgba8::new(w, h, pixels)).unwrap();
        assert!(difference.p99 <= 6 && difference.mean_abs * 4.0 / 3.0 < 1.5, "strength={strength}: {difference}");
    }
}
