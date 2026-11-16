//! Private, bounded pipe connection to the installed EffectCraft renderer. Project documents
//! cannot choose an executable; it comes only from local preferences or known installation paths.

use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;

use filmcraft_frame::{AudioBuffer, VideoFrame};
use filmcraft_media::{FrameCache, FrameRequest, MediaError, MediaInfo, MediaSource, SharedSource};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use super::{Catalog, Composition};

static WORKERS: AtomicUsize = AtomicUsize::new(0);
const MAX_WORKERS: usize = 16;
const MAX_BODY: usize = 64 * 1024 * 1024;
type Reply = Result<(Value, Vec<u8>), String>;

struct Permit;
impl Permit {
    fn acquire() -> Result<Self, String> {
        // Use the workspace's minimum Rust version as well as current compilers; the newer
        // `try_update` name is unavailable on some supported toolchains.
        for _ in 0..64 {
            let n = WORKERS.load(Ordering::Relaxed);
            if n >= MAX_WORKERS {
                return Err("Too many linked composition renderers (16); close unused projects or export jobs".into());
            }
            if WORKERS.compare_exchange(n, n + 1, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                return Ok(Self);
            }
        }
        Err("Composition renderers are busy; retry after the current job finishes".into())
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn executable(preference: &str) -> Result<PathBuf, String> {
    if !preference.trim().is_empty() {
        let path = PathBuf::from(preference);
        return path.is_file().then_some(path).ok_or_else(|| "The EffectCraft application in Settings > Media is missing; choose its executable again".into());
    }
    if let Some(path) = std::env::var_os("EFFECTCRAFT_EXECUTABLE").map(PathBuf::from).filter(|p| p.is_file()) {
        return Ok(path);
    }
    let name = if cfg!(windows) { "effectcraft.exe" } else { "effectcraft" };
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        candidates.push(dir.join(name));
        for parent in dir.ancestors().take(4) {
            candidates.push(parent.join("effectcraft").join(name));
            candidates.push(parent.join("EffectCraft.app/Contents/MacOS/effectcraft"));
        }
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| "Install EffectCraft using the fork launcher, or choose the EffectCraft application in Settings > Media".into())
}

fn read_reply(reader: &mut impl Read) -> Reply {
    let mut word = [0; 4];
    reader.read_exact(&mut word).map_err(|e| format!("EffectCraft renderer disconnected: {e}"))?;
    let len = u32::from_le_bytes(word) as usize;
    if len == 0 || len > 1024 * 1024 {
        return Err("Invalid EffectCraft response header length".into());
    }
    let mut header = vec![0; len];
    reader.read_exact(&mut header).map_err(|e| e.to_string())?;
    let meta: Value = serde_json::from_slice(&header).map_err(|e| e.to_string())?;
    if meta["protocol"].as_u64() != Some(1) {
        return Err("Update both FilmCraft and EffectCraft; the composition protocol is incompatible".into());
    }
    let bytes = meta["bytes"].as_u64().filter(|n| *n <= MAX_BODY as u64).ok_or("Invalid EffectCraft payload length")? as usize;
    let mut data = vec![0; bytes];
    reader.read_exact(&mut data).map_err(|e| e.to_string())?;
    Ok((meta, data))
}

pub struct Worker {
    child: Child,
    requests: mpsc::SyncSender<Vec<u8>>,
    replies: mpsc::Receiver<Reply>,
    failed: bool,
    _permit: Permit,
}

impl Worker {
    fn start(renderer: &Path) -> Result<Self, String> {
        let mut command = Command::new(renderer);
        command.arg("--linked-composition-worker").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        Self::start_command(command)
    }

