//! Exercise the actual native worker entry point, with no desktop window or external media.
#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use filmcraft_engine::{FsServices, Services, Session};
use serde_json::{Value, json};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        for _ in 0..1000 {
            let path = std::env::temp_dir().join(format!("filmcraft-backend-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("test directory: {e}"),
            }
        }
        panic!("test directory collision limit");
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn enable(session: &mut Session) {
    session.backend.enable(PathBuf::from(env!("CARGO_BIN_EXE_filmcraft")));
}

fn job(session: &mut Session, id: u64) -> Value {
    session.execute("jobs.list", json!({})).unwrap().as_array().unwrap().iter().find(|j| j["id"] == id).unwrap().clone()
}

fn finish(session: &mut Session, id: u64, timeout: Duration) -> Value {
    let start = Instant::now();
    loop {
        let job = job(session, id);
        if job["finished"] == true {
            return job;
        }
        assert!(start.elapsed() < timeout, "backend job timed out: {job}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wave() -> Vec<u8> {
    let pcm: Vec<u8> = (0..8000i16).flat_map(|n| ((n % 80 - 40) * 500).to_le_bytes()).collect();
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
    bytes.extend_from_slice(&8000u32.to_le_bytes());
    bytes.extend_from_slice(&16000u32.to_le_bytes());
    bytes.extend_from_slice(b"\x02\0\x10\0data");
    bytes.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&pcm);
    bytes
}

/// The input exists only in this host. This models a LAN/virtual source without handing the
/// child a token or copying the entire project media to its filesystem.
struct Host {
    path: String,
    bytes: Vec<u8>,
    entered: AtomicBool,
    reads: AtomicU64,
    blocked: Mutex<bool>,
    wake: Condvar,
}
impl Host {
    fn release(&self) {
        *self.blocked.lock().unwrap() = false;
        self.wake.notify_all();
    }
}
impl Services for Host {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        if path != self.path {
            return FsServices.read_file(path);
        }
        self.entered.store(true, Ordering::Release);
        let mut blocked = self.blocked.lock().unwrap();
        while *blocked {
            blocked = self.wake.wait(blocked).unwrap();
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.bytes.clone())
    }
    fn write_file(&self, path: &str, bytes: &[u8]) -> std::io::Result<()> {
        FsServices.write_file(path, bytes)
    }
    fn file_size(&self, path: &str) -> std::io::Result<u64> {
        if path == self.path { Ok(self.bytes.len() as u64) } else { FsServices.file_size(path) }
    }
}
struct Release(Arc<Host>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn audio_session(directory: &Directory, blocked: bool) -> (Session, Arc<Host>) {
    let host = Arc::new(Host {
        path: directory.path("host-only.wav").to_string_lossy().into_owned(),
        bytes: wave(),
        entered: AtomicBool::new(false),
        reads: AtomicU64::new(0),
        blocked: Mutex::new(blocked),
        wake: Condvar::new(),
    });
    assert!(!Path::new(&host.path).exists());
    let mut session = Session::new(host.clone());
    session.execute("file.newSequence", json!({"width":320,"height":180,"fps":25,"sampleRate":8000,"video":0,"audio":1})).unwrap();
    let item = filmcraft_engine::commands::import_bytes(&mut session, &host.path, host.bytes.clone().into(), None).unwrap();
    session.execute("timeline.place", json!({"item":item.0,"track":"A1","seconds":0,"video":false})).unwrap();
    enable(&mut session);
    (session, host)
}

#[test]
fn blocked_backend_source_does_not_block_frontend_or_cancel_or_replace_previous_aaf() {
    let directory = Directory::new();
    let (mut session, host) = audio_session(&directory, true);
    let _release = Release(host.clone());
    let destination = directory.path("previous.aaf");
    std::fs::write(&destination, b"previous export").unwrap();
    let start = Instant::now();
    let ack = session.execute("file.exportAaf", json!({"path":destination,"preserveMix":false})).unwrap();
    assert!(start.elapsed() < Duration::from_millis(200), "submission performed blocking work");
    let id = ack["job"].as_u64().expect("native frontend gets an asynchronous job");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !host.entered.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "source service not reached: {}", job(&mut session, id));
        std::thread::sleep(Duration::from_millis(10));
    }
    let running = job(&mut session, id);
    let pid = running["backend"]["workerPid"].as_u64().unwrap();
    assert_ne!(pid, u64::from(std::process::id()));
    assert_eq!(running["finished"], false);
    let duplicate = session.execute("file.exportAaf", json!({"path":destination,"preserveMix":false})).unwrap_err();
    assert!(duplicate.to_string().contains("already running"));
    let context = egui::Context::default();
    let frames = Instant::now();
    for _ in 0..20 {
        session.set_playhead(filmcraft_time::Tick::ZERO);
        assert_eq!(job(&mut session, id)["finished"], false);
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            ui.label("Frontend heartbeat");
        });
        assert!(!output.shapes.is_empty());
        output.textures_delta.clear();
    }
    assert!(frames.elapsed() < Duration::from_secs(1), "frontend stopped drawing during a blocked source read");
    session
        .edit("Keep editing", |project, _| {
            project.name = "Unsaved frontend work".into();
            Ok(())
        })
        .unwrap();
    let revision = session.revision;
    session.execute("jobs.cancel", json!({"job":id})).unwrap();
    let done = finish(&mut session, id, Duration::from_secs(3));
    assert!(done["result"]["error"].as_str().unwrap().contains("cancel"));
    assert_eq!(session.project.name, "Unsaved frontend work");
    assert_eq!(session.revision, revision);
    assert_eq!(std::fs::read(&destination).unwrap(), b"previous export");
    assert_eq!(host.reads.load(Ordering::Relaxed), 0, "cancel waited for the blocked I/O");
    host.release();
    let retry = session.execute("file.exportAaf", json!({"path":directory.path("retry.aaf"),"preserveMix":false})).unwrap();
    let retry = finish(&mut session, retry["job"].as_u64().unwrap(), Duration::from_secs(30));
    assert!(retry["result"]["error"].is_null(), "{retry}");
    assert!(host.reads.load(Ordering::Relaxed) > 0);
}

