//! Persistent monitor/thumbnail renderer, separate from both the window and long export jobs.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak, mpsc};
use std::time::Duration;

use filmcraft_project::{ItemId, Project};
use filmcraft_time::Tick;
use serde::{Deserialize, Serialize};
use web_time::Instant;

use super::{Request, Response, Snapshot, client::Client, protocol, services, worker};
use crate::{MediaPool, Services, Session};

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub executable: PathBuf,
    pub hardware_decoding: String,
    pub effectcraft_executable: String,
    pub gpu: bool,
}

impl Config {
    pub fn for_session(s: &Session) -> Option<Self> {
        Some(Self {
            executable: s.backend.executable()?.into(),
            hardware_decoding: s.prefs.playback.hardware_decoding.clone(),
            effectcraft_executable: s.prefs.media.effectcraft_executable.clone(),
            gpu: s.backend.gpu_enabled,
        })
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum Target {
    Sequence(ItemId),
    SequenceLive(ItemId),
    Item(ItemId),
    MulticamGrid(ItemId, u8, u16),
    MulticamAngle(ItemId, u32),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FrameRequest {
    pub target: Target,
    pub time: Tick,
    pub scale: f32,
    pub draft: bool,
    pub gpu: bool,
    pub preview_directory: Option<PathBuf>,
    pub preview_generation: u64,
    pub source_generation: u64,
}

pub struct Frame {
    pub worker_pid: u32,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub preview: bool,
    pub loading: bool,
}

pub struct Renderer {
    config: Config,
    client: Option<Client>,
    project: Weak<Project>,
    preview_generation: u64,
    source_generation: u64,
    failed_at: Option<Instant>,
    gpu_failed: bool,
}

impl Renderer {
    pub fn new(config: Config) -> Self {
        Self { config, client: None, project: Weak::new(), preview_generation: 0, source_generation: 0, failed_at: None, gpu_failed: false }
    }

    /// Called only on a frame coordinator thread. The UI only requests cached frames.
    pub fn render(
        &mut self,
        project: &Arc<Project>,
        pool: &Arc<MediaPool>,
        services: &Arc<dyn Services>,
        mut request: FrameRequest,
        cancel: &AtomicBool,
        stop: &AtomicBool,
    ) -> Result<Frame, String> {
        if cancel.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }
        if self.failed_at.is_some_and(|at| at.elapsed() < Duration::from_millis(500)) {
            return Err("preview backend is restarting".into());
        }
        request.gpu &= self.config.gpu && !self.gpu_failed;
        request.gpu &= project.settings.renderer != crate::project_tools::RENDERER_SOFTWARE;
        let result = (|| {
            if self.client.is_none() {
                self.client = Some(Client::start_frames(&self.config.executable, services.clone())?);
            }
            let changed = self.project.upgrade().is_none_or(|old| !Arc::ptr_eq(&old, project))
                || self.preview_generation != request.preview_generation
                || self.source_generation != request.source_generation;
            let snapshot = (changed || self.project.strong_count() == 0).then(|| Snapshot {
                project: project.clone(),
                active_sequence: None,
                selection: Vec::new(),
                effectcraft_executable: self.config.effectcraft_executable.clone(),
                hardware_decoding: self.config.hardware_decoding.clone(),
                stored_paths: pool.stored_snapshot(),
                gpu_enabled: self.config.gpu,
                use_proxies: pool.use_proxies(),
            });
            let preview_generation = request.preview_generation;
            let source_generation = request.source_generation;
            let client = self.client.as_mut().ok_or("preview backend unavailable")?;
            client.request(Request::Frame { snapshot, request }, &[])?;
            let frame = client.frame(stop)?;
            self.project = Arc::downgrade(project);
            self.preview_generation = preview_generation;
            self.source_generation = source_generation;
            Ok(frame)
        })();
        if result.is_err() {
            self.client = None; // kills/reaps this worker, always on the coordinator thread
            self.project = Weak::new();
            if !cancel.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                self.failed_at = Some(Instant::now());
                self.gpu_failed = true; // one driver failure must not become a crash/restart loop
            }
        } else {
            self.failed_at = None;
        }
        if result.is_ok() && cancel.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }
        result
    }
}

