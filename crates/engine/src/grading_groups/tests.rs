use std::sync::Arc;

use filmcraft_media::{DemoScene, Generator, generators::GeneratorSource};
use filmcraft_project::{ClipId, Label, ParamValue};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};
use serde_json::{Value, json};

use crate::Session;

fn session() -> (Session, Vec<ClipId>) {
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"width":128,"height":72,"fps":24})).unwrap();
    let item = crate::demo::add_generator(
        Arc::make_mut(&mut s.project),
        &s.media,
        GeneratorSource::new(Generator::Demo(DemoScene::OceanSunset), 128, 72, FrameRate::FPS_24, Tick(6 * TICKS_PER_SECOND)),
        "Controlled color source",
        Label::Iris,
        None,
    );
    let mut clips = Vec::new();
    for (index, (exposure, temperature, tint)) in [(-2.0, 20.0, 20.0), (-1.0, 0.0, 0.0), (0.0, -20.0, -20.0)].into_iter().enumerate() {
        s.execute("timeline.place", json!({"item":item.0,"track":"V1","time":index as i64*2*TICKS_PER_SECOND,"duration":2*TICKS_PER_SECOND,"audio":false}))
            .unwrap();
        let clip = s.active_sequence().unwrap().video_tracks[0].items.last().unwrap().id;
        s.execute("effects.apply", json!({"clips":[clip.0],"effect":"lumetri"})).unwrap();
        for (param, value) in [("exposure", exposure), ("temperature", temperature), ("tint", tint)] {
            s.execute("effects.setParam", json!({"clip":clip.0,"effect":"lumetri","param":param,"value":value})).unwrap();
        }
        clips.push(clip);
    }
    s.state.selection = clips.clone();
    (s, clips)
}

fn frames(s: &mut Session) -> Vec<Vec<f32>> {
    (0..3)
        .map(|index| {
            s.set_playhead(Tick((index * 2 + 1) * TICKS_PER_SECOND));
            s.render_program_working(1.0).unwrap().px
        })
        .collect()
}

fn maximum_difference(images: &[Vec<f32>]) -> f32 {
    images.iter().skip(1).flat_map(|image| image.iter().zip(&images[0]).map(|(a, b)| (a - b).abs())).fold(0.0, f32::max)
}

/// Keep structural/identity changes strict while allowing the existing JSON reader's tiny f64
/// rounding. Rendered working-light pixels are checked bit-for-bit separately.
fn assert_project_roundtrip(expected: &Value, actual: &Value, path: &str) {
    if expected == actual {
        return;
    }
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            assert_eq!(expected.len(), actual.len(), "{path}: changed field count");
            for (key, value) in expected {
                let restored = actual.get(key).unwrap_or_else(|| panic!("{path}: missing {key}"));
                assert_project_roundtrip(value, restored, &format!("{path}.{key}"));
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            assert_eq!(expected.len(), actual.len(), "{path}: changed array length");
            for (index, (value, restored)) in expected.iter().zip(actual).enumerate() {
                assert_project_roundtrip(value, restored, &format!("{path}[{index}]"));
            }
        }
        (Value::Number(expected), Value::Number(actual)) if expected.is_f64() && actual.is_f64() => {
            let (original, restored) = (expected.as_f64().unwrap(), actual.as_f64().unwrap());
            let ulps = original.to_bits().abs_diff(restored.to_bits());
            assert!(original.is_finite() && restored.is_finite() && ulps <= 2, "{path}: {original} -> {restored} ({ulps} ULP)");
            eprintln!("JSON f64 roundtrip {path}: {original} -> {restored} ({ulps} ULP)");
        }
        _ => assert_eq!(expected, actual, "{path}: changed project data"),
    }
}

