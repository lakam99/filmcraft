//! AAF documents use the ordinary Open/Save lifecycle, including background-save acknowledgements.
use std::hash::Hasher;
use std::sync::Arc;

use filmcraft_project::{ItemId, Project};
use serde_json::{Value, json};

use crate::{EngineError, Event, Result, Services, Session};

pub(crate) const MAX_DOCUMENT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct State {
    origin: Option<Origin>,
    pending: Vec<Pending>,
}

struct Origin {
    epoch: u64,
    path: String,
    fingerprint: u64,
    embedded: bool,
    backup: Option<String>,
    warnings: Vec<String>,
}

struct Pending {
    job: u64,
    epoch: u64,
    revision: u64,
    previous_path: Option<String>,
    path: String,
    adopt: bool,
    embedded: bool,
    backup: Option<String>,
    sequence: Option<ItemId>,
}

pub fn is_aaf_path(path: &str) -> bool {
    std::path::Path::new(path).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("aaf"))
}

fn same_path(a: &str, b: &str) -> bool {
    let normalized = |path: &str| {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.into()).to_string_lossy().replace('\\', "/");
        if cfg!(windows) { path.to_lowercase() } else { path }
    };
    normalized(a) == normalized(b)
}

fn summary(values: &Value) -> Vec<String> {
    values
        .as_array()
        .map(|values| values.iter().take(8).filter_map(Value::as_str).map(|message| message.chars().take(512).collect()).collect())
        .unwrap_or_default()
}

pub(crate) fn fingerprint(bytes: &[u8]) -> u64 {
    // This fingerprint lives only in this process; it detects external edits, not authenticity.
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    hash.write(bytes);
    hash.finish()
}

pub(crate) fn read(services: &dyn Services, path: &str) -> Result<Vec<u8>> {
    let size = services.file_size(path).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    if size > MAX_DOCUMENT_BYTES {
        return Err(EngineError::Other("AAF document exceeds the 256 MiB editing limit".into()));
    }
    let length = usize::try_from(size.saturating_add(1)).map_err(|_| EngineError::Other("AAF size exceeds this platform's limit".into()))?;
    let bytes = services.read_range(path, 0, length).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    if bytes.len() as u64 != size {
        return Err(EngineError::Other("AAF changed while it was being read; try again".into()));
    }
    Ok(bytes)
}

pub(crate) fn open(s: &mut Session, path: &str) -> Result<Value> {
    let bytes = read(&*s.services, path)?;
    // Parse, extract and link in an isolated session. A bad AAF leaves the open document intact.
    let mut opened = Session::new(s.services.clone());
    let mut result = crate::interchange::open_aaf_document(&mut opened, path, &bytes)?;
    let active = opened.state.active_sequence;
    let tabs = opened.state.open_sequences.clone();
    let project = Arc::make_mut(&mut opened.project);
    let name = std::path::Path::new(path).file_stem().and_then(|name| name.to_str()).unwrap_or("Untitled").to_string();
    project.name.clone_from(&name);
    project.root.name = name;
    crate::masks::discard_jobs(s);
    crate::commands::install_project(s, (*opened.project).clone(), Some(path.into()), true);
    let proxies = s.media.use_proxies();
    s.media = opened.media.clone();
    s.media.set_use_proxies(proxies);
    s.media.set_composition_renderer(&s.prefs.media.effectcraft_executable);
    s.state.active_sequence = active;
    s.state.open_sequences = tabs;
    s.aaf_document.origin = Some(Origin {
        epoch: s.document_epoch,
        path: path.into(),
        fingerprint: fingerprint(&bytes),
        embedded: result["extractedMedia"].as_u64().unwrap_or(0) > 0,
        backup: None,
        warnings: summary(&result["warnings"]),
    });
    if let Some(sequence) = active {
        s.events.push(Event::OpenSequence(sequence));
    }
    s.note_recent_project();
    result["path"] = json!(path);
    result["documentFormat"] = json!("aaf");
    if result["report"].as_array().is_some_and(|warnings| !warnings.is_empty()) {
        s.toast(format!("AAF opened: {}", summary(&result["report"]).join("; ")));
    }
    Ok(result)
}

