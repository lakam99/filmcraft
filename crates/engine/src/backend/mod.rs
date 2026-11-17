//! Native heavy-work backend. The frontend submits immutable snapshots without waiting for
//! serialization, I/O or rendering. Anonymous-pipe workers never own the editable session.

mod client;
pub mod frames;
mod protocol;
mod services;
mod staging;
mod worker;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_export::{ExportSettings, Progress, Report};
use filmcraft_project::{ClipId, ItemId, Project};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{EngineError, Job, MediaPool, Result, Services, Session};

pub use worker::serve_stdio;

const MAX_JOBS: usize = 2;
pub const WORKER_ARGUMENT: &str = "--backend-worker";
pub const FRAME_WORKER_ARGUMENT: &str = "--frame-worker";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    project: Arc<Project>,
    active_sequence: Option<ItemId>,
    selection: Vec<ClipId>,
    effectcraft_executable: String,
    hardware_decoding: String,
    stored_paths: Vec<(String, Option<PathBuf>)>,
    gpu_enabled: bool,
    use_proxies: bool,
}

impl Snapshot {
    fn capture(s: &Session, project: Arc<Project>) -> Self {
        Self {
            project,
            active_sequence: s.state.active_sequence,
            selection: s.state.selection.clone(),
            effectcraft_executable: s.prefs.media.effectcraft_executable.clone(),
            hardware_decoding: s.prefs.playback.hardware_decoding.clone(),
            stored_paths: Vec::new(),
            gpu_enabled: s.backend.gpu_enabled,
            use_proxies: s.media.use_proxies(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Task {
    Interchange { omf: bool, params: Value, media_dir: Option<String> },
    Media { sequence: ItemId, settings: Box<ExportSettings> },
    Preview { mode: Option<crate::previews::RenderMode>, params: Value, directory: PathBuf },
}

#[derive(Serialize, Deserialize)]
enum Request {
    Run { snapshot: Snapshot, task: Task },
    IoReply { id: u64, result: std::result::Result<Value, String> },
    Cancel,
    Frame { snapshot: Option<Snapshot>, request: frames::FrameRequest },
}

#[derive(Serialize, Deserialize)]
enum Response {
    Ready { pid: u32 },
    Progress { done: u64, total: u64, status: String },
    Io { id: u64, request: services::IoRequest },
    Finished { result: std::result::Result<Completion, String> },
    Frame { width: usize, height: usize, preview: bool, loading: bool, error: Option<String> },
}

#[derive(Serialize, Deserialize)]
struct Completion {
    output: Value,
    report: Report,
}

#[derive(Default)]
pub(crate) struct State {
    pid: AtomicU32,
    output: Mutex<Option<Value>>,
}

struct Tracked {
    state: Arc<State>,
    progress: Arc<Progress>,
    document_epoch: u64,
    revision: u64,
    sequence: Option<ItemId>,
    destination: Option<String>,
    preview: bool,
}

/// Opt-in by native hosts that support `--backend-worker`. Disabled in engine unit tests,
/// embedded hosts and wasm; those retain their existing execution model.
#[derive(Default)]
pub struct Runtime {
    executable: Option<PathBuf>,
    failure: Option<String>,
    gpu_enabled: bool,
    jobs: HashMap<u64, Tracked>,
}

impl Runtime {
    pub fn enable(&mut self, executable: PathBuf) {
        self.executable = Some(executable);
        self.failure = None;
        self.gpu_enabled = true;
    }

    pub fn set_gpu_rendering(&mut self, enabled: bool) {
        self.gpu_enabled = enabled;
    }

    pub fn unavailable(&mut self, reason: String) {
        self.executable = None;
        self.failure = Some(reason);
    }

    pub fn executable(&self) -> Option<&std::path::Path> {
        self.executable.as_deref()
    }

    /// Preview completion may update its content cache, but must not start playback in a
    /// different document/sequence or after further edits.
    pub fn can_play_preview(&self, job: u64, epoch: u64, revision: u64, sequence: Option<ItemId>) -> bool {
        self.jobs.get(&job).is_none_or(|t| t.document_epoch == epoch && t.revision == revision && t.sequence == sequence)
    }

    pub(crate) fn details(&self, id: u64) -> Option<Value> {
        let t = self.jobs.get(&id)?;
        Some(json!({
            "workerPid": match t.state.pid.load(Ordering::Relaxed) { 0 => None, p => Some(p) },
            "documentEpoch": t.document_epoch, "revision": t.revision,
            "output": t.state.output.lock().unwrap_or_else(PoisonError::into_inner).clone(),
        }))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Cancellation is nonblocking here. Coordinators own and reap children away from the
        // render loop, even if a worker or a decoder is stalled.
        for t in self.jobs.values() {
            if !t.progress.finished.load(Ordering::Acquire) {
                t.progress.cancel.store(true, Ordering::Release);
            }
        }
    }
}

pub(crate) fn enabled(s: &Session) -> bool {
    s.backend.executable.is_some() || s.backend.failure.is_some()
}

pub(crate) fn start_interchange(s: &mut Session, omf: bool, params: &Value) -> Result<Value> {
    let path = params.get("path").and_then(Value::as_str).ok_or_else(|| EngineError::Other("an export path is required".into()))?.to_string();
    let task = Task::Interchange { omf, params: params.clone(), media_dir: None };
    let name = if omf { "OMF" } else { "AAF" };
    let id = start(s, s.project.clone(), task, format!("Export {name}"))?;
    Ok(json!({"job":id, "path":path, "poll":"jobs.list", "cancel":"jobs.cancel", "background":true}))
}

pub(crate) fn start_media(s: &mut Session, project: Arc<Project>, sequence: ItemId, settings: ExportSettings, label: String) -> Result<u64> {
    start(s, project, Task::Media { sequence, settings: Box::new(settings) }, label)
}

pub(crate) fn start_preview(s: &mut Session, mode: Option<crate::previews::RenderMode>, params: &Value) -> Result<Value> {
    let directory = s.previews.dir().ok_or_else(|| EngineError::Other("render previews need a cache directory".into()))?;
    let label = if mode.is_some() { "Rendering previews" } else { "Rendering audio previews" };
    let id = start(s, s.project.clone(), Task::Preview { mode, params: params.clone(), directory }, label.into())?;
    Ok(json!({"job":id, "poll":"jobs.list", "cancel":"jobs.cancel", "background":true}))
}

fn start(s: &mut Session, project: Arc<Project>, task: Task, label: String) -> Result<u64> {
    if s.exec_depth > 1 {
        return Err(EngineError::Other("background renders cannot run inside an atomic editing batch; start the render after the batch".into()));
    }
    let destination = match &task {
        Task::Interchange { params, .. } => params["path"].as_str(),
        Task::Media { settings, .. } => Some(settings.path.as_str()),
        Task::Preview { .. } => None,
    }
    .map(|path| {
        let path = std::path::absolute(path).unwrap_or_else(|_| PathBuf::from(path)).to_string_lossy().replace('\\', "/");
        if cfg!(windows) { path.to_lowercase() } else { path }
    });
    let preview = matches!(task, Task::Preview { .. });
    for current in s.backend.jobs.values().filter(|t| !t.progress.finished.load(Ordering::Acquire)) {
        if destination.is_some() && current.destination == destination {
            return Err(EngineError::Other("an export to this destination is already running; wait or cancel it before replacing it".into()));
        }
        if preview && current.preview {
            return Err(EngineError::Other("a preview render is already running; wait or cancel it first".into()));
        }
    }
    if s.backend.jobs.values().filter(|t| !t.progress.finished.load(Ordering::Acquire)).count() >= MAX_JOBS {
        return Err(EngineError::Other("two backend jobs are already running; wait for one or cancel it in Progress".into()));
    }
    let executable = s
        .backend
        .executable
        .clone()
        .ok_or_else(|| EngineError::Other(s.backend.failure.clone().unwrap_or_else(|| "the native backend is unavailable".into())))?;
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0).checked_add(1).ok_or_else(|| EngineError::Other("job identity limit reached".into()))?;
    let snapshot = Snapshot::capture(s, project);
    let state: Arc<State> = Arc::default();
    let tracked_state = state.clone();
    let job = Job { id, label, progress: Arc::default(), result: Arc::default() };
    *job.progress.status.lock().unwrap_or_else(PoisonError::into_inner) = "Starting backend".into();
    let work = Work { executable, snapshot, task, pool: s.media.clone(), services: s.services.clone(), previews: s.previews.clone(), state: state.clone() };
    let (progress, result) = (job.progress.clone(), job.result.clone());
    let run = crate::export_tools::guard_job(progress.clone(), result.clone(), move || {
        let outcome = run(work, &progress).map(|done| {
            *state.output.lock().unwrap_or_else(PoisonError::into_inner) = Some(done.output);
            done.report
        });
        if let Err(error) = &outcome {
            *progress.error.lock().unwrap_or_else(PoisonError::into_inner) = Some(error.clone());
            *progress.status.lock().unwrap_or_else(PoisonError::into_inner) = error.clone();
        }
        *result.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
        progress.finished.store(true, Ordering::Release);
    });
    std::thread::Builder::new()
        .name("filmcraft-backend-supervisor".into())
        .spawn(run)
        .map_err(|e| EngineError::Other(format!("cannot start backend supervisor: {e}")))?;
    s.backend.jobs.insert(
        id,
        Tracked {
            state: tracked_state,
            progress: job.progress.clone(),
            document_epoch: s.document_epoch,
            revision: s.revision,
            sequence: s.state.active_sequence,
            destination,
            preview,
        },
    );
    s.jobs.push(job);
    Ok(id)
}

struct Work {
    executable: PathBuf,
    snapshot: Snapshot,
    task: Task,
    pool: Arc<MediaPool>,
    services: Arc<dyn Services>,
    previews: Arc<crate::previews::PreviewStore>,
    state: Arc<State>,
}

fn run(mut work: Work, progress: &Arc<Progress>) -> std::result::Result<Completion, String> {
    if progress.cancel.load(Ordering::Acquire) {
        return Err("cancelled".into());
    }
    let mut stage = staging::Stage::prepare(&mut work.task)?;
    work.snapshot.stored_paths = work.pool.stored_snapshot();
    let mut child = client::Client::start(&work.executable, work.services, work.state)?;
    child.request(Request::Run { snapshot: work.snapshot, task: work.task.clone() }, &[])?;
    let mut completion = child.finish(progress)?;
    if progress.cancel.load(Ordering::Acquire) {
        return Err("cancelled".into());
    }
    if let Some(stage) = &mut stage {
        stage.publish(&mut completion)?;
    }
    if let Task::Preview { directory, .. } = &work.task {
        work.previews.refresh_from_backend(directory);
    }
    Ok(completion)
}
