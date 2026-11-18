//! Option-drag of a timeline clip copies it (Premiere): the original stays, a copy lands where
//! the pointer lets go and is selected; a plain drag still moves.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
}

impl Driver {
    fn demo() -> Self {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).expect("demo project");
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx };
        d.frames(4);
        d
    }
    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }
    fn ok(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                assert_eq!(v["ok"], json!(true), "{method} {params} failed: {v}");
                return v["result"].clone();
            }
        }
        panic!("no reply to {method} {params}");
    }
    fn exec(&mut self, command: &str, params: Value) -> Value {
        self.ok("engine.execute", json!({"command": command, "params": params}))
    }
    fn rect(&mut self, id: &str) -> [f64; 4] {
        let v = self.ok("ui.elements", json!({"prefix": id}));
        let e = v.as_array().unwrap().iter().find(|e| e["id"] == id).unwrap_or_else(|| panic!("no element {id}")).clone();
        let r: Vec<f64> = e["rect"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
        [r[0], r[1], r[2], r[3]]
    }
}

fn clip_count(d: &mut Driver) -> usize {
    d.harness.state().session.active_sequence().map(|q| q.all_tracks().map(|t| t.items.len()).sum()).unwrap_or(0)
}

fn isolated_audio(d: &mut Driver) -> (filmcraft_project::ClipId, filmcraft_time::Tick) {
    let sid = d.harness.state().session.state.active_sequence.unwrap();
    let q = std::sync::Arc::make_mut(&mut d.harness.state_mut().session.project).sequence_mut(sid).unwrap();
    let mut clip = q.audio_tracks[0].items[0].clone();
    clip.start = q.settings.frame_rate.tick_of(96) + filmcraft_time::Tick(filmcraft_time::TICKS_PER_SECOND / 48_000);
    clip.duration = q.settings.frame_rate.tick_of(48);
    clip.link = None;
    clip.group = None;
    let out = (clip.id, clip.start);
    for track in q.video_tracks.iter_mut().chain(q.audio_tracks.iter_mut()) {
        track.items.clear();
        track.transitions.clear();
    }
    q.audio_tracks[0].items.push(clip);
    q.markers.clear();
    q.mark_in = None;
    q.mark_out = None;
    d.harness.state_mut().session.state.linked_selection = false;
    d.harness.state_mut().session.set_playhead(filmcraft_time::Tick(7 * filmcraft_time::TICKS_PER_SECOND));
    let view = &mut d.harness.state_mut().ui.timeline;
    view.pps = 72.0;
    view.target_pps = 72.0;
    view.scroll = 0.0;
    view.target_scroll = 0.0;
    view.fit_pending = false;
    view.split = 0.15;
    d.frames(4);
    out
}

#[test]
fn a_clip_click_places_the_playhead_at_its_exact_start() {
    let mut d = Driver::demo();
    let (clip, start) = isolated_audio(&mut d);
    let scroll = d.harness.state().ui.timeline.scroll;
    let rect = d.rect(&format!("timeline.clip.{}", clip.0));
    d.ok("ui.click", json!({"x":rect[0]+rect[2]*0.5,"y":rect[1]+rect[3]*0.25}));
    assert_eq!(d.harness.state().session.playhead(), start, "Selecting audio must not round its start to a video frame");
    assert_eq!(d.harness.state().session.state.selection, vec![clip]);
    assert_eq!(d.harness.state().ui.timeline.scroll, scroll, "Selection must not scroll the view under the pointer");
    if let Some(dir) = std::env::var_os("CRAFT_TIMELINE_FIXTURES") {
        std::fs::create_dir_all(&dir).unwrap();
        d.harness.render().unwrap().save(std::path::Path::new(&dir).join("clip-selection-playhead.png")).unwrap();
    }
}