#[test]
fn aaf_video_mixdown_uses_snapshot_and_publishes_decodable_media() {
    let directory = Directory::new();
    let mut session = Session::default();
    session.execute("file.newSequence", json!({"width":320,"height":180,"fps":25,"video":1,"audio":0})).unwrap();
    let item = session.execute("file.newColorMatte", json!({"seconds":0.12,"color":"#2040c0"})).unwrap()["item"].as_u64().unwrap();
    session.execute("timeline.place", json!({"item":item,"track":"V1","seconds":0,"audio":false})).unwrap();
    enable(&mut session);
    let destination = directory.path("video.aaf");
    let ack = session.execute("file.exportAaf", json!({"path":destination,"mixdownVideo":true,"preserveMix":false})).unwrap();
    session.execute("project.matteColor", json!({"item":item,"color":"#c02020"})).unwrap();
    let done = finish(&mut session, ack["job"].as_u64().unwrap(), Duration::from_secs(45));
    assert!(done["result"]["error"].is_null(), "{done}");
    assert_eq!(done["result"]["path"], destination.to_string_lossy().as_ref());
    let media = done["backend"]["output"]["mediaFiles"].as_array().unwrap();
    assert_eq!(media.len(), 1);
    assert!(Path::new(media[0].as_str().unwrap()).is_file());
    let mut imported = Session::default();
    imported.execute("file.importAaf", json!({"path":destination})).unwrap();
    let sequence = imported.state.active_sequence.unwrap();
    let provider = imported.media.full_res_provider(imported.project.clone(), imported.services.clone());
    let (_, _, rgba) = filmcraft_render::render_sequence_rgba8(&imported.project, sequence, filmcraft_time::Tick::ZERO, Default::default(), &provider);
    let pixel = rgba.get(..4).unwrap();
    assert!(i16::from(pixel[2]) > i16::from(pixel[0]) + 60, "worker must render the submitted blue snapshot, not later red edits: {pixel:?}");
}

