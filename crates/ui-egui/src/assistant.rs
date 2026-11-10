//! In-app OpenAI chat. Workers request tools; only the UI thread operates the editor.
//! Credentials, images and history stay in memory and never enter UiState or a project file.
mod api;
mod credentials;
mod tools;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::{FilmcraftApp, control};

struct ToolJob {
    tool: tools::Tool,
    reply: mpsc::Sender<Value>,
    control_reply: mpsc::Receiver<Value>,
    label: String,
    deadline: Instant,
}
enum Event {
    Tool(Box<ToolJob>),
    Done(Result<api::Completed, String>),
}
struct Pending {
    events: mpsc::Receiver<Event>,
    cancel: Arc<AtomicBool>,
    instance: Arc<()>,
    tool: Option<Box<ToolJob>>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

pub(crate) struct Runtime {
    key: String,
    model: String,
    prompt: String,
    attach: bool,
    message: String,
    transcript: Vec<(String, String)>,
    history: Vec<Value>,
    history_model: String,
    history_instance: Option<Arc<()>>,
    pending: Option<Pending>,
    preferences: filmcraft_engine::settings::AiPrefs,
    vault: credentials::Vault,
    remove_saved_key: bool,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            key: String::new(),
            model: api::DEFAULT_MODEL.into(),
            prompt: String::new(),
            attach: true,
            message: String::new(),
            transcript: Vec::new(),
            history: Vec::new(),
            history_model: String::new(),
            history_instance: None,
            pending: None,
            preferences: Default::default(),
            vault: Default::default(),
            remove_saved_key: false,
        }
    }
}

fn record(rt: &mut Runtime, role: &str, text: String) {
    if rt.transcript.len() >= 128 {
        rt.transcript.remove(0);
    }
    rt.transcript.push((role.into(), text.chars().take(32_768).collect()));
}

pub fn route(app: &mut FilmcraftApp, ctx: &egui::Context, id: &str) -> Option<Result<Value, String>> {
    let result = match id {
        "ai.chat" => {
            app.show_panel(crate::dock::PanelKind::AiChat);
            Ok(json!({"panel":"AiChat"}))
        }
        "ai.settings" => crate::panels::settings::open(app, "ai"),
        "ai.saveKey" => {
            app.assistant.vault.initialize(app.session.prefs_path.as_deref(), app.session.prefs.ai.remember_key, ctx);
            app.assistant.vault.start(credentials::Action::Save(app.assistant.key.clone()), ctx).map(|()| Value::Null)
        }
        "ai.loadKey" => app.assistant.vault.start(credentials::Action::Load, ctx).map(|()| Value::Null),
        "ai.forgetKey" if app.assistant.vault.busy() => Err("Wait for the OS credential operation to finish".into()),
        "ai.forgetKey" => {
            forget_key(&mut app.assistant, ctx);
            Ok(Value::Null)
        }
        "ai.send" => begin(app, ctx),
        "ai.stop" => {
            stop(&mut app.assistant);
            Ok(Value::Null)
        }
        "ai.clear" if app.assistant.pending.is_none() => {
            app.assistant.history.clear();
            app.assistant.transcript.clear();
            app.assistant.message.clear();
            Ok(Value::Null)
        }
        "ai.clear" => Err("Stop the active AI request before clearing the chat".into()),
        "ai.status" => Ok(json!({"open":app.ui.dock.contains(&crate::dock::PanelKind::AiChat),"busy":app.assistant.pending.is_some(),
                "hasKey":!app.assistant.key.is_empty(),"savedKey":app.assistant.vault.saved(),"credentialBusy":app.assistant.vault.busy(),
                "credentialMessage":app.assistant.vault.message,"message":app.assistant.message,"transcript":app.assistant.transcript})),
        _ => return None,
    };
    Some(result)
}

fn stop(rt: &mut Runtime) {
    if let Some(pending) = &rt.pending {
        pending.cancel.store(true, Ordering::Release);
        rt.message = "Stopping. No further tools will run; an in-flight API request may take up to 120 seconds to finish.".into();
    }
}

fn forget_key(rt: &mut Runtime, ctx: &egui::Context) {
    rt.key.clear();
    // Clearing the field must also remove the text widget's undo history, which contains old input.
    egui::text_edit::TextEditState::default().store(ctx, egui::Id::unique("ai.key.field"));
    egui::text_edit::TextEditState::default().store(ctx, egui::Id::unique("ai.key.preferences"));
    if rt.vault.configured()
        && let Err(error) = rt.vault.start(credentials::Action::Delete, ctx)
    {
        rt.vault.message = error;
    }
}

