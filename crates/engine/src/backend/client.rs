use std::io::{BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, mpsc};
use std::time::Duration;

use web_time::Instant;

use super::frames;
use super::{Completion, Request, Response, State, WORKER_ARGUMENT, protocol, services};
use crate::Services;

const MAX_SOURCE_THREADS: usize = 8;
static SOURCE_THREADS: AtomicUsize = AtomicUsize::new(0);

struct SourcePermit;
impl SourcePermit {
    fn acquire() -> Result<Self, String> {
        // Keep acquisition nonblocking even under contention, and support the workspace MSRV
        // without relying on newer atomic convenience methods.
        for _ in 0..64 {
            let count = SOURCE_THREADS.load(Ordering::Acquire);
            if count >= MAX_SOURCE_THREADS {
                break;
            }
            if SOURCE_THREADS.compare_exchange(count, count + 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Ok(Self);
            }
        }
        Err("source reads are still busy or stopping; wait for slow media reads before starting another render".into())
    }
}
impl Drop for SourcePermit {
    fn drop(&mut self) {
        SOURCE_THREADS.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) struct Client {
    child: Child,
    requests: mpsc::SyncSender<Vec<u8>>,
    replies: mpsc::Receiver<Result<(Response, Vec<u8>), String>>,
}

impl Client {
    pub(super) fn start(executable: &Path, services: Arc<dyn Services>, state: Arc<State>) -> Result<Self, String> {
        Self::start_mode(executable, services, state, WORKER_ARGUMENT)
    }

    pub(super) fn start_frames(executable: &Path, services: Arc<dyn Services>) -> Result<Self, String> {
        Self::start_mode(executable, services, Arc::default(), super::FRAME_WORKER_ARGUMENT)
    }