#[test]
fn grading_groups_match_exposure_and_white_balance_and_keep_shared_changes_uniform() {
    let (mut s, clips) = session();
    assert!(maximum_difference(&frames(&mut s)) > 0.1, "fixture must start with visibly different clips");
    let project = s.project.clone();
    let undo = s.history.undo.len();
    let result =
        s.execute("lumetri.group.create", json!({"clips":clips.iter().map(|c|c.0).collect::<Vec<_>>(),"name":"Matched scene","protectSkin":false})).unwrap();
    let group = result["group"].as_u64().unwrap();
    assert_eq!(s.history.undo.len(), undo + 1);
    assert!(maximum_difference(&frames(&mut s)) < 0.003, "calibrated footage must agree, not just share copied parameter values");
    let links =
        clips.iter().map(|clip| s.active_sequence().unwrap().find_item(*clip).unwrap().1.effects.last().unwrap().color_group.clone()).collect::<Vec<_>>();
    for edit in [json!({"group":group,"style":"warm-cozy","strength":65.0}), json!({"group":group,"brightness":25.0,"warmth":-15.0,"color":30.0})] {
        s.execute("lumetri.group.setVibe", edit).unwrap();
        assert!(maximum_difference(&frames(&mut s)) < 0.003, "shared Vibes changes must preserve member calibration");
    }
    for (param, value) in [("contrast", json!(25.0)), ("curve_red", json!([[0.0, 0.0], [0.5, 0.65], [1.0, 1.0]])), ("saturation", json!(70.0))] {
        s.execute("lumetri.group.setParam", json!({"group":group,"param":param,"value":value})).unwrap();
        assert!(maximum_difference(&frames(&mut s)) < 0.004, "advanced shared changes must preserve member calibration");
    }
    assert_eq!(
        links,
        clips.iter().map(|clip| s.active_sequence().unwrap().find_item(*clip).unwrap().1.effects.last().unwrap().color_group.clone()).collect::<Vec<_>>()
    );
    s.execute("lumetri.group.setEnabled", json!({"group":group,"enabled":false})).unwrap();
    assert!(maximum_difference(&frames(&mut s)) > 0.1, "comparison must bypass both the automatic balance and shared mood");
    s.execute("lumetri.group.setEnabled", json!({"group":group,"enabled":true})).unwrap();
    assert!(maximum_difference(&frames(&mut s)) < 0.004);
    let expected = frames(&mut s);
    let encoded = filmcraft_format::encode(&s.project, false);
    s.project = Arc::new(filmcraft_format::decode(&encoded).unwrap().project);
    assert_eq!(frames(&mut s), expected, "the shared grade and individual calibration must survive reopening");
    s.project.check_color_groups().unwrap();
    while s.history.undo.len() > undo {
        s.execute("edit.undo", json!({})).unwrap();
    }
    assert_eq!(s.project, project, "group creation and all later look changes remain independently undoable");
}

#[test]
fn grading_group_vibes_do_not_accumulate_and_a_drag_is_one_undo_step() {
    let (mut s, _) = session();
    let result = s.execute("lumetri.group.create", json!({"vibe":"nostalgic"})).unwrap();
    let id = result["group"].as_u64().unwrap();
    let baseline = s.project.color_groups.get(&id).unwrap().look.clone();
    let undo = s.history.undo.len();
    for (warmth, begin) in [(40.0, true), (60.0, false), (0.0, false)] {
        s.execute("lumetri.group.setVibe", json!({"group":id,"warmth":warmth,"merge":true,"begin":begin})).unwrap();
    }
    assert_eq!(s.history.undo.len(), undo + 1);
    assert_eq!(s.project.color_groups.get(&id).unwrap().look, baseline);
    s.execute("lumetri.group.setVibe", json!({"group":id,"warmth":20.0,"merge":true,"begin":true})).unwrap();
    assert_eq!(s.history.undo.len(), undo + 2, "a later gesture starts a new undo step");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.color_groups.get(&id).unwrap().look, baseline);
}