#[test]
fn a_cross_track_drag_keeps_exact_time_despite_horizontal_hand_jitter() {
    let mut d = Driver::demo();
    let (clip, start) = isolated_audio(&mut d);
    let source = d.rect(&format!("timeline.clip.{}", clip.0));
    let target = d.rect("timeline.track.A2.target");
    let destination = d.harness.state().session.active_sequence().unwrap().audio_tracks[1].id;
    d.ok("ui.drag",json!({"from":{"x":source[0]+source[2]*0.5,"y":source[1]+source[3]*0.25},"to":{"x":source[0]+source[2]*0.5+6.0,"y":target[1]+target[3]*0.5},"steps":12}));
    let q = d.harness.state().session.active_sequence().unwrap();
    let (track, item) = q.find_item(clip).unwrap();
    assert_eq!(track, destination);
    assert_eq!(item.start, start, "Changing tracks must keep the original audio-sample position through small sideways movement");
    d.exec("edit.undo", json!({}));
    assert_eq!(d.harness.state().session.active_sequence().unwrap().audio_tracks[0].items[0].start, start);
}

#[test]
fn dragging_an_existing_selection_keeps_a_parked_playhead_as_a_snap_target() {
    let mut d = Driver::demo();
    let (clip, start) = isolated_audio(&mut d);
    d.exec("timeline.select", json!({"clips":[clip.0]}));
    d.exec("playhead.set", json!({"frame":120}));
    let parked = d.harness.state().session.playhead();
    let dx = (parked - start).seconds() * 72.0;
    let source = d.rect(&format!("timeline.clip.{}", clip.0));
    let target = d.rect("timeline.track.A2.target");
    d.ok(
        "ui.drag",
        json!({"from":{"x":source[0]+source[2]*0.5,"y":source[1]+source[3]*0.25},"to":{"x":source[0]+source[2]*0.5+dx,"y":target[1]+target[3]*0.5},"steps":12}),
    );
    assert_eq!(d.harness.state().session.playhead(), parked);
    let q = d.harness.state().session.active_sequence().unwrap();
    assert_eq!(q.find_item(clip).unwrap().0, q.audio_tracks[1].id);
    assert_eq!(q.find_item(clip).unwrap().1.start, parked);
}

#[test]
fn original_time_snapping_works_with_playhead_follow_disabled_and_allows_free_moves() {
    for (snap, dx, stays) in [(true, 6.0, true), (true, 50.0, false), (false, 6.0, false)] {
        let mut d = Driver::demo();
        let (clip, start) = isolated_audio(&mut d);
        d.exec("prefs.set", json!({"key":"timeline.playheadFollowsSelection","value":false}));
        d.exec("sequence.snap", json!({"on":snap}));
        let source = d.rect(&format!("timeline.clip.{}", clip.0));
        let target = d.rect("timeline.track.A2.target");
        d.ok("ui.drag",json!({"from":{"x":source[0]+source[2]*0.5,"y":source[1]+source[3]*0.25},"to":{"x":source[0]+source[2]*0.5+dx,"y":target[1]+target[3]*0.5},"steps":12}));
        let q = d.harness.state().session.active_sequence().unwrap();
        let (track, item) = q.find_item(clip).unwrap();
        assert_eq!(track, q.audio_tracks[1].id);
        if stays {
            assert_eq!(item.start, start);
        } else {
            assert!(item.start > start, "Deliberate or unsnapped moves must remain free");
        }
    }
}

#[test]
fn playhead_follow_can_be_disabled_without_moving_the_cursor() {
    let mut d = Driver::demo();
    let (clip, _) = isolated_audio(&mut d);
    d.exec("prefs.set", json!({"key":"timeline.playheadFollowsSelection","value":false}));
    let before = d.harness.state().session.playhead();
    let rect = d.rect(&format!("timeline.clip.{}", clip.0));
    d.ok("ui.click", json!({"x":rect[0]+rect[2]*0.5,"y":rect[1]+rect[3]*0.25}));
    assert_eq!(d.harness.state().session.playhead(), before);
    assert_eq!(d.harness.state().session.state.selection, vec![clip]);
}