    fn start_mode(executable: &Path, services: Arc<dyn Services>, state: Arc<State>, argument: &str) -> Result<Self, String> {
        // Cancellation can kill a renderer while a host/network read is still blocked. Hold
        // this permit until its I/O thread really exits, rather than when its job is cancelled.
        // Repeated scrubbing/cancellation must not create an unbounded number of stalled reads.
        let permit = SourcePermit::acquire()?;
        let mut command = Command::new(executable);
        for key in ["FILMCRAFT_CONTROL_TOKEN", "FILMCRAFT_CONTROL_PORT", "OPENAI_API_KEY", "OPENAI_API_TOKEN"] {
            command.env_remove(key);
        }
        command.arg(argument).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        // Leave CPU capacity for input, audio and presentation, including rayon codec pools.
        let threads = std::thread::available_parallelism().map(|n| n.get().saturating_sub(2).clamp(1, 8)).unwrap_or(2);
        command.env("RAYON_NUM_THREADS", if argument == super::FRAME_WORKER_ARGUMENT { 2 } else { threads }.to_string());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().map_err(|e| format!("cannot start FilmCraft backend: {e}"))?;
        state.pid.store(child.id(), Ordering::Release);
        let (Some(mut input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("backend pipes are unavailable".into());
        };
        let (requests, incoming) = mpsc::sync_channel::<Vec<u8>>(2);
        let (replies_tx, replies) = mpsc::sync_channel(32);
        let client = Self { child, requests, replies };
        let errors = replies_tx.clone();
        std::thread::Builder::new()
            .name("filmcraft-backend-write".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    while let Ok(packet) = incoming.recv() {
                        input.write_all(&packet).and_then(|()| input.flush()).map_err(|e| e.to_string())?;
                    }
                    Ok::<_, String>(())
                }));
                if !matches!(result, Ok(Ok(()))) {
                    let _ = errors.send(Err("backend input pipe stopped".into()));
                }
            })
            .map_err(|e| format!("cannot start backend writer: {e}"))?;
        let send = client.requests.clone();
        std::thread::Builder::new()
            .name("filmcraft-backend-read".into())
            .spawn(move || {
                let _permit = permit;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut host = services::Host::new(services);
                    let mut reader = BufReader::new(output);
                    loop {
                        let (response, data) = protocol::read::<Response>(&mut reader).map_err(|e| e.to_string())?;
                        if !data.is_empty() && !matches!(response, Response::Frame { .. }) {
                            return Err("unexpected backend response data".to_string());
                        }
                        if let Response::Io { id, request } = response {
                            let (message, data) = services::reply(id, host.perform(request));
                            let packet = protocol::encode(&message, &data).map_err(|e| e.to_string())?;
                            send.try_send(packet).map_err(|_| "backend service queue stopped".to_string())?;
                        } else {
                            if replies_tx.send(Ok((response, data))).is_err() {
                                return Ok(());
                            }
                        }
                    }
                }));
                let error = match result {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => error,
                    Err(_) => "internal error in backend source service".into(),
                };
                let _ = replies_tx.send(Err(error));
            })
            .map_err(|e| format!("cannot start backend reader: {e}"))?;
        Ok(client)
    }

    pub(super) fn request(&self, request: Request, data: &[u8]) -> Result<(), String> {
        let packet = protocol::encode(&request, data).map_err(|e| e.to_string())?;
        self.requests.try_send(packet).map_err(|_| "backend request queue is unavailable".into())
    }

    pub(super) fn finish(&mut self, progress: &Arc<filmcraft_export::Progress>) -> Result<Completion, String> {
        let started = Instant::now();
        let mut ready = false;
        let mut cancelling = None;
        let mut exited = None;
        loop {
            if progress.cancel.load(Ordering::Acquire) && cancelling.is_none() {
                cancelling = Some(Instant::now());
                let _ = self.request(Request::Cancel, &[]);
            }
            if cancelling.is_some_and(|at| at.elapsed() >= Duration::from_millis(500)) {
                return Err("cancelled".into());
            }
            match self.replies.recv_timeout(Duration::from_millis(25)) {
                Ok(Ok((Response::Ready { pid }, _))) => {
                    if pid != self.child.id() {
                        return Err("backend process identity mismatch".into());
                    }
                    ready = true;
                }
                Ok(Ok((Response::Progress { done, total, status }, _))) => {
                    progress.total.store(total.max(1), Ordering::Relaxed);
                    progress.done.store(done.min(total), Ordering::Relaxed);
                    *progress.status.lock().unwrap_or_else(PoisonError::into_inner) = status;
                }
                Ok(Ok((Response::Finished { result }, _))) => {
                    return if cancelling.is_some() { Err("cancelled".into()) } else { result };
                }
                Ok(Ok(_)) => return Err("unexpected backend response".into()),
                Ok(Err(error)) => {
                    return Err(if cancelling.is_some() { "cancelled".into() } else { format!("backend stopped: {error}; your project is unchanged") });
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("backend disconnected; your project is unchanged".into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if !ready && started.elapsed() > Duration::from_secs(15) {
                return Err("backend did not start; retry the job or reinstall the matching FilmCraft build".into());
            }
            if self.child.try_wait().map_err(|e| e.to_string())?.is_some() {
                let at = exited.get_or_insert_with(Instant::now);
                // Let the pipe reader drain a final completion before treating exit as failure.
                if at.elapsed() > Duration::from_millis(500) {
                    return Err("backend exited without a result; your project is unchanged and the job can be retried".into());
                }
            }
        }
    }

    pub(super) fn frame(&mut self, stop: &AtomicBool) -> Result<frames::Frame, String> {
        let start = Instant::now();
        loop {
            // A superseded frame is discarded by its coordinator after this reply is drained.
            // Killing its process would throw away decoder/GPU caches on every playback tick,
            // starving compressed-video playback before any frame could ever be displayed.
            if stop.load(Ordering::Acquire) {
                return Err("cancelled".into());
            }
            match self.replies.recv_timeout(Duration::from_millis(10)) {
                Ok(Ok((Response::Ready { pid }, _))) if pid == self.child.id() => {}
                Ok(Ok((Response::Frame { width, height, preview, loading, error }, data))) => {
                    if let Some(error) = error {
                        return Err(error);
                    }
                    if width == 0 || height == 0 || width.checked_mul(height).and_then(|n| n.checked_mul(4)) != Some(data.len()) {
                        return Err("invalid backend frame size".into());
                    }
                    return Ok(frames::Frame { worker_pid: self.child.id(), width, height, rgba: data, preview, loading });
                }
                Ok(Ok(_)) => return Err("unexpected preview backend response".into()),
                Ok(Err(error)) => return Err(error),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("preview backend disconnected".into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if start.elapsed() > Duration::from_secs(30) || self.child.try_wait().map_err(|e| e.to_string())?.is_some() {
                return Err("preview backend stopped or timed out; retrying with a fresh worker".into());
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Only supervisor threads own Client. Never wait for an OS process on the UI thread.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_io_keeps_its_slot_until_it_really_exits() {
        let permits: Vec<_> = (0..MAX_SOURCE_THREADS).map(|_| SourcePermit::acquire().unwrap()).collect();
        assert!(SourcePermit::acquire().is_err());
        drop(permits);
        let replacement = SourcePermit::acquire().unwrap();
        drop(replacement);
        assert_eq!(SOURCE_THREADS.load(Ordering::Acquire), 0);
    }
}
