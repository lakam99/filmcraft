use super::*;
use crate::control;
use filmcraft_engine::Session;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

#[test]
fn batch_renames_forty_bins_in_one_tool_call_with_normal_undo() {
    let mut app = FilmcraftApp::new(Session::default());
    app.session.prefs.ai.attach_frames = false;
    let ids: Vec<u64> =
        (0..40).map(|i| app.session.execute("file.newBin", json!({"name":format!("Original {i}")})).unwrap()["bin"].as_u64().unwrap()).collect();
    let commands: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| json!({"command":"project.renameBin","params_json":json!({"bin":id,"name":format!("Take {i:02}")}).to_string()}))
        .collect();
    *app.assistant.panel.prompt_mut() = "Rename all forty bins".into();
    let mut request = 0;
    begin_with(&mut app,&egui::Context::default(),move|body|{
        request+=1;if request==1 {Ok(json!({"status":"completed","output":[{"type":"function_call","call_id":"bulk","name":"batch_commands","arguments":json!({"editor":"filmcraft","label":"Rename 40 bins","commands":commands}).to_string()}]}))}
        else {let output:Value=serde_json::from_str(body["input"].as_array().unwrap().last().unwrap()["output"].as_str().unwrap()).unwrap();assert_eq!(output["completed"],40);Ok(json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Renamed all forty bins."}]}]}))}
    }).unwrap();
    turn(&mut app);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(app.session.project.root.find_bin(filmcraft_project::BinId(*id)).unwrap().name, format!("Take {i:02}"));
    }
    assert_eq!(app.assistant.panel.transcript().iter().filter(|(role, _)| role == "Tool").count(), 1);
    app.session.execute("edit.undo", json!({})).unwrap();
    assert_eq!(app.session.project.root.find_bin(filmcraft_project::BinId(ids[39])).unwrap().name, "Original 39");
}

#[test]
fn guide_observation_preserves_widget_local_value_changes() {
    let mut app = FilmcraftApp::new(Session::default());
    let ctx = egui::Context::default();
    let mut output = ctx.run_ui(Default::default(), |ui| {
        let mut response = ui.button("Value editor");
        response.mark_changed();
        app.auto.add_response("value.editor", &response, "Value editor");
        assert!(!ui.ctx().read_response(response.id).unwrap().changed(), "the reconstructed response cannot contain the widget-local flag");
        let adapter = Adapter { app: &mut app, ctx: ui.ctx() };
        assert!(adapter.guide_observation("value.editor", ui.ctx()).unwrap().changed);
    });
    output.textures_delta.clear();
}

