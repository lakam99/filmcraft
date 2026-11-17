//! Source-byte broker: network credentials and virtual files stay in the host. The child owns
//! its decoders but reads through bounded ranges of the host's stable random-access readers.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use filmcraft_media::{ByteReader, SharedReader};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Request, Response, protocol};
use crate::{FsServices, Services};

#[derive(Serialize, Deserialize)]
pub(super) enum IoRequest {
    Reset,
    Open { path: String },
    Read { reader: u64, offset: u64, length: usize },
    Whole { path: String },
    List { path: String },
}

pub(super) type Reply = (u64, std::result::Result<Value, String>, Vec<u8>);
pub(super) type Output = Arc<Mutex<io::Stdout>>;

pub(super) struct Host {
    services: Arc<dyn Services>,
    readers: HashMap<u64, SharedReader>,
    paths: HashMap<String, u64>,
    next: u64,
}

impl Host {
    pub(super) fn new(services: Arc<dyn Services>) -> Self {
        Self { services, readers: HashMap::new(), paths: HashMap::new(), next: 0 }
    }

    pub(super) fn perform(&mut self, request: IoRequest) -> std::result::Result<(Value, Vec<u8>), String> {
        match request {
            IoRequest::Reset => {
                self.readers.clear();
                self.paths.clear();
                Ok((Value::Null, Vec::new()))
            }
            IoRequest::Open { path } => {
                if let Some(id) = self.paths.get(&path)
                    && let Some(reader) = self.readers.get(id)
                {
                    return Ok((json!({"reader":id, "length":reader.len()}), Vec::new()));
                }
                let reader = match self.services.reader(&path) {
                    Some(reader) => reader.map_err(|e| e.to_string())?,
                    None => {
                        if self.services.file_size(&path).map_err(|e| e.to_string())? > crate::media_pool::PROBE_WHOLE_FILE_MAX {
                            return Err("host cannot stream this source and its whole-file read exceeds 64 MiB".into());
                        }
                        Arc::new(filmcraft_media::reader::MemReader(self.services.read_file(&path).map_err(|e| e.to_string())?.into())) as SharedReader
                    }
                };
                // A decoder may retain more than 128 reader identities; evicting a live one
                // would turn working sources offline. Bound identities by rejecting new opens.
                if self.readers.len() >= 4_096 {
                    return Err("backend source reader limit reached".into());
                }
                self.next = self.next.checked_add(1).ok_or("backend reader identity limit reached")?;
                let length = reader.len();
                self.readers.insert(self.next, reader);
                self.paths.insert(path, self.next);
                Ok((json!({"reader":self.next, "length":length}), Vec::new()))
            }
            IoRequest::Read { reader, offset, length } => {
                if length > protocol::RANGE_BYTES {
                    return Err("backend source range exceeds 1 MiB".into());
                }
                let source = self.readers.get(&reader).ok_or("backend source reader no longer exists")?;
                if offset.checked_add(length as u64).is_none_or(|end| end > source.len()) {
                    return Err("backend source read exceeds the file length".into());
                }
                let mut data = vec![0; length];
                source.read_at(offset, &mut data).map_err(|e| e.to_string())?;
                Ok((Value::Null, data))
            }
            IoRequest::Whole { path } => {
                if self.services.file_size(&path).map_err(|e| e.to_string())? > protocol::MAX_DATA as u64 {
                    return Err("backend whole-file read exceeds 64 MiB; use a streamable source format".into());
                }
                let data = self.services.read_file(&path).map_err(|e| e.to_string())?;
                if data.len() > protocol::MAX_DATA {
                    return Err("source grew beyond the backend whole-file budget".into());
                }
                Ok((Value::Null, data))
            }
            IoRequest::List { path } => {
                let names = self.services.list_dir(&path).ok_or("host cannot list this source directory")?.map_err(|e| e.to_string())?;
                if names.len() > 100_000 || names.iter().map(String::len).fold(0usize, usize::saturating_add) > 8 * 1024 * 1024 {
                    return Err("source directory exceeds the backend listing budget".into());
                }
                Ok((json!(names), Vec::new()))
            }
        }
    }
}