fn sync_preferences(app: &mut FilmcraftApp) {
    let prefs = &app.session.prefs.ai;
    if app.assistant.preferences != *prefs {
        if app.assistant.preferences.remember_key && !prefs.remember_key && app.assistant.vault.configured() {
            app.assistant.remove_saved_key = true;
        }
        app.assistant.model = prefs.model.clone();
        app.assistant.attach = prefs.attach_frames;
        app.assistant.preferences = prefs.clone();
    }
}

fn begin(app: &mut FilmcraftApp, ctx: &egui::Context) -> Result<Value, String> {
    sync_preferences(app);
    if app.assistant.vault.busy() {
        return Err("Wait for the OS credential operation to finish".into());
    }
    let request = api::http(app.assistant.key.trim().to_string())?;
    let result = begin_with(app, ctx, request)?;
    if app.session.prefs.ai.remember_key
        && app.session.prefs_path.is_some()
        && !app.assistant.vault.matches(&app.assistant.key)
        && let Err(error) = app.assistant.vault.start(credentials::Action::Save(app.assistant.key.trim().to_string()), ctx)
    {
        app.assistant.vault.message = error;
    }
    Ok(result)
}

fn begin_with(app: &mut FilmcraftApp, ctx: &egui::Context, request: impl FnMut(&Value) -> Result<Value, String> + Send + 'static) -> Result<Value, String> {
    let rt = &mut app.assistant;
    if rt.pending.is_some() {
        return Err("An AI request is already running".into());
    }
    let prompt = rt.prompt.trim().to_string();
    if prompt.is_empty() || prompt.len() > 32_768 {
        return Err("Enter a message of 1–32768 bytes".into());
    }
    let model = rt.model.trim().to_string();
    if model.is_empty() || model.len() > 128 || !model.bytes().all(|c| c.is_ascii_alphanumeric() || b"-._/".contains(&c)) {
        return Err("Enter a valid OpenAI model ID".into());
    }
    if rt.history_model != model || rt.history_instance.as_ref().is_some_and(|old| !Arc::ptr_eq(old, &app.session.project_instance)) {
        rt.history.clear();
        if !rt.transcript.is_empty() {
            record(rt, "Notice", "New model or project: starting a new conversation context.".into());
        }
    }
    let history = rt.history.clone();
    let attach = rt.attach;
    let (sender, events) = mpsc::sync_channel(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancelled = cancel.clone();
    let context = ctx.clone();
    let worker_prompt = prompt.clone();
    let worker_model = model.clone();
    std::thread::Builder::new()
        .name("filmcraft-ai".into())
        .spawn(move || {
            let work = || {
                let call = |name: &str, args: &Value| -> Result<Value, String> {
                    api::check_cancel(&cancelled)?;
                    let (tool, control_reply, label) = tools::parse(name, args)?;
                    let (reply, receive) = mpsc::channel();
                    sender
                        .send(Event::Tool(Box::new(ToolJob { tool, reply, control_reply, label, deadline: Instant::now() + Duration::from_secs(30) })))
                        .map_err(|_| "The editor closed".to_string())?;
                    context.request_repaint();
                    context.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
                    let deadline = Instant::now() + Duration::from_secs(35);
                    loop {
                        api::check_cancel(&cancelled)?;
                        match receive.recv_timeout(Duration::from_millis(100)) {
                            Ok(result) => return Ok(result),
                            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("The editor tool stopped".into()),
                            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                                cancelled.store(true, Ordering::Release);
                                return Err("The editor did not respond in time; no further tools will run".into());
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => context.request_repaint(),
                        }
                    }
                };
                let mut content = vec![json!({"type":"input_text","text":worker_prompt})];
                if attach {
                    for monitor in ["Program", "Source"] {
                        let frame = call("video_frame", &json!({"monitor":monitor}))?;
                        if let Some(parts) = frame.as_array() {
                            content.extend(parts.iter().cloned());
                        }
                    }
                }
                let mut history = history;
                history.push(json!({"role":"user","content":content}));
                api::run(&worker_model, history, &cancelled, request, call)
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err("The AI worker failed. Your project is still open.".into()));
            let _ = sender.send(Event::Done(result));
            context.request_repaint();
        })
        .map_err(|_| "Cannot start the AI worker".to_string())?;
    rt.pending = Some(Pending { events, cancel, instance: app.session.project_instance.clone(), tool: None });
    rt.history_model = model;
    rt.history_instance = Some(app.session.project_instance.clone());
    rt.prompt.clear();
    rt.message = "Working…".into();
    record(rt, "You", prompt);
    ctx.request_repaint();
    Ok(json!({"pending":true}))
}

pub(crate) fn poll(app: &mut FilmcraftApp, ctx: &egui::Context) {
    sync_preferences(app);
    app.assistant.vault.poll(&mut app.assistant.key);
    if app.assistant.remove_saved_key && !app.assistant.vault.busy() {
        app.assistant.remove_saved_key = false;
        if let Err(error) = app.assistant.vault.start(credentials::Action::Delete, ctx) {
            app.assistant.vault.message = error;
        }
    }
    if app.assistant.vault.busy() {
        ctx.request_repaint_after(Duration::from_millis(100));
    }
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll_inner(app, ctx))).is_err() {
        app.assistant.pending = None;
        app.assistant.history.clear();
        app.assistant.message = "The AI tool failed. Your editor session is still open; review the last action before continuing.".into();
        record(&mut app.assistant, "Notice", "AI tool stopped after an internal error.".into());
    }
}

