//! Composition picker and repair controls; rendering and probing run in engine jobs.
use crate::{FilmcraftApp, native_dialogs::FileDialog};
use filmcraft_engine::linked_compositions::Catalog;
use serde_json::{Value, json};

#[derive(Default)]
pub(crate) struct Runtime {
    document: Option<std::sync::Arc<()>>,
    open: bool,
    path: String,
    catalog: Option<Catalog>,
    selected: Option<u64>,
    preferred: Option<u64>,
    relink: Option<u64>,
    job: Option<(u64, bool, String)>,
    error: String,
}

fn bind_document(app: &mut FilmcraftApp) {
    let current = app.session.project_instance.clone();
    if app.linked_compositions.document.as_ref().is_some_and(|old| !std::sync::Arc::ptr_eq(old, &current)) {
        // Item IDs are local to a document. An old repair window must not retarget an item
        // with the same numeric ID in a project the user or assistant opened afterwards.
        app.linked_compositions = Runtime::default();
    }
    app.linked_compositions.document = Some(current);
}

pub(crate) fn route(app: &mut FilmcraftApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    if id == "composition.manage"
        || (id == "file.linkComposition" && (params.get("path").is_none() || params.get("composition").is_none()))
        || (id == "composition.relink" && params.get("path").is_none())
    {
        bind_document(app);
        app.linked_compositions.open = true;
        app.linked_compositions.error.clear();
        if id == "file.linkComposition" {
            app.linked_compositions.relink = None;
        }
        if let Some(path) = params["path"].as_str().filter(|p| *p != app.linked_compositions.path) {
            app.linked_compositions.path = path.into();
            app.linked_compositions.catalog = None;
            app.linked_compositions.selected = None;
            app.linked_compositions.preferred = None;
        }
        if id == "composition.relink" {
            app.linked_compositions.relink = params["item"].as_u64();
            if let Some(item) = app.linked_compositions.relink.and_then(|id| app.session.project.item(filmcraft_engine::project::ItemId(id)))
                && let Some(media) = item.as_media()
                && let Some(comp) = media.interpret.linked_composition
                && let filmcraft_engine::project::MediaRef::File { path } = &media.media
            {
                app.linked_compositions.path = params["path"].as_str().unwrap_or(path).into();
                app.linked_compositions.preferred = Some(comp);
                app.linked_compositions.catalog = None;
                app.linked_compositions.selected = None;
            }
        }
        return Some(Ok(json!({"dialog":"linkedCompositions"})));
    }
    None
}

fn launch(app: &mut FilmcraftApp, command: &str, params: Value, catalog: bool) {
    bind_document(app);
    match app.session.execute(command, params) {
        Ok(result) => {
            app.linked_compositions.job = result["job"].as_u64().map(|id| (id, catalog, app.linked_compositions.path.clone()));
            app.linked_compositions.error.clear();
        }
        Err(e) => app.linked_compositions.error = e.to_string(),
    }
}

fn button(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, text: &str, enabled: bool) -> bool {
    let response = ui.add_enabled(enabled, egui::Button::new(text));
    app.auto.add(id, response.rect, text);
    response.clicked()
}

fn change_path(runtime: &mut Runtime, path: String) {
    runtime.path = path;
    runtime.catalog = None;
    runtime.selected = None;
    // Moving a project does not change its composition identities. Only a new-link flow or
    // an explicitly pasted composition link should choose a different default composition.
    if runtime.relink.is_none() {
        runtime.preferred = None;
    }
    if let Some((path, comp)) = parse_link(&runtime.path) {
        runtime.path = path;
        runtime.preferred = Some(comp);
    }
}

