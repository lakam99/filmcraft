use std::io::Cursor;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::frames::{FrameKey, Target};
use crate::{ControlRequest, FilmcraftApp};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditorArgs {
    method: String,
    params_json: String,
}

pub(super) enum Tool {
    Editor(ControlRequest),
    Frame { monitor: String, snapshot: Option<Snapshot> },
}

pub(super) struct Snapshot {
    key: FrameKey,
    time: filmcraft_time::Tick,
    scale: f32,
}

pub(super) fn parse(name: &str, args: &Value) -> Result<(Tool, std::sync::mpsc::Receiver<Value>, String), String> {
    match name {
        "filmcraft" => {
            let args: EditorArgs = serde_json::from_value(args.clone()).map_err(|_| "Expected method and params_json strings")?;
            // Only the existing editor surface is callable, never a shell, URL, or another AI run.
            let supported = super::api::tools();
            let allowed = supported
                .pointer("/0/parameters/properties/method/enum")
                .and_then(Value::as_array)
                .is_some_and(|methods| methods.iter().any(|m| m.as_str() == Some(&args.method)));
            if !allowed {
                return Err("Unsupported FilmCraft method; use the tool's declared methods".into());
            }
            if args.params_json.len() > 64 * 1024 {
                return Err("Editor parameters exceed 64 KiB".into());
            }
            let params: Value = serde_json::from_str(&args.params_json).map_err(|_| "params_json must contain a valid JSON object")?;
            if !params.is_object() {
                return Err("params_json must contain a JSON object".into());
            }
            if args.method == "ui.set" && params.pointer("/settings/values/ai").is_some() {
                return Err("AI preferences are reserved for the user".into());
            }
            if let Some(id) = params.get("command").or_else(|| params.get("id")).and_then(Value::as_str)
                && id.starts_with("ai.")
            {
                return Err("The assistant cannot invoke or configure itself".into());
            }
            // The credential and Send controls must never be driven by the assistant itself.
            if targets_chat(&params, 0) {
                return Err("AI chat controls are reserved for the user".into());
            }
            let label = format!("{} {}", args.method, args.params_json.chars().take(1024).collect::<String>());
            let (request, receiver) = ControlRequest::new(args.method, params);
            Ok((Tool::Editor(request), receiver, label))
        }
        "video_frame" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                monitor: String,
            }
            let args: Args = serde_json::from_value(args.clone()).map_err(|_| "Expected a monitor: Program or Source")?;
            if !matches!(args.monitor.as_str(), "Program" | "Source") {
                return Err("monitor must be Program or Source".into());
            }
            // The frame reply uses the same bounded one-shot channel as normal controls.
            let (_, rx) = std::sync::mpsc::channel();
            Ok((Tool::Frame { monitor: args.monitor.clone(), snapshot: None }, rx, format!("Video frame: {}", args.monitor)))
        }
        _ => Err("Unknown AI tool".into()),
    }
}

fn targets_chat(value: &Value, depth: usize) -> bool {
    if depth > 32 {
        return true;
    }
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            key.starts_with("ai.")
                || matches!(key.as_str(), "command" | "id" | "key")
                    && value.as_str().is_some_and(|id| id.starts_with("ai.") || id.starts_with("settings.ai.") || id == "app.settings.ai")
                || targets_chat(value, depth + 1)
        }),
        Value::Array(values) => values.iter().any(|value| targets_chat(value, depth + 1)),
        _ => false,
    }
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