fn poll_inner(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let Some(mut pending) = app.assistant.pending.take() else { return };
    if !Arc::ptr_eq(&pending.instance, &app.session.project_instance) {
        pending.cancel.store(true, Ordering::Release);
    }
    match pending.events.try_recv() {
        Ok(Event::Done(result)) => {
            let result = api::check_cancel(&pending.cancel).and(result);
            match result {
                Ok(done) => {
                    app.assistant.history = done.history;
                    record(&mut app.assistant, "Assistant", done.text);
                    app.assistant.message = "Ready".into();
                }
                Err(error) => {
                    // Never resend an incomplete function-call history, or replay an old plan after partial edits.
                    app.assistant.history.clear();
                    record(&mut app.assistant, "Notice", error.clone());
                    app.assistant.message = error;
                }
            }
            return;
        }
        Ok(Event::Tool(job)) => pending.tool = Some(job),
        Err(mpsc::TryRecvError::Disconnected) => {
            app.assistant.message = "The AI worker stopped. Your project is still open.".into();
            return;
        }
        Err(mpsc::TryRecvError::Empty) => {}
    }
    if let Some(mut job) = pending.tool.take() {
        let result = if pending.cancel.load(Ordering::Acquire) {
            Some(Value::String(json!({"ok":false,"error":"AI stopped or a different project was opened"}).to_string()))
        } else if Instant::now() > job.deadline {
            pending.cancel.store(true, Ordering::Release);
            Some(Value::String(json!({"ok":false,"error":"Editor tool timed out"}).to_string()))
        } else {
            match &mut job.tool {
                tools::Tool::Frame { monitor, snapshot } => match tools::frame(app, monitor, snapshot) {
                    Ok(result) => result,
                    Err(error) => Some(Value::String(json!({"ok":false,"error":error}).to_string())),
                },
                tools::Tool::Editor(req) => {
                    // Input tools reply after a later frame consumes their events. Do not dispatch twice.
                    match job.control_reply.try_recv() {
                        Ok(result) => Some(Value::String(result.to_string())),
                        Err(mpsc::TryRecvError::Disconnected) => Some(Value::String("Editor input channel closed".into())),
                        Err(mpsc::TryRecvError::Empty) if req.method.is_empty() => None,
                        Err(mpsc::TryRecvError::Empty) => match control::handle(app, ctx, req) {
                            control::Outcome::Done(value) => Some(Value::String(value.to_string())),
                            control::Outcome::Retry(_) => None,
                            control::Outcome::AfterInput => {
                                app.input_waiters.push(req.reply.clone());
                                req.method.clear();
                                None
                            }
                            control::Outcome::Screenshot { .. } => Some(Value::String("Use video_frame to inspect video".into())),
                        },
                    }
                }
            }
        };
        if let Some(result) = result {
            let detail =
                if result.is_array() { "frame attached".into() } else { result.as_str().unwrap_or("completed").chars().take(1024).collect::<String>() };
            record(&mut app.assistant, "Tool", format!("{}\n{detail}", job.label));
            let _ = job.reply.send(result);
        } else {
            pending.tool = Some(job);
        }
    }
    app.assistant.pending = Some(pending);
    ctx.request_repaint_after(Duration::from_millis(100));
}

