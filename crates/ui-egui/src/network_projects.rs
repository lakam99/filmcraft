//! Native network-project dialog. Connecting and saving run off the UI thread. A completion is
//! applied only to the document it started for; edits made during a save remain unsaved.
use std::sync::{Arc, mpsc};

use filmcraft_engine::{Services, Session};
use filmcraft_network::{Relay, RelayServices};
use serde_json::{Value, json};

use crate::FilmcraftApp;
use crate::widgets::CenteredDialog as _;

enum Completed {
    Connected(Arc<Relay>, Box<Session>),
    Saved,
}
struct Pending {
    receiver: mpsc::Receiver<Result<Completed, String>>,
    instance: Arc<()>,
    revision: u64,
}

#[derive(Default)]
pub(crate) struct Runtime {
    open: bool,
    address: String,
    token: String,
    message: String,
    relay: Option<Arc<Relay>>,
    fallback: Option<Arc<dyn Services>>,
    pending: Option<Pending>,
}

fn active(app: &FilmcraftApp) -> Option<Arc<Relay>> {
    app.network_project.relay.as_ref().filter(|r| app.session.path.as_deref().is_some_and(|p| std::path::Path::new(p) == r.project_path())).cloned()
}
fn begin(app: &mut FilmcraftApp, ctx: &egui::Context, work: impl FnOnce() -> Result<Completed, String> + Send + 'static) -> Result<Value, String> {
    if app.network_project.pending.is_some() {
        return Err("A network project operation is already running".into());
    }
    let (sender, receiver) = mpsc::channel();
    let context = ctx.clone();
    std::thread::Builder::new()
        .name("network-project".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err("The network operation failed; your current project is still open".into()));
            let _ = sender.send(result);
            context.request_repaint();
        })
        .map_err(|_| "Cannot start the network project worker".to_string())?;
    app.network_project.pending = Some(Pending { receiver, instance: app.session.project_instance.clone(), revision: app.session.revision });
    Ok(json!({"pending":true}))
}

pub fn route(app: &mut FilmcraftApp, ctx: &egui::Context, id: &str, params: &Value) -> Option<Result<Value, String>> {
    match id {
        "network.project" => {
            app.network_project.open = true;
            Some(Ok(json!({"dialog":"networkProject"})))
        }
        "network.status" => {
            Some(Ok(json!({"connected":active(app).is_some(),"busy":app.network_project.pending.is_some(),"message":app.network_project.message})))
        }
        "network.connect" => Some(connect(app, ctx)),
        "network.save" => Some(save(app, ctx)),
        "network.disconnect" => Some(disconnect(app)),
        "file.save" if active(app).is_some() && params.get("path").is_none() => Some(save(app, ctx)),
        _ => None,
    }
}

fn disconnect(app: &mut FilmcraftApp) -> Result<Value, String> {
    if app.session.is_dirty() || app.network_project.pending.is_some() {
        return Err("Save or close the project and wait for network operations before disconnecting".into());
    }
    app.stop();
    app.stop_source();
    app.session.execute("file.closeProject", json!({})).map_err(|e| e.to_string())?;
    if let Some(fallback) = app.network_project.fallback.take() {
        app.session.services = fallback;
    }
    app.network_project.relay = None;
    app.network_project.token.clear();
    app.network_project.message = "Network project disconnected".into();
    Ok(Value::Null)
}

fn save(app: &mut FilmcraftApp, ctx: &egui::Context) -> Result<Value, String> {
    let relay = active(app).ok_or("Open a network project first")?;
    let bytes = filmcraft_format::encode_with_view(&app.session.project, Some(&app.session.project_view()), false);
    app.network_project.message = "Saving to host…".into();
    begin(app, ctx, move || relay.save_project(&bytes).map(|()| Completed::Saved).map_err(|e| e.to_string()))
}

fn connect(app: &mut FilmcraftApp, ctx: &egui::Context) -> Result<Value, String> {
    if app.session.is_dirty() {
        return Err("Save or close the current project before connecting".into());
    }
    let address = app.network_project.address.clone();
    let token = std::mem::take(&mut app.network_project.token);
    let fallback = app.network_project.fallback.get_or_insert_with(|| app.session.services.clone()).clone();
    let prefs = app.session.prefs.clone();
    app.network_project.message = "Connecting and opening media…".into();
    begin(app, ctx, move || {
        let relay = Relay::connect(&address, token, &std::env::temp_dir().join("filmcraft-network-projects"), fallback).map_err(|e| e.to_string())?;
        let mut session = Session::new(Arc::new(RelayServices(relay.clone())));
        session.prefs = prefs;
        session.media.set_use_proxies(session.prefs.media.enable_proxies);
        session.execute("file.open", json!({"path":relay.project_path().to_string_lossy()})).map_err(|e| e.to_string())?;
        Ok(Completed::Connected(relay, Box::new(session)))
    })
}

