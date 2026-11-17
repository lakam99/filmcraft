//! File ▸ Export ▸ AAF / OMF and their import, end to end with real media written by our encoders.

use std::sync::Arc;

use filmcraft_project::{ItemId, ItemKind, MediaRef, Param, ParamValue, TrackKind};
use filmcraft_time::FrameRate;
use serde_json::json;

use crate::Session;
use crate::media_test_util::{make_movie, session_with, tmp_dir};

/// Two 2-second ProRes + PCM movies cut back to back on V1/A1 (24 fps).
fn setup(name: &str) -> (Session, Vec<ItemId>, ItemId, std::path::PathBuf) {
    let dir = tmp_dir(name);
    let a = dir.join("a.mov");
    let b = dir.join("b.mov");
    make_movie(&a, filmcraft_media::DemoScene::Aurora, 64, 36, 48);
    make_movie(&b, filmcraft_media::DemoScene::Plasma, 64, 36, 48);
    let (mut s, items, seq) = session_with(&[&a, &b]);
    // trim both clips so handles exist: A plays media 12..36, B 6..30
    let rate = FrameRate::FPS_24;
    let mut p = (*s.project).clone();
    let q = p.sequence_mut(seq).unwrap();
    for t in q.all_tracks_mut() {
        for (i, c) in t.items.iter_mut().enumerate() {
            c.start = rate.tick_of(i as i64 * 24);
            c.source_in = rate.tick_of(if i == 0 { 12 } else { 6 });
            c.duration = rate.tick_of(24);
        }
    }
    s.project = Arc::new(p);
    (s, items, seq, dir)
}

fn audio_of(s: &Session, item: ItemId, start: i64, n: usize) -> Vec<Vec<f32>> {
    let src = s.media.full_res_source(&s.project, item, &*s.services).expect("online");
    src.audio(start, n, 48_000).unwrap().channels
}

fn imported_seq(r: &serde_json::Value) -> ItemId {
    let docs = r["documents"].as_array().cloned().unwrap_or_default();
    let seq = r["sequences"][0].as_u64().or_else(|| docs.first().and_then(|d| d["sequences"][0].as_u64())).unwrap_or_else(|| panic!("no sequence in {r}"));
    ItemId(seq)
}