fn root_sequence(project: &Project) -> Result<ItemId> {
    let referenced: std::collections::HashSet<_> = project
        .sequences()
        .filter_map(|item| project.sequence(item.id))
        .flat_map(|q| q.all_tracks())
        .flat_map(|track| &track.items)
        .map(|clip| clip.item)
        .collect();
    let mut roots = project.sequences().filter(|item| !referenced.contains(&item.id));
    let first = roots.next().ok_or(EngineError::NoSequence)?.id;
    if roots.next().is_some() {
        return Err(EngineError::Other("AAF Save requires one top-level sequence; use Save As .fcproj to keep multiple independent sequences".into()));
    }
    Ok(first)
}

pub(crate) fn check_expected(services: &dyn Services, params: &Value) -> Result<()> {
    if let Some(path) = params["documentAbsentPath"].as_str() {
        match services.file_size(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(EngineError::Other(format!("{path}: {e}"))),
            Ok(_) => return Err(EngineError::Other("The AAF destination appeared while saving; choose another filename or confirm replacement again".into())),
        }
    }
    if let (Some(path), Some(expected)) = (params["documentOriginalPath"].as_str(), params["documentExpected"].as_u64())
        && fingerprint(&read(services, path)?) != expected
    {
        return Err(EngineError::Other(
            "The AAF changed outside FilmCraft. Reopen it or use Save As to another file; the external changes were not replaced".into(),
        ));
    }
    Ok(())
}

fn backup(services: &dyn Services, path: &str, bytes: &[u8]) -> Result<String> {
    let stem = path.get(..path.len().saturating_sub(4)).unwrap_or(path);
    for number in 1..=1000 {
        let suffix = if number == 1 { String::new() } else { format!(" {number}") };
        let candidate = format!("{stem} (before FilmCraft{suffix}).aaf");
        match services.write_file_new(&candidate, bytes) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(EngineError::Other(format!("Cannot preserve the original AAF at {candidate}: {e}"))),
        }
    }
    Err(EngineError::Other("No unused AAF backup filename; use Save As to another file".into()))
}

fn check_editable_mix(project: &Project, sequence: Option<ItemId>) -> Result<()> {
    let mut reachable = std::collections::HashSet::new();
    let mut visit = sequence.into_iter().collect::<Vec<_>>();
    while let Some(id) = visit.pop() {
        if !reachable.insert(id) {
            continue;
        }
        if let Some(q) = project.sequence(id) {
            // Only follow existing sequences, once each: hostile projects can contain cycles.
            for child in q.all_tracks().flat_map(|track| &track.items).map(|clip| clip.item) {
                if project.sequence(child).is_some() && !reachable.contains(&child) && !visit.contains(&child) {
                    visit.push(child);
                }
            }
        }
    }
    for item in project.sequences() {
        if sequence.is_some() && !reachable.contains(&item.id) {
            continue;
        }
        let Some(q) = project.sequence(item.id) else { continue };
        let strip_processing = |strip: &filmcraft_project::MixerStrip| {
            !strip.lanes.is_empty() || !strip.sends.is_empty() || strip.output.is_some() || strip.input_map != Default::default()
        };
        if q.master_volume_db != 0.0
            || q.master_effects.iter().any(|effect| effect.enabled)
            || strip_processing(&q.master_mixer)
            || !q.submix_tracks.is_empty()
            || q.all_tracks().any(|track| {
                !track.enabled
                    || track.muted
                    || track.solo
                    || track.volume_db != 0.0
                    || track.pan != 0.0
                    || track.effects.iter().any(|effect| effect.enabled)
                    || strip_processing(&track.mixer)
            })
        {
            return Err(EngineError::Other("Editable AAF cannot retain track/bus mixer processing or mute/solo state. Use Save As .fcproj, or Export AAF with mix matching to render that processing".into()));
        }
    }
    Ok(())
}

