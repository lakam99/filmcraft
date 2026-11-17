use crate::frames::{FrameKey, Target};
use crate::{ControlRequest, FilmcraftApp, control};
use serde_json::{Value, json};
use std::io::Cursor;
pub(super) enum Tool {
    Editor(ControlRequest),
    Frame { monitor: String, snapshot: Option<Snapshot> },
    Job(JobWait),
}

pub(super) struct JobWait {
    id: u64,
    envelope: Value,
    progress: std::sync::Arc<filmcraft_engine::export::Progress>,
    completed: bool,
}

impl Drop for JobWait {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        if !self.completed && !self.progress.finished.load(Ordering::Acquire) {
            self.progress.cancel.store(true, Ordering::Release);
        }
    }
}

impl JobWait {
    fn poll(&mut self, app: &mut FilmcraftApp) -> Result<Option<Value>, String> {
        let job = app.session.jobs.iter().find(|job| job.id == self.id).ok_or("Background job no longer exists")?;
        let result = job.result.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(result) = result else { return Ok(None) };
        self.completed = true;
        if let Err(error) = result {
            let mut envelope = self.envelope.clone();
            envelope["ok"] = false.into();
            envelope["error"] = error.into();
            if let Some(fields) = envelope.as_object_mut() {
                fields.remove("result");
            }
            return Ok(Some(Value::String(envelope.to_string())));
        }
        // One local query on completion enriches the reply with AAF warnings/media paths.
        // Waiting consumes no additional provider requests or model tool calls.
        let jobs = app.session.execute("jobs.list", json!({})).map_err(|e| e.to_string())?;
        let done = jobs.as_array().and_then(|jobs| jobs.iter().find(|job| job["id"] == self.id)).ok_or("Completed job result is unavailable")?;
        let mut envelope = self.envelope.clone();
        let output = envelope["result"].as_object_mut().ok_or("Invalid background job acknowledgement")?;
        for key in ["poll", "cancel", "background"] {
            output.remove(key);
        }
        if let Some(fields) = done["backend"]["output"].as_object() {
            output.extend(fields.clone());
        }
        output.insert("job".into(), self.id.into());
        output.insert("finished".into(), true.into());
        output.insert("result".into(), done["result"].clone());
        Ok(Some(Value::String(envelope.to_string())))
    }
}

pub(super) struct Snapshot {
    key: FrameKey,
    time: filmcraft_time::Tick,
    scale: f32,
}

pub(crate) struct Pending {
    tool: Tool,
    control_reply: std::sync::mpsc::Receiver<Value>,
}
pub(super) fn parse(name: &str, args: &Value) -> Result<(Tool, std::sync::mpsc::Receiver<Value>, String), String> {
    let (tool, rx) = match super::editor_tools().parse(name, args)? {
        craft_ai_chat_egui::core::editor::EditorCall::Command { method, params } => {
            let (req, rx) = ControlRequest::new(method, params);
            (Tool::Editor(req), rx)
        }
        craft_ai_chat_egui::core::editor::EditorCall::View(monitor) => {
            let (_, rx) = std::sync::mpsc::channel();
            (Tool::Frame { monitor, snapshot: None }, rx)
        }
    };
    Ok((tool, rx, name.into()))
}
pub(super) fn start(call: &craft_ai_chat_egui::ToolCall) -> Result<Pending, String> {
    let (tool, control_reply, _) = parse(&call.name, &call.arguments)?;
    Ok(Pending { tool, control_reply })
}
pub(super) fn poll(app: &mut FilmcraftApp, ctx: &egui::Context, pending: &mut Pending) -> Result<Option<Value>, String> {
    let response = match &mut pending.tool {
        Tool::Frame { monitor, snapshot } => return frame(app, monitor, snapshot),
        Tool::Job(wait) => return wait.poll(app),
        Tool::Editor(req) => match pending.control_reply.try_recv() {
            Ok(value) => Some(value),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return Err("Editor input channel closed".into()),
            Err(std::sync::mpsc::TryRecvError::Empty) if req.method.is_empty() => None,
            Err(std::sync::mpsc::TryRecvError::Empty) => match control::handle(app, ctx, req) {
                control::Outcome::Done(value) => Some(value),
                control::Outcome::Retry(_) => None,
                control::Outcome::AfterInput => {
                    app.input_waiters.push(req.reply.clone());
                    req.method.clear();
                    None
                }

                control::Outcome::Screenshot { .. } => return Err("Use active-view capture to inspect media".into()),
            },
        },
    };
    let Some(response) = response else { return Ok(None) };
    if response["ok"] == true
        && response["result"]["poll"] == "jobs.list"
        && let Some(id) = response["result"]["job"].as_u64()
        && let Some(job) = app.session.jobs.iter().find(|job| job.id == id)
    {
        pending.tool = Tool::Job(JobWait { id, envelope: response, progress: job.progress.clone(), completed: false });
        return Ok(None);
    }
    Ok(Some(Value::String(response.to_string())))
}