#[test]
fn how_to_guidance_highlights_a_live_panel_without_editing_the_project() {
    use egui_kittest::{Harness, kittest::Queryable};
    let mut app = FilmcraftApp::new(Session::default());
    app.show_panel(crate::dock::PanelKind::AiChat);
    app.session.prefs.ai.attach_frames = false;
    let mut h = Harness::builder().with_size(egui::vec2(1500.0, 1000.0)).build_eframe(move |_| app);
    h.run_steps(4);
    let target = format!("panel.{}", crate::dock::PanelKind::Project.id());
    assert!(h.state().auto.find(&target).is_some());
    let project = h.state().session.project.clone();
    *h.state_mut().assistant.panel.prompt_mut() = "How do I find my media?".into();
    let ctx = h.ctx.clone();
    let id = target.clone();
    let mut turn = 0;
    begin_with(h.state_mut(), &ctx, move |_| {
        turn += 1;
        if turn == 1 { Ok(json!({"status":"completed","output":[{"type":"function_call","call_id":"guide","name":"highlight_ui","arguments":json!({"targets":[{"id":id,"label":"Find your media in the Project panel"}]}).to_string()}]})) }
        else { Ok(json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Use the highlighted Project panel to find your media."}]}]})) }
    }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while h.state().assistant.panel.busy() && Instant::now() < deadline {
        h.run_steps(1);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!h.state().assistant.panel.busy());
    h.run_steps(3);
    let status = route(h.state_mut(), &ctx, "ai.status").unwrap().unwrap();
    assert_eq!(status["highlights"][0]["id"], target);
    assert_eq!(status["highlights"][0]["visible"], true);
    assert!(h.query_by_label("1. Find your media in the Project panel (control hidden)").is_none());
    assert!(Arc::ptr_eq(&project, &h.state().session.project));
    if let Some(dir) = std::env::var_os("CRAFT_AI_EDITOR_FIXTURES") {
        std::fs::create_dir_all(&dir).unwrap();
        h.render().unwrap().save(std::path::Path::new(&dir).join("filmcraft-guidance.png")).unwrap();
    }
    route(h.state_mut(), &ctx, "ai.clearHighlights").unwrap().unwrap();
    assert_eq!(route(h.state_mut(), &ctx, "ai.status").unwrap().unwrap()["highlights"], json!([]));
}

fn turn(app: &mut FilmcraftApp) {
    let ctx = egui::Context::default();
    let deadline = Instant::now() + Duration::from_secs(8);
    while app.assistant.panel.busy() && Instant::now() < deadline {
        poll(app, &ctx);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!app.assistant.panel.busy(), "AI turn did not complete");
}

#[test]
fn mock_openai_turn_uses_real_command_dispatch_and_undo() {
    let mut app = FilmcraftApp::new(Session::default());
    *app.assistant.panel.prompt_mut() = "Set project notes to AI edit".into();
    app.session.prefs.ai.attach_frames = false;
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
    let label = filmcraft_engine::commands::command_specs().iter().find(|spec| spec.id == "project.setNotes").unwrap().label;
    let activity = app.assistant.panel.transcript().iter().find(|(role, _)| role == "Tool").unwrap();
    assert_eq!(activity.1, format!("{label} — Done"));
    assert!(!activity.1.contains("params_json"));
    assert!(app.assistant.panel.transcript().iter().any(|(role, _)| role == "Assistant"));
    app.session.execute("edit.undo", json!({})).unwrap();
    assert!(app.session.project.notes.is_empty());
}

#[test]
fn project_replacement_rejects_an_awaiting_edit() {
    let mut app = FilmcraftApp::new(Session::default());
    *app.assistant.panel.prompt_mut() = "Do work".into();
    app.session.prefs.ai.attach_frames = false;
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
    assert!(app.assistant.panel.context_items() == 0);
}

#[test]
fn stop_rejects_queued_actions() {
    let mut app = FilmcraftApp::new(Session::default());
    *app.assistant.panel.prompt_mut() = "Do work".into();
    app.session.prefs.ai.attach_frames = false;
    begin_with(&mut app, &egui::Context::default(), |_| Ok(json!({"status":"completed","output":[]}))).unwrap();
    app.assistant.panel.stop();
    turn(&mut app);
    assert!(app.assistant.panel.message().contains("Stopped"));
    assert!(app.session.project.notes.is_empty());
}

#[test]
fn chat_credentials_and_messages_are_not_in_serialized_ui_or_projects() {
    let mut app = FilmcraftApp::new(Session::default());
    app.assistant.panel.set_session_key("sk-private-test-credential".into()).unwrap();
    // Queue a private conversation through the shared plugin; no serializer can observe it.
    *app.assistant.panel.prompt_mut() = "private conversation".into();
    begin_with(&mut app, &egui::Context::default(), |_| {
        Ok(json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"private conversation"}]}]}))
    })
    .unwrap();
    turn(&mut app);
    *app.assistant.panel.prompt_mut() = "private draft".into();
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
    *app.assistant.panel.prompt_mut() = "Look at my shot".into();
    begin_with(&mut app, &egui::Context::default(), |body| {
        let content = body["input"][0]["content"].as_array().unwrap();
        let images: Vec<_> = content.iter().filter(|part| part["type"] == "input_image").collect();
        assert_eq!(images.len(), 2);
        assert!(images.iter().all(|image| image["image_url"].as_str().unwrap().starts_with("data:image/png;base64,iVBOR")));
        Ok(json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"I can see both frames."}]}]}))
    })
    .unwrap();
    turn(&mut app);
    assert_eq!(app.assistant.panel.message(), "Ready");
    assert_eq!(app.session.playhead(), playhead);
    assert_eq!(app.session.state.source_playhead, source_time);
}

