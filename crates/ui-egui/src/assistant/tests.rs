use super::*;
use filmcraft_engine::Session;

fn turn(app: &mut FilmcraftApp) {
    let ctx = egui::Context::default();
    let deadline = Instant::now() + Duration::from_secs(8);
    while app.assistant.pending.is_some() && Instant::now() < deadline {
        poll(app, &ctx);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(app.assistant.pending.is_none(), "AI turn did not complete");
}

#[test]
fn mock_openai_turn_uses_real_command_dispatch_and_undo() {
    let mut app = FilmcraftApp::new(Session::default());
    app.assistant.prompt = "Set project notes to AI edit".into();
    app.assistant.attach = false;
    let mut count = 0;
    begin_with(&mut app, &egui::Context::default(), move |body| {
        count += 1;
        if count == 1 {
            Ok(json!({"status":"completed","output":[{"type":"function_call","call_id":"edit1","name":"filmcraft",
                "arguments":json!({"method":"engine.execute","params_json":json!({"command":"project.setNotes","params":{"text":"AI edit"}}).to_string()}).to_string()}]}))
        } else {
            let output = body["input"].as_array().unwrap().last().unwrap()["output"].as_str().unwrap();
            assert_eq!(serde_json::from_str::<Value>(output).unwrap()["ok"], true);
            Ok(json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Updated the project notes."}]}]}))
        }
    }).unwrap();
    turn(&mut app);
    assert_eq!(app.session.project.notes, "AI edit");
    assert!(app.assistant.transcript.iter().any(|(role, _)| role == "Tool"));
    assert!(app.assistant.transcript.iter().any(|(role, _)| role == "Assistant"));
    app.session.execute("edit.undo", json!({})).unwrap();
    assert!(app.session.project.notes.is_empty());
}

#[test]
fn project_replacement_rejects_an_awaiting_edit() {
    let mut app = FilmcraftApp::new(Session::default());
    app.assistant.prompt = "Do work".into();
    app.assistant.attach = false;
    let (start, wait) = mpsc::channel();
    begin_with(&mut app, &egui::Context::default(), move |_| {
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        Ok(json!({"status":"completed","output":[{"type":"function_call","call_id":"c1","name":"filmcraft",
            "arguments":json!({"method":"engine.execute","params_json":json!({"command":"project.setNotes","params":{"text":"stale edit"}}).to_string()}).to_string()}]}))
    }).unwrap();
    app.session.execute("file.newProject", json!({"name":"New project"})).unwrap();
    start.send(()).unwrap();
    turn(&mut app);
    assert!(app.session.project.notes.is_empty());
    assert!(app.assistant.history.is_empty());
}

#[test]
fn stop_rejects_queued_actions() {
    let mut app = FilmcraftApp::new(Session::default());
    app.assistant.prompt = "Do work".into();
    app.assistant.attach = false;
    begin_with(&mut app, &egui::Context::default(), |_| Ok(json!({"status":"completed","output":[]}))).unwrap();
    stop(&mut app.assistant);
    turn(&mut app);
    assert!(app.assistant.message.contains("Stopped"));
    assert!(app.session.project.notes.is_empty());
}

#[test]
fn chat_credentials_and_messages_are_not_in_serialized_ui_or_projects() {
    let mut app = FilmcraftApp::new(Session::default());
    app.assistant.key = "sk-private-test-credential".into();
    app.assistant.prompt = "private draft".into();
    record(&mut app.assistant, "You", "private conversation".into());
    let inspected = control::inspect(&app, &egui::Context::default()).to_string();
    let project = filmcraft_format::encode_with_view(&app.session.project, Some(&app.session.project_view()), false);
    for secret in ["sk-private-test-credential", "private draft", "private conversation"] {
        assert!(!inspected.contains(secret));
        assert!(!String::from_utf8_lossy(&project).contains(secret));
    }
    let status = route(&mut app, &egui::Context::default(), "ai.status").unwrap().unwrap().to_string();
    assert!(!status.contains("sk-private-test-credential"));
}

#[test]
fn tool_arguments_are_hostile_and_self_invocation_is_rejected() {
    for (name, args) in [
        ("shell", json!({})),
        ("filmcraft", json!({"method":"app.quit","params_json":"{}"})),
        ("filmcraft", json!({"method":"ui.type","params_json":"[]"})),
        ("filmcraft", json!({"method":"engine.execute","params_json":"{bad"})),
        ("filmcraft", json!({"method":"engine.execute","params_json":"{\"command\":\"ai.send\"}"})),
        ("filmcraft", json!({"method":"ui.focus","params_json":"{\"id\":\"ai.key\"}"})),
        ("video_frame", json!({"monitor":"Desktop"})),
        ("video_frame", json!({"monitor":"Program","path":"secret"})),
    ] {
        assert!(tools::parse(name, &args).is_err(), "accepted {name} {args}");
    }
    assert!(
        tools::parse(
            "filmcraft",
            &json!({"method":"engine.execute","params_json":json!({"command":"project.setNotes","params":{"text":"https://ai.example"}}).to_string()})
        )
        .is_ok()
    );
}

#[test]
fn rendered_program_and_source_frames_are_attached_without_seeking() {
    let mut app = FilmcraftApp::new(Session::default());
    app.session.execute("file.newSequence", json!({"name":"Shot","width":128,"height":72,"fps":24})).unwrap();
    let source = filmcraft_engine::demo::add_generator(
        Arc::make_mut(&mut app.session.project),
        &app.session.media,
        filmcraft_media::generators::GeneratorSource::new(
            filmcraft_media::Generator::Demo(filmcraft_media::DemoScene::Aurora),
            128,
            72,
            filmcraft_time::FrameRate::FPS_24,
            filmcraft_time::Tick::from_seconds_f64(5.0),
        ),
        "Shot",
        filmcraft_project::Label::Iris,
        None,
    );
    app.session.execute("source.open", json!({"item":source.0})).unwrap();
    let playhead = app.session.playhead();
    let source_time = app.session.state.source_playhead;
    app.assistant.prompt = "Look at my shot".into();
    begin_with(&mut app, &egui::Context::default(), |body| {
        let content = body["input"][0]["content"].as_array().unwrap();
        let images: Vec<_> = content.iter().filter(|part| part["type"] == "input_image").collect();
        assert_eq!(images.len(), 2);
        assert!(images.iter().all(|image| image["image_url"].as_str().unwrap().starts_with("data:image/png;base64,iVBOR")));
        Ok(json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"I can see both frames."}]}]}))
    })
    .unwrap();
    turn(&mut app);
    assert_eq!(app.assistant.message, "Ready");
    assert_eq!(app.session.playhead(), playhead);
    assert_eq!(app.session.state.source_playhead, source_time);
}