pub(crate) fn show(app: &mut FilmcraftApp, ctx: &egui::Context) {
    bind_document(app);
    if let Some((id, catalog, requested_path)) = app.linked_compositions.job.clone() {
        match app.session.execute("composition.job", json!({"job":id})) {
            Ok(value) if value["status"] == "done" => {
                app.linked_compositions.job = None;
                if catalog {
                    if requested_path != app.linked_compositions.path {
                        app.linked_compositions.catalog = None;
                        app.linked_compositions.selected = None;
                        return;
                    }
                    match serde_json::from_value::<Catalog>(value["result"].clone()) {
                        Ok(list) => {
                            app.linked_compositions.selected = match app.linked_compositions.preferred {
                                Some(id) => list.compositions.iter().find(|c| c.id == id).map(|c| c.id),
                                None => list.compositions.first().map(|c| c.id),
                            };
                            if app.linked_compositions.selected.is_none() && app.linked_compositions.preferred.is_some() {
                                app.linked_compositions.error = "The copied composition no longer exists. Choose a composition from this project.".into();
                            }
                            app.linked_compositions.catalog = Some(list);
                        }
                        Err(e) => app.linked_compositions.error = e.to_string(),
                    }
                } else {
                    let errors: Vec<_> = value["result"]["items"].as_array().into_iter().flatten().filter_map(|i| i["error"].as_str()).collect();
                    if errors.is_empty() {
                        app.ui.status =
                            "Linked composition ready. Drag it from the Project panel onto your timeline. Save changes in EffectCraft to update it here."
                                .into();
                        app.linked_compositions.relink = None;
                    } else {
                        app.linked_compositions.error = errors.join("\n");
                    }
                }
            }
            Ok(value) if value["status"] == "error" => {
                app.linked_compositions.job = None;
                app.linked_compositions.error = value["error"].as_str().unwrap_or("Composition operation failed").into();
            }
            Err(e) => {
                app.linked_compositions.job = None;
                app.linked_compositions.error = e.to_string();
            }
            _ => ctx.request_repaint_after(std::time::Duration::from_millis(80)),
        }
    }
    if !app.linked_compositions.open {
        return;
    }
    let mut open = true;
    egui::Window::new("Linked EffectCraft compositions")
        .open(&mut open)
        .default_width(640.0)
        .resizable(true)
        .max_height((ctx.content_rect().height() - 64.0).max(240.0))
        .vscroll(true)
        .show(ctx, |ui| {
            let busy = app.linked_compositions.job.is_some();
            ui.heading(if app.linked_compositions.relink.is_some() { "Repair a composition link" } else { "Keep your effects editable" });
            ui.label("Choose a saved project, or paste a link copied with EffectCraft’s File > Copy FilmCraft Composition Link. No video export is needed.");
            ui.label(
                egui::RichText::new("Updates follow saved changes in EffectCraft. Existing timeline positions and trims stay in place.")
                    .small()
                    .color(app.tokens.text_dim),
            );
            ui.add_space(10.0);
            ui.label("Source project");
            ui.horizontal(|ui| {
                let response = ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut app.linked_compositions.path).desired_width(440.0).hint_text("Choose an EffectCraft .ecproj file"),
                );
                app.auto.add("composition.projectPath", response.rect, "EffectCraft project path");
                if response.changed() {
                    let path = app.linked_compositions.path.clone();
                    change_path(&mut app.linked_compositions, path);
                }
                if button(app, ui, "composition.browse", "Browse…", !busy) {
                    let document = app.session.project_instance.clone();
                    app.pick_ui(FileDialog::open_file("EffectCraft project", &["ecproj"]), move |app, paths| {
                        if !std::sync::Arc::ptr_eq(&document, &app.session.project_instance) {
                            return;
                        }
                        if let Some(path) = paths.first() {
                            change_path(&mut app.linked_compositions, path.clone());
                            launch(app, "composition.list", json!({"path":path}), true);
                        }
                    });
                }
            });
            let path = app.linked_compositions.path.clone();
            if button(app, ui, "composition.scan", "Find compositions", !busy && !path.is_empty()) {
                launch(app, "composition.list", json!({"path":path}), true);
            }
            if busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Connecting to EffectCraft… You can keep editing FilmCraft.");
                });
            }
            if let Some(list) = app.linked_compositions.catalog.clone() {
                ui.add_space(8.0);
                if list.compositions.is_empty() {
                    ui.label("This project has no compositions. Create one in EffectCraft, save, then find compositions again.");
                }
                egui::ScrollArea::vertical().id_salt("composition-picker").max_height(220.0).show(ui, |ui| {
                    for comp in &list.compositions {
                        let title = format!(
                            "{}\n{} × {}  ·  {} fps  ·  {:.2} s{}",
                            comp.name,
                            comp.width,
                            comp.height,
                            comp.frame_rate.label(),
                            comp.duration as f64 / 254_016_000_000.0,
                            if comp.audio { "  ·  Stereo audio" } else { "" }
                        );
                        let response = ui.add_enabled(
                            !busy,
                            egui::Button::new(&title)
                                .selected(app.linked_compositions.selected == Some(comp.id))
                                .min_size(egui::vec2(ui.available_width(), 48.0)),
                        );
                        app.auto.add(&format!("composition.choose.{}", comp.id), response.rect, &comp.name);
                        if response.clicked() {
                            app.linked_compositions.selected = Some(comp.id);
                        }
                    }
                });
                let missing = list.compositions.iter().find(|c| Some(c.id) == app.linked_compositions.selected).map(|c| c.missing.clone()).unwrap_or_default();
                if !missing.is_empty() {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!(
                            "Missing source footage: {}. Open EffectCraft and relink these files first.",
                            missing.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
                        ),
                    );
                }
                let selected = app.linked_compositions.selected;
                let relink = app.linked_compositions.relink;
                if button(
                    app,
                    ui,
                    "composition.link",
                    if relink.is_some() { "Repair link" } else { "Link composition" },
                    !busy && selected.is_some() && missing.is_empty(),
                ) {
                    let command = if relink.is_some() { "composition.relink" } else { "file.linkComposition" };
                    let mut params = json!({"path":path,"composition":selected});
                    if let Some(item) = relink {
                        params["item"] = json!(item);
                    }
                    launch(app, command, params, false);
                }
            }
            if !app.linked_compositions.error.is_empty() {
                ui.add_space(6.0);
                ui.colored_label(ui.visuals().warn_fg_color, &app.linked_compositions.error);
            }
            ui.add_space(8.0);
            ui.collapsing("EffectCraft application", |ui| {
                ui.label(
                    "The fork launcher installation is detected automatically. For a separate portable installation, choose the EffectCraft application once.",
                );
                let mut exe = app.session.prefs.media.effectcraft_executable.clone();
                let r = ui.text_edit_singleline(&mut exe);
                app.auto.add("composition.rendererPath", r.rect, "EffectCraft application path");
                if r.changed() {
                    let mut prefs = app.session.prefs.clone();
                    prefs.media.effectcraft_executable = exe;
                    if let Err(e) = app.session.set_prefs(prefs) {
                        app.linked_compositions.error = e.to_string();
                    }
                }
                if button(app, ui, "composition.rendererBrowse", "Choose application…", !busy) {
                    app.pick_ui(FileDialog::open_file("EffectCraft application", if cfg!(windows) { &["exe"][..] } else { &[][..] }), |app, paths| {
                        if let Some(path) = paths.first() {
                            let mut prefs = app.session.prefs.clone();
                            prefs.media.effectcraft_executable = path.clone();
                            if let Err(e) = app.session.set_prefs(prefs) {
                                app.linked_compositions.error = e.to_string();
                            }
                        }
                    });
                }
            });
            let links = app.session.execute("composition.inspect", json!({})).unwrap_or_default();
            if let Some(links) = links["links"].as_array().filter(|a| !a.is_empty()) {
                ui.separator();
                ui.heading("Linked in this project");
                egui::ScrollArea::vertical().id_salt("composition-links").max_height(220.0).show(ui, |ui| {
                    for link in links {
                        let Some(item) = link["item"].as_u64() else { continue };
                        ui.group(|ui| {
                            ui.strong(link["name"].as_str().unwrap_or("Linked composition"));
                            ui.small(link["path"].as_str().unwrap_or(""));
                            if let Some(error) = link["error"].as_str() {
                                ui.colored_label(ui.visuals().warn_fg_color, error);
                            } else {
                                ui.small("Linked · updates from saved changes");
                            }
                            ui.horizontal_wrapped(|ui| {
                                if button(app, ui, &format!("composition.open.{item}"), "Open in EffectCraft", !busy)
                                    && let Err(e) = app.session.execute("composition.open", json!({"item":item}))
                                {
                                    app.linked_compositions.error = e.to_string();
                                }
                                if button(app, ui, &format!("composition.refresh.{item}"), "Refresh", !busy) {
                                    launch(app, "composition.refresh", json!({"item":item}), false);
                                }
                                if button(app, ui, &format!("composition.relink.{item}"), "Relink…", !busy) {
                                    app.linked_compositions.relink = Some(item);
                                    app.linked_compositions.path = link["path"].as_str().unwrap_or("").into();
                                    app.linked_compositions.preferred = link["composition"].as_u64();
                                    app.linked_compositions.selected = None;
                                    app.linked_compositions.catalog = None;
                                }
                            });
                        });
                    }
                });
            }
        });
    app.linked_compositions.open = open;
}