#[test]
fn missing_worker_reports_error_without_running_export_on_frontend() {
    let directory = Directory::new();
    let (mut session, _) = audio_session(&directory, false);
    session.backend.enable(directory.path("missing-backend-executable"));
    let ack = session.execute("file.exportAaf", json!({"path":directory.path("missing.aaf")})).unwrap();
    let done = finish(&mut session, ack["job"].as_u64().unwrap(), Duration::from_secs(3));
    assert!(done["result"]["error"].as_str().unwrap().contains("cannot start"));
    assert!(!directory.path("missing.aaf").exists());
}

#[test]
fn media_export_and_audio_preview_use_backend_and_keep_virtual_sources() {
    let directory = Directory::new();
    let (mut session, host) = audio_session(&directory, false);
    let destination = directory.path("mix.wav");
    // Even an explicit wait request from a frontend must return a pollable job.
    let ack = session.execute("file.exportMedia", json!({"path":destination,"format":"wav","wait":true})).unwrap();
    assert_eq!(ack["poll"], "jobs.list");
    let done = finish(&mut session, ack["job"].as_u64().unwrap(), Duration::from_secs(30));
    assert!(done["result"]["error"].is_null(), "{done}");
    assert_eq!(done["result"]["path"], destination.to_string_lossy().as_ref());
    assert!(std::fs::metadata(&destination).unwrap().len() > 8000);
    assert!(host.reads.load(Ordering::Relaxed) > 0);
    session.previews.set_dir(Some(directory.path("previews")));
    let ack = session.execute("sequence.renderAudio", json!({"wait":true})).unwrap();
    let done = finish(&mut session, ack["job"].as_u64().unwrap(), Duration::from_secs(30));
    assert!(done["result"]["error"].is_null(), "{done}");
    assert!(session.previews.count() > 0, "completed backend cache must be visible to the frontend");
}

#[test]
fn monitor_backend_is_persistent_and_updates_its_project_snapshot() {
    use filmcraft_engine::backend::frames::{Config, FrameRequest, Renderer, Target};
    let mut session = Session::default();
    let sequence = session.execute("file.newSequence", json!({"width":320,"height":180,"fps":25,"video":1,"audio":0})).unwrap()["sequence"].as_u64();
    let sequence = sequence.map(filmcraft_project::ItemId).or(session.state.active_sequence).unwrap();
    let item = session.execute("file.newColorMatte", json!({"seconds":1,"color":"#2040c0"})).unwrap()["item"].as_u64().unwrap();
    session.execute("timeline.place", json!({"item":item,"track":"V1","seconds":0,"audio":false})).unwrap();
    enable(&mut session);
    let mut config = Config::for_session(&session).unwrap();
    config.gpu = false;
    let mut renderer = Renderer::new(config);
    let request = FrameRequest {
        target: Target::Sequence(sequence),
        time: filmcraft_time::Tick::ZERO,
        scale: 1.0,
        draft: false,
        gpu: false,
        preview_directory: None,
        preview_generation: 0,
        source_generation: session.media.generation(),
    };
    let cancel = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    let first = renderer.render(&session.project, &session.media, &session.services, request.clone(), &cancel, &stop).unwrap();
    assert_ne!(first.worker_pid, std::process::id());
    assert_eq!((first.width, first.height), (320, 180));
    assert!(first.rgba[2] > first.rgba[0] + 60);
    let second = renderer.render(&session.project, &session.media, &session.services, request.clone(), &cancel, &stop).unwrap();
    assert_eq!(second.worker_pid, first.worker_pid, "reuse the renderer instead of launching a process per frame");
    session.execute("project.matteColor", json!({"item":item,"color":"#c02020"})).unwrap();
    let changed = renderer.render(&session.project, &session.media, &session.services, request, &cancel, &stop).unwrap();
    assert_eq!(changed.worker_pid, first.worker_pid);
    assert!(changed.rgba[0] > changed.rgba[2] + 60, "backend did not apply the new snapshot");
}

