//! Headless UI tests of File ▸ Export from the menus (#382): a menu entry carries no parameters, so
//! the entries whose command needs a `path` ask the host's save panel for it, and Media… opens the
//! Export mode like ⌘M, instead of failing with a missing `path`.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use filmcraft_ui_egui::state::Mode;
use serde_json::{Value, json};

/// What the save panel was asked: filter label, extensions, suggested name.
type Asked = Arc<Mutex<Vec<(String, Vec<String>, String)>>>;

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
}

impl Driver {
    /// `save_to: None` is a cancelled save panel.
    fn new(session: Session, save_to: Option<std::path::PathBuf>, asked: Asked) -> Self {
        let (tx, rx) = channel();
        let mut app = FilmcraftApp::new(session).with_control(rx);
        app.hooks.pick_save_as = Some(Box::new(move |filter, exts, suggested| {
            asked.lock().unwrap().push((filter.to_string(), exts.iter().map(|e| e.to_string()).collect(), suggested.to_string()));
            save_to.as_ref().map(|d| d.join(suggested).to_string_lossy().into_owned())
        }));
        let mut builder = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000);
        if std::env::var_os("FILMCRAFT_UI_SNAPSHOT_DIR").is_some() || std::env::var_os("FILMCRAFT_SEQUENCE_AAF_UI_SHOTS").is_some() {
            builder = builder.wgpu();
        }
        let harness = builder.build_eframe(move |_cc| app);
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

    fn call(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                return v;
            }
        }
        panic!("no reply to {method} {params}");
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let v = self.call(method, params.clone());
        assert_eq!(v["ok"], json!(true), "{method} {params} failed: {v}");
        v["result"].clone()
    }
}

/// The demo project (sequences, clips) with one clip selected in the Project panel.
fn session() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let clip = s.project.items.iter().find(|(_, it)| it.name.ends_with(".mp4")).map(|(id, _)| id.0).unwrap();
    s.execute("project.select", json!({"items": [clip]})).unwrap();
    s
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fc-export-menu-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Every File ▸ Export entry whose command needs a path, and the extension it saves.
const SAVED: [(&str, &str); 6] = [
    ("file.exportEdl", "edl"),
    ("file.exportFcp7Xml", "xml"),
    ("file.exportFcpxml", "fcpxml"),
    ("file.exportOtio", "otio"),
    ("file.exportAle", "ale"),
    ("file.exportSelectionProject", "fcproj"),
];

#[test]
fn file_export_entries_ask_for_the_destination() {
    let dir = tmp("save");
    let asked = Asked::default();
    let mut d = Driver::new(session(), Some(dir.clone()), asked.clone());
    let menu = d.ok("ui.menu.list", json!({}));
    for (id, ext) in SAVED {
        let entry = menu.as_array().unwrap().iter().find(|i| i["id"] == id).unwrap_or_else(|| panic!("{id} missing from the menus"));
        assert_eq!(entry["path"], json!(["File", "Export"]), "{id}");
        let r = d.ok("ui.menu.invoke", json!({"id": id}));
        d.frames(2);
        let (_, exts, suggested) = asked.lock().unwrap().last().cloned().unwrap_or_else(|| panic!("{id} did not ask where to save"));
        assert_eq!(exts, [ext], "{id}");
        assert!(suggested.ends_with(&format!(".{ext}")) && suggested.len() > ext.len() + 1, "{id}: {suggested}");
        let written = dir.join(&suggested);
        assert!(written.metadata().is_ok_and(|m| m.len() > 0), "{id} wrote nothing at {} ({r})", written.display());
    }
    assert_eq!(asked.lock().unwrap().len(), SAVED.len());
}

#[test]
fn cancelling_the_save_panel_writes_nothing_and_reports_no_error() {
    let asked = Asked::default();
    let mut d = Driver::new(session(), None, asked.clone());
    for (id, _) in SAVED {
        assert_eq!(d.ok("ui.menu.invoke", json!({"id": id})), Value::Null, "{id}");
    }
    assert_eq!(asked.lock().unwrap().len(), SAVED.len());
    assert!(!d.harness.state().ui.status.contains("path"), "{}", d.harness.state().ui.status);
}