pub(crate) fn save(s: &mut Session, path: &str, adopt: bool) -> Result<Value> {
    if s.exec_depth > 1 {
        return Err(EngineError::Other("Save AAF after the atomic editing batch completes".into()));
    }
    poll(s);
    if let Some(pending) = s.aaf_document.pending.iter().find(|p| p.epoch == s.document_epoch) {
        if pending.sequence.is_none() && pending.path == path && pending.adopt == adopt && pending.revision == s.revision {
            return Ok(json!({"job":pending.job,"path":path,"background":true}));
        }
        return Err(EngineError::Other("An AAF save is still running; wait for it before saving to another file".into()));
    }
    if !adopt && s.path.as_deref().is_some_and(|original| same_path(original, path)) {
        return Err(EngineError::Other("Save a Copy needs a different filename".into()));
    }
    let sequence = root_sequence(&s.project)?;
    check_editable_mix(&s.project, None)?;
    let document_origin = s.aaf_document.origin.as_ref().filter(|origin| origin.epoch == s.document_epoch && s.path.as_deref() == Some(origin.path.as_str()));
    // Native recovery snapshots retain the edits and source path, but not the original AAF
    // fingerprint or unsupported-object warnings. Never guess that it is safe to overwrite.
    if document_origin.is_none() && s.path.as_deref().is_some_and(|current| is_aaf_path(current) && same_path(current, path)) {
        return Err(EngineError::Other(
            "This recovered AAF has no original-file checkpoint. Use Save As to a different filename to preserve both the recovered edits and the original AAF"
                .into(),
        ));
    }
    let embedded = document_origin.is_some_and(|origin| origin.embedded);
    let origin = document_origin.filter(|origin| same_path(&origin.path, path));
    if let Some(origin) = origin.filter(|origin| !origin.warnings.is_empty()) {
        // Unsupported foreign objects were not represented by the imported timeline. Never
        // overwrite those objects silently; Save As retains the original AAF unchanged.
        return Err(EngineError::Other(format!(
            "This AAF contains unsupported content: {}. Use Save As to another AAF or a FilmCraft project to retain the original file",
            origin.warnings.join("; ")
        )));
    }
    let previous_backup = origin.and_then(|origin| origin.backup.clone());
    let expected = origin.map(|origin| origin.fingerprint);
    let original = match s.services.file_size(path) {
        Ok(_) => Some(read(&*s.services, path)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && expected.is_none() => None,
        Err(e) => return Err(EngineError::Other(format!("{path}: {e}"))),
    };
    if let Some(expected) = expected
        && original.as_ref().is_none_or(|bytes| fingerprint(bytes) != expected)
    {
        return Err(EngineError::Other("The AAF changed outside FilmCraft; reopen it or use Save As to another file".into()));
    }
    let preserved = match (previous_backup, original.as_ref()) {
        (Some(path), _) => Some(path),
        (None, Some(bytes)) => Some(backup(&*s.services, path, bytes)?),
        (None, None) => None,
    };
    if let Some(origin) = s.aaf_document.origin.as_mut().filter(|origin| origin.epoch == s.document_epoch && origin.path == path) {
        origin.backup = preserved.clone();
    }
    // Save is editable interchange, not a mixer print that can flatten the timeline into one clip.
    let mut params = json!({"path":path,"sequence":sequence.0,"audio":if embedded {"embedded"} else {"linked"},
        "preserveMix":false,"trimAudio":false,"renderAudioEffects":false,"bitDepth":24,"documentSave":true});
    if let Some(bytes) = original.as_ref() {
        params["documentOriginalPath"] = json!(path);
        params["documentExpected"] = json!(fingerprint(bytes));
    }
    let mut result = crate::aaf_omf::export_aaf(s, &params)?;
    let pending = Pending {
        job: result["job"].as_u64().unwrap_or(0),
        epoch: s.document_epoch,
        revision: s.revision,
        previous_path: s.path.clone(),
        path: path.into(),
        adopt,
        embedded,
        backup: preserved,
        sequence: None,
    };
    result["documentFormat"] = json!("aaf");
    result["backup"] = json!(pending.backup);
    if pending.job != 0 {
        s.aaf_document.pending.push(pending);
    } else {
        complete(s, &pending, &result);
    }
    Ok(result)
}

fn complete(s: &mut Session, pending: &Pending, result: &Value) {
    if s.document_epoch != pending.epoch || s.path != pending.previous_path {
        return;
    }
    let Some(fingerprint) = result["documentFingerprint"].as_u64() else {
        s.events.push(Event::Toast { message: "AAF save acknowledgement is incomplete; the document remains unsaved".into(), error: true });
        return;
    };
    if pending.sequence.is_some() {
        if let Some(origin) = s.aaf_document.origin.as_mut().filter(|origin| origin.epoch == pending.epoch && same_path(&origin.path, &pending.path)) {
            origin.fingerprint = fingerprint;
            origin.backup = pending.backup.clone();
            origin.embedded = pending.embedded;
            origin.warnings.clear();
        }
        // This saves one sequence, never the entire native project or its undo state.
        s.toast(format!("Sequence saved to AAF: {}. Save the FilmCraft project separately to retain all sequences", pending.path));
        return;
    }
    if !pending.adopt {
        return;
    }
    s.path = Some(pending.path.clone());
    if let Some(name) = std::path::Path::new(&pending.path).file_stem().and_then(|name| name.to_str()) {
        let project = Arc::make_mut(&mut s.project);
        project.name = name.into();
        project.root.name = name.into();
    }
    s.saved_revision = pending.revision;
    s.previews_follow_path();
    s.note_recent_project();
    s.aaf_document.origin = Some(Origin {
        epoch: s.document_epoch,
        path: pending.path.clone(),
        fingerprint,
        embedded: pending.embedded,
        backup: pending.backup.clone(),
        warnings: Vec::new(),
    });
    s.toast(if s.revision == pending.revision {
        format!("AAF saved to {}", pending.path)
    } else {
        format!("AAF saved to {}; newer edits remain unsaved", pending.path)
    });
}

/// Explicit Sequence-menu save. The user chooses the destination on every invocation, including
/// for legacy imports that never recorded their source. Never adopt the AAF as the project file.
pub(crate) fn save_sequence(s: &mut Session, params: &Value) -> Result<Value> {
    use crate::commands::{bad, str_p};
    let cmd = "sequence.saveAaf";
    if s.exec_depth > 1 {
        return Err(EngineError::Other("Save AAF after the atomic editing batch completes".into()));
    }
    let path = str_p(params, "path")
        .filter(|path| path.len() <= 32_768 && !path.contains('\0') && is_aaf_path(path))
        .ok_or_else(|| bad(cmd, "choose a destination ending in .aaf"))?;
    if params.get("sequence").is_some_and(|value| value.as_u64().is_none()) {
        return Err(bad(cmd, "`sequence` must be an unsigned sequence id"));
    }
    if params.get("expectedTarget").is_some_and(|value| value.as_u64().is_none())
        || params.get("replaceUnsupported").is_some_and(|value| value.as_bool().is_none())
    {
        return Err(bad(cmd, "replacement requires a boolean and the unsigned target checkpoint"));
    }
    let sequence = crate::export_tools::sequence_param(s, params, cmd)?;
    if s.project.sequence(sequence).is_none() {
        return Err(EngineError::NoSequence);
    }
    poll(s);
    if let Some(pending) = s.aaf_document.pending.iter().find(|pending| pending.epoch == s.document_epoch) {
        if pending.sequence == Some(sequence) && pending.path == path && pending.revision == s.revision {
            return Ok(json!({"job":pending.job,"path":path,"sequence":sequence.0,"background":true}));
        }
        return Err(EngineError::Other("An AAF save is still running; wait for it before saving another sequence".into()));
    }
    check_editable_mix(&s.project, Some(sequence))?;
    let origin = s.aaf_document.origin.as_ref().filter(|origin| origin.epoch == s.document_epoch && same_path(&origin.path, path));
    let original = match s.services.file_size(path) {
        Ok(_) => Some(read(&*s.services, path)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && origin.is_none() => None,
        Err(e) => return Err(EngineError::Other(format!("{path}: {e}"))),
    };
    if let Some(origin) = origin
        && original.as_ref().is_none_or(|bytes| fingerprint(bytes) != origin.fingerprint)
    {
        return Err(EngineError::Other("The AAF changed outside FilmCraft; choose another filename".into()));
    }
    if let Some(expected) = params["expectedTarget"].as_u64()
        && original.as_ref().is_none_or(|bytes| fingerprint(bytes) != expected)
    {
        return Err(EngineError::Other("The destination AAF changed after the replacement prompt; choose the destination again".into()));
    }
    let mut embedded = s.aaf_document.origin.as_ref().is_some_and(|origin| origin.epoch == s.document_epoch && origin.embedded);
    if let Some(bytes) = &original {
        // An explicit replace must not silently discard other compositions or foreign objects.
        // Parsing here does not extract/write media or change the editor's project.
        let (imported, extracted, report) = filmcraft_interchange::aaf::import(bytes, &Default::default())
            .map_err(|e| EngineError::Other(format!("Cannot safely replace this AAF: {e}. Choose another filename")))?;
        if imported.sequences.len() != 1 {
            return Err(EngineError::Other("The destination AAF contains multiple compositions; choose another filename to preserve them".into()));
        }
        let mut warnings = report.warnings().take(8).map(|entry| entry.message.chars().take(512).collect::<String>()).collect::<Vec<_>>();
        if let Some(origin) = origin {
            warnings.extend(origin.warnings.iter().take(8).cloned());
        }
        if !warnings.is_empty() && (params["replaceUnsupported"].as_bool() != Some(true) || params["expectedTarget"].as_u64().is_none()) {
            // A generic file-picker replace is not an acknowledgement that unsupported effects
            // will be omitted. Confirm the known loss before backing up or writing anything.
            return Ok(json!({"needsConfirmation":true,"path":path,"sequence":sequence.0,"warnings":warnings,
                "expectedTarget":fingerprint(bytes)}));
        }
        embedded = !extracted.is_empty();
    }
    let preserved = original.as_ref().map(|bytes| backup(&*s.services, path, bytes)).transpose()?;
    let mut export = json!({"path":path,"sequence":sequence.0,"audio":if embedded {"embedded"} else {"linked"},
        "preserveMix":false,"trimAudio":false,"renderAudioEffects":false,"bitDepth":24,"documentSave":true});
    if let Some(bytes) = &original {
        export["documentOriginalPath"] = json!(path);
        export["documentExpected"] = json!(fingerprint(bytes));
    } else {
        // Keep the destination separate from `path`, which the worker rewrites for staging.
        export["documentAbsentPath"] = json!(path);
    }
    let mut result = crate::aaf_omf::export_aaf(s, &export)?;
    let pending = Pending {
        job: result["job"].as_u64().unwrap_or(0),
        epoch: s.document_epoch,
        revision: s.revision,
        previous_path: s.path.clone(),
        path: path.into(),
        adopt: false,
        embedded,
        backup: preserved,
        sequence: Some(sequence),
    };
    result["sequence"] = json!(sequence.0);
    result["backup"] = json!(pending.backup);
    if pending.job != 0 {
        s.aaf_document.pending.push(pending);
    } else {
        complete(s, &pending, &result);
    }
    Ok(result)
}

pub(crate) fn saving(s: &Session) -> bool {
    s.aaf_document.pending.iter().any(|pending| pending.epoch == s.document_epoch)
}

pub(crate) fn native_saved(s: &mut Session) {
    // Saving the native project during a sequence export must not forget the running job:
    // document replacement is guarded until its output has been acknowledged.
    s.aaf_document.origin = None;
}

pub(crate) fn poll(s: &mut Session) {
    if s.aaf_document.origin.as_ref().is_some_and(|origin| origin.epoch != s.document_epoch) {
        s.aaf_document.origin = None;
    }
    let mut waiting = Vec::new();
    for pending in std::mem::take(&mut s.aaf_document.pending) {
        if pending.epoch != s.document_epoch {
            continue;
        }
        let Some(job) = s.jobs.iter().find(|job| job.id == pending.job) else { continue };
        if !job.progress.finished.load(std::sync::atomic::Ordering::Acquire) {
            waiting.push(pending);
            continue;
        }
        let succeeded = job.result.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().is_some_and(|result| result.is_ok());
        #[cfg(not(target_arch = "wasm32"))]
        if succeeded && let Some(output) = s.backend.details(pending.job).and_then(|details| details.get("output").cloned()) {
            complete(s, &pending, &output);
        }
        #[cfg(target_arch = "wasm32")]
        let _ = succeeded;
    }
    s.aaf_document.pending = waiting;
}

#[cfg(test)]
mod tests;