#[test]
fn chat_window_has_registered_controls_without_exposing_the_key() {
    use egui_kittest::Harness;
    let mut app = FilmcraftApp::new(Session::default());
    app.show_panel(crate::dock::PanelKind::AiChat);
    app.assistant.panel.set_session_key("sk-private-test-credential".into()).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1500.0, 1000.0)).build_eframe(move |_| app);
    h.run_steps(4);
    assert!(h.state().auto.find("ai.key").is_none());
    let ctx = h.ctx.clone();
    route(h.state_mut(), &ctx, "ai.connection").unwrap().unwrap();
    h.run_steps(3);
    for id in ["ai.key", "ai.settings", "ai.prompt", "ai.send", "ai.attach", "ai.clear", "ai.forgetKey", "ai.saveKey"] {
        assert!(h.state().auto.find(id).is_some(), "missing {id}");
    }
    assert!(!serde_json::to_string(h.state().auto.all()).unwrap().contains("sk-private-test-credential"));
}

#[test]
fn assistant_synthetic_key_runs_once_and_waits_for_the_ui() {
    use egui_kittest::Harness;
    let mut app = FilmcraftApp::new(Session::default());
    app.ui.tool = crate::state::Tool::Razor;
    *app.assistant.panel.prompt_mut() = "Switch to the Selection tool".into();
    app.session.prefs.ai.attach_frames = false;
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
    while h.state().assistant.panel.busy() && Instant::now() < deadline {
        let mut raw = std::mem::take(h.input_mut());
        eframe::App::raw_input_hook(h.state_mut(), &ctx, &mut raw);
        *h.input_mut() = raw;
        ctx.request_repaint();
        h.step();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!h.state().assistant.panel.busy());
    assert_eq!(h.state().assistant.panel.message(), "Ready");
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
    route(h.state_mut(), &ctx, "ai.connection").unwrap().unwrap();
    h.run_steps(3);
    let id = h.state().auto.find("ai.key").unwrap().response_id.unwrap();
    ctx.memory_mut(|memory| memory.request_focus(id));
    h.input_mut().events.push(egui::Event::Text("sk-private-test-credential".into()));
    h.run_steps(3);
    assert!(h.state().assistant.panel.has_key());
    h.get_by_label("Forget key").click();
    h.run_steps(3);
    assert!(!h.state().assistant.panel.has_key());
    ctx.memory_mut(|memory| memory.request_focus(id));
    h.input_mut().events.push(egui::Event::ModifiersChanged(egui::Modifiers { command: true, ..Default::default() }));
    h.input_mut().events.push(egui::Event::Key {
        key: egui::Key::Z,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers { command: true, ..Default::default() },
    });
    h.run_steps(3);
    assert!(!h.state().assistant.panel.has_key(), "Undo must not restore a forgotten API key");
}

#[test]
fn ai_preferences_persist_with_the_editor_and_never_include_the_key() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("preferences.json");
    let mut app = FilmcraftApp::new(Session::default());
    app.session.prefs_path = Some(path.clone());
    app.session
        .execute("prefs.set", json!({"values":{"ai.model":"gpt-4.1","ai.attachFrames":false,"ai.rememberKey":true,"ai.rememberHistory":false}}))
        .unwrap();
    app.assistant.panel.set_session_key("sk-private-test-credential".into()).unwrap();
    let loaded = filmcraft_engine::autosave::Preferences::load(&path);
    assert_eq!(loaded.ai.model, "gpt-4.1");
    assert!(!loaded.ai.attach_frames);
    assert!(loaded.ai.remember_key);
    assert!(!loaded.ai.remember_history);
    let mut reopened_session = Session::default();
    reopened_session.prefs = loaded;
    let mut reopened = FilmcraftApp::new(reopened_session);
    poll(&mut reopened, &egui::Context::default());
    let preferences = with_panel(&mut reopened, &egui::Context::default(), |_, host| host.preferences()).unwrap();
    assert_eq!(preferences.model, "gpt-4.1");
    assert!(!reopened.session.prefs.ai.attach_frames);
    assert!(!std::fs::read_to_string(&path).unwrap().contains("sk-private-test-credential"));
    assert!(app.session.execute("prefs.set", json!({"key":"ai.apiKey","value":"sk-private-test-credential"})).is_err());
    assert!(app.session.execute("prefs.set", json!({"key":"ai.model","value":"a".repeat(129)})).is_err());
    let repaired = filmcraft_engine::autosave::Preferences::from_value(
        json!({"ai":{"model":[],"attachFrames":"bad","rememberKey":3,"rememberHistory":[]},"general":{"showToolTips":false}}),
    );
    assert_eq!(repaired.ai, filmcraft_engine::settings::AiPrefs::default());
    assert!(repaired.ai.remember_history);
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
    app.assistant.panel.set_session_key("sk-private-test-credential".into()).unwrap();
    *app.assistant.panel.prompt_mut() = "Inspect this shot".into();
    app.assistant.panel.fail("A previous turn failed; review completed edits before continuing.");
    let mut h = Harness::builder().with_size(egui::vec2(220.0, 490.0)).build_ui_state(
        |ui, app| {
            app.auto.elements.clear();
            show(app, ui, ui.max_rect());
        },
        app,
    );
    h.run_steps(5);
    for id in ["ai.prompt", "ai.send", "ai.clear"] {
        let [x, y, width, height] = h.state().auto.find(id).unwrap().rect;
        assert!(x >= 0.0 && y >= 0.0 && x + width <= 220.0 && y + height <= 490.0, "clipped {id}: {x},{y},{width},{height}");
    }
}