#[test]
fn sequence_aaf_menu_saves_only_the_active_timeline_and_preserves_project_save() {
    let dir = tmp("sequence-aaf");
    let asked = Asked::default();
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"name":"Championship"})).unwrap();
    s.execute("file.newSequence", json!({"name":"Main sequence"})).unwrap();
    s.execute("file.newSequence", json!({"name":"TGOU"})).unwrap();
    let native = dir.join("All sequences.fcproj");
    s.execute("file.saveAs", json!({"path":native})).unwrap();
    s.edit("Keep unsaved work", |project, _| {
        project.name = "Native project".into();
        Ok(())
    })
    .unwrap();
    let before = s.project.clone();
    let mut d = Driver::new(s, Some(dir.clone()), asked.clone());
    let menu = d.ok("ui.menu.list", json!({}));
    let entry = menu.as_array().unwrap().iter().find(|i| i["id"] == "sequence.saveAaf").unwrap();
    assert_eq!(entry["path"], json!(["Sequence"]));
    assert_eq!(entry["enabled"], true);
    d.ok("ui.click", json!({"id":"menu.Sequence"}));
    d.frames(3);
    assert!(!d.ok("ui.elements", json!({"prefix":"menu.sequence.saveAaf"})).as_array().unwrap().is_empty());
    if let Some(folder) = std::env::var_os("FILMCRAFT_SEQUENCE_AAF_UI_SHOTS") {
        std::fs::create_dir_all(&folder).unwrap();
        d.harness.render().unwrap().save(std::path::PathBuf::from(folder).join("sequence-save-aaf-menu.png")).unwrap();
    }
    d.ok("ui.click", json!({"id":"menu.sequence.saveAaf"}));
    d.frames(3);
    assert_eq!(*asked.lock().unwrap(), vec![("Editable AAF sequence".into(), vec!["aaf".into()], "TGOU.aaf".into())]);
    let bytes = std::fs::read(dir.join("TGOU.aaf")).unwrap();
    let (imported, _, _) = filmcraft_interchange::aaf::import(&bytes, &Default::default()).unwrap();
    assert_eq!(imported.sequences.len(), 1);
    assert_eq!(imported.project.item(imported.sequences[0]).unwrap().name, "TGOU");
    assert_eq!(*d.harness.state().session.project, *before);
    assert!(d.harness.state().session.is_dirty());
    assert_eq!(d.harness.state().session.path.as_deref(), native.to_str());
    assert!(d.harness.state().ui.status.contains("TGOU.aaf"));
    d.ok("ui.menu.invoke", json!({"id":"file.save"}));
    assert!(!d.harness.state().session.is_dirty());
    let mut restored = Session::default();
    restored.execute("file.open", json!({"path":native})).unwrap();
    assert_eq!(restored.project.sequences().count(), 3);
}

#[test]
fn sequence_aaf_menu_cancel_and_no_sequence_do_not_change_the_project() {
    let asked = Asked::default();
    let s = session();
    let before = s.project.clone();
    let mut d = Driver::new(s, None, asked.clone());
    assert_eq!(d.ok("ui.menu.invoke", json!({"id":"sequence.saveAaf"})), Value::Null);
    assert_eq!(*d.harness.state().session.project, *before);
    assert_eq!(asked.lock().unwrap().len(), 1);
    let asked = Asked::default();
    let mut d = Driver::new(Session::default(), Some(tmp("sequence-aaf-disabled")), asked.clone());
    assert_eq!(d.call("ui.menu.invoke", json!({"id":"sequence.saveAaf"}))["ok"], false);
    assert!(asked.lock().unwrap().is_empty());
}

#[test]
fn sequence_aaf_picker_keeps_the_chosen_tab_and_refuses_a_replaced_project() {
    for replace_project in [false, true] {
        let dir = tmp(if replace_project { "aaf-picker-replace" } else { "aaf-picker-tab" });
        let mut s = Session::default();
        s.execute("file.newSequence", json!({"name":"First"})).unwrap();
        let first = s.state.active_sequence;
        s.execute("file.newSequence", json!({"name":"Chosen"})).unwrap();
        let mut d = Driver::new(s, None, Asked::default());
        let (tx, rx) = channel();
        let mut rx = Some(rx);
        d.harness.state_mut().hooks.run_dialog = Some(Box::new(move |_| rx.take().unwrap()));
        d.ok("ui.menu.invoke", json!({"id":"sequence.saveAaf"}));
        if replace_project {
            d.harness.state_mut().session.execute("file.newProject", json!({"name":"Different project"})).unwrap();
        } else {
            d.harness.state_mut().session.state.active_sequence = first;
        }
        let output = dir.join("chosen.aaf");
        tx.send(vec![output.to_string_lossy().into_owned()]).unwrap();
        d.frames(4);
        if replace_project {
            assert!(!output.exists());
            assert!(d.harness.state().ui.status.contains("project changed"));
        } else {
            let (imported, _, _) = filmcraft_interchange::aaf::import(&std::fs::read(output).unwrap(), &Default::default()).unwrap();
            assert_eq!(imported.project.item(imported.sequences[0]).unwrap().name, "Chosen");
            assert_eq!(d.harness.state().session.state.active_sequence, first);
        }
    }
}