pub(super) struct Broker {
    pub(super) output: Output,
    pub(super) replies: Mutex<mpsc::Receiver<Reply>>,
    next: AtomicU64,
}

impl Broker {
    pub(super) fn new(output: Output, replies: mpsc::Receiver<Reply>) -> Self {
        Self { output, replies: Mutex::new(replies), next: AtomicU64::new(1) }
    }

    pub(super) fn call(&self, request: IoRequest) -> io::Result<(Value, Vec<u8>)> {
        let replies = self.replies.lock().unwrap_or_else(PoisonError::into_inner);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        protocol::send(&mut *self.output.lock().unwrap_or_else(PoisonError::into_inner), &Response::Io { id, request }, &[])?;
        let (reply_id, result, data) =
            replies.recv_timeout(Duration::from_secs(60)).map_err(|_| io::Error::other("host source service stopped or timed out"))?;
        if id != reply_id {
            return Err(io::Error::other("backend source response identity mismatch"));
        }
        Ok((result.map_err(io::Error::other)?, data))
    }
}

pub(super) struct WorkerServices(pub(super) Arc<Broker>);

struct Reader {
    broker: Arc<Broker>,
    id: u64,
    length: u64,
}

impl ByteReader for Reader {
    fn len(&self) -> u64 {
        self.length
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        if offset.checked_add(buffer.len() as u64).is_none_or(|end| end > self.length) {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "backend source read past end"));
        }
        for (n, chunk) in buffer.chunks_mut(protocol::RANGE_BYTES).enumerate() {
            let offset = offset.checked_add((n * protocol::RANGE_BYTES) as u64).ok_or_else(|| io::Error::other("backend source offset overflow"))?;
            let (_, bytes) = self.broker.call(IoRequest::Read { reader: self.id, offset, length: chunk.len() })?;
            if bytes.len() != chunk.len() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "backend source returned a short read"));
            }
            chunk.copy_from_slice(&bytes);
        }
        Ok(())
    }
}

impl Services for WorkerServices {
    fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
        self.0.call(IoRequest::Whole { path: path.into() }).map(|(_, data)| data)
    }
    fn write_file(&self, path: &str, data: &[u8]) -> io::Result<()> {
        FsServices.write_file(path, data)
    }
    fn write_file_new(&self, path: &str, data: &[u8]) -> io::Result<()> {
        FsServices.write_file_new(path, data)
    }
    fn file_size(&self, path: &str) -> io::Result<u64> {
        let reader = self.reader(path).ok_or_else(|| io::Error::other("backend reader unavailable"))??;
        Ok(reader.len())
    }
    fn reader(&self, path: &str) -> Option<io::Result<SharedReader>> {
        Some((|| {
            let (meta, _) = self.0.call(IoRequest::Open { path: path.into() })?;
            let id = meta["reader"].as_u64().ok_or_else(|| io::Error::other("missing backend reader identity"))?;
            let length = meta["length"].as_u64().ok_or_else(|| io::Error::other("missing backend source length"))?;
            Ok(Arc::new(Reader { broker: self.0.clone(), id, length }) as SharedReader)
        })())
    }
    fn list_dir(&self, path: &str) -> Option<io::Result<Vec<String>>> {
        Some(self.0.call(IoRequest::List { path: path.into() }).and_then(|(meta, _)| serde_json::from_value(meta).map_err(io::Error::other)))
    }
    fn file_loader(&self) -> Option<filmcraft_media::sequence::FrameLoader> {
        let broker = self.0.clone();
        Some(Arc::new(move |path| WorkerServices(broker.clone()).read_file(path)))
    }
}

pub(super) fn reply(id: u64, result: std::result::Result<(Value, Vec<u8>), String>) -> (Request, Vec<u8>) {
    let (result, data) = match result {
        Ok((meta, data)) => (Ok(meta), data),
        Err(error) => (Err(error), Vec::new()),
    };
    (Request::IoReply { id, result }, data)
}