pub fn credentials_ui(app: &mut FilmcraftApp, ui: &mut egui::Ui, preferences: bool) {
    let ctx = ui.ctx().clone();
    app.assistant.vault.initialize(app.session.prefs_path.as_deref(), app.session.prefs.ai.remember_key, &ctx);
    let busy = app.assistant.pending.is_some() || app.assistant.vault.busy();
    let prefix = if preferences { "settings.ai" } else { "ai" };
    let field = if preferences { "ai.key.preferences" } else { "ai.key.field" };
    ui.label("OpenAI API key");
    let response = ui.add_enabled(
        !busy,
        egui::TextEdit::singleline(&mut app.assistant.key).id(egui::Id::unique(field)).password(true).char_limit(512).desired_width(f32::INFINITY),
    );
    app.auto.add_response(&format!("{prefix}.key"), &response, "OpenAI API key");
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        let response =
            ui.add_enabled(!busy && !app.assistant.key.trim().is_empty() && app.session.prefs.ai.remember_key, egui::Button::new("Save key securely"));
        app.auto.add_response(&format!("{prefix}.saveKey"), &response, "Save key securely");
        if response.clicked() {
            action = Some("ai.saveKey");
        }
        let response = ui.add_enabled(!busy && app.session.prefs.ai.remember_key, egui::Button::new("Load saved key"));
        app.auto.add_response(&format!("{prefix}.loadKey"), &response, "Load saved API key");
        if response.clicked() {
            action = Some("ai.loadKey");
        }
        let response = ui.add_enabled(!busy, egui::Button::new("Forget key"));
        app.auto.add_response(&format!("{prefix}.forgetKey"), &response, "Forget API key");
        if response.clicked() {
            action = Some("ai.forgetKey");
        }
        if app.assistant.vault.busy() {
            ui.spinner();
        }
    });
    if app.session.prefs.ai.remember_key {
        ui.small("Saved in the OS credential store, never in preferences.json. Sending also saves the entered key.");
    } else {
        ui.small("Session only. Enable secure key saving in Preferences → AI Chat to remember it.");
    }
    if !app.assistant.vault.message.is_empty() {
        ui.label(&app.assistant.vault.message);
    }
    if let Some(action) = action
        && let Some(Err(error)) = route(app, &ctx, action)
    {
        app.assistant.vault.message = error;
    }
}

/// A normal dock body: tabs, splitting, floating and closing use the shared panel system.
pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: egui::Rect) {
    sync_preferences(app);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(8.0, 6.0))));
    let ui = &mut child;
    let ctx = ui.ctx().clone();
    let busy = app.assistant.pending.is_some();
    let mut action = None;
    egui::Panel::bottom("ai.composer").frame(egui::Frame::NONE).show(ui, |ui| {
        let mut attach = app.session.prefs.ai.attach_frames;
        let response = ui.add_enabled(!busy, egui::Checkbox::new(&mut attach, "Attach Program and Source frames"));
        app.auto.add_response("ai.attach", &response, "Attach video frames");
        if response.changed()
            && let Err(error) = app.session.execute("prefs.set", json!({"key":"ai.attachFrames","value":attach}))
        {
            app.assistant.message = error.to_string();
        }
        let response = ui.add_enabled(
            !busy,
            egui::TextEdit::multiline(&mut app.assistant.prompt)
                .char_limit(32_768)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("Describe what you want to do…"),
        );
        app.auto.add_response("ai.prompt", &response, "AI message");
        ui.horizontal_wrapped(|ui| {
            let response = ui.add_enabled(!busy && !app.assistant.vault.busy() && !app.assistant.prompt.trim().is_empty(), egui::Button::new("Send"));
            app.auto.add_response("ai.send", &response, "Send AI message");
            if response.clicked() {
                action = Some("ai.send");
            }
            let response = ui.add_enabled(busy, egui::Button::new("Stop"));
            app.auto.add_response("ai.stop", &response, "Stop AI");
            if response.clicked() {
                action = Some("ai.stop");
            }
            let response = ui.add_enabled(!busy, egui::Button::new("Clear chat"));
            app.auto.add_response("ai.clear", &response, "Clear AI chat");
            if response.clicked() {
                action = Some("ai.clear");
            }
            if busy {
                ui.spinner();
            }
        });
        ui.label(&app.assistant.message);
    });
    egui::Panel::top("ai.toolbar").frame(egui::Frame::NONE).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Model: {}", app.session.prefs.ai.model));
            let response = ui.button("Preferences…");
            app.auto.add_response("ai.settings", &response, "AI Chat preferences");
            if response.clicked() {
                action = Some("ai.settings");
            }
        });
    });
    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
        let scroll =
            egui::ScrollArea::vertical().id_salt("ai.messages").min_scrolled_height(0.0).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                let connection =
                    egui::CollapsingHeader::new("Connection").id_salt("ai.connection").default_open(true).show(ui, |ui| credentials_ui(app, ui, false));
                app.auto.add_response("ai.connection", &connection.header_response, "OpenAI connection");
                ui.small("Messages, requested editor data and frames go to OpenAI. API usage is billed to your key. Use Undo to revert edits.");
                ui.separator();
                if app.assistant.transcript.is_empty() {
                    ui.label("Ask about a shot, request edits, apply effects, or export with FilmCraft’s tools.");
                }
                for (index, (role, text)) in app.assistant.transcript.iter().enumerate() {
                    ui.strong(role);
                    let response = ui.add(egui::Label::new(text).wrap().selectable(true));
                    app.auto.add_response(&format!("ai.message.{index}"), &response, role);
                    ui.add_space(7.0);
                }
            });
        app.auto.add("ai.messages", scroll.inner_rect, "AI conversation");
    });
    if let Some(action) = action
        && let Some(Err(error)) = route(app, &ctx, action)
    {
        app.assistant.message = error;
    }
}

#[cfg(test)]
mod tests;