#[test]
fn foreign_aaf_replacement_explains_omitted_effects_and_requires_an_explicit_button() {
    for replace in [false, true] {
        let dir = tmp(if replace { "aaf-foreign-replace" } else { "aaf-foreign-cancel" });
        let wav = dir.join("original.wav");
        std::fs::write(&wav, filmcraft_interchange::wav::wav_file(&vec![0u8; 16_000], 1, 8000, 16)).unwrap();
        let mut s = Session::default();
        s.execute("file.newSequence", json!({"name":"TGOU","video":0,"audio":1})).unwrap();
        let sequence = s.state.active_sequence.unwrap();
        let item = s.execute("file.import", json!({"paths":[wav]})).unwrap()["items"][0].clone();
        s.execute("timeline.place", json!({"item":item,"track":"A1","seconds":0,"video":false})).unwrap();
        s.edit("Gain operation fixture", |project, _| {
            project.sequence_mut(sequence).unwrap().audio_tracks[0].items[0].gain_db = -6.0;
            Ok(())
        })
        .unwrap();
        let (mut bytes, _) = filmcraft_interchange::aaf::export(&s.project, sequence, &Default::default()).unwrap();
        // Our own generated AAF, with its standard audio-gain AUID changed to an unknown
        // operation. This reproduces a foreign effect without shipping a third-party fixture.
        let gain = [0x94, 0xa8, 0x2e, 0x9d, 0x68, 0x09, 0xd3, 0x11, 0x8a, 0x38, 0x00, 0x50, 0x04, 0x0e, 0xf7, 0xd2];
        let offsets: Vec<_> = bytes.windows(gain.len()).enumerate().filter_map(|(i, value)| (value == gain).then_some(i)).collect();
        assert!(!offsets.is_empty());
        for offset in offsets {
            bytes[offset] = 0x95;
        }
        let target = dir.join("TGOU.aaf");
        std::fs::write(&target, &bytes).unwrap();
        let (_, _, report) = filmcraft_interchange::aaf::import(&bytes, &Default::default()).unwrap();
        assert!(report.has_warnings());
        s.execute("file.importAaf", json!({"path":target})).unwrap();
        let imported = s.state.active_sequence.unwrap();
        let offset = filmcraft_time::FrameRate::FPS_24.tick_of(12);
        s.edit("Align imported lav", |project, _| {
            project.sequence_mut(imported).unwrap().audio_tracks[0].items[0].start = offset;
            Ok(())
        })
        .unwrap();
        let before = s.project.clone();
        let mut d = Driver::new(s, Some(dir.clone()), Asked::default());
        let prompt = d.ok("ui.menu.invoke", json!({"id":"sequence.saveAaf"}));
        assert_eq!(prompt["needsConfirmation"], true);
        d.frames(3);
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        assert!(d.harness.state().ui.clip_dialog.is_some());
        assert!(!d.ok("ui.elements", json!({"prefix":"saveSequenceAaf.warning"})).as_array().unwrap().is_empty());
        if let Some(folder) = std::env::var_os("FILMCRAFT_SEQUENCE_AAF_UI_SHOTS") {
            std::fs::create_dir_all(&folder).unwrap();
            d.harness.render().unwrap().save(std::path::PathBuf::from(folder).join("sequence-aaf-replace-warning.png")).unwrap();
        }
        d.harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        });
        d.frames(2);
        assert_eq!(std::fs::read(&target).unwrap(), bytes, "Enter must not confirm a lossy replacement");
        d.ok("ui.click", json!({"id":if replace {"saveSequenceAaf.ok"} else {"saveSequenceAaf.cancel"}}));
        d.frames(3);
        assert!(d.harness.state().ui.clip_dialog.is_none());
        assert_eq!(*d.harness.state().session.project, *before);
        if replace {
            assert_eq!(std::fs::read(dir.join("TGOU (before FilmCraft).aaf")).unwrap(), bytes);
            let mut reopened = Session::default();
            reopened.execute("file.open", json!({"path":target})).unwrap();
            assert_eq!(reopened.active_sequence().unwrap().audio_tracks[0].items[0].start, offset);
        } else {
            assert_eq!(std::fs::read(&target).unwrap(), bytes);
            assert!(!dir.join("TGOU (before FilmCraft).aaf").exists());
        }
    }
}

