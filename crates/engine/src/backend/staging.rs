//! Output ownership stays with the supervisor. A killed worker cannot replace the user's
//! previous document, or leave that document referring to incomplete new media.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{Completion, Task};

static NEXT: AtomicU64 = AtomicU64::new(1);

pub(super) struct Stage {
    root: PathBuf,
    destination: PathBuf,
    media: Option<PathBuf>,
    keep_backup: bool,
    published: bool,
}

fn fresh(parent: &Path, prefix: &str) -> Result<PathBuf, String> {
    for _ in 0..1_000 {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("{prefix}-{}-{id}", std::process::id()));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot prepare export folder {}: {e}", path.display())),
        }
    }
    Err("no unused export staging folder is available".into())
}

impl Stage {
    pub(super) fn prepare(task: &mut Task) -> Result<Option<Self>, String> {
        let (path, media_suffix) = match task {
            Task::Interchange { omf, params, .. } => {
                let audio = params["audio"].as_str().unwrap_or("embedded");
                let has_media = !matches!(audio, "embedded" | "embed" | "encapsulate") || params["mixdownVideo"].as_bool() == Some(true);
                (
                    params.get("path").and_then(serde_json::Value::as_str).ok_or("export needs a path")?,
                    has_media.then_some(if *omf { "Audio Files" } else { "Media" }),
                )
            }
            Task::Media { settings, .. } => (settings.path.as_str(), None),
            Task::Preview { .. } | Task::ColorGroup { .. } => return Ok(None),
        };
        let destination = std::path::absolute(path).map_err(|e| e.to_string())?;
        let name = destination.file_name().ok_or("export path needs a file name")?.to_owned();
        let parent = destination.parent().ok_or("export path needs a parent directory")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let root = fresh(parent, ".filmcraft-export")?;
        let mut stage = Self { root, destination: destination.clone(), media: None, keep_backup: false, published: false };
        if let Some(suffix) = media_suffix {
            let stem = destination.file_stem().ok_or("export path needs a file name")?.to_string_lossy();
            let folder = parent.join(format!("{stem} {suffix}"));
            std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
            stage.media = Some(fresh(&folder, "Take")?);
        }
        let staged_path = stage.root.join(name).to_string_lossy().into_owned();
        match task {
            Task::Interchange { params, media_dir, .. } => {
                params["path"] = staged_path.into();
                // The host checks absence at publication using the original parameters.
                // Source-broker errors in the child do not preserve NotFound, so it cannot
                // reliably distinguish a new destination from an inaccessible existing one.
                if let Some(params) = params.as_object_mut() {
                    params.remove("documentAbsentPath");
                }
                *media_dir = stage.media.as_ref().map(|p| p.to_string_lossy().into_owned());
            }
            Task::Media { settings, .. } => settings.path = staged_path,
            Task::Preview { .. } | Task::ColorGroup { .. } => {}
        }
        Ok(Some(stage))
    }