#[test]
fn superseded_frame_keeps_its_worker_and_source_cache_alive() {
    use filmcraft_engine::backend::frames::{Config, FrameRequest, Renderer, Target};
    let directory = Directory::new();
    // Original 4x4 BGR bitmap, held only by the host's virtual filesystem.
    let mut bitmap = vec![0u8; 102];
    bitmap[..2].copy_from_slice(b"BM");
    bitmap[2..6].copy_from_slice(&102u32.to_le_bytes());
    bitmap[10..14].copy_from_slice(&54u32.to_le_bytes());
    bitmap[14..18].copy_from_slice(&40u32.to_le_bytes());
    bitmap[18..22].copy_from_slice(&4i32.to_le_bytes());
    bitmap[22..26].copy_from_slice(&4i32.to_le_bytes());
    bitmap[26..28].copy_from_slice(&1u16.to_le_bytes());
    bitmap[28..30].copy_from_slice(&24u16.to_le_bytes());
    for pixel in bitmap[54..].as_chunks_mut::<3>().0 {
        pixel.copy_from_slice(&[192, 64, 32]);
    }
    let host = Arc::new(Host {
        path: directory.path("host-only.bmp").to_string_lossy().into_owned(),
        bytes: bitmap,
        entered: AtomicBool::new(false),
        reads: AtomicU64::new(0),
        blocked: Mutex::new(false),
        wake: Condvar::new(),
    });
    let _release = Release(host.clone());
    let mut session = Session::new(host.clone());
    let image = filmcraft_engine::commands::import_bytes(&mut session, &host.path, host.bytes.clone().into(), None).unwrap();
    let matte = session.execute("file.newColorMatte", json!({"width":4,"height":4,"seconds":1,"color":"#2040c0"})).unwrap()["item"].as_u64().unwrap();
    enable(&mut session);
    let mut config = Config::for_session(&session).unwrap();
    config.gpu = false;
    let mut renderer = Renderer::new(config);
    let mut request = FrameRequest {
        target: Target::Item(filmcraft_project::ItemId(matte)),
        time: filmcraft_time::Tick::ZERO,
        scale: 1.0,
        draft: false,
        gpu: false,
        preview_directory: None,
        preview_generation: 0,
        source_generation: session.media.generation(),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = AtomicBool::new(false);
    let first = renderer.render(&session.project, &session.media, &session.services, request.clone(), &cancel, &stop).unwrap();
    *host.blocked.lock().unwrap() = true;
    host.entered.store(false, Ordering::Release);
    request.target = Target::Item(image);
    let retry_request = request.clone();
    let thread_cancel = cancel.clone();
    let project = session.project.clone();
    let pool = session.media.clone();
    let services = session.services.clone();
    let render = std::thread::spawn(move || {
        let result = renderer.render(&project, &pool, &services, request, &thread_cancel, &AtomicBool::new(false));
        (renderer, result)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !host.entered.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "frame worker never requested the virtual bitmap");
        std::thread::sleep(Duration::from_millis(5));
    }
    cancel.store(true, Ordering::Release);
    std::thread::sleep(Duration::from_millis(50));
    host.release();
    let (mut renderer, result) = render.join().unwrap();
    assert!(matches!(result, Err(error) if error.contains("cancel")));
    cancel.store(false, Ordering::Release);
    let next = renderer.render(&session.project, &session.media, &session.services, retry_request, &cancel, &stop).unwrap();
    assert_eq!(next.worker_pid, first.worker_pid, "discarding a stale frame must preserve the decoder process");
    assert_eq!((next.width, next.height), (4, 4));
    assert!(next.rgba[2] > next.rgba[0] + 60, "source cache was poisoned by cancellation");
}