/// Native host entry point. The renderer factory is supplied by the L6 host, preserving layers.
pub fn serve_stdio(factory: filmcraft_export::FrameRendererFactory) -> Result<(), String> {
    let output = Arc::new(Mutex::new(io::stdout()));
    protocol::send(&mut *output.lock().unwrap_or_else(PoisonError::into_inner), &Response::Ready { pid: std::process::id() }, &[])
        .map_err(|e| e.to_string())?;
    let (reply, replies) = mpsc::sync_channel(1);
    let (requests, incoming) = mpsc::sync_channel(1);
    let broker = Arc::new(services::Broker::new(output.clone(), replies));
    std::thread::Builder::new()
        .name("filmcraft-preview-input".into())
        .spawn(move || {
            let guarded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                loop {
                    match protocol::read::<Request>(&mut io::stdin()) {
                        Ok((Request::IoReply { id, result }, data)) => {
                            if reply.send((id, result, data)).is_err() {
                                return;
                            }
                        }
                        Ok((Request::Frame { snapshot, request }, data)) if data.is_empty() => {
                            if requests.try_send((snapshot, request)).is_err() {
                                std::process::exit(1);
                            }
                        }
                        _ => std::process::exit(1),
                    }
                }
            }));
            if guarded.is_err() {
                std::process::exit(1);
            }
        })
        .map_err(|e| e.to_string())?;
    let mut session = Session::new(Arc::new(services::WorkerServices(broker.clone())));
    let mut renderer = None;
    let mut gpu_tried = false;
    let mut source_generation = None;
    let mut preview_generation = None;
    while let Ok((snapshot, request)) = incoming.recv() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(snapshot) = snapshot {
                worker::apply_snapshot(&mut session, snapshot)?;
            }
            if source_generation != Some(request.source_generation) {
                broker.call(services::IoRequest::Reset).map_err(|e| e.to_string())?;
                session.media.clear();
                source_generation = Some(request.source_generation);
            }
            if preview_generation != Some(request.preview_generation) {
                session.previews.set_dir(request.preview_directory.clone());
                preview_generation = Some(request.preview_generation);
            }
            if request.gpu && !gpu_tried {
                renderer = factory();
                gpu_tried = true;
            }
            render(&session, &request, if request.gpu { renderer.as_deref_mut() } else { None })
        }))
        .unwrap_or_else(|_| Err("preview renderer failed; retrying without GPU rendering".into()));
        let (message, data) = match result {
            Ok(frame) => {
                (Response::Frame { width: frame.width, height: frame.height, preview: frame.preview, loading: frame.loading, error: None }, frame.rgba)
            }
            Err(error) => (Response::Frame { width: 0, height: 0, preview: false, loading: false, error: Some(error) }, Vec::new()),
        };
        protocol::send(&mut *output.lock().unwrap_or_else(PoisonError::into_inner), &message, &data).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn render(s: &Session, request: &FrameRequest, gpu: Option<&mut (dyn filmcraft_export::FrameRenderer + 'static)>) -> Result<Frame, String> {
    if !request.scale.is_finite() || request.scale <= 0.0 || request.scale > 16.0 {
        return Err("preview scale must be within (0, 16]".into());
    }
    let item = match request.target {
        Target::Sequence(id) | Target::SequenceLive(id) | Target::Item(id) | Target::MulticamGrid(id, _, _) | Target::MulticamAngle(id, _) => id,
    };
    let size = s
        .project
        .sequence(item)
        .map(|q| (q.settings.width, q.settings.height))
        .or_else(|| s.project.resolve_media(item).and_then(|(_, m, _)| m.info.video.as_ref()).map(|v| (v.width, v.height)));
    if let Some((w, h)) = size {
        let w = (f64::from(w) * f64::from(request.scale)).ceil().max(1.0) as u64;
        let h = (f64::from(h) * f64::from(request.scale)).ceil().max(1.0) as u64;
        if w.checked_mul(h).and_then(|n| n.checked_mul(4)).is_none_or(|n| n > protocol::MAX_DATA as u64) {
            return Err("preview exceeds 64 MiB; lower the monitor resolution".into());
        }
    }
    let _ = filmcraft_media::pending::take();
    let provider = s.media.provider(s.project.clone(), s.services.clone());
    let opts = filmcraft_render::RenderOptions { scale: request.scale, captions: true, ..Default::default() };
    let mut preview = false;
    let image = filmcraft_media::cancel::with_draft(request.draft, || match request.target {
        Target::Sequence(id) | Target::SequenceLive(id) => {
            if matches!(request.target, Target::Sequence(_))
                && let Some(q) = s.project.sequence(id)
                && let Some(frame) = s.previews.frame(&s.media, &s.project, id, q.settings.frame_rate.frame_at(request.time), request.scale)
            {
                preview = true;
                let image = filmcraft_render::Image { w: frame.width as usize, h: frame.height as usize, px: frame.to_linear_f32() };
                let q = s.project.sequence(id)?;
                let (w, h) = filmcraft_render::output_size(q, request.scale);
                let mut image = if (image.w, image.h) == (w, h) {
                    image
                } else {
                    image.transformed(w, h, &filmcraft_geom::Affine::scale(w as f64 / image.w.max(1) as f64, h as f64 / image.h.max(1) as f64))
                };
                for overlay in filmcraft_render::caption_overlays(q, request.time, w, h) {
                    overlay.composite_onto(&mut image.px, w, h);
                }
                return Some(image);
            }
            gpu.and_then(|gpu| gpu.render(&s.project, id, request.time, opts, &provider))
                .or_else(|| Some(filmcraft_render::render_sequence(&s.project, id, request.time, opts, &provider)))
        }
        Target::Item(id) => filmcraft_render::render_item(&s.project, id, request.time, request.scale, &provider),
        Target::MulticamGrid(id, side, page) => filmcraft_render::multicam::render_grid_page(
            &s.project,
            id,
            request.time,
            request.scale,
            (side > 0).then_some(side as usize),
            page as usize,
            &provider,
        )
        .map(|(image, _)| image),
        Target::MulticamAngle(id, angle) => {
            filmcraft_render::multicam::render_angle_thumbnail(&s.project, id, angle as usize, request.time, request.scale, &provider)
        }
    });
    let image = image.unwrap_or_else(|| filmcraft_render::Image::new(1, 1));
    if image.w.checked_mul(image.h).and_then(|n| n.checked_mul(4)).is_none_or(|n| n > protocol::MAX_DATA) {
        return Err("preview exceeds 64 MiB; lower the monitor resolution".into());
    }
    Ok(Frame {
        worker_pid: std::process::id(),
        width: image.w,
        height: image.h,
        rgba: image.over_black_with_alpha_rgba8(),
        preview,
        loading: filmcraft_media::pending::take(),
    })
}