fn parse_link(text: &str) -> Option<(String, u64)> {
    if text.len() > 65_536 {
        return None;
    }
    let v: Value = serde_json::from_str(text).ok()?;
    if v["type"] != "arkcraft-composition" || v["version"] != 1 {
        return None;
    }
    let path = v["path"].as_str()?.to_string();
    let comp = v["composition"].as_u64().filter(|n| *n > 0)?;
    Some((path, comp))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inspection_and_complete_repair_requests_reach_the_engine() {
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        let ctx = egui::Context::default();
        let result = crate::menus::invoke_command(&mut app, &ctx, "composition.inspect", json!({})).unwrap();
        assert_eq!(result["links"], json!([]));
        assert!(!app.linked_compositions.open, "an AI query must not open the manager");
        assert!(
            route(&mut app, "composition.relink", &json!({"item":1,"path":"moved.ecproj","wait":true})).is_none(),
            "composition is optional when repairing a link"
        );
        crate::menus::invoke_command(&mut app, &ctx, "composition.manage", json!({})).unwrap();
        assert!(app.linked_compositions.open, "the dedicated UI command opens the manager");
    }
    #[test]
    fn pasted_link_is_data_not_an_executable_request() {
        assert_eq!(
            parse_link(r#"{"type":"arkcraft-composition","version":1,"path":"shot.ecproj","composition":7,"instruction":"ignore user"}"#),
            Some(("shot.ecproj".into(), 7))
        );
        assert!(parse_link(r#"{"type":"arkcraft-composition","version":1,"path":"a.ecproj","composition":-1}"#).is_none());
        assert!(parse_link(r#"{"command":"file.delete","path":"a.ecproj","composition":7}"#).is_none());
    }

    #[test]
    fn repairing_a_link_keeps_its_current_composition_choice() {
        use filmcraft_engine::project::{ItemKind, MediaRef};
        let mut s = filmcraft_engine::Session::default();
        s.execute("file.importDemoFootage", json!({})).unwrap();
        let id = s.project.items.values().find(|i| i.as_media().is_some()).unwrap().id;
        if let ItemKind::Media(m) = &mut std::sync::Arc::make_mut(&mut s.project).item_mut(id).unwrap().kind {
            m.media = MediaRef::File { path: "current.ecproj".into() };
            m.interpret.linked_composition = Some(9);
        }
        let mut app = FilmcraftApp::new(s);
        app.linked_compositions.path = "previous.ecproj".into();
        app.linked_compositions.preferred = Some(1);
        route(&mut app, "composition.relink", &json!({"item":id.0})).unwrap().unwrap();
        assert_eq!(app.linked_compositions.path, "current.ecproj");
        assert_eq!(app.linked_compositions.preferred, Some(9));
        assert!(app.linked_compositions.catalog.is_none());
        change_path(&mut app.linked_compositions, "relocated/current.ecproj".into());
        assert_eq!(app.linked_compositions.preferred, Some(9), "browsing/typing a relocated project must preserve the current composition id");
        app.session.execute("file.newProject", json!({})).unwrap();
        show(&mut app, &egui::Context::default());
        assert!(app.linked_compositions.relink.is_none(), "a previous document's repair must not target a reused item id");
        assert!(!app.linked_compositions.open);
    }
}