#[test]
fn grading_group_rebalance_preserves_the_creative_look_and_removal_is_reversible() {
    let (mut s, clips) = session();
    let result = s.execute("lumetri.group.create", json!({"vibe":"cool-tense"})).unwrap();
    let id = result["group"].as_u64().unwrap();
    s.execute("lumetri.group.setVibe", json!({"group":id,"color":-20.0,"strength":55.0})).unwrap();
    let old = s.project.color_groups.get(&id).unwrap().clone();
    s.execute("lumetri.group.balance", json!({"group":id})).unwrap();
    let current = s.project.color_groups.get(&id).unwrap();
    assert_eq!(current.look, old.look);
    assert_eq!(current.vibe, old.vibe);
    assert_eq!(current.look_strength, old.look_strength);
    let before = s.project.clone();
    s.execute("lumetri.group.removeClips", json!({"group":id,"clips":[clips[0].0]})).unwrap();
    let clip = s.active_sequence().unwrap().find_item(clips[0]).unwrap().1;
    assert!(clip.effects.iter().all(|e| e.color_group.is_none()));
    assert!(clip.effects.iter().any(|e| e.effect == "lumetri"), "underlying clip grades must remain");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
    s.execute("lumetri.group.delete", json!({"group":id})).unwrap();
    assert!(s.project.color_groups.is_empty());
    s.project.check_color_groups().unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
}

#[test]
fn grading_group_rejects_hostile_requests_without_partial_edits() {
    let (mut s, clips) = session();
    let before = s.project.clone();
    for args in [json!({"clips":[]}), json!({"clips":[-1]}), json!({"clips":[u64::MAX]}), json!({"vibe":"missing-style"})] {
        assert!(s.execute("lumetri.group.create", args).is_err());
        assert_eq!(s.project, before);
    }
    let result = s.execute("lumetri.group.create", json!({"clips":[clips[0].0,clips[0].0,clips[1].0]})).unwrap();
    let id = result["group"].as_u64().unwrap();
    assert_eq!(result["members"], 2, "selection-derived membership is deduplicated");
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let stage = s.active_sequence().unwrap().find_item(clips[0]).unwrap().1.effects.len() - 1;
    for (cmd, args) in [
        ("lumetri.group.create", json!({"clips":[clips[0].0]})),
        ("lumetri.group.setParam", json!({"group":id,"param":"exposure","value":999.0})),
        ("lumetri.group.setParam", json!({"group":id,"param":"look","value":9999})),
        ("lumetri.group.setParam", json!({"group":id,"param":"input_lut","value":"missing-lut"})),
        ("lumetri.group.setParam", json!({"group":id,"param":"wheel_midtones","value":[1e300,0]})),
        ("lumetri.group.setVibe", json!({"group":id,"warmth":101.0,"begin":true})),
        ("lumetri.group.setVibe", json!({"group":id,"style":"unknown"})),
        ("lumetri.group.setVibe", json!({"group":id,"style":10})),
        ("lumetri.group.setVibe", json!({"group":id})),
        ("lumetri.group.setEnabled", json!({"group":id,"enabled":"false"})),
        ("effects.apply", json!({"clips":[clips[0].0],"effect":"grading_group"})),
        ("presets.save", json!({"clip":clips[0].0,"effects":[stage],"name":"Unsupported project link"})),
    ] {
        assert!(s.execute(cmd, args.clone()).is_err(), "{cmd} {args}");
        assert_eq!(s.project, before, "{cmd} left a partial group edit");
        assert_eq!(s.history.undo.len(), undo);
    }
    let link = s.active_sequence().unwrap().find_item(clips[0]).unwrap().1.effects.last().unwrap().color_group.as_ref().unwrap();
    assert!(matches!(link.balance.get("saturation").map(|p| &p.value), Some(ParamValue::Float(_))));
}

#[test]
fn grading_group_matching_handles_hdr_and_wide_gamut_working_images() {
    use filmcraft_color::{ColorPipeline, WorkingSpace};
    for (working, wide_gamut) in [(WorkingSpace::Rec709, true), (WorkingSpace::Rec2100Pq, false), (WorkingSpace::Rec2100Hlg, false)] {
        let (mut s, _) = session();
        let sequence = s.state.active_sequence.unwrap();
        Arc::make_mut(&mut s.project).sequence_mut(sequence).unwrap().settings.color = ColorPipeline { working, wide_gamut, auto_tone_map: true };
        let before = maximum_difference(&frames(&mut s));
        s.execute("lumetri.group.create", json!({"protectSkin":false})).unwrap();
        let images = frames(&mut s);
        let after = maximum_difference(&images);
        assert!(images.iter().flatten().all(|v| v.is_finite()));
        assert!(after < before * 0.05, "{working:?}/{wide_gamut}: before={before}, after={after}");
        let encoded = filmcraft_format::encode(&s.project, false);
        let original = s.project.clone();
        let restored = filmcraft_format::decode(&encoded).unwrap().project;
        s.project = Arc::new(restored);
        assert_eq!(frames(&mut s), images, "reopening must preserve the actual working-light render");
        assert_project_roundtrip(&serde_json::to_value(&*original).unwrap(), &serde_json::to_value(&*s.project).unwrap(), "project");
    }
}