#[test]
fn original_time_magnet_keeps_drag_previews_stable_before_drop() {
    let mut d = Driver::demo();
    let (clip, start) = isolated_audio(&mut d);
    d.exec("prefs.set", json!({"key":"timeline.playheadFollowsSelection","value":false}));
    let source = d.rect(&format!("timeline.clip.{}", clip.0));
    let target = d.rect("timeline.track.A2.target");
    let origin = egui::pos2((source[0] + source[2] * 0.5) as f32, (source[1] + source[3] * 0.25) as f32);
    d.harness.input_mut().events.push(egui::Event::PointerMoved(origin));
    d.frames(1);
    d.harness.input_mut().events.push(egui::Event::PointerButton {
        pos: origin,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    d.frames(1);
    let y = (target[1] + target[3] * 0.5) as f32;
    for dx in [2.0, 5.0, -3.0, 6.0] {
        d.harness.input_mut().events.push(egui::Event::PointerMoved(egui::pos2(origin.x + dx, y)));
        d.frames(1);
        match &d.harness.state().tl.drag {
            Some(filmcraft_ui_egui::panels::timeline::Drag::Move { offset, .. }) => assert_eq!(*offset, filmcraft_time::Tick::ZERO),
            other => panic!("Expected a move preview, got {other:?}"),
        }
        assert_eq!(d.harness.state().session.active_sequence().unwrap().find_item(clip).unwrap().1.start, start, "Preview must not edit the project");
    }
    if let Some(dir) = std::env::var_os("CRAFT_TIMELINE_FIXTURES") {
        std::fs::create_dir_all(&dir).unwrap();
        d.harness.render().unwrap().save(std::path::Path::new(&dir).join("track-move-preview.png")).unwrap();
    }
    let release = egui::pos2(origin.x + 6.0, y);
    d.harness.input_mut().events.push(egui::Event::PointerButton {
        pos: release,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    d.frames(2);
    let q = d.harness.state().session.active_sequence().unwrap();
    assert_eq!(q.find_item(clip).unwrap().0, q.audio_tracks[1].id);
    assert_eq!(q.find_item(clip).unwrap().1.start, start);
    if let Some(dir) = std::env::var_os("CRAFT_TIMELINE_FIXTURES") {
        d.harness.render().unwrap().save(std::path::Path::new(&dir).join("track-move-dropped.png")).unwrap();
    }
}

#[test]
fn option_drag_copies_a_clip_and_a_plain_drag_moves_it() {
    let mut d = Driver::demo();
    // the clip on V2 (no linked sound), and empty space to its right
    let q = d.harness.state().session.active_sequence().unwrap().clone();
    let v2 = q.video_tracks[1].items[0].clone();
    let n = clip_count(&mut d);
    let r = d.rect(&format!("timeline.clip.{}", v2.id.0));
    let (x, y) = (r[0] + 10.0, r[1] + r[3] / 2.0);
    d.ok("ui.drag", json!({"from": {"x": x, "y": y}, "to": {"x": x + 240.0, "y": y}, "steps": 12, "modifiers": {"alt": true}}));
    d.frames(3);
    let q2 = d.harness.state().session.active_sequence().unwrap().clone();
    assert_eq!(clip_count(&mut d), n + 1, "one copy was added");
    assert_eq!(q2.find_item(v2.id).map(|(_, i)| i.start), Some(v2.start), "the original stays");
    let sel = d.harness.state().session.state.selection.clone();
    assert_eq!(sel.len(), 1);
    let (track, copy) = q2.find_item(sel[0]).unwrap();
    assert_ne!(copy.id, v2.id);
    assert_eq!((track, copy.item, copy.duration), (q.video_tracks[1].id, v2.item, v2.duration));
    assert!(copy.start > v2.start, "the copy is where the pointer let go");
    // one undo removes the copy
    d.exec("edit.undo", json!({}));
    assert_eq!(clip_count(&mut d), n);
    // without Option the same drag moves the clip
    d.ok("ui.drag", json!({"from": {"x": x, "y": y}, "to": {"x": x + 240.0, "y": y}, "steps": 12}));
    d.frames(3);
    assert_eq!(clip_count(&mut d), n);
    assert!(d.harness.state().session.active_sequence().unwrap().find_item(v2.id).unwrap().1.start > v2.start, "moved");
}

#[test]
fn option_drag_preserves_an_already_selected_linked_group() {
    for audio in [false, true] {
        let mut d = Driver::demo();
        d.exec("file.newSequence", json!({"name": "Group copy", "video": 1, "audio": 1}));
        let session = &d.harness.state().session;
        let item = session.project.items.values().find(|i| i.has_video() && i.has_audio() && i.as_media().is_some()).unwrap().id;
        let duration = session.sequence_rate().tick_of(48).0;
        for frame in [0, 72] {
            d.exec("timeline.place", json!({"item": item.0, "frame": frame, "duration": duration}));
        }
        d.ok("ui.set", json!({"timeline": {"pps": 40.0, "scroll": 0.0}}));
        d.frames(3);
        let before = d.harness.state().session.project.clone();
        let q = d.harness.state().session.active_sequence().unwrap().clone();
        let videos: Vec<u64> = q.video_tracks[0].items.iter().map(|i| i.id.0).collect();
        d.exec("timeline.select", json!({"clips": videos}));
        let original_selection = d.harness.state().session.state.selection.clone();
        assert_eq!(original_selection.len(), 4);
        let grabbed = if audio { &q.audio_tracks[0].items[0] } else { &q.video_tracks[0].items[0] };
        let r = d.rect(&format!("timeline.clip.{}", grabbed.id.0));
        let (x, y) = (r[0] + r[2] * 0.5, r[1] + r[3] * 0.3);
        d.ok("ui.drag", json!({"from": {"x": x, "y": y}, "to": {"x": x + 300.0, "y": y}, "steps": 12, "modifiers": {"alt": true}}));
        d.frames(3);
        let session = &d.harness.state().session;
        let copied = &session.state.selection;
        assert_eq!(copied.len(), 4, "Alt-drag copies the entire selected group");
        assert!(copied.iter().all(|id| !original_selection.contains(id)));
        assert!(session.state.linked_selection);
        let after = session.active_sequence().unwrap();
        after.check().unwrap();
        assert_eq!(after.all_tracks().map(|t| t.items.len()).sum::<usize>(), 8);
        let delta = copied.iter().map(|id| after.find_item(*id).unwrap().1.start).min().unwrap();
        assert!(delta > q.duration(), "the copies land in empty time");
        for (track, original) in q.all_tracks().flat_map(|t| t.items.iter().map(move |i| (t.id, i))) {
            assert_eq!(after.find_item(original.id).unwrap().1, original, "the originals stay put");
            let copy = after.track(track).unwrap().items.iter().find(|i| copied.contains(&i.id) && i.start == original.start + delta).unwrap();
            assert_eq!((copy.item, copy.source_in, copy.duration), (original.item, original.source_in, original.duration));
            assert_ne!(copy.link, original.link, "the copies have a new link group");
        }
        d.exec("edit.undo", json!({}));
        assert_eq!(*d.harness.state().session.project, *before, "one undo removes the group copy");
    }
}

#[test]
fn dragging_after_frame_hold_with_repeated_clip_arguments_edits_once() {
    for copy in [false, true] {
        let mut d = Driver::demo();
        let original = d.harness.state().session.active_sequence().unwrap().video_tracks[1].items[0].clone();
        d.exec("clip.frameHold", json!({"clips": [original.id.0, original.id.0], "time": original.start.0}));
        let before = d.harness.state().session.project.clone();
        let undo = d.harness.state().session.history.undo.len();
        let n = clip_count(&mut d);
        let r = d.rect(&format!("timeline.clip.{}", original.id.0));
        let (x, y) = (r[0] + 10.0, r[1] + r[3] / 2.0);
        d.ok("ui.drag", json!({"from": {"x": x, "y": y}, "to": {"x": x + 240.0, "y": y}, "steps": 12, "modifiers": {"alt": copy}}));
        d.frames(3);
        let s = &d.harness.state().session;
        let q = s.active_sequence().unwrap();
        if copy {
            assert_eq!(s.state.selection.len(), 1, "one selected clip produces one copy");
            let copied = s.state.selection[0];
            assert_ne!(copied, original.id);
            assert!(q.find_item(copied).unwrap().1.start > original.start);
            assert_eq!(q.find_item(original.id).unwrap().1.start, original.start);
        } else {
            assert!(q.find_item(original.id).unwrap().1.start > original.start, "a repeated selection must still drag");
        }
        let ids: Vec<_> = q.all_tracks().flat_map(|t| t.items.iter().map(|i| i.id)).collect();
        assert_eq!(ids.len(), n + usize::from(copy));
        assert_eq!(ids.iter().copied().collect::<std::collections::HashSet<_>>().len(), ids.len());
        q.check().unwrap();
        assert_eq!(s.history.undo.len(), undo + 1);
        let after = s.project.clone();
        d.exec("edit.undo", json!({}));
        assert_eq!(d.harness.state().session.project, before);
        d.exec("edit.redo", json!({}));
        assert_eq!(d.harness.state().session.project, after);
    }
}