#[test]
fn chat_window_has_registered_controls_without_exposing_the_key() {
    use egui_kittest::Harness;
    let mut app = FilmcraftApp::new(Session::default());
    app.show_panel(crate::dock::PanelKind::AiChat);
    app.assistant.key = "sk-private-test-credential".into();
    let mut h = Harness::builder().with_size(egui::vec2(1500.0, 1000.0)).build_eframe(move |_| app);
    h.run_steps(4);
    for id in ["ai.key", "ai.settings", "ai.prompt", "ai.send", "ai.stop", "ai.attach", "ai.clear", "ai.forgetKey", "ai.saveKey"] {
        assert!(h.state().auto.find(id).is_some(), "missing {id}");
    }
    assert!(!serde_json::to_string(h.state().auto.all()).unwrap().contains("sk-private-test-credential"));
}

#[test]
fn assistant_synthetic_key_runs_once_and_waits_for_the_ui() {
    use egui_kittest::Harness;
    let mut app = FilmcraftApp::new(Session::default());
    app.ui.tool = crate::state::Tool::Razor;
    app.assistant.prompt = "Switch to the Selection tool".into();
    app.assistant.attach = false;
    let mut h = Harness::builder().with_size(egui::vec2(1500.0, 1000.0)).build_eframe(move |_| app);
    h.run_steps(4);
    let ctx = h.ctx.clone();
    let mut count = 0;
    begin_with(h.state_mut(), &ctx, move |body| {
        count += 1;
        if count == 1 {
            Ok(json!({"status":"completed","output":[{"type":"function_call","call_id":"key1","name":"filmcraft",
                "arguments":json!({"method":"ui.key","params_json":"{\"key\":\"V\"}"}).to_string()}]}))
        } else {
            assert_eq!(count, 2, "tool dispatched once");
            let output = body["input"].as_array().unwrap().last().unwrap()["output"].as_str().unwrap();
            assert_eq!(serde_json::from_str::<Value>(output).unwrap()["ok"], true);
            Ok(json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Selection tool is active."}]}]}))
        }
    }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while h.state().assistant.pending.is_some() && Instant::now() < deadline {
        let mut raw = std::mem::take(h.input_mut());
        eframe::App::raw_input_hook(h.state_mut(), &ctx, &mut raw);
        *h.input_mut() = raw;
        ctx.request_repaint();
        h.step();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(h.state().assistant.pending.is_none());
    assert_eq!(h.state().assistant.message, "Ready");
    assert_eq!(h.state().ui.tool, crate::state::Tool::Selection);
}

#[test]
fn forget_key_removes_the_password_fields_undo_history() {
    use egui_kittest::{Harness, kittest::Queryable};
    let mut app = FilmcraftApp::new(Session::default());
    app.show_panel(crate::dock::PanelKind::AiChat);
    let mut h = Harness::builder().with_size(egui::vec2(1500.0, 1000.0)).build_eframe(move |_| app);
    h.run_steps(4);
    let ctx = h.ctx.clone();
    let id = h.state().auto.find("ai.key").unwrap().response_id.unwrap();
    ctx.memory_mut(|memory| memory.request_focus(id));
    h.input_mut().events.push(egui::Event::Text("sk-private-test-credential".into()));
    h.run_steps(3);
    assert_eq!(h.state().assistant.key, "sk-private-test-credential");
    h.get_by_label("Forget key").click();
    h.run_steps(3);
    assert!(h.state().assistant.key.is_empty());
    ctx.memory_mut(|memory| memory.request_focus(id));
    h.input_mut().events.push(egui::Event::Key {
        key: egui::Key::Z,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers { command: true, ..Default::default() },
    });
    h.run_steps(3);
    assert!(h.state().assistant.key.is_empty(), "Undo must not restore a forgotten API key");
}

#[test]
fn ai_preferences_persist_with_the_editor_and_never_include_the_key() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("preferences.json");
    let mut app = FilmcraftApp::new(Session::default());
    app.session.prefs_path = Some(path.clone());
    app.session.execute("prefs.set", json!({"values":{"ai.model":"gpt-4.1","ai.attachFrames":false,"ai.rememberKey":true}})).unwrap();
    app.assistant.key = "sk-private-test-credential".into();
    let loaded = filmcraft_engine::autosave::Preferences::load(&path);
    assert_eq!(loaded.ai.model, "gpt-4.1");
    assert!(!loaded.ai.attach_frames);
    assert!(loaded.ai.remember_key);
    let mut reopened_session = Session::default();
    reopened_session.prefs = loaded;
    let mut reopened = FilmcraftApp::new(reopened_session);
    sync_preferences(&mut reopened);
    assert_eq!(reopened.assistant.model, "gpt-4.1");
    assert!(!reopened.assistant.attach);
    assert!(!std::fs::read_to_string(&path).unwrap().contains("sk-private-test-credential"));
    assert!(app.session.execute("prefs.set", json!({"key":"ai.apiKey","value":"sk-private-test-credential"})).is_err());
    assert!(app.session.execute("prefs.set", json!({"key":"ai.model","value":"a".repeat(129)})).is_err());
    let repaired =
        filmcraft_engine::autosave::Preferences::from_value(json!({"ai":{"model":[],"attachFrames":"bad","rememberKey":3},"general":{"showToolTips":false}}));
    assert_eq!(repaired.ai, filmcraft_engine::settings::AiPrefs::default());
    assert!(!repaired.general.show_tool_tips);
}

#[test]
fn ai_panel_floats_docks_and_round_trips_in_a_saved_workspace() {
    use crate::dock::{PanelKind, SavedWorkspace, WorkspacePrefs};
    use craft_ui::docking::Action;
    let mut app = FilmcraftApp::new(Session::default());
    app.show_panel(PanelKind::AiChat);
    assert!(app.ui.dock.contains(&PanelKind::AiChat));
    app.apply_dock(Action::Float { panel: PanelKind::AiChat, rect: [50.0, 50.0, 500.0, 700.0] }).unwrap();
    assert!(app.ui.dock.floating.iter().any(|window| window.contains(&PanelKind::AiChat)));
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("workspaces.json");
    WorkspacePrefs { saved: vec![SavedWorkspace::new("AI Editing", &app.ui.dock).unwrap()], current: "AI Editing".into() }.save(&path).unwrap();
    let restored = crate::dock::saved_workspace(&WorkspacePrefs::load(&path), "AI Editing");
    assert!(restored.layout.contains(&PanelKind::AiChat));
    app.apply_dock(Action::Close { panel: PanelKind::AiChat }).unwrap();
    assert!(!app.ui.dock.contains(&PanelKind::AiChat));
    app.show_panel(PanelKind::AiChat);
    assert!(app.ui.dock.contains(&PanelKind::AiChat));
}

#[test]
fn narrow_chat_panel_keeps_its_composer_and_actions_visible() {
    use egui_kittest::Harness;
    let mut app = FilmcraftApp::new(Session::default());
    app.assistant.key = "sk-private-test-credential".into();
    app.assistant.prompt = "Inspect this shot".into();
    app.assistant.message = "Ready".into();
    app.assistant.vault.message = "API key saved in the OS credential store".into();
    let mut h = Harness::builder().with_size(egui::vec2(220.0, 490.0)).build_ui_state(
        |ui, app| {
            app.auto.elements.clear();
            show(app, ui, ui.max_rect());
        },
        app,
    );
    h.run_steps(5);
    for id in ["ai.prompt", "ai.send", "ai.stop", "ai.clear"] {
        let [x, y, width, height] = h.state().auto.find(id).unwrap().rect;
        assert!(x >= 0.0 && y >= 0.0 && x + width <= 220.0 && y + height <= 490.0, "clipped {id}: {x},{y},{width},{height}");
    }
}