fn path_of(s: &Session, item: ItemId) -> String {
    match &s.project.item(item).unwrap().kind {
        ItemKind::Media(m) => match &m.media {
            MediaRef::File { path } => path.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// AAF must reproduce the mixer output, rather than merely retain clip metadata.
fn assert_aaf_mix_matches(s: &mut Session, seq: ItemId, path: &std::path::Path) -> serde_json::Value {
    let frames = 96_000;
    let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
    let want = filmcraft_render::mixer::mix_graph(&s.project, s.project.sequence(seq).unwrap(), 0, frames, &provider, None);
    let before = serde_json::to_value(&s.project).unwrap();
    let result = s.execute("file.exportAaf", json!({"path":path,"sequence":seq.0})).unwrap();
    assert_eq!(result["preserveMix"], true, "the command defaults to matching playback");
    assert_eq!(serde_json::to_value(&s.project).unwrap(), before, "export cannot alter the editor project");
    let imported = s.execute("file.importAaf", json!({"path":path})).unwrap();
    let q = s.project.sequence(imported_seq(&imported)).unwrap();
    let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
    let got = filmcraft_render::mixer::mix_graph(&s.project, q, 0, frames, &provider, None);
    let error = want.channels.iter().zip(&got.channels).flat_map(|(a, b)| a.iter().zip(b).map(|(a, b)| (a - b).abs())).fold(0.0f32, f32::max);
    assert!(want.channels.iter().flatten().any(|v| v.abs() > 0.001));
    assert!(error < 2e-6, "full mix sample error {error}");
    result
}

#[test]
fn aaf_preserves_track_gain_mute_pan_and_sample_grid_without_flattening_edits() {
    use filmcraft_project::{AudioChannels, TrackId};
    use filmcraft_time::Tick;
    let (mut s, _, seq, dir) = setup("aaf-mixer-fidelity");
    let mut p = (*s.project).clone();
    let q = p.sequence_mut(seq).unwrap();
    let mut muted = q.audio_tracks[0].clone();
    muted.id = TrackId(900_001);
    muted.muted = true;
    for c in &mut muted.items {
        c.id.0 += 900_000;
    }
    let t = &mut q.audio_tracks[0];
    t.volume_db = -13.067522525787354;
    t.pan = -35.0;
    t.channels = AudioChannels::Mono;
    t.items[0].start = Tick::from_units(130, 48_000) + Tick::from_units(3, 480_000);
    let first_start = t.items[0].start;
    t.items[0].duration -= first_start;
    t.items[0].gain_db = 4.0;
    q.audio_tracks.push(muted);
    q.master_volume_db = -2.0;
    s.project = Arc::new(p);
    assert_aaf_mix_matches(&mut s, seq, &dir.join("mix.aaf"));
    let q = s.active_sequence().unwrap();
    assert_eq!(q.audio_tracks.len(), 2);
    assert_eq!(q.audio_tracks[0].items.len(), 2, "editable events survive a simple mixer");
    assert_eq!(q.audio_tracks[0].channels, AudioChannels::Stereo, "printed pan must not be panned twice");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_prints_nonportable_master_processing_once_after_summing() {
    use filmcraft_project::{TrackId, find_effect};
    let (mut s, _, seq, dir) = setup("aaf-master-fidelity");
    let mut p = (*s.project).clone();
    let q = p.sequence_mut(seq).unwrap();
    let mut other = q.audio_tracks[0].clone();
    other.id = TrackId(900_002);
    for c in &mut other.items {
        c.id.0 += 800_000;
    }
    q.audio_tracks.push(other);
    let effect = find_effect("hard_limiter").expect("a dynamics effect");
    let mut limiter = effect.instance();
    limiter.params.insert("boost".into(), Param::new(ParamValue::Float(20.0)));
    q.master_effects.push(limiter);
    s.project = Arc::new(p);
    let result = assert_aaf_mix_matches(&mut s, seq, &dir.join("master.aaf"));
    assert_eq!(result["audioLayout"], "mixdown");
    assert_eq!(s.active_sequence().unwrap().audio_tracks.len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_mix_matching_preserves_solo_and_sequence_time_automation() {
    use filmcraft_project::{TrackId, mixer::InputMap};
    use filmcraft_time::Tick;
    let (mut s, _, seq, dir) = setup("aaf-mix-automation");
    let mut p = (*s.project).clone();
    let q = p.sequence_mut(seq).unwrap();
    let mut other = q.audio_tracks[0].clone();
    other.id = TrackId(900_003);
    for c in &mut other.items {
        c.id.0 += 700_000;
    }
    q.audio_tracks.push(other);
    let t = &mut q.audio_tracks[0];
    t.solo = true;
    t.mixer.input_map = InputMap::Left;
    for (lane, a, b) in [("volume", -15.0, 3.0), ("pan", -75.0, 60.0), ("mute", 0.0, 1.0)] {
        let mut param = Param::new(ParamValue::Float(a));
        param.put_keyframe(Tick::ZERO, ParamValue::Float(a));
        param.put_keyframe(Tick::from_units(72_000, 48_000), ParamValue::Float(b));
        t.mixer.lanes.insert(lane.into(), param);
    }
    s.project = Arc::new(p);
    assert_aaf_mix_matches(&mut s, seq, &dir.join("automation.aaf"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_mix_matching_rejects_mono_breakout_before_writing() {
    let (mut s, _, seq, dir) = setup("aaf-mix-breakout");
    let path = dir.join("must-not-exist.aaf");
    let error = s.execute("file.exportAaf", json!({"path":path,"sequence":seq.0,"breakoutToMono":true})).unwrap_err();
    assert!(error.to_string().contains("mono pan law"));
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_mix_matching_keeps_prepared_generator_audio() {
    let (mut s, items, seq, dir) = setup("aaf-mix-generator");
    let mut p = (*s.project).clone();
    p.item_mut(items[0]).unwrap().as_media_mut().unwrap().media = MediaRef::Generator(filmcraft_media::Generator::Demo(filmcraft_media::DemoScene::Aurora));
    s.project = Arc::new(p);
    assert_aaf_mix_matches(&mut s, seq, &dir.join("generator.aaf"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_mix_matching_bounds_complete_prints_before_allocating_or_writing() {
    use filmcraft_project::find_effect;
    let (mut s, _, seq, dir) = setup("aaf-mix-budget");
    let mut p = (*s.project).clone();
    let q = p.sequence_mut(seq).unwrap();
    q.audio_tracks[0].items[0].duration = filmcraft_time::Tick::from_units(100_000_000, 48_000);
    q.master_effects.push(find_effect("hard_limiter").unwrap().instance());
    s.project = Arc::new(p);
    let path = dir.join("must-not-exist.aaf");
    let error = s.execute("file.exportAaf", json!({"path":path,"sequence":seq.0})).unwrap_err();
    assert!(error.to_string().contains("sample budget"));
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_retimed_audio_matches_the_original_clip_samples() {
    use filmcraft_time::Tick;
    for (speed, reverse, pitch, handles) in
        [(0.5, false, false, 0), (1.5, false, false, 6), (0.85, false, true, 0), (1.25, false, true, 6), (0.5, true, false, 6), (1.5, true, true, 6)]
    {
        let (mut s, _, seq, dir) = setup("aaf-retimed-samples");
        let mut p = (*s.project).clone();
        let track = &mut p.sequence_mut(seq).unwrap().audio_tracks[0];
        track.items.truncate(1);
        track.transitions.clear();
        let c = &mut track.items[0];
        c.start = Tick::from_units(130, 48_000);
        c.speed = speed;
        c.reverse = reverse;
        c.maintain_pitch = pitch;
        c.gain_db = -3.0;
        let start = c.start.to_units_floor(48_000);
        let frames = c.duration.to_units_floor(48_000) as usize;
        s.project = Arc::new(p);
        let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
        let wanted = filmcraft_render::audio::track_input(&s.project, &s.project.sequence(seq).unwrap().audio_tracks[0], start, frames, 48_000, &provider);
        assert!(wanted.channels.iter().flatten().any(|v| v.abs() > 0.01));
        let path = dir.join("retimed.aaf").to_string_lossy().into_owned();
        s.execute(
            "file.exportAaf",
            json!({"preserveMix":false,"path":path,"sequence":seq.0,"audio":"embedded","trimAudio":true,"handles":handles,"bitDepth":24}),
        )
        .unwrap();
        let imported = s.execute("file.importAaf", json!({"path":path})).unwrap();
        let imported = imported_seq(&imported);
        let q = s.project.sequence(imported).unwrap();
        let c = &q.audio_tracks[0].items[0];
        assert_eq!((c.start.to_units_floor(48_000), c.duration.to_units_floor(48_000)), (start, frames as i64));
        assert_eq!((c.speed, c.reverse), (1.0, false), "timing is baked into portable PCM");
        let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
        let got = filmcraft_render::audio::track_input(&s.project, &q.audio_tracks[0], start, frames, 48_000, &provider);
        let error = wanted.channels.iter().zip(&got.channels).flat_map(|(a, b)| a.iter().zip(b).map(|(a, b)| (a - b).abs())).fold(0.0f32, f32::max);
        assert!(error < 2e-6, "speed={speed}, reverse={reverse}, maintain pitch={pitch}, handles={handles}: max sample error {error}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Compare audible samples, rather than merely checking that speed flags disappeared.
fn assert_aaf_audio_matches(s: &mut Session, seq: ItemId, mut params: serde_json::Value, command: &str) {
    params["preserveMix"] = json!(false);
    let original = s.project.sequence(seq).unwrap().audio_tracks[0].clone();
    let start = original.items.iter().map(|c| c.start.to_units_floor(48_000)).min().unwrap();
    let end = original.items.iter().map(|c| c.end().to_units_floor(48_000)).max().unwrap();
    let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
    let wanted = filmcraft_render::audio::track_input(&s.project, &original, start, (end - start) as usize, 48_000, &provider);
    let result = s.execute(command, params.clone()).unwrap();
    assert!(result["report"].to_string().contains("retimed audio"), "{result}");
    let result = s.execute("file.importAaf", json!({"path":params["path"]})).unwrap();
    let q = s.project.sequence(imported_seq(&result)).unwrap();
    let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
    let got = filmcraft_render::audio::track_input(&s.project, &q.audio_tracks[0], start, (end - start) as usize, 48_000, &provider);
    assert_eq!(wanted.channels.len(), got.channels.len());
    let peak = wanted.channels.iter().flatten().fold(0.0f32, |a, b| a.max(b.abs()));
    let error = wanted
        .channels
        .iter()
        .zip(&got.channels)
        .flat_map(|(a, b)| {
            assert_eq!(a.len(), b.len());
            a.iter().zip(b).map(|(a, b)| (a - b).abs())
        })
        .fold(0.0f32, f32::max);
    assert!(peak > 0.01 && error < peak.max(1.0) * 2e-6, "peak {peak}, max sample error {error}");
}

#[test]
fn aaf_retiming_preserves_automated_hot_audio_and_linked_straight_clips() {
    use filmcraft_time::Tick;
    for mode in ["embedded", "separate", "linked"] {
        let (mut s, items, seq, dir) = setup("aaf-retimed-gain");
        let mut p = (*s.project).clone();
        let c = &mut p.sequence_mut(seq).unwrap().audio_tracks[0].items[0];
        c.start = Tick::from_units(130, 48_000);
        c.duration -= c.start;
        c.speed = 0.85;
        c.maintain_pitch = true;
        c.gain_db = 20.0;
        let mt = c.source_in;
        let level = c.effect_mut("volume").unwrap().params.get_mut("level").unwrap();
        level.put_keyframe(mt, ParamValue::Float(-4.0));
        level.put_keyframe(mt + Tick::from_units(24_000, 48_000), ParamValue::Float(6.0));
        s.project = Arc::new(p);
        let path = dir.join("hot.aaf").to_string_lossy().into_owned();
        assert_aaf_audio_matches(&mut s, seq, json!({"path":path,"sequence":seq.0,"audio":mode,"trimAudio":false,"handles":6,"bitDepth":24}), "file.exportAaf");
        if mode == "linked" {
            let imported = s.active_sequence().unwrap();
            let straight = &imported.audio_tracks[0].items[1];
            assert_eq!(
                std::path::Path::new(&path_of(&s, straight.item)),
                std::path::Path::new(&path_of(&s, items[1])),
                "straight clips keep their linked source"
            );
            assert!(path_of(&s, imported.audio_tracks[0].items[0].item).ends_with(".wav"));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn aaf_generic_export_renders_retimed_audio() {
    let (mut s, _, seq, dir) = setup("aaf-generic-retiming");
    let mut p = (*s.project).clone();
    let track = &mut p.sequence_mut(seq).unwrap().audio_tracks[0];
    track.items.truncate(1);
    track.items[0].speed = 0.85;
    track.items[0].maintain_pitch = true;
    s.project = Arc::new(p);
    let path = dir.join("generic.aaf").to_string_lossy().into_owned();
    assert_aaf_audio_matches(&mut s, seq, json!({"format":"aaf","path":path,"sequence":seq.0,"bitDepth":24}), "file.exportInterchange");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_retimed_nested_audio_matches_playback() {
    let (mut s, _, seq, _, dir) = setup_nested("aaf-retimed-nest");
    let mut p = (*s.project).clone();
    let c = &mut p.sequence_mut(seq).unwrap().audio_tracks[0].items[0];
    c.speed = 0.85;
    c.reverse = true;
    c.maintain_pitch = true;
    s.project = Arc::new(p);
    let path = dir.join("nest.aaf").to_string_lossy().into_owned();
    assert_aaf_audio_matches(&mut s, seq, json!({"path":path,"sequence":seq.0,"bitDepth":24,"handles":6}), "file.exportAaf");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_retiming_rejects_hostile_speed_without_writing_a_document() {
    for speed in [0.0, f64::NAN, f64::INFINITY, 1e30] {
        let (mut s, _, seq, dir) = setup("aaf-hostile-speed");
        let mut p = (*s.project).clone();
        p.sequence_mut(seq).unwrap().audio_tracks[0].items[0].speed = speed;
        s.project = Arc::new(p);
        let path = dir.join("bad.aaf");
        let result = s.execute("file.exportAaf", json!({"preserveMix":false,"path":path.to_string_lossy(),"sequence":seq.0}));
        assert!(result.is_err(), "{speed}: {result:?}");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn aaf_retimed_breakout_keeps_output_channel_mapping() {
    let (mut s, _, seq, dir) = setup("aaf-retimed-channel-map");
    let mut p = (*s.project).clone();
    let t = &mut p.sequence_mut(seq).unwrap().audio_tracks[0];
    t.items.truncate(1);
    t.items[0].speed = 0.85;
    t.items[0].maintain_pitch = true;
    t.items[0].source_channels = vec![1, 0];
    s.project = Arc::new(p);
    let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
    let wanted = filmcraft_render::audio::track_input(&s.project, &s.project.sequence(seq).unwrap().audio_tracks[0], 0, 48_000, 48_000, &provider);
    assert!(wanted.channels[0].iter().zip(&wanted.channels[1]).any(|(a, b)| (a - b).abs() > 0.01), "fixture channels must differ");
    let path = dir.join("channels.aaf").to_string_lossy().into_owned();
    s.execute("file.exportAaf", json!({"preserveMix":false,"path":path,"sequence":seq.0,"breakoutToMono":true,"bitDepth":24})).unwrap();
    let result = s.execute("file.importAaf", json!({"path":path})).unwrap();
    let q = s.project.sequence(imported_seq(&result)).unwrap();
    assert_eq!(q.audio_tracks.len(), 2);
    let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
    for (track, want) in q.audio_tracks.iter().zip(&wanted.channels) {
        let got = filmcraft_render::audio::track_input(&s.project, track, 0, 48_000, 48_000, &provider);
        assert!(want.iter().zip(&got.channels[0]).all(|(a, b)| (a - b).abs() < 2e-6));
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_retimed_crossfade_preserves_pitch_stretch_edge_grains_without_requested_handles() {
    let (mut s, _, seq, dir) = setup("aaf-retimed-crossfade");
    let mut p = (*s.project).clone();
    for c in &mut p.sequence_mut(seq).unwrap().audio_tracks[0].items {
        c.speed = 0.85;
        c.maintain_pitch = true;
    }
    s.project = Arc::new(p);
    let clip = s.project.sequence(seq).unwrap().audio_tracks[0].items[1].id;
    s.execute("sequence.applyAudioTransition", json!({"clip":clip.0,"effect":"constant_power","frames":6,"edge":"in","align":"center"})).unwrap();
    assert!(!s.project.sequence(seq).unwrap().audio_tracks[0].transitions.is_empty());
    let path = dir.join("fade.aaf").to_string_lossy().into_owned();
    assert_aaf_audio_matches(&mut s, seq, json!({"path":path,"sequence":seq.0,"bitDepth":24,"handles":0}), "file.exportAaf");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_retimed_decode_failure_is_reported_instead_of_baking_silence() {
    struct BrokenAudio(filmcraft_media::MediaInfo);
    impl filmcraft_media::MediaSource for BrokenAudio {
        fn info(&self) -> &filmcraft_media::MediaInfo {
            &self.0
        }
        fn video_frame(&self, _: filmcraft_media::FrameRequest) -> filmcraft_media::Result<Arc<filmcraft_frame::VideoFrame>> {
            Err(filmcraft_media::MediaError::NoStream("video"))
        }
        fn audio(&self, _: i64, _: usize, _: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
            Err(filmcraft_media::MediaError::Decode("test audio failure".into()))
        }
    }
    let (mut s, items, seq, dir) = setup("aaf-retimed-decode-error");
    let info = s.project.item(items[0]).unwrap().as_media().unwrap().info.clone();
    s.media.insert_file(items[0], &path_of(&s, items[0]), Arc::new(BrokenAudio(info)));
    let mut p = (*s.project).clone();
    p.sequence_mut(seq).unwrap().audio_tracks[0].items[0].speed = 0.5;
    p.sequence_mut(seq).unwrap().audio_tracks[0].items[0].maintain_pitch = false;
    s.project = Arc::new(p);
    let path = dir.join("bad.aaf");
    let error = s.execute("file.exportAaf", json!({"preserveMix":false,"path":path.to_string_lossy(),"sequence":seq.0})).unwrap_err();
    assert!(error.to_string().contains("test audio failure"), "{error}");
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_retimed_subsample_duration_has_complete_pcm() {
    use filmcraft_time::Tick;
    let (mut s, _, seq, dir) = setup("aaf-retimed-subsample");
    let mut p = (*s.project).clone();
    let t = &mut p.sequence_mut(seq).unwrap().audio_tracks[0];
    t.items.truncate(1);
    let c = &mut t.items[0];
    c.start = Tick::from_units(130, 48_000) + Tick(Tick::from_units(1, 48_000).0 / 5);
    c.duration = Tick::from_units(48_000, 48_000) + Tick(Tick::from_units(1, 48_000).0 * 3 / 5);
    c.speed = 0.5;
    s.project = Arc::new(p);
    let path = dir.join("fractional.aaf").to_string_lossy().into_owned();
    s.execute("file.exportAaf", json!({"preserveMix":false,"path":path,"sequence":seq.0,"bitDepth":24,"handles":0})).unwrap();
    let result = s.execute("file.importAaf", json!({"path":path})).unwrap();
    let c = &s.project.sequence(imported_seq(&result)).unwrap().audio_tracks[0].items[0];
    assert_eq!(c.duration.to_units_floor(48_000), 48_001);
    let source = s.media.full_res_source(&s.project, c.item, &*s.services).unwrap();
    assert!(source.info().duration >= c.source_in + c.duration, "rounded AAF duration must fit the actual PCM");
    let samples = source.audio(c.source_in.to_units_floor(48_000), 48_001, 48_000).unwrap();
    assert_eq!(samples.channels[0][48_000], 0.0, "pad the fractional trailing sample");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_reverse_audio_keeps_timeline_handles_at_source_zero() {
    use filmcraft_time::Tick;
    let (mut s, _, seq, dir) = setup("aaf-reverse-zero-handles");
    let mut p = (*s.project).clone();
    let t = &mut p.sequence_mut(seq).unwrap().audio_tracks[0];
    t.items.truncate(1);
    t.items[0].source_in = Tick::ZERO;
    t.items[0].speed = 0.85;
    t.items[0].reverse = true;
    t.items[0].maintain_pitch = true;
    s.project = Arc::new(p);
    let path = dir.join("reverse.aaf").to_string_lossy().into_owned();
    s.execute("file.exportAaf", json!({"preserveMix":false,"path":path,"sequence":seq.0,"bitDepth":24,"handles":6})).unwrap();
    let result = s.execute("file.importAaf", json!({"path":path})).unwrap();
    let c = &s.project.sequence(imported_seq(&result)).unwrap().audio_tracks[0].items[0];
    assert_eq!(c.source_in.to_units_floor(48_000), 12_000, "rendered handles use timeline coordinates, not the original source in-point");
    let samples = audio_of(&s, c.item, 0, 12_000);
    assert!(samples.iter().flatten().any(|v| v.abs() > 0.01), "reverse leading handles have usable source audio");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_embedded_trimmed_audio_round_trips_sample_exact() {
    let (mut s, items, seq, dir) = setup("aaf-embed");
    let path = dir.join("cut.aaf").to_string_lossy().into_owned();
    let r = s
        .execute("file.exportAaf", json!({"preserveMix":false,"path": path, "audio": "embedded", "trimAudio": true, "handles": 6, "sequence": seq.0}))
        .unwrap();
    assert!(r["mediaFiles"].as_array().unwrap().is_empty(), "{r}");
    let r = s.execute("file.import", json!({"paths": [path]})).unwrap();
    let n = imported_seq(&r);
    let q = s.project.sequence(n).unwrap().clone();
    let orig = s.project.sequence(seq).unwrap().clone();
    for kind in [TrackKind::Video, TrackKind::Audio] {
        let a: Vec<_> = q.tracks(kind)[0].items.iter().map(|c| (c.start, c.duration)).collect();
        let b: Vec<_> = orig.tracks(kind)[0].items.iter().map(|c| (c.start, c.duration)).collect();
        assert_eq!(a, b, "{kind:?}");
    }
    // video links to the original movies, audio to WAV files extracted next to the AAF
    assert_eq!(std::path::Path::new(&path_of(&s, q.video_tracks[0].items[0].item)), std::path::Path::new(&path_of(&s, items[0])));
    for (c, oc) in q.audio_tracks[0].items.iter().zip(&orig.audio_tracks[0].items) {
        let p = path_of(&s, c.item);
        assert!(p.ends_with(".wav") && std::path::Path::new(&p).exists(), "{p}");
        assert!(!s.project.item(c.item).unwrap().as_media().unwrap().offline);
        // trimmed: the essence starts 6 frames (handles) before the clip
        assert_eq!(c.source_in, FrameRate::FPS_24.tick_of(6));
        let got = audio_of(&s, c.item, c.source_in.to_units_floor(48_000), 4800);
        let want = audio_of(&s, oc.item, oc.source_in.to_units_floor(48_000), 4800);
        for ch in 0..2 {
            for i in 0..4800 {
                assert!((got[ch][i] - want[ch][i]).abs() <= 1.0 / 32_768.0 + 1e-6, "ch {ch} sample {i}: {} vs {}", got[ch][i], want[ch][i]);
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn embedded_aaf_and_omf_import_leave_existing_media_files_intact() {
    for (format, extension) in [(filmcraft_interchange::Format::Aaf, "aaf"), (filmcraft_interchange::Format::Omf, "omf")] {
        let (mut s, _items, seq, dir) = setup(&format!("embedded-collision-{extension}"));
        let path = dir.join(format!("cut.{extension}"));
        let path_text = path.to_string_lossy().into_owned();
        let command = if format == filmcraft_interchange::Format::Aaf { "file.exportAaf" } else { "file.exportOmf" };
        s.execute(command, json!({"path": path_text, "audio": "embedded", "sequence": seq.0})).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let opts = filmcraft_interchange::ImportOptions { base_dir: Some(dir.to_string_lossy().into_owned()), name: Some("cut".into()), ..Default::default() };
        let (_, extracted, _) = if format == filmcraft_interchange::Format::Aaf {
            filmcraft_interchange::aaf::import(&bytes, &opts).unwrap()
        } else {
            filmcraft_interchange::omf::import(&bytes, &opts).unwrap()
        };
        let first = extracted.first().expect("embedded audio");
        let original = b"ORIGINAL USER DATA";
        std::fs::create_dir_all(std::path::Path::new(&first.path).parent().unwrap()).unwrap();
        std::fs::write(&first.path, original).unwrap();

        let response = s.execute("file.import", json!({"paths": [path_text]})).unwrap();
        assert!(std::fs::read(&first.path).unwrap().as_slice() == original, "{extension}: existing file was overwritten");
        let imported = s.project.sequence(imported_seq(&response)).unwrap();
        let new_path = path_of(&s, imported.audio_tracks[0].items[0].item);
        assert_ne!(new_path, first.path, "{extension}: imported audio still points at the existing file");
        assert!(new_path.ends_with(" (2).wav"), "{extension}: {new_path}");
        assert!(std::fs::read(&new_path).unwrap() == first.wav, "{extension}: imported audio was not extracted to the new path");
        #[cfg(unix)]
        {
            let protected = dir.join("protected.wav");
            std::fs::write(&protected, original).unwrap();
            std::fs::remove_file(&first.path).unwrap();
            std::os::unix::fs::symlink(&protected, &first.path).unwrap();
            let again = s.execute("file.import", json!({"paths": [path.to_string_lossy()]})).unwrap();
            assert!(std::fs::read(&protected).unwrap().as_slice() == original, "{extension}: symlink target was overwritten");
            let imported_again = s.project.sequence(imported_seq(&again)).unwrap();
            let third_path = path_of(&s, imported_again.audio_tracks[0].items[0].item);
            assert!(third_path.ends_with(" (3).wav"), "{extension}: {third_path}");
            assert!(std::fs::read(&third_path).unwrap() == first.wav, "{extension}: the third extraction is not the embedded audio");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn aaf_separate_aiff_breakout_and_linked() {
    let (mut s, items, seq, dir) = setup("aaf-separate");
    let path = dir.join("sep.aaf").to_string_lossy().into_owned();
    let r = s
        .execute(
            "file.exportAaf",
            json!({"preserveMix":false,"path": path, "audio": "separate", "audioFormat": "aiff", "bitDepth": 24, "breakoutToMono": true, "trimAudio": true, "handles": 0}),
        )
        .unwrap();
    let files: Vec<String> = r["mediaFiles"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(files.len(), 4, "two media × two channels: {r}");
    for f in &files {
        let b = std::fs::read(f).unwrap();
        let (pcm, ch, sr, bits) = filmcraft_interchange::wav::parse_aiff(&b).unwrap();
        assert_eq!((ch, sr, bits), (1, 48_000, 24));
        assert_eq!(pcm.len(), 24 * 2000 * 3, "one second of mono 24-bit audio (no handles)");
    }
    let r = s.execute("file.importAaf", json!({"path": path})).unwrap();
    let n = imported_seq(&r);
    let q = s.project.sequence(n).unwrap();
    let mono: Vec<_> = q.audio_tracks.iter().filter(|t| !t.items.is_empty()).collect();
    assert_eq!(mono.len(), 2);
    assert!(mono.iter().all(|t| t.channels == filmcraft_project::AudioChannels::Mono));

    // Avid-style consolidated OP-Atom MXF audio: our MXF reader opens every file
    let path = dir.join("mxf.aaf").to_string_lossy().into_owned();
    let r = s
        .execute(
            "file.exportAaf",
            json!({"preserveMix":false,"path": path, "audio": "separate", "audioFormat": "mxf", "breakoutToMono": true, "trimAudio": true}),
        )
        .unwrap();
    let files: Vec<String> = r["mediaFiles"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(files.len(), 4);
    for f in &files {
        let b = std::fs::read(f).unwrap();
        let m = filmcraft_mxf::open(&b).unwrap();
        assert_eq!(m.operational_pattern.name(), "OP-Atom");
        let t = m.track_of_kind(filmcraft_mxf::TrackKind::Sound).unwrap();
        assert_eq!(m.tracks[t].stored_sample_frames(), 48_000);
    }
    let r = s.execute("file.importAaf", json!({"path": path})).unwrap();
    let n = imported_seq(&r);
    let a = &s.project.sequence(n).unwrap().audio_tracks[0].items[0];
    assert!(path_of(&s, a.item).ends_with(".mxf"));
    assert!(!s.project.item(a.item).unwrap().as_media().unwrap().offline, "the MXF audio links");

    // linked: no media is written, the document points at the original movies
    let path = dir.join("linked.aaf").to_string_lossy().into_owned();
    let r = s.execute("file.exportAaf", json!({"preserveMix":false,"path": path, "audio": "linked", "sequence": seq.0})).unwrap();
    assert!(r["mediaFiles"].as_array().unwrap().is_empty());
    let r = s.execute("file.import", json!({"paths": [path]})).unwrap();
    let n = imported_seq(&r);
    let q = s.project.sequence(n).unwrap();
    let a = &q.audio_tracks[0].items[0];
    assert_eq!(std::path::Path::new(&path_of(&s, a.item)), std::path::Path::new(&path_of(&s, items[0])));
    assert_eq!(a.source_in, FrameRate::FPS_24.tick_of(12));
    assert!(!s.project.item(a.item).unwrap().as_media().unwrap().offline);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_video_mixdown() {
    let (mut s, _items, seq, dir) = setup("aaf-mixdown");
    let path = dir.join("mix.aaf").to_string_lossy().into_owned();
    let r = s.execute("file.exportAaf", json!({"preserveMix":false,"path": path, "mixdownVideo": true, "audio": "linked", "sequence": seq.0})).unwrap();
    let files = r["mediaFiles"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    let mov = files[0].as_str().unwrap();
    assert!(mov.ends_with("Video Mixdown.mp4") && std::path::Path::new(mov).exists(), "the default must play in DAWs without a DNxHR decoder: {mov}");
    let bytes = std::fs::read(mov).unwrap();
    let movie = filmcraft_isobmff::open(bytes.as_slice()).unwrap();
    let video = movie.track_of_kind(filmcraft_isobmff::TrackKind::Video).unwrap();
    assert_eq!(movie.tracks[video].entries[0].format.0, *b"avc1", "the mixdown must contain AVC, not merely have an MP4 extension");
    let filmcraft_isobmff::CodecConfig::Avc(config) = &movie.tracks[video].entries[0].codec else { panic!("missing AVC configuration") };
    assert_eq!(config.profile, 100, "use ordinary 8-bit High, not High 10/4:2:2/4:4:4");
    assert_eq!(movie.tracks[video].samples.len(), 48);
    let r = s.execute("file.import", json!({"paths": [path]})).unwrap();
    let n = imported_seq(&r);
    let q = s.project.sequence(n).unwrap();
    assert_eq!(q.video_tracks.iter().map(|t| t.items.len()).sum::<usize>(), 1);
    let v = &q.video_tracks[0].items[0];
    assert_eq!(v.duration, FrameRate::FPS_24.tick_of(48));
    assert_eq!(std::path::Path::new(&path_of(&s, v.item)), std::path::Path::new(mov));
    let m = s.project.item(v.item).unwrap().as_media().unwrap();
    assert!(!m.offline);
    assert_eq!(m.info.video.as_ref().map(|v| (v.width, v.height)), Some((64, 36)));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_video_mixdown_formats_keep_fractional_frame_timing_and_audio_edits() {
    let (mut s, _items, seq, dir) = setup("aaf-mixdown-formats");
    let rate = FrameRate::FPS_59_94;
    let q = Arc::make_mut(&mut s.project).sequence_mut(seq).unwrap();
    q.settings.frame_rate = rate;
    for track in q.all_tracks_mut() {
        for (i, clip) in track.items.iter_mut().enumerate() {
            clip.start = rate.tick_of(i as i64 * 3);
            clip.duration = rate.tick_of(3);
        }
    }
    let mut audio_edits = None;
    for extension in ["mp4", "mov", "mxf"] {
        let path = dir.join(format!("{extension}.aaf"));
        let result = s
            .execute("file.exportAaf", json!({"path":path,"sequence":seq.0,"preserveMix":false,"audio":"linked","mixdownVideo":true,"mixdownFormat":extension}))
            .unwrap();
        let files = result["mediaFiles"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        let video_path = files[0].as_str().unwrap();
        assert!(video_path.ends_with(&format!("Video Mixdown.{extension}")));
        let bytes = std::fs::read(video_path).unwrap();
        if extension == "mxf" {
            assert_eq!(filmcraft_mxf::open(&bytes).unwrap().operational_pattern.name(), "OP-Atom");
        } else {
            let movie = filmcraft_isobmff::open(bytes.as_slice()).unwrap();
            let video = &movie.tracks[movie.track_of_kind(filmcraft_isobmff::TrackKind::Video).unwrap()];
            assert_eq!(video.entries[0].format.0, if extension == "mp4" { *b"avc1" } else { *b"AVdh" });
            assert_eq!(video.samples.len(), 6);
            assert_eq!(video.timescale, 60_000);
            assert_eq!(video.samples.iter().map(|s| u64::from(s.duration)).sum::<u64>(), 6_006);
        }
        let imported = s.execute("file.importAaf", json!({"path":path})).unwrap();
        let q = s.project.sequence(imported_seq(&imported)).unwrap();
        assert_eq!(q.settings.frame_rate, rate);
        let video = &q.video_tracks[0].items[0];
        assert_eq!(video.start, filmcraft_time::Tick::ZERO);
        assert_eq!(video.duration, rate.tick_of(6));
        let media = s.project.item(video.item).unwrap().as_media().unwrap();
        assert!(!media.offline);
        assert_eq!(media.info.video.as_ref().unwrap().frame_rate, rate);
        let audio: Vec<_> = q.audio_tracks.iter().flat_map(|t| &t.items).map(|c| (c.start, c.duration, c.source_in)).collect();
        assert!(!audio.is_empty());
        match &audio_edits {
            Some(expected) => assert_eq!(&audio, expected, "video format must not change audio placement"),
            None => audio_edits = Some(audio),
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn aaf_video_mixdown_rejects_invalid_formats_before_writing_media() {
    let (mut s, _items, seq, dir) = setup("aaf-mixdown-hostile");
    let path = dir.join("invalid.aaf");
    let before = serde_json::to_value(&s.project).unwrap();
    for format in [json!("mpeg2"), json!(false), json!(17), json!({})] {
        let error = s.execute("file.exportAaf", json!({"path":path,"sequence":seq.0,"mixdownVideo":true,"mixdownFormat":format})).unwrap_err();
        assert!(error.to_string().contains("mixdownFormat"));
        assert!(!path.exists());
        assert!(!dir.join("invalid Media").exists());
        assert_eq!(serde_json::to_value(&s.project).unwrap(), before);
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn omf_renders_clip_effects_into_embedded_audio() {
    let (mut s, _items, seq, dir) = setup("omf-render");
    // −6.0206 dB on the first audio clip: half amplitude
    let mut p = (*s.project).clone();
    let q = p.sequence_mut(seq).unwrap();
    q.audio_tracks[0].items[0].effect_mut("volume").unwrap().params.insert("level".into(), Param::new(ParamValue::Float(-6.020_599_913)));
    s.project = Arc::new(p);
    let orig = s.project.sequence(seq).unwrap().clone();
    let path = dir.join("mix.omf").to_string_lossy().into_owned();
    let r = s.execute("file.exportOmf", json!({"path": path, "renderAudioEffects": true, "handles": 0, "sequence": seq.0})).unwrap();
    assert!(r["bytes"].as_u64().unwrap() > 48_000 * 2 * 2, "audio is encapsulated: {r}");
    let r = s.execute("file.importAaf", json!({"path": path})).unwrap();
    let n = imported_seq(&r);
    let q = s.project.sequence(n).unwrap().clone();
    assert!(q.video_tracks.iter().all(|t| t.items.is_empty()));
    let a = &q.audio_tracks[0].items;
    assert_eq!(a.len(), 2);
    assert_eq!(
        a.iter().map(|c| (c.start, c.duration)).collect::<Vec<_>>(),
        orig.audio_tracks[0].items.iter().map(|c| (c.start, c.duration)).collect::<Vec<_>>()
    );
    // no gain left on the clips: it is in the samples
    assert!(a.iter().all(|c| c.effect("volume").and_then(|e| e.param("level")).is_none_or(|p| p.value.as_f64() == Some(0.0))));
    let oc = &orig.audio_tracks[0].items[0];
    let got = audio_of(&s, a[0].item, a[0].source_in.to_units_floor(48_000), 2400);
    let want = audio_of(&s, oc.item, oc.source_in.to_units_floor(48_000), 2400);
    let peak = want[0].iter().fold(0f32, |m, x| m.max(x.abs()));
    assert!(peak > 0.01, "the demo scene has audio");
    for i in 0..2400 {
        assert!((got[0][i] - want[0][i] * 0.5).abs() < 2e-3, "sample {i}: {} vs {}", got[0][i], want[0][i] * 0.5);
    }
    // the second clip is unchanged
    let oc2 = &orig.audio_tracks[0].items[1];
    let got = audio_of(&s, a[1].item, a[1].source_in.to_units_floor(48_000), 2400);
    let want = audio_of(&s, oc2.item, oc2.source_in.to_units_floor(48_000), 2400);
    for i in 0..2400 {
        assert!((got[0][i] - want[0][i]).abs() < 2e-3);
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// `setup`, with both clips nested into "Inner" (one linked picture + sound nest on V1 / A1) and
/// −6.0206 dB (half amplitude) on the nest's sound clip.
fn setup_nested(name: &str) -> (Session, Vec<ItemId>, ItemId, ItemId, std::path::PathBuf) {
    let (mut s, items, seq, dir) = setup(name);
    s.execute("sequence.open", json!({"item": seq.0})).unwrap();
    let clips: Vec<u64> = s.project.sequence(seq).unwrap().all_tracks().flat_map(|t| t.items.iter().map(|c| c.id.0)).collect();
    s.execute("timeline.select", json!({"clips": clips})).unwrap();
    let inner = ItemId(s.execute("clip.nest", json!({"name": "Inner"})).unwrap()["sequence"].as_u64().unwrap());
    let mut p = (*s.project).clone();
    let q = p.sequence_mut(seq).unwrap();
    assert_eq!((q.video_tracks[0].items.len(), q.audio_tracks[0].items.len()), (1, 1), "one linked nest");
    q.audio_tracks[0].items[0].effect_mut("volume").unwrap().params.insert("level".into(), Param::new(ParamValue::Float(-6.020_599_913)));
    s.project = Arc::new(p);
    (s, items, seq, inner, dir)
}

/// Premiere Pro renders the sound of a nested sequence into an OMF as one clip named after the
/// sequence (seen in an OMF it exported). So do we: the nest is mixed, with what is on its clip.
#[test]
fn omf_renders_the_sound_of_a_nested_sequence() {
    let (mut s, items, seq, inner, dir) = setup_nested("omf-nest");
    let nest = s.project.sequence(seq).unwrap().audio_tracks[0].items[0].clone();
    let path = dir.join("nest.omf").to_string_lossy().into_owned();
    let r = s.execute("file.exportOmf", json!({"path": path, "handles": 0, "sequence": seq.0})).unwrap();
    assert!(!r["report"].to_string().contains("nested"), "{r}");
    let r = s.execute("file.importAaf", json!({"path": path})).unwrap();
    let q = s.project.sequence(imported_seq(&r)).unwrap().clone();
    let a = &q.audio_tracks[0].items;
    assert_eq!(a.len(), 1, "the nest is one clip");
    assert_eq!((a[0].name.as_str(), a[0].start, a[0].duration), ("Inner", nest.start, nest.duration));
    assert!(s.project.sequence(a[0].item).is_none(), "media in the document, not a sequence");
    // what it plays: the nested sequence's two clips one after the other, at half amplitude
    let inside = s.project.sequence(inner).unwrap().audio_tracks[0].items.clone();
    assert_eq!(inside.len(), 2);
    let got = audio_of(&s, a[0].item, a[0].source_in.to_units_floor(48_000), 96_000);
    for (k, oc) in inside.iter().enumerate() {
        let want = audio_of(&s, oc.item, oc.source_in.to_units_floor(48_000), 48_000);
        assert_eq!(oc.item, items[k]);
        let peak = want[0].iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.01, "the demo scene has audio");
        // (away from the cut between the two clips)
        for i in 480..47_520 {
            let g = got[0][k * 48_000 + i];
            assert!((g - want[0][i] * 0.5).abs() < 2e-3, "clip {k} sample {i}: {g} vs {}", want[0][i] * 0.5);
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// An AAF holds a nested sequence as a composition of its own (as Premiere Pro's does): it comes
/// back as a sequence that the nest clips use, with its own clips, and with embedded audio the
/// media inside it is in the document too.
#[test]
fn aaf_keeps_a_nested_sequence_as_a_sequence() {
    let (mut s, items, seq, inner, dir) = setup_nested("aaf-nest");
    let path = dir.join("nest.aaf").to_string_lossy().into_owned();
    let r = s
        .execute("file.exportAaf", json!({"preserveMix":false,"path": path, "audio": "embedded", "trimAudio": true, "handles": 6, "sequence": seq.0}))
        .unwrap();
    assert!(!r["report"].to_string().contains("gap"), "{r}");
    let r = s.execute("file.import", json!({"paths": [path]})).unwrap();
    let q = s.project.sequence(imported_seq(&r)).unwrap().clone();
    let (v, a) = (&q.video_tracks[0].items, &q.audio_tracks[0].items);
    assert_eq!((v.len(), a.len()), (1, 1));
    let nested = v[0].item;
    assert_ne!(nested, inner, "a sequence of the imported document");
    assert_eq!((a[0].item, s.project.item(nested).unwrap().name.as_str()), (nested, "Inner"));
    assert!(v[0].link.is_some() && v[0].link == a[0].link, "picture and sound of the nest are linked");
    let level = a[0].effect("volume").and_then(|e| e.param("level")).and_then(|p| p.value.as_f64()).unwrap();
    assert!((level + 6.0206).abs() < 1e-2, "the level stays on the nest's clip: {level}");
    // inside: the two clips, their picture linked to the movies, their sound in the document
    let n = s.project.sequence(nested).unwrap().clone();
    let orig = s.project.sequence(inner).unwrap().clone();
    for kind in [TrackKind::Video, TrackKind::Audio] {
        let got: Vec<_> = n.tracks(kind)[0].items.iter().map(|c| (c.start, c.duration)).collect();
        let want: Vec<_> = orig.tracks(kind)[0].items.iter().map(|c| (c.start, c.duration)).collect();
        assert_eq!(got, want, "{kind:?}");
    }
    assert_eq!(std::path::Path::new(&path_of(&s, n.video_tracks[0].items[0].item)), std::path::Path::new(&path_of(&s, items[0])));
    for (c, oc) in n.audio_tracks[0].items.iter().zip(&orig.audio_tracks[0].items) {
        let p = path_of(&s, c.item);
        assert!(p.ends_with(".wav") && std::path::Path::new(&p).exists(), "{p}");
        let got = audio_of(&s, c.item, c.source_in.to_units_floor(48_000), 4800);
        let want = audio_of(&s, oc.item, oc.source_in.to_units_floor(48_000), 4800);
        for i in 0..4800 {
            assert!((got[0][i] - want[0][i]).abs() <= 1.0 / 32_768.0 + 1e-6, "sample {i}");
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn commands_are_registered_with_menus_and_validate_params() {
    for (id, menu) in [("file.exportAaf", &["File", "Export"][..]), ("file.exportOmf", &["File", "Export"][..]), ("file.importAaf", &[][..])] {
        let c = crate::commands::find(id).unwrap_or_else(|| panic!("{id}"));
        assert_eq!(c.menu, menu);
    }
    let (mut s, _, seq, dir) = setup("aaf-params");
    let path = dir.join("x.aaf").to_string_lossy().into_owned();
    assert!(s.execute("file.exportAaf", json!({"preserveMix":false,"sequence": seq.0})).is_err(), "path is required");
    assert!(s.execute("file.exportAaf", json!({"preserveMix":false,"path": path, "bitDepth": 20})).is_err());
    assert!(s.execute("file.exportOmf", json!({"path": path, "audio": "linked"})).is_err(), "OMF has no linked mode");
    assert!(s.execute("file.importAaf", json!({"path": dir.join("a.mov").to_string_lossy()})).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

// `bitDepth` as `describe` lists it ("16|24", a string) or as a number. A string was read as 16
// without a word, and 65560 wrapped to 24 (#4019).
#[test]
fn bit_depth_as_a_string_or_a_number_and_nothing_else() {
    let (mut s, _, _, dir) = setup("aaf-bit-depth");
    for (k, (cmd, ext, depth)) in [
        ("file.exportAaf", "aaf", json!("24")),
        ("file.exportOmf", "omf", json!("24")),
        ("file.exportAaf", "aaf", json!(24)),
        ("file.exportAaf", "aaf", json!(24.0)),
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.join(format!("d{k}.{ext}")).to_string_lossy().into_owned();
        let r = s.execute(cmd, json!({"path": path, "audio": "separate", "audioFormat": "aiff", "bitDepth": depth})).unwrap();
        let files = r["mediaFiles"].as_array().unwrap();
        assert!(!files.is_empty(), "{r}");
        for f in files {
            let (_, _, _, bits) = filmcraft_interchange::wav::parse_aiff(&std::fs::read(f.as_str().unwrap()).unwrap()).unwrap();
            assert_eq!(bits, 24, "{cmd} with bitDepth {depth}");
        }
    }
    for (cmd, ext) in [("file.exportAaf", "aaf"), ("file.exportOmf", "omf")] {
        let path = dir.join(format!("refused.{ext}")).to_string_lossy().into_owned();
        for depth in [json!("20"), json!("24-bit"), json!(65560), json!(true)] {
            assert!(s.execute(cmd, json!({"path": path, "bitDepth": depth})).is_err(), "{cmd} with bitDepth {depth}");
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

// Selected-stream essence uses only PR361 schema13 placement and decoder fields.
mod selected_streams {
    use crate::{Services, Session};
    use filmcraft_frame::{AudioBuffer, VideoFrame};
    use filmcraft_media::{FrameRequest, MediaError, MediaInfo, MediaKind, MediaSource};
    use filmcraft_project::{ClipId, ItemId, ItemKind, Label, MediaClip, MediaRef, SequenceSettings};
    use filmcraft_time::{FrameRate, Tick};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    type Calls = Arc<Mutex<Vec<(usize, i64, usize, u32)>>>;

    struct Tones {
        info: MediaInfo,
        calls: Calls,
    }

    impl Tones {
        fn new(streams: usize) -> Self {
            let source = filmcraft_media::generators::GeneratorSource::new(
                filmcraft_media::Generator::BarsAndTone,
                32,
                18,
                FrameRate::FPS_24,
                Tick::from_seconds_f64(2.0),
            );
            let mut info = source.info().clone();
            info.kind = MediaKind::AudioOnly;
            info.video = None;
            info.audio_streams = vec![info.audio_streams[0].clone(); streams];
            Self { info, calls: Arc::default() }
        }

        fn sample(stream: usize, channel: usize, frame: i64, rate: u32) -> f32 {
            let frequency = (stream * 2 + channel + 1) as f64 * 240.0;
            let amplitude = 0.05 * (stream + 1) as f64;
            (amplitude * (std::f64::consts::TAU * frequency * frame as f64 / rate as f64).sin()) as f32
        }
    }

    impl MediaSource for Tones {
        fn info(&self) -> &MediaInfo {
            &self.info
        }
        fn video_frame(&self, _: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
            Err(MediaError::NoStream("video"))
        }
        fn audio(&self, start: i64, frames: usize, sample_rate: u32) -> filmcraft_media::Result<AudioBuffer> {
            self.audio_stream(0, start, frames, sample_rate)
        }
        fn audio_stream(&self, stream: usize, start: i64, frames: usize, sample_rate: u32) -> filmcraft_media::Result<AudioBuffer> {
            if stream >= self.info.audio_streams.len() {
                return Err(MediaError::NoStream("audio"));
            }
            self.calls.lock().unwrap().push((stream, start, frames, sample_rate));
            Ok(AudioBuffer {
                sample_rate,
                channels: (0..2).map(|channel| (0..frames).map(|i| Self::sample(stream, channel, start + i as i64, sample_rate)).collect()).collect(),
            })
        }
    }

    #[derive(Default)]
    struct MemoryFiles {
        writes: Mutex<Vec<(String, Vec<u8>)>>,
    }
    impl Services for MemoryFiles {
        fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
            Err(std::io::ErrorKind::NotFound.into())
        }
        fn write_file(&self, path: &str, bytes: &[u8]) -> std::io::Result<()> {
            self.writes.lock().unwrap().push((path.to_owned(), bytes.to_vec()));
            Ok(())
        }
        fn export_in_memory(&self) -> bool {
            true
        }
    }

    fn add_source(s: &mut Session, streams: usize) -> (ItemId, Calls) {
        let source = Arc::new(Tones::new(streams));
        let calls = source.calls.clone();
        let mut project = (*s.project).clone();
        let item = project.add_item(
            "distinct tones",
            Label::Iris,
            ItemKind::Media(Box::new(MediaClip {
                media: MediaRef::File { path: format!("tone-source-{streams}.wav") },
                info: source.info.clone(),
                interpret: Default::default(),
                mark_in: None,
                mark_out: None,
                markers: vec![],
                offline: false,
                proxy: None,
                identity: None,
            })),
            None,
        );
        s.project = Arc::new(project);
        s.media.insert(item, source);
        (item, calls)
    }

    fn fixture() -> (Session, ItemId, ItemId, Calls, Arc<MemoryFiles>) {
        let files = Arc::new(MemoryFiles::default());
        let mut session = Session::new(files.clone());
        let mut project = (*session.project).clone();
        let sequence =
            project.new_sequence("streams", SequenceSettings { width: 32, height: 18, frame_rate: FrameRate::FPS_24, ..Default::default() }, 0, 1, None);
        session.project = Arc::new(project);
        session.state.active_sequence = Some(sequence);
        session.state.open_sequences = vec![sequence];
        let (item, calls) = add_source(&mut session, 3);
        session.state.project_selection = vec![item];
        session.execute("sequence.linkedSelection", json!({"on": false})).unwrap();
        (session, item, sequence, calls, files)
    }

    fn place(s: &mut Session, item: ItemId, seconds: f64) -> Vec<ClipId> {
        let result = s.execute("timeline.place", json!({"item": item.0, "track": "A1", "seconds": seconds})).unwrap();
        result["clips"].as_array().unwrap().iter().map(|id| ClipId(id.as_u64().unwrap())).collect()
    }

    #[test]
    fn aaf_essence_distinguishes_streams_and_linked_refusal_precedes_writes() {
        use crate::aaf_omf::{AudioMode, AudioPlan};
        use filmcraft_interchange::essence::{EssenceData, EssenceKey, NestNeeds};
        let (mut s, item, sequence, calls, files) = fixture();
        place(&mut s, item, 0.0);
        let plan = AudioPlan {
            mode: AudioMode::Embedded,
            aiff: false,
            mxf: false,
            sample_rate: 48_000,
            bits: 16,
            trim: true,
            handles: Tick::ZERO,
            render_effects: false,
            preserve_mix: false,
            breakout: false,
            nests: NestNeeds::Inside,
        };
        let (essences, paths) = crate::aaf_omf::prepare_audio(&s, sequence, &plan, "ignored").unwrap();
        assert!(paths.is_empty());
        assert_eq!(essences.len(), 3);
        for (stream, essence) in essences.iter().enumerate() {
            assert_eq!(essence.key, EssenceKey::media(item, stream));
            let EssenceData::Embedded(bytes) = &essence.data else { panic!("expected embedded PCM") };
            for channel in 0..2 {
                let offset = (25 * 2 + channel) * 2;
                let actual = i16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
                let expected = (Tones::sample(stream, channel, 25, 48_000) * 32767.0).round() as i16;
                assert_eq!(actual, expected);
            }
        }
        let options = filmcraft_interchange::aaf::AafOptions {
            media: filmcraft_interchange::essence::MediaOptions { essence: essences, ..Default::default() },
            ..Default::default()
        };
        let (aaf, _) = filmcraft_interchange::aaf::export(&s.project, sequence, &options).unwrap();
        let omf_options = filmcraft_interchange::omf::OmfOptions { media: options.media.clone(), ..Default::default() };
        let (omf, _) = filmcraft_interchange::omf::export(&s.project, sequence, &omf_options).unwrap();
        for (kind, document) in [("AAF", aaf), ("OMF", omf)] {
            let (imported, extracted, _) = if kind == "AAF" {
                filmcraft_interchange::aaf::import(&document, &Default::default()).unwrap()
            } else {
                filmcraft_interchange::omf::import(&document, &Default::default()).unwrap()
            };
            assert_eq!(extracted.len(), 3, "{kind}: composition memo must retain distinct selected-stream essence");
            let tracks = &imported.project.sequence(imported.sequences[0]).unwrap().audio_tracks;
            assert_eq!(tracks.len(), 3, "{kind}");
            let mut source_ids: Vec<_> = tracks.iter().map(|track| track.items[0].item).collect();
            source_ids.sort();
            source_ids.dedup();
            assert_eq!(source_ids.len(), 3, "{kind}: each stream remains a distinct imported source");
            let mut waveforms: Vec<_> = extracted.iter().map(|media| media.wav.clone()).collect();
            waveforms.sort();
            waveforms.dedup();
            assert_eq!(waveforms.len(), 3, "{kind}");
        }
        assert!(calls.lock().unwrap().iter().any(|call| call.0 == 2));
        assert!(files.writes.lock().unwrap().is_empty());
        let error = s
            .execute(
                "file.exportAaf",
                json!({"preserveMix":false,"path": "must-not-be-written.aaf", "sequence": sequence.0, "audio": "linked", "trimAudio": false}),
            )
            .unwrap_err();
        assert!(error.to_string().contains("embedded") || error.to_string().contains("separate"));
        assert!(files.writes.lock().unwrap().is_empty());
    }
}