#[test]
fn media_from_the_menu_opens_the_export_mode() {
    let mut d = Driver::new(session(), None, Asked::default());
    assert_eq!(d.harness.state().ui.mode, Mode::Edit);
    d.ok("ui.menu.invoke", json!({"id": "file.exportMedia"}));
    d.frames(2);
    assert_eq!(d.harness.state().ui.mode, Mode::Export);
    // with a path it still exports directly, as scripts and the CLI use it
    let out = tmp("media").join("direct.wav");
    let r = d.ok("ui.menu.invoke", json!({"id": "file.exportMedia", "params": {"path": out.to_string_lossy(), "format": "wav", "wait": true}}));
    assert!(out.metadata().is_ok_and(|m| m.len() > 0), "{r}");
}

#[test]
fn file_export_entries_need_an_open_sequence() {
    let mut d = Driver::new(Session::default(), Some(tmp("noseq")), Asked::default());
    for id in ["file.exportMedia", "file.exportEdl", "file.exportFcp7Xml", "file.exportFcpxml", "file.exportOtio"] {
        assert_eq!(d.call("ui.menu.invoke", json!({"id": id}))["ok"], json!(false), "{id}");
        assert_eq!(d.harness.state().ui.mode, Mode::Edit, "{id}");
    }
}

#[test]
fn marker_csv_menu_exports_sequence_review_notes() {
    let dir = tmp("markers");
    let asked = Asked::default();
    let mut s = session();
    s.execute("markers.clearAll", json!({"target":"program"})).unwrap();
    s.execute("markers.add", json!({"target":"program", "frame":24, "name":"Review, 日本語", "comment":"Change \"this\"\nNext line"})).unwrap();
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let mut d = Driver::new(s, Some(dir.clone()), asked.clone());
    let menu = d.ok("ui.menu.list", json!({}));
    let entry = menu.as_array().unwrap().iter().find(|i| i["id"] == "markers.exportCsv").unwrap();
    assert_eq!(entry["path"], json!(["Markers"]));
    assert_eq!(entry["enabled"], true);
    d.ok("ui.click", json!({"id":"menu.Markers"}));
    d.frames(3);
    let elements = d.ok("ui.elements", json!({"prefix":"menu.markers.exportCsv"}));
    assert!(!elements.as_array().unwrap().is_empty(), "the menu command has an automation id");
    if let Some(dir) = std::env::var_os("FILMCRAFT_UI_SNAPSHOT_DIR") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        d.harness.render().unwrap().save(dir.join("marker-csv-menu.png")).unwrap();
    }
    d.ok("ui.click", json!({"id":"menu.markers.exportCsv"}));
    d.frames(3);
    let (filter, exts, suggested) = asked.lock().unwrap().last().cloned().unwrap();
    assert_eq!(filter, "Marker report (CSV)");
    assert_eq!(exts, ["csv"]);
    assert!(suggested.ends_with(".csv"));
    let bytes = std::fs::read(dir.join(suggested)).unwrap();
    let csv = String::from_utf8(bytes).unwrap();
    assert!(csv.contains("\"Review, 日本語\",\"Change \"\"this\"\"\nNext line\""));
    assert_eq!(csv.matches("\"Review, 日本語\"").count(), 1);
    assert_eq!(*d.harness.state().session.project, *before);
    assert_eq!(d.harness.state().session.history.undo.len(), undo);
    assert!(d.harness.state().ui.status.starts_with("Exported "));
    let direct = dir.join("direct, 字幕.csv");
    d.ok("engine.execute", json!({"command":"markers.exportCsv", "params":{"path":direct.to_string_lossy()}}));
    assert_eq!(std::fs::read(direct).unwrap(), csv.as_bytes());
    assert_eq!(asked.lock().unwrap().len(), 1);
}

#[test]
fn marker_csv_save_cancel_and_disabled_menu_leave_the_session_alone() {
    let asked = Asked::default();
    let mut d = Driver::new(session(), None, asked.clone());
    let before = d.harness.state().session.project.clone();
    assert_eq!(d.ok("ui.menu.invoke", json!({"id":"markers.exportCsv"})), Value::Null);
    assert_eq!(*d.harness.state().session.project, *before);
    assert_eq!(asked.lock().unwrap().len(), 1);
    let asked = Asked::default();
    let mut d = Driver::new(Session::default(), None, asked.clone());
    assert_eq!(d.call("ui.menu.invoke", json!({"id":"markers.exportCsv"}))["ok"], false);
    assert!(asked.lock().unwrap().is_empty(), "disabled exports must not open a save dialog");
}