    fn start_command(mut command: Command) -> Result<Self, String> {
        let permit = Permit::acquire()?;
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().map_err(|e| format!("Could not start EffectCraft: {e}"))?;
        let Some(mut input) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("EffectCraft input pipe is unavailable".into());
        };
        let Some(output) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("EffectCraft output pipe is unavailable".into());
        };
        let (tx, replies) = mpsc::sync_channel(1);
        let (requests, incoming) = mpsc::sync_channel::<Vec<u8>>(1);
        if let Err(e) = std::thread::Builder::new().name("effectcraft-link-io".into()).spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut reader = BufReader::new(output);
                while let Ok(line) = incoming.recv() {
                    let reply = input.write_all(&line).and_then(|()| input.flush()).map_err(|e| e.to_string()).and_then(|()| read_reply(&mut reader));
                    let failed = reply.is_err();
                    if tx.send(reply).is_err() || failed {
                        break;
                    }
                }
            }));
            if result.is_err() {
                log::error!("EffectCraft response reader stopped unexpectedly");
            }
        }) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Could not start composition reader: {e}"));
        }
        Ok(Self { child, requests, replies, failed: false, _permit: permit })
    }

    fn request(&mut self, p: Value) -> Reply {
        self.request_with_deadline(p, Duration::from_secs(120))
    }

    fn request_with_deadline(&mut self, p: Value, deadline: Duration) -> Reply {
        if self.failed {
            return Err("EffectCraft renderer stopped; refresh the linked composition to reconnect".into());
        }
        let mut line = serde_json::to_vec(&p).map_err(|e| e.to_string())?;
        if line.len() > 65_535 {
            return Err("Composition request is too large".into());
        }
        line.push(b'\n');
        // Sending never waits for the child to read. The supervisor's deadline includes both
        // pipe writes and reads, and killing/reaping the child unblocks stalled OS pipe I/O.
        let reply = self
            .requests
            .try_send(line)
            .map_err(|_| "EffectCraft request queue is unavailable".to_string())
            .and_then(|()| {
                self.replies.recv_timeout(deadline).map_err(|_| "EffectCraft renderer timed out; simplify the composition or refresh to retry".to_string())
            })
            .and_then(|r| r);
        if reply.is_err() {
            self.failed = true;
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let (meta, data) = reply?;
        if let Some(error) = meta["error"].as_str() {
            return Err(error.to_string());
        }
        Ok((meta, data))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Opened {
    pub catalog: Catalog,
    worker: Arc<Mutex<Worker>>,
    // A project may expose thousands of compositions. Sharing one byte budget keeps that
    // from multiplying a 64 MiB cache for every linked composition or duplicate source item.
    frames: Arc<FrameCache<(u64, i64, u32)>>,
}

impl Opened {
    pub fn open(path: &str, renderer: &Path) -> Result<Self, String> {
        let mut worker = Worker::start(renderer)?;
        let (meta, _) = worker.request(json!({"method": "open", "path": path}))?;
        let catalog: Catalog = serde_json::from_value(meta).map_err(|e| format!("Invalid composition list: {e}"))?;
        if catalog.compositions.len() > 4096 {
            return Err("EffectCraft project contains too many compositions".into());
        }
        Ok(Self { catalog, worker: Arc::new(Mutex::new(worker)), frames: Arc::new(FrameCache::new(64 * 1024 * 1024)) })
    }

    pub fn source(&self, id: u64) -> Result<SharedSource, String> {
        let comp = self.catalog.compositions.iter().find(|c| c.id == id).ok_or("The linked composition was deleted; relink to an existing composition")?;
        if !comp.missing.is_empty() {
            return Err(format!(
                "EffectCraft has missing footage: {}. Open the source project and relink it before refreshing",
                comp.missing.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        Ok(Arc::new(Source { worker: self.worker.clone(), comp: comp.clone(), info: comp.media_info()?, frames: self.frames.clone() }))
    }
}

struct Source {
    worker: Arc<Mutex<Worker>>,
    comp: Composition,
    info: MediaInfo,
    frames: Arc<FrameCache<(u64, i64, u32)>>,
}

impl MediaSource for Source {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn video_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        if !req.scale.is_finite() || req.scale <= 0.0 {
            return Err(MediaError::Decode("Invalid linked frame scale".into()));
        }
        let side = ((self.comp.width.max(self.comp.height) as f32 * req.scale.min(1.0)).ceil() as u32).max(1);
        let time = self.comp.frame_rate.snap(req.time);
        self.frames.get_or_insert_with((self.comp.id, time.0, side), || {
            let (meta, bytes) = self
                .worker
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .request(json!({"method":"frame", "composition":self.comp.id, "time": time.0, "maxSide": side}))
                .map_err(MediaError::Decode)?;
            let dimension =
                |key| meta[key].as_u64().filter(|n| *n > 0 && *n <= 16_384).ok_or_else(|| MediaError::Decode("Invalid linked frame dimensions".into()));
            let (w, h) = (dimension("width")?, dimension("height")?);
            if w * h * 4 != bytes.len() as u64 || meta["encoding"] != "srgb-rgba8-straight" {
                return Err(MediaError::Decode("Invalid linked frame payload".into()));
            }
            let mut frame = VideoFrame::rgba8(w as u32, h as u32, bytes).with_pts(time);
            frame.par = self.info.video.as_ref().map(|v| v.par).unwrap_or((1, 1));
            Ok(Arc::new(frame))
        })
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        filmcraft_media::validate_audio_buffer_shape(2, frames)?;
        if !(8000..=192_000).contains(&rate) {
            return Err(MediaError::Decode("Linked audio supports 8000..192000 Hz".into()));
        }
        if !self.comp.audio {
            return Ok(AudioBuffer::silence(rate, 2, frames));
        }
        // Keep requests small and bounded; this is called by FilmCraft's audio producer, never
        // by the device callback. The same private project snapshot serves picture and sound.
        let mut out = AudioBuffer::silence(rate, 2, frames);
        for offset in (0..frames).step_by(8192) {
            let n = (frames - offset).min(8192);
            let sample = start.saturating_add(offset as i64);
            let time = Tick::from_units(sample, i64::from(rate));
            let (meta, bytes) = self
                .worker
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .request(json!({"method":"audio", "composition":self.comp.id, "time": time.0, "frames":n, "rate":rate}))
                .map_err(MediaError::Decode)?;
            if bytes.len() != n * 8 || meta["encoding"] != "f32le-interleaved" || meta["channels"] != 2 || meta["rate"] != rate || meta["frames"] != n {
                return Err(MediaError::Decode("Invalid linked audio payload".into()));
            }
            for (i, pair) in bytes.as_chunks::<8>().0.iter().enumerate() {
                for (channel, word) in pair.as_chunks::<4>().0.iter().enumerate() {
                    let sample = f32::from_le_bytes(*word);
                    if let Some(dst) = out.channels.get_mut(channel).and_then(|c| c.get_mut(offset + i)) {
                        *dst = if sample.is_finite() { sample } else { 0.0 };
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_untrusted_protocol_payloads() {
        for len in [0_u32, 1_048_577, u32::MAX] {
            assert!(read_reply(&mut std::io::Cursor::new(len.to_le_bytes())).is_err());
        }
        let meta = serde_json::to_vec(&json!({"protocol":1,"bytes":u64::MAX})).unwrap();
        let mut data = (meta.len() as u32).to_le_bytes().to_vec();
        data.extend(meta);
        assert!(read_reply(&mut std::io::Cursor::new(data)).is_err());
    }

    #[test]
    fn deadline_includes_a_child_that_never_reads_its_input() {
        // A test harness prints a banner, causing an immediate protocol error. This isolated
        // child emits nothing, so the actual stalled-I/O deadline must fire.
        let mut command = if cfg!(windows) { Command::new("powershell.exe") } else { Command::new("sh") };
        if cfg!(windows) {
            command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 10"]);
        } else {
            command.args(["-c", "sleep 10"]);
        }
        let mut worker = Worker::start_command(command).unwrap();
        let before = web_time::Instant::now();
        let result = worker.request_with_deadline(json!({"method":"open","path":"x".repeat(65500)}), Duration::from_millis(150));
        assert!(result.unwrap_err().contains("timed out"));
        assert!(before.elapsed() < Duration::from_secs(5));
        assert!(worker.child.try_wait().unwrap().is_some(), "a stalled child must be reaped");
    }
}