fn snapshot(app: &FilmcraftApp, monitor: &str) -> Result<Snapshot, String> {
    let (target, time, rate, width, height) = if monitor == "Program" {
        let item = app.session.state.active_sequence.ok_or("No sequence is open in the Program monitor")?;
        let sequence = app.session.active_sequence().ok_or("The active sequence is unavailable")?;
        (Target::Sequence(item), app.session.playhead(), sequence.settings.frame_rate, sequence.settings.width, sequence.settings.height)
    } else {
        let item = app.session.state.source_item.ok_or("No video is open in the Source monitor")?;
        let view = filmcraft_engine::clip_ops::source_view(&app.session, item).ok_or("The source item is unavailable")?;
        let (width, height) = match app.session.project.item(view.media).map(|i| &i.kind) {
            Some(filmcraft_project::ItemKind::Media(m)) => m.info.video.as_ref().map(|v| (v.width, v.height)).ok_or("The Source item has no video")?,
            Some(filmcraft_project::ItemKind::Sequence(s)) => (s.settings.width, s.settings.height),
            _ => return Err("The Source item has no video".into()),
        };
        (Target::Item(item), app.session.state.source_playhead, view.rate, width, height)
    };
    if width == 0 || height == 0 || width > 32_768 || height > 32_768 {
        return Err("The video has invalid dimensions".into());
    }
    // Bound both axes (portrait footage too); CPU rendering also works with GPU-only monitors.
    let scale = (1280.0 / width.max(height) as f32).min(1.0);
    let revision = match target {
        Target::Item(item) => app.item_revision(item),
        _ => app.session.revision,
    };
    Ok(Snapshot {
        key: FrameKey { target, frame: rate.frame_at(time), size: 0x8000_0000 | (scale * 1000.0) as u32, revision, draft: false },
        time: rate.tick_of(rate.frame_at(time)),
        scale,
    })
}

/// None means the pinned frame is still rendering. Playback never changes the requested time.
pub(super) fn frame(app: &FilmcraftApp, monitor: &str, pending: &mut Option<Snapshot>) -> Result<Option<Value>, String> {
    if pending.is_none() {
        *pending = Some(snapshot(app, monitor)?);
    }
    let Some(snapshot) = pending.as_ref() else {
        return Err("Cannot prepare the video frame".into());
    };
    let revision = match snapshot.key.target {
        Target::Item(item) => app.item_revision(item),
        _ => app.session.revision,
    };
    if snapshot.key.revision != revision {
        return Err("The video changed while its frame was rendering. Request another frame.".into());
    }
    let Some(rgba) = app.frames.get(&snapshot.key) else {
        app.frames.request(snapshot.key, snapshot.time, snapshot.scale, &app.session.project, 1);
        return Ok(None);
    };
    if rgba.w == 0 || rgba.h == 0 || rgba.w > 1280 || rgba.h > 1280 {
        return Err("The preview exceeds the AI image size limit".into());
    }
    if rgba.px.len() != rgba.w.saturating_mul(rgba.h).saturating_mul(4) {
        return Err("Invalid preview pixel buffer".into());
    }
    let mut pixels = rgba.px.clone();
    // FrameServer stores colour over black; opaque PNGs must preserve that appearance.
    for [_, _, _, alpha] in pixels.as_chunks_mut::<4>().0 {
        *alpha = 255;
    }
    let Some(image) = image::RgbaImage::from_raw(rgba.w as u32, rgba.h as u32, pixels) else {
        return Err("Invalid preview pixel buffer".into());
    };
    let mut png = Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).map_err(|_| "Cannot encode the preview frame")?;
    let png = png.into_inner();
    if png.len() > 4 * 1024 * 1024 {
        return Err("Preview PNG exceeds the AI image size limit".into());
    }
    Ok(Some(json!([
        {"type":"input_text","text":json!({"monitor":monitor,"ticks":snapshot.time.0,"frame":snapshot.key.frame,"width":rgba.w,"height":rgba.h}).to_string()},
        {"type":"input_image","image_url":format!("data:image/png;base64,{}", crate::control::base64(&png)),"detail":"auto"}
    ])))
}

#[cfg(test)]
mod job_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn setup() -> (FilmcraftApp, Pending) {
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        let job = filmcraft_engine::Job { id: 7, label: "Export AAF".into(), progress: Default::default(), result: Default::default() };
        let wait = JobWait {
            id: 7,
            envelope: json!({"ok":true,"result":{"job":7,"path":"out.aaf","poll":"jobs.list","cancel":"jobs.cancel"}}),
            progress: job.progress.clone(),
            completed: false,
        };
        app.session.jobs.push(job);
        let (_, control_reply) = std::sync::mpsc::channel();
        (app, Pending { tool: Tool::Job(wait), control_reply })
    }

    #[test]
    fn background_tool_stays_pending_until_a_success_or_error_is_available() {
        for success in [true, false] {
            let (mut app, mut pending) = setup();
            let context = egui::Context::default();
            for _ in 0..40 {
                assert!(poll(&mut app, &context, &mut pending).unwrap().is_none());
            }
            *app.session.jobs[0].result.lock().unwrap() = Some(if success {
                Ok(filmcraft_engine::export::Report { path: "out.aaf".into(), frames: 3, seconds: 0.1, bytes: 128, render_fps: 30.0, extra_files: Vec::new() })
            } else {
                Err("cancelled".into())
            });
            app.session.jobs[0].progress.finished.store(true, Ordering::Release);
            let result = poll(&mut app, &context, &mut pending).unwrap().unwrap();
            let result: Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
            assert_eq!(result["ok"], success);
            if success {
                assert_eq!(result["result"]["finished"], true);
                assert_eq!(result["result"]["path"], "out.aaf");
                assert_eq!(result["result"]["result"]["frames"], 3);
                assert!(result["result"].get("poll").is_none());
            } else {
                assert_eq!(result["error"], "cancelled");
            }
            drop(pending);
            assert!(!app.session.jobs[0].progress.cancel.load(Ordering::Acquire));
        }
    }

    #[test]
    fn stopping_the_pending_tool_cancels_its_job() {
        let (app, pending) = setup();
        drop(pending);
        assert!(app.session.jobs[0].progress.cancel.load(Ordering::Acquire));
    }
}