    pub(super) fn publish(&mut self, completion: &mut Completion) -> Result<(), String> {
        let parent = self.destination.parent().ok_or("export destination has no parent")?;
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&self.root).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if files.len() >= 16_384 || !entry.file_type().map_err(|e| e.to_string())?.is_file() {
                return Err("backend produced too many files or an unexpected output directory".into());
            }
            files.push((entry.path(), parent.join(entry.file_name())));
        }
        if files.is_empty() {
            return Err("backend completed without producing an export".into());
        }
        files.sort_by(|a, b| a.1.cmp(&b.1));
        if files.len() == 1 {
            if let Some((source, destination)) = files.first() {
                // rename replaces a file atomically on supported native hosts. In particular,
                // never move an existing AAF out of the way before its replacement is durable.
                std::fs::rename(source, destination).map_err(|e| e.to_string())?;
            }
            self.published = true;
            self.remap(completion);
            return Ok(());
        }
        let backups = self.root.join("previous");
        std::fs::create_dir(&backups).map_err(|e| e.to_string())?;
        let mut saved = Vec::new();
        let mut installed = Vec::new();
        // This is the short commit boundary: cancellation is accepted before it, then publication
        // either completes or rolls back. Expensive rendering has already finished in the child.
        let result = (|| {
            for (source, destination) in &files {
                if destination.exists() {
                    if !destination.is_file() {
                        return Err(format!("export destination {} is not a file", destination.display()));
                    }
                    let backup = backups.join(destination.file_name().ok_or("invalid destination name")?);
                    std::fs::rename(destination, &backup).map_err(|e| e.to_string())?;
                    saved.push((backup, destination.clone()));
                }
                std::fs::rename(source, destination).map_err(|e| e.to_string())?;
                installed.push(destination.clone());
            }
            Ok(())
        })();
        if let Err(error) = result {
            for path in installed.iter().rev() {
                if let Err(e) = std::fs::remove_file(path) {
                    log::error!("could not roll back export {}: {e}", path.display());
                }
            }
            for (backup, destination) in saved.iter().rev() {
                if let Err(e) = std::fs::rename(backup, destination) {
                    self.keep_backup = true;
                    log::error!("previous export retained at {}: {e}", backup.display());
                }
            }
            return Err(if self.keep_backup { format!("{error}; previous files retained in {}", backups.display()) } else { error });
        }
        self.published = true;
        self.remap(completion);
        Ok(())
    }

    fn remap(&self, completion: &mut Completion) {
        let Some(parent) = self.destination.parent() else { return };
        let remap = |path: &str| {
            Path::new(path).strip_prefix(&self.root).ok().map(|name| parent.join(name).to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
        };
        completion.report.path = remap(&completion.report.path);
        for path in &mut completion.report.extra_files {
            *path = remap(path);
        }
        for key in ["path", "basePath", "lastPath", "sidecar"] {
            if let Some(path) = completion.output.get(key).and_then(serde_json::Value::as_str) {
                completion.output[key] = remap(path).into();
            }
        }
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if self.keep_backup {
            return;
        }
        // Both directories were exclusively created by this Stage; never remove the shared
        // parent, an existing export's media, or a directory supplied by a worker message.
        if let Err(e) = std::fs::remove_dir_all(&self.root) {
            log::warn!("export staging folder {} could not be removed: {e}", self.root.display());
        }
        if let Some(media) = &self.media {
            let result = if self.published { std::fs::remove_dir(media) } else { std::fs::remove_dir_all(media) };
            if let Err(e) = result
                && !self.published
            {
                log::warn!("unfinished export media {} could not be removed: {e}", media.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            Self(fresh(&std::env::temp_dir(), "filmcraft-stage-test").unwrap())
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cancelling_an_export_keeps_previous_document_and_shared_media() {
        let directory = Directory::new();
        let destination = directory.0.join("example.aaf");
        let shared_media = directory.0.join("example Media");
        std::fs::create_dir(&shared_media).unwrap();
        std::fs::write(&destination, b"previous document").unwrap();
        std::fs::write(shared_media.join("keep.wav"), b"previous media").unwrap();
        let mut task = Task::Interchange { omf: false, params: json!({"path":destination,"audio":"separate"}), media_dir: None };
        let stage = Stage::prepare(&mut task).unwrap().unwrap();
        let root = stage.root.clone();
        let media = stage.media.clone().unwrap();
        std::fs::write(root.join("example.aaf"), b"unfinished document").unwrap();
        std::fs::write(media.join("new.wav"), b"unfinished media").unwrap();
        drop(stage);
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous document");
        assert_eq!(std::fs::read(shared_media.join("keep.wav")).unwrap(), b"previous media");
        assert!(!root.exists() && !media.exists());
    }

    #[test]
    fn failed_multifile_publication_restores_already_replaced_files() {
        let directory = Directory::new();
        let destination = directory.0.join("a.wav");
        std::fs::write(&destination, b"previous audio").unwrap();
        std::fs::create_dir(directory.0.join("z.wav")).unwrap();
        let mut task = Task::Media {
            sequence: filmcraft_project::ItemId(1),
            settings: Box::new(filmcraft_export::ExportSettings { path: destination.to_string_lossy().into_owned(), ..Default::default() }),
        };
        let mut stage = Stage::prepare(&mut task).unwrap().unwrap();
        std::fs::write(stage.root.join("a.wav"), b"new audio").unwrap();
        std::fs::write(stage.root.join("z.wav"), b"new sidecar").unwrap();
        let mut completion = Completion {
            output: json!({}),
            report: filmcraft_export::Report {
                path: stage.root.join("a.wav").to_string_lossy().into_owned(),
                frames: 0,
                seconds: 0.0,
                bytes: 9,
                render_fps: 0.0,
                extra_files: Vec::new(),
            },
        };
        assert!(stage.publish(&mut completion).is_err());
        assert_eq!(std::fs::read(destination).unwrap(), b"previous audio");
        assert!(directory.0.join("z.wav").is_dir());
        let root = stage.root.clone();
        drop(stage);
        assert!(!root.exists());
    }
}