#[test]
fn activity_names_use_the_editors_command_labels() {
    let mut app = FilmcraftApp::new(Session::default());
    let ctx = egui::Context::default();
    let host = Adapter { app: &mut app, ctx: &ctx };
    for command in filmcraft_engine::commands::command_specs().iter().take(8) {
        let call = ToolCall::new("filmcraft", json!({"method":"engine.execute","params_json":json!({"command":command.id,"params":{}}).to_string()}));
        assert_eq!(host.tool_label(&call), command.label);
    }
}

#[test]
fn walkthrough_follows_real_menu_clicks_without_editing_the_project() {
    use egui_kittest::{Harness, kittest::Queryable};
    let mut app = FilmcraftApp::new(Session::default());
    app.show_panel(crate::dock::PanelKind::AiChat);
    app.session.prefs.ai.attach_frames = false;
    let before = app.session.project.clone();
    let mut h = Harness::builder().with_size(egui::vec2(1500.0, 1000.0)).build_eframe(move |_| app);
    h.run_steps(4);
    let ctx = h.ctx.clone();
    *h.state_mut().assistant.panel.prompt_mut() = "Guide me through the File and Edit menus".into();
    let mut turn = 0;
    begin_with(h.state_mut(),&ctx,move |_|{
        let current=turn;turn+=1;
        if current<2 {
            let name=if current==0 {"File"} else {"Edit"};
            Ok(json!({"status":"completed","output":[{"type":"function_call","call_id":format!("step{current}"),"name":"guide_ui","arguments":json!({"id":format!("menu.{name}"),"label":format!("Open the {name} menu"),"instruction":format!("Click **{name}** in the menu bar."),"completion":"activate","watch_id":"","state_pointer":"","comparison":"equal","expected_json":""}).to_string()}]}))
        }else{Ok(json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Both menus are ready."}]}]}))}
    }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while route(h.state_mut(), &ctx, "ai.status").unwrap().unwrap()["walkthrough"]["step"] != 1 && Instant::now() < deadline {
        h.run_steps(1);
        std::thread::sleep(Duration::from_millis(2));
    }
    h.run_steps(4);
    assert!(Arc::ptr_eq(&before, &h.state().session.project));
    h.query_by_role_and_label(egui::accesskit::Role::Button, "File").unwrap().click();
    h.run_steps(3);
    while route(h.state_mut(), &ctx, "ai.status").unwrap().unwrap()["walkthrough"]["step"] != 2 && Instant::now() < deadline {
        h.run_steps(1);
        std::thread::sleep(Duration::from_millis(2));
    }
    h.run_steps(3);
    let status = route(h.state_mut(), &ctx, "ai.status").unwrap().unwrap();
    assert_eq!(status["highlights"][0]["id"], "menu.Edit");
    assert_eq!(status["highlights"][0]["number"], 2);
    if let Some(dir) = std::env::var_os("CRAFT_AI_EDITOR_FIXTURES") {
        std::fs::create_dir_all(&dir).unwrap();
        h.render().unwrap().save(std::path::Path::new(&dir).join("filmcraft-walkthrough.png")).unwrap();
    }
    h.query_by_role_and_label(egui::accesskit::Role::Button, "Edit").unwrap().click();
    h.run_steps(3);
    while h.state().assistant.panel.busy() && Instant::now() < deadline {
        h.run_steps(1);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!h.state().assistant.panel.busy());
    assert!(Arc::ptr_eq(&before, &h.state().session.project));
    assert_eq!(route(h.state_mut(), &ctx, "ai.status").unwrap().unwrap()["highlights"], json!([]));
}