fn poll(app: &mut FilmcraftApp) {
    let Some(pending) = &app.network_project.pending else { return };
    let result = match pending.receiver.try_recv() {
        Ok(result) => result,
        Err(mpsc::TryRecvError::Empty) => return,
        Err(mpsc::TryRecvError::Disconnected) => Err("The network worker stopped; your current project is still open".into()),
    };
    let Some(pending) = app.network_project.pending.take() else { return };
    if !Arc::ptr_eq(&pending.instance, &app.session.project_instance) {
        app.network_project.message = "The operation completed for a project that is no longer open".into();
        return;
    }
    match result {
        Ok(Completed::Connected(relay, mut session)) if app.session.revision == pending.revision => {
            app.stop();
            app.stop_source();
            session.persistence = app.session.persistence.take();
            session.prefs_path = app.session.prefs_path.take();
            session.shortcuts = app.session.shortcuts.clone();
            session.presets = std::mem::take(&mut app.session.presets);
            session.export_presets = std::mem::take(&mut app.session.export_presets);
            session.document_epoch = app.session.document_epoch.wrapping_add(1);
            app.session = *session;
            app.session.sync_persistence();
            app.network_project.relay = Some(relay);
            app.network_project.message = "Network project connected. Save writes to the host; autosaves stay on this computer.".into();
            app.ui.mode = crate::state::Mode::Edit;
        }
        Ok(Completed::Connected(..)) => {
            app.network_project.message = "The current project changed while connecting. It was kept open; connect again after saving.".into()
        }
        Ok(Completed::Saved) => {
            // Saving an earlier snapshot must not clear the dirty flag of edits made meanwhile.
            app.session.saved_revision = app.session.saved_revision.max(pending.revision);
            app.session.loaded_schema = filmcraft_format::SCHEMA_VERSION;
            app.session.sync_persistence();
            app.network_project.message = "Project saved on the host".into();
        }
        Err(error) => app.network_project.message = error,
    }
    app.ui.status = app.network_project.message.clone();
}

pub fn show(app: &mut FilmcraftApp, ctx: &egui::Context) {
    poll(app);
    if app.network_project.pending.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    if !app.network_project.open {
        return;
    }
    let mut open = true;
    let mut connecting = false;
    let mut saving = false;
    let mut disconnecting = false;
    let connected = active(app).is_some();
    let busy = app.network_project.pending.is_some();
    egui::Window::new("Network Project")
        .id(egui::Id::unique("networkProject"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(470.0)
        .centered(ctx)
        .show(ctx, |ui| {
            ui.label("Open a project from a FilmCraft RedWeb relay on another computer.");
            ui.label("Host address");
            let response =
                ui.add_enabled(!busy, egui::TextEdit::singleline(&mut app.network_project.address).hint_text("http://192.168.1.10:9877").desired_width(440.0));
            app.auto.add_response("network.address", &response, "Host address");
            ui.label("Access token");
            let response = ui.add_enabled(!busy, egui::TextEdit::singleline(&mut app.network_project.token).password(true).desired_width(440.0));
            app.auto.add_response("network.token", &response, "Access token");
            ui.horizontal(|ui| {
                let response = ui.add_enabled(!busy && !app.session.is_dirty(), egui::Button::new("Connect"));
                app.auto.add_response("network.connect", &response, "Connect");
                connecting = response.clicked();
                let response = ui.add_enabled(connected && !busy, egui::Button::new("Save to Host"));
                app.auto.add_response("network.save", &response, "Save to Host");
                saving = response.clicked();
                let response = ui.add_enabled(connected && !busy && !app.session.is_dirty(), egui::Button::new("Disconnect"));
                app.auto.add_response("network.disconnect", &response, "Disconnect");
                disconnecting = response.clicked();
                if busy {
                    ui.spinner();
                }
            });
            if app.session.is_dirty() && !connected {
                ui.label("Save or close your current project before connecting.");
            }
            if !app.network_project.message.is_empty() {
                ui.label(&app.network_project.message);
            }
            ui.separator();
            ui.label("Start the FilmCraft network relay on the storage computer. Media stays there; imported local files are uploaded when you save.");
            ui.label("Use Media Browser → Network → Network Project to browse the host’s files.");
        });
    app.network_project.open = open;
    if !open {
        app.network_project.token.clear();
    }
    let result = if connecting {
        connect(app, ctx)
    } else if saving {
        save(app, ctx)
    } else if disconnecting {
        disconnect(app)
    } else {
        return;
    };
    if let Err(error) = result {
        app.network_project.message = error;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_save(app: &mut FilmcraftApp) {
        let (sender, receiver) = mpsc::channel();
        app.network_project.pending = Some(Pending { receiver, instance: app.session.project_instance.clone(), revision: app.session.revision });
        sender.send(Ok(Completed::Saved)).unwrap();
    }

    #[test]
    fn save_completion_keeps_later_edits_dirty() {
        let mut app = FilmcraftApp::new(Session::default());
        app.session.execute("project.setNotes", json!({"text":"saved snapshot"})).unwrap();
        let saved = app.session.revision;
        pending_save(&mut app);
        app.session.execute("project.setNotes", json!({"text":"typed while saving"})).unwrap();
        poll(&mut app);
        assert_eq!(app.session.saved_revision, saved);
        assert!(app.session.is_dirty());
        assert_eq!(app.session.project.notes, "typed while saving");
    }

    #[test]
    fn old_document_completion_cannot_mark_a_new_project_saved() {
        let mut app = FilmcraftApp::new(Session::default());
        pending_save(&mut app);
        app.session.execute("file.newProject", json!({"name":"another project"})).unwrap();
        app.session.execute("project.setNotes", json!({"text":"new work"})).unwrap();
        let before = app.session.saved_revision;
        poll(&mut app);
        assert_eq!(app.session.saved_revision, before);
        assert!(app.session.is_dirty());
    }

    #[test]
    fn connecting_never_discards_an_unsaved_project() {
        let mut app = FilmcraftApp::new(Session::default());
        app.session.execute("project.setNotes", json!({"text":"unsaved edit"})).unwrap();
        assert!(connect(&mut app, &egui::Context::default()).unwrap_err().contains("Save or close"));
        assert_eq!(app.session.project.notes, "unsaved edit");
        assert!(app.network_project.pending.is_none());
    }
}
