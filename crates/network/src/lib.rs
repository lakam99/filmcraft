//! A native filesystem adapter for the separate RedWeb LAN relay. The engine still sees local
//! paths; the adapter resolves its private mount to portable host paths and reads media by range.
//! Credentials live only in memory and never occur in a project, URL, cache key or error message.
#![cfg(not(target_arch = "wasm32"))]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use filmcraft_engine::Services;
use filmcraft_media::reader::{ByteReader, SharedReader};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

const CHUNK: usize = 1024 * 1024;
const CACHE_CHUNKS: usize = 8;
const MAX_DOCUMENT: u64 = 32 * 1024 * 1024;
const MAX_WHOLE_FILE: u64 = 512 * 1024 * 1024;

fn error(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
fn http_error(e: ureq::Error) -> io::Error {
    // ureq errors may include the request URL; do not expose credentials or arbitrary server text.
    match e {
        ureq::Error::StatusCode(401 | 403) => error("Relay authentication or file access was refused"),
        ureq::Error::StatusCode(409) => {
            error("The project or media changed on the host. Your edits are still open; save a local recovery copy before reopening.")
        }
        ureq::Error::StatusCode(404) => io::Error::new(io::ErrorKind::NotFound, "File is not available on the host"),
        ureq::Error::StatusCode(507) => error("The host disk is full. Free space on the storage computer and save again; your edits are still open."),
        ureq::Error::StatusCode(413) => error("The project or upload exceeds the relay size limit; your edits are still open."),
        _ => error("Cannot reach the RedWeb relay or the relay request failed; check the host and connection"),
    }
}

#[derive(Deserialize)]
struct Snapshot {
    protocol: u32,
    project: String,
    version: String,
    document: Value,
}

/// Shared by the editor, decoder workers and export jobs. Never derive Debug: it holds a secret.
pub struct Relay {
    agent: ureq::Agent,
    url: url::Url,
    token: String,
    mount: PathBuf,
    project: String,
    version: Mutex<String>,
    uploads: Mutex<HashMap<PathBuf, (u64, std::time::SystemTime, String)>>,
    save_lock: Mutex<()>,
    fallback: Arc<dyn Services>,
}

impl Relay {
    /// Contact an explicitly selected host. HTTP is supported for isolated LANs; HTTPS uses the
    /// operating system's certificate verifier. Redirects are disabled so a token cannot move hosts.
    pub fn connect(address: &str, token: String, cache: &Path, fallback: Arc<dyn Services>) -> io::Result<Arc<Self>> {
        let url = url::Url::parse(address).map_err(|_| error("Enter an http:// or https:// relay address"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(error("Use a relay origin such as http://192.168.1.10:9877, without credentials or a path"));
        }
        if !(32..=256).contains(&token.len()) || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(error("The access token must contain 32–256 printable non-space ASCII characters"));
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::Rustls)
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .unversioned_rustls_crypto_provider(Arc::new(rustls_rustcrypto::provider()))
                    .build(),
            )
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        let endpoint = url.join("v1/project").map_err(|_| error("Invalid relay address"))?;
        let mut response = agent.get(endpoint.as_str()).header("Authorization", format!("Bearer {token}")).call().map_err(http_error)?;
        let snapshot: Snapshot = serde_json::from_slice(&bounded(response.body_mut().as_reader(), MAX_DOCUMENT)?)
            .map_err(|_| error("The host returned an invalid project response"))?;
        if snapshot.protocol != 1 {
            return Err(error("The host uses an unsupported relay protocol"));
        }
        let project = key(Path::new(&snapshot.project))?;
        if !project.to_ascii_lowercase().ends_with(".fcproj") {
            return Err(error("The host did not name a FilmCraft project"));
        }
        let id = format!("{:x}", Sha256::digest(format!("{}\n{project}", url.as_str()).as_bytes()));
        let mount = std::path::absolute(cache.join(id)).map_err(|_| error("Cannot prepare the local network-project cache"))?;
        std::fs::create_dir_all(&mount)?;
        let relay = Arc::new(Self {
            agent,
            url,
            token,
            mount,
            project,
            version: Mutex::new(snapshot.version),
            uploads: Default::default(),
            save_lock: Default::default(),
            fallback,
        });
        // Store a portable working snapshot, never credentials. Engine open then resolves media
        // relative to this mount, regardless of the storage machine's operating system.
        let bytes = relay.local_document(snapshot.document)?;
        let project_path = relay.project_path();
        let parent = project_path.parent().ok_or_else(|| error("Invalid working project path"))?;
        std::fs::create_dir_all(parent)?;
        filmcraft_engine::FsServices.write_file(&relay.project_path().to_string_lossy(), &bytes)?;
        Ok(relay)
    }

    pub fn project_path(&self) -> PathBuf {
        self.mount.join(&self.project)
    }
    pub fn mount(&self) -> &Path {
        &self.mount
    }

    fn endpoint(&self, resource: &str, file: Option<&str>) -> io::Result<String> {
        let mut url = self.url.join(resource).map_err(|_| error("Invalid relay endpoint"))?;
        if let Some(file) = file {
            url.query_pairs_mut().append_pair("path", file);
        }
        Ok(url.into())
    }
    fn remote_key(&self, file: &str) -> Option<io::Result<String>> {
        Path::new(file).strip_prefix(&self.mount).ok().map(key)
    }
    fn local_document(&self, mut document: Value) -> io::Result<Vec<u8>> {
        // Check every reference before engine open can touch it. Relative .. is allowed only
        // while resolving a nested project inside the explicitly shared root.
        for reference in file_refs(&mut document)? {
            let path = reference.get("path").and_then(Value::as_str).ok_or_else(|| error("Invalid remote media reference"))?;
            let relative = resolve_key(Path::new(&self.project).parent().unwrap_or(Path::new("")), path)?;
            reference["path"] = Value::String(self.mount.join(relative).to_string_lossy().into_owned());
        }
        let project = project_mut(&mut document)?;
        if let Some(settings) = project.get_mut("settings").and_then(Value::as_object_mut) {
            settings.insert("scratch".into(), serde_json::json!({}));
        }
        serde_json::to_vec(&document).map_err(|_| error("Cannot encode the working project"))
    }
    fn metadata(&self, file: &str) -> io::Result<(u64, String)> {
        let response =
            self.agent.head(self.endpoint("v1/file", Some(file))?).header("Authorization", format!("Bearer {}", self.token)).call().map_err(http_error)?;
        let length = response
            .headers()
            .get("Content-Length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| error("Missing relay file length"))?;
        let etag = response.headers().get("ETag").and_then(|v| v.to_str().ok()).ok_or_else(|| error("Missing relay file version"))?.to_string();
        Ok((length, etag))
    }
    fn range(&self, file: &str, version: &str, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let end = offset.checked_add(length as u64).and_then(|v| v.checked_sub(1)).ok_or_else(|| error("Invalid byte range"))?;
        let mut response = self
            .agent
            .get(self.endpoint("v1/file", Some(file))?)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("If-Match", version)
            .header("Range", format!("bytes={offset}-{end}"))
            .call()
            .map_err(http_error)?;
        let expected = format!("bytes {offset}-{end}/");
        if response.status().as_u16() != 206 || !response.headers().get("Content-Range").and_then(|h| h.to_str().ok()).is_some_and(|h| h.starts_with(&expected))
        {
            return Err(error("The relay did not honor the requested media range"));
        }
        let bytes = bounded(response.body_mut().as_reader(), length as u64)?;
        if bytes.len() != length {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Incomplete relay media range"));
        }
        Ok(bytes)
    }
    fn upload(&self, file: &Path) -> io::Result<String> {
        let stat = file.metadata()?;
        let modified = stat.modified()?;
        if !stat.is_file() {
            return Err(error("Only regular media files can be uploaded"));
        }
        if let Some((size, time, stored)) = self.uploads.lock().unwrap_or_else(PoisonError::into_inner).get(file)
            && *size == stat.len()
            && *time == modified
        {
            return Ok(stored.clone());
        }
        let name = file.file_name().and_then(|s| s.to_str()).ok_or_else(|| error("Media has an invalid file name"))?;
        let mut url = self.url.join("v1/upload").map_err(|_| error("Invalid upload endpoint"))?;
        url.query_pairs_mut().append_pair("name", name);
        let mut input = std::fs::File::open(file)?;
        let mut response = self
            .agent
            .put(url.as_str())
            .config()
            .timeout_global(Some(Duration::from_secs(3600)))
            .build()
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Length", stat.len().to_string())
            .header("Content-Type", "application/octet-stream")
            .send(ureq::SendBody::from_reader(&mut input))
            .map_err(http_error)?;
        let result: Value = serde_json::from_slice(&bounded(response.body_mut().as_reader(), 16384)?).map_err(|_| error("Invalid upload result"))?;
        let after = file.metadata()?;
        if after.len() != stat.len() || after.modified()? != modified {
            return Err(error("Media changed during upload; wait for the file to finish being written and save again"));
        }
        let stored = key(Path::new(result["path"].as_str().ok_or_else(|| error("Upload did not return a path"))?))?;
        self.uploads.lock().unwrap_or_else(PoisonError::into_inner).insert(file.to_path_buf(), (stat.len(), modified, stored.clone()));
        Ok(stored)
    }
    pub fn save_project(&self, bytes: &[u8]) -> io::Result<()> {
        let _save = self.save_lock.lock().unwrap_or_else(PoisonError::into_inner);
        if bytes.len() as u64 > MAX_DOCUMENT {
            return Err(error("Project exceeds the 32 MiB relay limit"));
        }
        let mut document: Value = serde_json::from_slice(bytes).map_err(|_| error("Cannot send an invalid project"))?;
        if let Some(items) = project_mut(&mut document)?.get("items").and_then(Value::as_object) {
            for item in items.values() {
                if let Some(media) = item.get("kind").and_then(|k| k.get("Media"))
                    && media.pointer("/info/kind").and_then(Value::as_str) == Some("ImageSequence")
                    && media.pointer("/media/File/path").and_then(Value::as_str).is_some_and(|p| Path::new(p).is_file())
                {
                    return Err(error("Copy newly imported image-sequence frames to the host and import them from the Network Project browser before saving"));
                }
            }
        }
        let project_dir = Path::new(&self.project).parent().unwrap_or(Path::new(""));
        for reference in file_refs(&mut document)? {
            let source = reference.get("path").and_then(Value::as_str).ok_or_else(|| error("Invalid media reference"))?;
            let local = Path::new(source);
            let target = if local.is_file() {
                self.upload(local)?
            } else if let Some(remote) = self.remote_key(source) {
                remote?
            } else {
                return Err(error("Media is unavailable on this computer and is not part of the host project; reconnect or relink it before saving"));
            };
            reference["path"] = Value::String(relative_key(project_dir, Path::new(&target))?);
        }
        let body = serde_json::to_vec(&document).map_err(|_| error("Cannot encode the remote project"))?;
        let version = self.version.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let mut response = self
            .agent
            .put(self.endpoint("v1/project", None)?)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("If-Match", &version)
            .header("Content-Type", "application/octet-stream")
            .send(body.as_slice())
            .map_err(http_error)?;
        let result: Value = serde_json::from_slice(&bounded(response.body_mut().as_reader(), 16384)?)
            .map_err(|_| error("Invalid save response; reopen to confirm the host version before retrying"))?;
        let next = result["version"].as_str().ok_or_else(|| error("Missing host project version"))?.to_string();
        *self.version.lock().unwrap_or_else(PoisonError::into_inner) = next;
        // Local recovery has the last accepted working snapshot, independent of the connection.
        self.fallback.write_file(&self.project_path().to_string_lossy(), bytes)
    }
}

fn bounded(reader: impl Read, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(error("Relay response exceeds the size limit"));
    }
    Ok(bytes)
}
fn project_mut(doc: &mut Value) -> io::Result<&mut Value> {
    if doc.get("project").is_some() {
        return doc.get_mut("project").ok_or_else(|| error("Invalid project"));
    }
    Ok(doc)
}
fn file_refs(doc: &mut Value) -> io::Result<Vec<&mut Value>> {
    let items = project_mut(doc)?.get_mut("items").and_then(Value::as_object_mut).ok_or_else(|| error("Invalid project items"))?;
    Ok(items
        .values_mut()
        .filter_map(|item| item.get_mut("kind")?.get_mut("Media")?.as_object_mut())
        .flat_map(|media| media.iter_mut().filter(|(name, _)| matches!(name.as_str(), "media" | "proxy")).filter_map(|(_, r)| r.get_mut("File")))
        .collect())
}
fn key(path: &Path) -> io::Result<String> {
    let mut parts = Vec::new();
    for part in path.components() {
        let Component::Normal(part) = part else {
            return Err(error("Invalid relative relay path"));
        };
        let part = part.to_str().ok_or_else(|| error("Invalid relay file name"))?;
        if part.contains(['\\', ':', '\0']) || part.chars().any(char::is_control) {
            return Err(error("Invalid relay file name"));
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return Err(error("Empty relay path"));
    }
    Ok(parts.join("/"))
}
fn resolve_key(base: &Path, stored: &str) -> io::Result<String> {
    if stored.contains(['\\', ':', '\0']) || stored.starts_with('/') {
        return Err(error("The host returned a nonportable media path"));
    }
    let mut parts: Vec<&str> = base.iter().filter_map(|p| p.to_str()).collect();
    for part in stored.split('/') {
        match part {
            ".." => {
                if parts.pop().is_none() {
                    return Err(error("Media path escapes the shared root"));
                }
            }
            "" | "." => {}
            p => parts.push(p),
        }
    }
    key(Path::new(&parts.join("/")))
}
fn relative_key(base: &Path, target: &Path) -> io::Result<String> {
    let base: Vec<_> = base.components().collect();
    let target_key = key(target)?;
    let target: Vec<_> = Path::new(&target_key).components().collect();
    let common = base.iter().zip(&target).take_while(|(a, b)| a == b).count();
    Ok(std::iter::repeat_n("..".to_string(), base.len() - common)
        .chain(target.iter().skip(common).map(|p| p.as_os_str().to_string_lossy().into_owned()))
        .collect::<Vec<_>>()
        .join("/"))
}

/// The filesystem adapter holds an Arc so each decoder's reader can use the same connection pool.
pub struct RelayServices(pub Arc<Relay>);
impl Services for RelayServices {
    fn requires_explicit_save(&self, path: &str) -> bool {
        Path::new(path) == self.0.project_path()
    }
    fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
        if Path::new(path).is_file() {
            return self.0.fallback.read_file(path);
        }
        if let Some(remote) = self.0.remote_key(path) {
            let remote = remote?;
            let (length, version) = self.0.metadata(&remote)?;
            if length > MAX_WHOLE_FILE {
                return Err(error("This format needs a whole-file read larger than the relay limit; use a proxy or a streamable format"));
            }
            let mut data = Vec::with_capacity(length as usize);
            while (data.len() as u64) < length {
                let n = CHUNK.min((length - data.len() as u64) as usize);
                data.extend(self.0.range(&remote, &version, data.len() as u64, n)?);
            }
            return Ok(data);
        }
        self.0.fallback.read_file(path)
    }
    fn write_file(&self, path: &str, data: &[u8]) -> io::Result<()> {
        if Path::new(path) == self.0.project_path() { self.0.save_project(data) } else { self.0.fallback.write_file(path, data) }
    }
    fn write_file_new(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.0.fallback.write_file_new(path, data)
    }
    fn file_size(&self, path: &str) -> io::Result<u64> {
        if Path::new(path).is_file() {
            return self.0.fallback.file_size(path);
        }
        match self.0.remote_key(path) {
            Some(remote) => self.0.metadata(&remote?).map(|m| m.0),
            None => self.0.fallback.file_size(path),
        }
    }
    fn read_range(&self, path: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        // Container-derived lengths must not turn a streaming read into an unbounded allocation.
        if len > MAX_WHOLE_FILE as usize {
            return Err(error("Requested range exceeds the 512 MiB network-project read limit"));
        }
        if let Some(reader) = self.reader(path) {
            let reader = reader?;
            let n = len.min(reader.len().saturating_sub(offset) as usize);
            let mut bytes = vec![0; n];
            reader.read_at(offset, &mut bytes)?;
            return Ok(bytes);
        }
        self.0.fallback.read_range(path, offset, len)
    }
    fn reader(&self, path: &str) -> Option<io::Result<SharedReader>> {
        if Path::new(path).is_file() {
            return self.0.fallback.reader(path);
        }
        self.0
            .remote_key(path)
            .map(|remote| {
                let remote = remote?;
                let (length, version) = self.0.metadata(&remote)?;
                Ok(Arc::new(RemoteReader { relay: self.0.clone(), file: remote, version, length, cache: Default::default() }) as SharedReader)
            })
            .or_else(|| self.0.fallback.reader(path))
    }
    fn list_dir(&self, path: &str) -> Option<io::Result<Vec<String>>> {
        self.list_entries(path).map(|r| r.map(|v| v.into_iter().map(|e| e.name).collect()))
    }
    fn list_entries(&self, path: &str) -> Option<io::Result<Vec<filmcraft_engine::media_browser::DirEntry>>> {
        let key = if Path::new(path) == self.0.mount { Some(Ok(String::new())) } else { self.0.remote_key(path) };
        match key {
            Some(remote) => Some((|| {
                let mut response = self
                    .0
                    .agent
                    .get(self.0.endpoint("v1/list", Some(&remote?))?)
                    .header("Authorization", format!("Bearer {}", self.0.token))
                    .call()
                    .map_err(http_error)?;
                serde_json::from_slice(&bounded(response.body_mut().as_reader(), MAX_DOCUMENT)?).map_err(|_| error("Invalid remote directory listing"))
            })()),
            None => self.0.fallback.list_entries(path),
        }
    }
    fn volumes(&self) -> Vec<filmcraft_engine::media_browser::Volume> {
        let mut volumes = self.0.fallback.volumes();
        volumes.push(filmcraft_engine::media_browser::Volume {
            name: "Network Project".into(),
            path: self.0.mount.to_string_lossy().into_owned(),
            kind: filmcraft_engine::media_browser::VolumeKind::Network,
        });
        volumes
    }
    fn home_dir(&self) -> Option<String> {
        self.0.fallback.home_dir()
    }
    fn file_loader(&self) -> Option<filmcraft_media::sequence::FrameLoader> {
        let relay = self.0.clone();
        Some(Arc::new(move |path| RelayServices(relay.clone()).read_file(path)))
    }
}

struct RemoteReader {
    relay: Arc<Relay>,
    file: String,
    version: String,
    length: u64,
    cache: Mutex<VecDeque<(u64, Arc<Vec<u8>>)>>,
}
impl ByteReader for RemoteReader {
    fn len(&self) -> u64 {
        self.length
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if offset.checked_add(buf.len() as u64).is_none_or(|end| end > self.length) {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let block = at / CHUNK as u64 * CHUNK as u64;
            let cached = self.cache.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(start, _)| *start == block).map(|(_, b)| b.clone());
            let bytes = match cached {
                Some(bytes) => bytes,
                None => {
                    let bytes = Arc::new(self.relay.range(&self.file, &self.version, block, CHUNK.min((self.length - block) as usize))?);
                    let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
                    if cache.len() >= CACHE_CHUNKS {
                        cache.pop_front();
                    }
                    cache.push_back((block, bytes.clone()));
                    bytes
                }
            };
            let start = (at - block) as usize;
            let n = (buf.len() - done).min(bytes.len().saturating_sub(start));
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            buf.get_mut(done..done + n)
                .ok_or_else(|| error("Invalid media destination"))?
                .copy_from_slice(bytes.get(start..start + n).ok_or_else(|| error("Invalid media range"))?);
            done += n;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