#[test]
fn grading_group_copy_retains_membership_and_corrupt_group_projects_are_rejected() {
    let (mut s, clips) = session();
    let result = s.execute("lumetri.group.create", json!({"clips":[clips[0].0]})).unwrap();
    let id = result["group"].as_u64().unwrap();
    s.state.selection = vec![clips[0]];
    s.execute("edit.copy", json!({})).unwrap();
    s.set_playhead(Tick(7 * TICKS_PER_SECOND));
    s.execute("edit.paste", json!({})).unwrap();
    assert_eq!(super::members(&s.project, id).count(), 2);
    s.project.check_color_groups().unwrap();
    let original = s.project.clone();
    for mutation in 0..4 {
        let mut damaged = (*original).clone();
        match mutation {
            0 => {
                damaged.color_groups.clear();
            }
            1 => {
                Arc::make_mut(damaged.color_groups.get_mut(&id).unwrap()).look.color_group = Some(Box::new(filmcraft_project::GradeGroupLink {
                    group: id,
                    gain_rgb: [1.0; 3],
                    balance: Default::default(),
                    distance_before: 0.0,
                    distance_after: 0.0,
                }));
            }
            2 => {
                Arc::make_mut(damaged.color_groups.get_mut(&id).unwrap()).look.params.get_mut("curve_red").unwrap().value =
                    ParamValue::Curve(vec![[0.0, 0.0]; 513]);
            }
            _ => {
                Arc::make_mut(damaged.color_groups.get_mut(&id).unwrap()).look.params.get_mut("exposure").unwrap().value = ParamValue::Float(1000.0);
            }
        }
        let bytes = filmcraft_format::encode(&damaged, false);
        let outcome = std::panic::catch_unwind(|| filmcraft_format::decode(&bytes));
        assert!(outcome.is_ok(), "malformed group file panicked");
        assert!(outcome.unwrap().is_err(), "malformed group file accepted: {mutation}");
    }
}

#[test]
fn grading_group_add_and_explicit_regroup_preserve_other_members_and_undo() {
    let (mut s, clips) = session();
    let id = s.execute("lumetri.group.create", json!({"clips":[clips[0].0,clips[1].0],"vibe":"nostalgic"})).unwrap()["group"].as_u64().unwrap();
    let original = s.project.clone();
    let look = original.color_groups.get(&id).unwrap().look.clone();
    s.execute("lumetri.group.addClips", json!({"group":id,"clips":[clips[2].0]})).unwrap();
    assert_eq!(super::members(&s.project, id).count(), 3);
    assert_eq!(s.project.color_groups.get(&id).unwrap().look, look);
    assert!(maximum_difference(&frames(&mut s)) < 0.004);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, original);
    let new = s.execute("lumetri.group.create", json!({"clips":[clips[0].0,clips[2].0],"replaceGroups":true,"vibe":"cool-tense"})).unwrap()["group"]
        .as_u64()
        .unwrap();
    assert_ne!(new, id);
    assert_eq!(super::members(&s.project, id).collect::<Vec<_>>(), vec![(s.state.active_sequence.unwrap(), clips[1])]);
    assert_eq!(super::members(&s.project, new).count(), 2);
    assert_eq!(s.project.color_groups.get(&id).unwrap().look, look);
    s.execute("lumetri.group.setVibe", json!({"group":new,"brightness":40})).unwrap();
    assert_eq!(s.project.color_groups.get(&id).unwrap().look, look);
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, original);
}
