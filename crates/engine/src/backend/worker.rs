use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use web_time::Instant;

use filmcraft_export::{Progress, Report};
use serde_json::{Value, json};

use super::{Completion, Request, Response, Snapshot, Task, protocol, services};
use crate::Session;

/// Called by a native host before creating a window, loading preferences or starting recovery.
/// A worker has no editable document and cannot overwrite the frontend's unsaved project.
pub fn serve_stdio() -> Result<(), String> {
    let output = Arc::new(Mutex::new(io::stdout()));
    protocol::send(&mut *output.lock().unwrap_or_else(PoisonError::into_inner), &Response::Ready { pid: std::process::id() }, &[])
        .map_err(|e| e.to_string())?;
    let (Request::Run { snapshot, task }, data) = protocol::read::<Request>(&mut io::stdin()).map_err(|e| e.to_string())? else {
        return Err("backend expected a project snapshot and task".into());
    };
    if !data.is_empty() {
        return Err("unexpected binary data in backend task".into());
    }
    let progress: Arc<Progress> = Arc::default();
    let (reply, replies) = mpsc::sync_channel(1);
    let broker = Arc::new(services::Broker::new(output.clone(), replies));
    let cancellation = progress.clone();
    std::thread::Builder::new()
        .name("filmcraft-backend-input".into())
        .spawn(move || {
            let guarded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                loop {
                    match protocol::read::<Request>(&mut io::stdin()) {
                        Ok((Request::IoReply { id, result }, data)) => {
                            if reply.send((id, result, data)).is_err() {
                                return;
                            }
                        }
                        Ok((Request::Cancel, data)) if data.is_empty() => cancellation.cancel.store(true, Ordering::Release),
                        _ => {
                            // Anonymous-pipe EOF means the owner died. The child owns only staged
                            // outputs, so stop even a decoder that does not poll cooperative cancel.
                            std::process::exit(1);
                        }
                    }
                }
            }));
            if guarded.is_err() {
                std::process::exit(1);
            }
        })
        .map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let status_output = output.clone();
    let status_stop = stop.clone();
    let status_progress = progress.clone();
    std::thread::Builder::new()
        .name("filmcraft-backend-progress".into())
        .spawn(move || {
            let guarded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                while !status_stop.load(Ordering::Acquire) {
                    let message = Response::Progress {
                        done: status_progress.done.load(Ordering::Relaxed),
                        total: status_progress.total.load(Ordering::Relaxed),
                        status: status_progress.status.lock().unwrap_or_else(PoisonError::into_inner).clone(),
                    };
                    if protocol::send(&mut *status_output.lock().unwrap_or_else(PoisonError::into_inner), &message, &[]).is_err() {
                        status_progress.cancel.store(true, Ordering::Release);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }));
            if guarded.is_err() {
                status_progress.cancel.store(true, Ordering::Release);
            }
        })
        .map_err(|e| e.to_string())?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(snapshot, task, broker, progress)))
        .unwrap_or_else(|_| Err("internal error in backend task; your project is unchanged".into()));
    stop.store(true, Ordering::Release);
    protocol::send(&mut *output.lock().unwrap_or_else(PoisonError::into_inner), &Response::Finished { result }, &[]).map_err(|e| e.to_string())
}

fn run(snapshot: Snapshot, task: Task, broker: Arc<services::Broker>, progress: Arc<Progress>) -> Result<Completion, String> {
    let mut session = Session::new(Arc::new(services::WorkerServices(broker)));
    apply_snapshot(&mut session, snapshot)?;
    session.worker_progress = Some(progress.clone());
    let started = Instant::now();
    match task {
        Task::ColorGroup { operation } => {
            let plan = crate::grading_groups::calculate(&session, &operation, &progress).map_err(|e| e.to_string())?;
            let output = serde_json::to_value(plan).map_err(|e| e.to_string())?;
            let report = Report {
                path: String::new(),
                frames: progress.done.load(Ordering::Relaxed),
                seconds: started.elapsed().as_secs_f64(),
                bytes: 0,
                render_fps: 0.0,
                extra_files: Vec::new(),
            };
            Ok(Completion { output, report })
        }
        Task::Interchange { omf, params, media_dir } => {
            let output = if omf {
                crate::aaf_omf::export_omf_at(&mut session, &params, media_dir.as_deref())
            } else {
                crate::aaf_omf::export_aaf_at(&mut session, &params, media_dir.as_deref())
            }
            .map_err(|e| e.to_string())?;
            let path = output["path"].as_str().unwrap_or_default().to_string();
            let report = Report {
                path,
                frames: progress.done.load(Ordering::Relaxed),
                seconds: started.elapsed().as_secs_f64(),
                bytes: output["bytes"].as_u64().unwrap_or(0),
                render_fps: 0.0,
                extra_files: output["mediaFiles"].as_array().map(|v| v.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default(),
            };
            Ok(Completion { output, report })
        }
        Task::Media { sequence, mut settings } => {
            if !session.worker_gpu {
                settings.gpu_rendering = filmcraft_export::GpuRendering::Off;
            }
            crate::export_tools::write_sidecar(&session, &session.project, sequence, &settings).map_err(|e| e.to_string())?;
            let provider = session
                .media
                .full_res_provider(session.project.clone(), session.services.clone())
                .own_sources()
                .recording()
                .stop_on_link_error(progress.clone());
            let report = filmcraft_export::export(&session.project, sequence, &settings, &provider, &progress).map_err(|e| e.to_string())?;
            if let Some(error) = provider.link_error() {
                return Err(error);
            }
            let output = json!({"path":report.path, "offlineMedia":provider.offline_used()});
            Ok(Completion { output, report })
        }
        Task::Preview { mode, mut params, directory } => {
            session.previews.set_dir(Some(directory));
            params["wait"] = true.into();
            let mut output = match mode {
                Some(mode) => crate::previews::render(&mut session, mode, &params),
                None => crate::previews::render_audio(&mut session, &params),
            }
            .map_err(|e| e.to_string())?;
            if let Some(fields) = output.as_object_mut() {
                fields.remove("job");
            }
            let report = match session.jobs.last() {
                Some(job) => job.result.lock().unwrap_or_else(PoisonError::into_inner).clone().ok_or("preview worker returned no result")??,
                None => Report { path: String::new(), frames: 0, seconds: started.elapsed().as_secs_f64(), bytes: 0, render_fps: 0.0, extra_files: Vec::new() },
            };
            Ok(Completion { output, report })
        }
    }
}

pub(super) fn apply_snapshot(session: &mut Session, snapshot: Snapshot) -> Result<(), String> {
    snapshot.project.check_color_groups()?;
    for item in snapshot.project.sequences() {
        if let Some(sequence) = snapshot.project.sequence(item.id) {
            sequence.settings.validate()?;
        }
    }
    session.project = snapshot.project;
    session.worker_gpu = snapshot.gpu_enabled;
    session.media.set_use_proxies(snapshot.use_proxies);
    session.state.active_sequence = snapshot.active_sequence;
    session.state.selection = snapshot.selection;
    session.prefs.media.effectcraft_executable = snapshot.effectcraft_executable;
    session.prefs.playback.hardware_decoding = snapshot.hardware_decoding;
    session.prefs.apply_hardware_decoding();
    session.media.set_composition_renderer(&session.prefs.media.effectcraft_executable);
    session.media.clear_stored();
    for (path, document) in snapshot.stored_paths {
        session.media.add_stored_paths([path], document.as_deref().and_then(std::path::Path::to_str));
    }
    Ok(())
}
