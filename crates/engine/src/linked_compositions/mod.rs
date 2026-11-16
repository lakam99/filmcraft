//! Linked EffectCraft projects: saved project + stable composition ID, rendered on demand.
//! All expensive connection/probing work is available as a background job. Jobs are bound to
//! the originating editor document so a late response cannot edit a subsequently opened one.

use crate::{
    EngineError, Result, Session,
    commands::{CommandSpec, NamedKeys, always, bad},
};
use filmcraft_media::{AudioStreamInfo, MediaInfo, MediaKind, SharedSource, VideoStreamInfo};
use filmcraft_project::{ItemId, ItemKind, MediaClip, MediaRef};
use filmcraft_time::{FrameRate, Tick};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, PoisonError};

#[cfg(not(target_arch = "wasm32"))]
pub mod native;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Composition {
    pub id: u64,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
    pub duration: i64,
    pub pixel_aspect: f64,
    pub audio: bool,
    #[serde(default)]
    pub missing: Vec<String>,
}

impl Composition {
    pub fn media_info(&self) -> std::result::Result<MediaInfo, String> {
        if self.id == 0
            || self.width == 0
            || self.height == 0
            || self.width > 16_384
            || self.height > 16_384
            || u64::from(self.width) * u64::from(self.height) > 16_777_216
            || self.duration <= 0
            || self.duration > Tick::MAX.0
            || self.frame_rate.num <= 0
            || self.frame_rate.den <= 0
            || !(1.0..=240.0).contains(&self.frame_rate.as_f64())
            || !self.pixel_aspect.is_finite()
            || !(0.01..=100.0).contains(&self.pixel_aspect)
        {
            return Err("The composition has unsupported dimensions, duration, frame rate or pixel aspect".into());
        }
        Ok(MediaInfo {
            name: self.name.clone(),
            kind: MediaKind::Movie,
            duration: Tick(self.duration),
            video: Some(VideoStreamInfo {
                width: self.width,
                height: self.height,
                frame_rate: self.frame_rate,
                par: ((self.pixel_aspect * 10_000.0).round() as u32, 10_000),
                codec: "EffectCraft linked composition".into(),
                pixel_format: "RGBA8 (straight alpha)".into(),
                color: filmcraft_color::ColorInfo::SRGB_FULL,
                has_alpha: true,
                bitrate: None,
                hdr: None,
                color_assumed: None,
            }),
            audio_streams: if self.audio {
                vec![AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "EffectCraft composition mix".into(), bits_per_sample: Some(32) }]
            } else {
                vec![]
            },
            container: "EffectCraft project (linked)".into(),
            start_timecode: None,
            file_size: None,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Catalog {
    pub compositions: Vec<Composition>,
    pub missing: Vec<String>,
}

/// One export's project snapshots, shared by all compositions from the same source project.
#[derive(Default)]
pub(crate) struct JobSources {
    #[cfg(not(target_arch = "wasm32"))]
    projects: std::collections::HashMap<String, std::result::Result<native::Opened, String>>,
}
impl JobSources {
    pub(crate) fn source(&mut self, path: &str, comp: u64, renderer: &str) -> filmcraft_media::Result<SharedSource> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.projects
                .entry(path.to_string())
                .or_insert_with(|| native::executable(renderer).and_then(|exe| native::Opened::open(path, &exe)))
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|opened| opened.source(comp))
                .map_err(filmcraft_media::MediaError::Decode)
        }
        #[cfg(target_arch = "wasm32")]
        {
            open_source(path, comp, renderer)
        }
    }
}

pub fn open_source(path: &str, composition: u64, renderer: &str) -> filmcraft_media::Result<SharedSource> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        native::executable(renderer)
            .and_then(|exe| native::Opened::open(path, &exe))
            .and_then(|opened| opened.source(composition))
            .map_err(filmcraft_media::MediaError::Decode)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (path, composition, renderer);
        Err(filmcraft_media::MediaError::Unsupported("Linked compositions need the native FilmCraft and EffectCraft apps".into()))
    }
}

#[derive(Clone)]
enum Operation {
    List { path: String },
    Link { path: String, composition: u64, item: Option<ItemId>, expected: Option<String> },
    Refresh { items: Vec<(ItemId, String, u64)> },
}

struct Prepared {
    catalog: Option<Catalog>,
    sources: Vec<(Option<ItemId>, String, u64, std::result::Result<SharedSource, String>)>,
}
type Pending = Arc<Mutex<Option<std::result::Result<Prepared, String>>>>;
struct Job {
    id: u64,
    operation: Operation,
    document: Arc<()>,
    result: Pending,
}

#[derive(Default)]
pub struct Runtime {
    #[cfg(not(target_arch = "wasm32"))]
    next: u64,
    pending: Option<Job>,
    completed: std::collections::BTreeMap<u64, Value>,
    refresh_again: std::collections::BTreeSet<String>,
    missing_scans: std::collections::HashMap<String, u8>,
}

/// Debounce atomic delete/rename saves, but mark a genuinely missing source once, and refresh
/// when it returns. A removed source must not remain a silently cached, apparently live link.
pub(crate) fn observe_file(s: &mut Session, path: &str, present: bool) -> bool {
    if present {
        return s.linked.missing_scans.remove(path).is_some_and(|n| n >= 2);
    }
    let n = s.linked.missing_scans.entry(path.to_string()).or_default();
    *n = n.saturating_add(1);
    *n == 2
}

fn prepare(operation: &Operation, renderer: &str, resolved: &std::collections::HashMap<String, String>) -> std::result::Result<Prepared, String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let exe = native::executable(renderer)?;
        match operation {
            Operation::List { path } => {
                let opened = native::Opened::open(path, &exe)?;
                Ok(Prepared { catalog: Some(opened.catalog), sources: vec![] })
            }
            Operation::Link { path, composition, item, .. } => {
                let opened = native::Opened::open(path, &exe)?;
                let source = opened.source(*composition)?;
                Ok(Prepared { catalog: None, sources: vec![(*item, path.clone(), *composition, Ok(source))] })
            }
            Operation::Refresh { items } => {
                let mut opened = std::collections::HashMap::new();
                let mut sources = Vec::new();
                for (item, path, composition) in items {
                    let source = opened
                        .entry(path.clone())
                        .or_insert_with(|| {
                            let file = resolved.get(path).ok_or("The source project path could not be followed".to_string())?;
                            native::Opened::open(file, &exe)
                        })
                        .as_ref()
                        .map_err(Clone::clone)
                        .and_then(|o| o.source(*composition));
                    sources.push((Some(*item), path.clone(), *composition, source));
                }
                Ok(Prepared { catalog: None, sources })
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (operation, renderer, resolved);
        Err("Linked compositions need the native FilmCraft and EffectCraft apps".into())
    }
}

fn apply(s: &mut Session, operation: &Operation, prepared: Prepared) -> Result<Value> {
    if let Some(catalog) = prepared.catalog {
        return Ok(json!(catalog));
    }
    let mut results = vec![];
    let mut view_changed = false;
    for (item, path, composition, source) in prepared.sources {
        // Both errors and successful results belong to the captured target, not merely an item
        // ID. A delayed refresh/repair must never poison or overwrite a newer relink/replace.
        if let Some(id) = item {
            let Some(current) = s.project.item(id).and_then(|i| i.as_media()) else { continue };
            let valid = match operation {
                Operation::Refresh { .. } => {
                    !current.offline && current.interpret.linked_composition == Some(composition) && matches!(&current.media,MediaRef::File{path:p} if p==&path)
                }
                Operation::Link { expected, .. } => expected.as_ref().is_some_and(|k| *k == crate::media_pool::media_key(current)),
                _ => true,
            };
            if !valid {
                results.push(json!({"item":id.0,"error":"The source reference changed while this operation was running; no change was applied"}));
                continue;
            }
        }
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                if let Some(item) = item {
                    s.media.link_failed(&s.project, item, &path, &error);
                    view_changed = true;
                }
                results.push(json!({"item":item.map(|i| i.0), "error":error}));
                continue;
            }
        };
        let info = source.info().clone();
        let name = info.name.clone();
        let old = item.and_then(|id| s.project.item(id)).and_then(|i| i.as_media()).cloned();
        let metadata_changed = old.as_ref().is_some_and(|m| m.info != info);
        let id = match operation {
            Operation::Refresh { .. } => {
                let Some(id) = item else { continue };
                let Some(m) = s.project.item(id).and_then(|i| i.as_media()) else { continue };
                if m.interpret.linked_composition != Some(composition) || !matches!(&m.media, MediaRef::File { path: p } if p == &path) {
                    continue;
                }
                // Refreshing source metadata does not move/trim timeline clips, rename the item,
                // create Undo entries or mark a previously clean FilmCraft project dirty.
                if let Some(m) = Arc::make_mut(&mut s.project).item_mut(id).and_then(|i| i.as_media_mut()) {
                    m.info = info;
                }
                view_changed = true;
                id
            }
            Operation::Link { .. } => s.edit(if item.is_some() { "Relink EffectCraft Composition" } else { "Link EffectCraft Composition" }, |p, st| {
                if let Some(id) = item {
                    let m = p.item_mut(id).and_then(|i| i.as_media_mut()).ok_or_else(|| EngineError::Other("The linked item no longer exists".into()))?;
                    m.media = MediaRef::File { path: path.clone() };
                    m.info = info;
                    m.interpret.linked_composition = Some(composition);
                    m.offline = false;
                    m.identity = None;
                    m.proxy = None;
                    Ok(id)
                } else {
                    let id = p.add_item(
                        &name,
                        filmcraft_project::Label::Rose,
                        ItemKind::Media(Box::new(MediaClip {
                            media: MediaRef::File { path: path.clone() },
                            info,
                            interpret: filmcraft_project::Interpretation { linked_composition: Some(composition), ..Default::default() },
                            mark_in: None,
                            mark_out: None,
                            markers: vec![],
                            offline: false,
                            proxy: None,
                            identity: None,
                        })),
                        None,
                    );
                    st.project_selection = vec![id];
                    Ok(id)
                }
            })?,
            Operation::List { .. } => continue,
        };
        if matches!(operation, Operation::Link { .. }) {
            s.media.allow(&path);
        }
        if let Some(m) = s.project.item(id).and_then(|i| i.as_media()) {
            s.media.insert_keyed(id, crate::media_pool::media_key(m), source);
        }
        results.push(json!({"item":id.0, "composition":composition, "name": name, "metadataChanged":metadata_changed,
            "notice": if metadata_changed { "Source metadata changed; existing timeline positions and trims were preserved" } else { "Updates from saved EffectCraft changes" }}));
    }
    if view_changed {
        s.bump_view();
    }
    Ok(json!({"items":results}))
}

fn start(s: &mut Session, operation: Operation, wait: bool) -> Result<Value> {
    if s.exec_depth > 1 && !wait {
        return Err(EngineError::Other("Composition commands inside an atomic batch require wait:true".into()));
    }
    if s.linked.pending.is_some() {
        return Err(EngineError::Other("A linked composition operation is running; wait for it to finish".into()));
    }
    s.media.set_composition_renderer(&s.prefs.media.effectcraft_executable);
    let renderer = s.prefs.media.effectcraft_executable.clone();
    let resolved = match &operation {
        Operation::Refresh { items } => items.iter().filter_map(|(_, path, _)| s.media.follow(path).ok().map(|file| (path.clone(), file))).collect(),
        _ => std::collections::HashMap::new(),
    };
    if wait {
        let prepared = prepare(&operation, &renderer, &resolved).map_err(EngineError::Other)?;
        return apply(s, &operation, prepared);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let result: Pending = Arc::default();
        let output = result.clone();
        let work = operation.clone();
        std::thread::Builder::new()
            .name("linked-composition".into())
            .spawn(move || {
                let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepare(&work, &renderer, &resolved)))
                    .unwrap_or_else(|_| Err("Linked composition worker stopped unexpectedly".into()));
                *output.lock().unwrap_or_else(PoisonError::into_inner) = Some(done);
            })
            .map_err(|e| EngineError::Other(e.to_string()))?;
        s.linked.next = s.linked.next.checked_add(1).ok_or_else(|| EngineError::Other("Composition job IDs exhausted".into()))?;
        let id = s.linked.next;
        s.linked.pending = Some(Job { id, operation, document: s.project_instance.clone(), result });
        Ok(json!({"job":id, "status":"running"}))
    }
    #[cfg(target_arch = "wasm32")]
    {
        Err(EngineError::Other("Linked compositions are available in native builds".into()))
    }
}

pub fn poll(s: &mut Session) {
    let Some(job) = &s.linked.pending else { return };
    let done = job.result.lock().unwrap_or_else(PoisonError::into_inner).take();
    let Some(done) = done else { return };
    let Some(job) = s.linked.pending.take() else { return };
    let result = if Arc::ptr_eq(&job.document, &s.project_instance) {
        done.map_err(EngineError::Other).and_then(|prepared| apply(s, &job.operation, prepared))
    } else {
        Err(EngineError::Other("Project changed before the composition operation finished; no edits applied".into()))
    };
    let status = match result {
        Ok(value) => json!({"job":job.id,"status":"done","result":value}),
        Err(e) => json!({"job":job.id,"status":"error","error":e.to_string()}),
    };
    s.linked.completed.insert(job.id, status);
    while s.linked.completed.len() > 16 {
        s.linked.completed.pop_first();
    }
    let paths = std::mem::take(&mut s.linked.refresh_again);
    if !paths.is_empty() {
        let _ = refresh_paths(s, &paths);
    }
}

pub fn refresh_paths(s: &mut Session, paths: &std::collections::BTreeSet<String>) -> Result<Value> {
    if s.linked.pending.is_some() {
        s.linked.refresh_again.extend(paths.iter().cloned());
        return Ok(json!({"queued":true}));
    }
    let items = links(s).into_iter().filter(|(_, p, _)| paths.contains(p)).collect();
    start(s, Operation::Refresh { items }, false)
}

fn links(s: &Session) -> Vec<(ItemId, String, u64)> {
    s.project
        .items
        .values()
        .filter_map(|i| {
            let m = i.as_media()?;
            let MediaRef::File { path } = &m.media else { return None };
            Some((i.id, path.clone(), m.interpret.linked_composition?))
        })
        .collect()
}

fn path(p: &Value, command: &str) -> Result<String> {
    let path = p["path"].as_str().filter(|p| !p.is_empty() && p.len() <= 32_768).ok_or_else(|| bad(command, "choose a saved .ecproj project"))?;
    if !std::path::Path::new(path).extension().is_some_and(|e| e.eq_ignore_ascii_case("ecproj")) {
        return Err(bad(command, "choose an EffectCraft .ecproj project"));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::path::absolute(path).map(|p| p.to_string_lossy().into_owned()).map_err(|e| bad(command, format!("could not resolve project path: {e}")))
    }
    #[cfg(target_arch = "wasm32")]
    {
        Ok(path.to_string())
    }
}

fn commands_run(s: &mut Session, p: &Value, command: &str) -> Result<Value> {
    let wait = p["wait"].as_bool().unwrap_or(false);
    let item = p.get("item").map(|v| v.as_u64().map(ItemId).ok_or_else(|| bad(command, "invalid item id"))).transpose()?;
    match command {
        "composition.list" => start(s, Operation::List { path: path(p, command)? }, wait),
        "file.linkComposition" | "composition.relink" => {
            if command == "file.linkComposition" && item.is_some() {
                return Err(bad(command, "use composition.relink to change an existing source item"));
            }
            let path = path(p, command)?;
            let old = item.and_then(|id| s.project.item(id)).and_then(|i| i.as_media()).and_then(|m| m.interpret.linked_composition);
            if command == "composition.relink" && old.is_none() {
                return Err(bad(command, "choose a linked composition item"));
            }
            let comp = match p.get("composition") {
                Some(value) => value.as_u64().filter(|c| *c > 0).ok_or_else(|| bad(command, "composition must be a positive id"))?,
                None => old.ok_or_else(|| bad(command, "choose a composition id from composition.list"))?,
            };
            let expected = item.and_then(|id| s.project.item(id)).and_then(|i| i.as_media()).map(crate::media_pool::media_key);
            start(s, Operation::Link { path, composition: comp, item, expected }, wait)
        }
        "composition.refresh" => {
            let items: Vec<_> = links(s).into_iter().filter(|(id, _, _)| item.is_none_or(|i| i == *id)).collect();
            if items.is_empty() {
                return Err(bad(command, "no linked compositions to refresh"));
            }
            start(s, Operation::Refresh { items }, wait)
        }
        "composition.job" => {
            poll(s);
            let id = p["job"].as_u64().ok_or_else(|| bad(command, "need job id"))?;
            if s.linked.pending.as_ref().is_some_and(|j| j.id == id) {
                return Ok(json!({"job":id,"status":"running"}));
            }
            s.linked.completed.get(&id).cloned().ok_or_else(|| bad(command, "no such composition job"))
        }
        "composition.inspect" => Ok(json!({"links":links(s).into_iter().filter(|(id, _, _)| item.is_none_or(|i| i == *id)).map(|(id, path, comp)| {
            let offline = s.media.offline_status(id);
            json!({"item":id.0,"path":path,"composition":comp,"status":if offline.is_some(){"unavailable"}else{"linked"},
                "error":offline.map(|o|o.error),"updates":"saved changes", "name":s.project.item(id).map(|i|&i.name)})
        }).collect::<Vec<_>>(), "runningJob":s.linked.pending.as_ref().map(|j|j.id)})),
        "composition.open" => {
            let timeline = s
                .state
                .selection
                .first()
                .and_then(|clip| s.active_sequence()?.find_item(*clip))
                .and_then(|(_, clip)| s.project.resolve_media(clip.item).map(|(id, _, _)| id));
            let id = item.or(timeline).or_else(|| s.state.project_selection.first().copied()).ok_or_else(|| bad(command, "select a linked composition"))?;
            let (_, path, comp) = links(s).into_iter().find(|(i, _, _)| *i == id).ok_or_else(|| bad(command, "select a linked composition"))?;
            #[cfg(not(target_arch = "wasm32"))]
            {
                let exe = native::executable(&s.prefs.media.effectcraft_executable).map_err(EngineError::Other)?;
                let path = s.media.follow(&path).map_err(|e| EngineError::Other(e.to_string()))?;
                let mut child = std::process::Command::new(exe);
                child.arg("--open-composition").arg(comp.to_string()).arg(&path);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    child.creation_flags(0x0800_0000);
                }
                let mut child = child.spawn().map_err(|e| EngineError::Other(e.to_string()))?;
                if let Err(error) = std::thread::Builder::new().name("effectcraft-editor".into()).spawn(move || {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| child.wait())) {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => log::warn!("Could not observe EffectCraft closing: {error}"),
                        Err(_) => log::error!("EffectCraft process observer stopped unexpectedly"),
                    }
                }) {
                    log::warn!("EffectCraft opened, but its process observer could not start: {error}");
                }
                Ok(json!({"opened":path,"composition":comp}))
            }
            #[cfg(target_arch = "wasm32")]
            {
                let _ = (path, comp);
                Err(EngineError::Other("Opening EffectCraft needs the native app".into()))
            }
        }
        _ => Err(bad(command, "unknown composition operation")),
    }
}

pub fn commands() -> Vec<CommandSpec> {
    [
        (
            "composition.list",
            "List EffectCraft Compositions",
            &[][..],
            r#"{"path":str,"wait":bool=false} -> {job}; poll composition.job (or wait=true for headless callers)"#,
            false,
        ),
        (
            "file.linkComposition",
            "Link EffectCraft Composition…",
            &["File"][..],
            r#"{"path":str,"composition":positiveId,"wait":bool=false} -> {job}; uses saved project, no export"#,
            true,
        ),
        (
            "composition.refresh",
            "Refresh Linked Compositions",
            &["Clip", "Linked Composition"][..],
            r#"{"item":id?,"wait":bool=false} -> {job}; absent item refreshes all links; also reloads changed footage"#,
            true,
        ),
        (
            "composition.relink",
            "Relink EffectCraft Composition…",
            &[][..],
            r#"{"item":id,"path":str,"composition":positiveId?,"wait":bool=false} -> {job}; keeps stable comp id when omitted"#,
            true,
        ),
        ("composition.inspect", "Inspect Linked Compositions", &[][..], r#"{"item":id?}"#, false),
        ("composition.job", "Read Composition Job", &[][..], r#"{"job":positiveId}"#, false),
        ("composition.open", "Open in EffectCraft", &["Clip", "Linked Composition"][..], r#"{"item":id?}"#, false),
    ]
    .into_iter()
    .map(|(id, label, menu, params, journal)| CommandSpec {
        id,
        label,
        menu,
        shortcut: None,
        params,
        enabled: always,
        run: match id {
            "composition.list" => |s, p| commands_run(s, p, "composition.list"),
            "file.linkComposition" => |s, p| commands_run(s, p, "file.linkComposition"),
            "composition.refresh" => |s, p| commands_run(s, p, "composition.refresh"),
            "composition.relink" => |s, p| commands_run(s, p, "composition.relink"),
            "composition.inspect" => |s, p| commands_run(s, p, "composition.inspect"),
            "composition.job" => |s, p| commands_run(s, p, "composition.job"),
            _ => |s, p| commands_run(s, p, "composition.open"),
        },
        journal,
        named: NamedKeys::NONE,
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_media::{FrameRequest, Generator, generators::GeneratorSource};

    fn source(name: &str) -> SharedSource {
        Arc::new(GeneratorSource::new(Generator::BarsAndTone, 32, 24, FrameRate::FPS_24, Tick::from_seconds_f64(2.0)).with_name(name))
    }
    fn link(s: &mut Session, comp: u64, name: &str) -> ItemId {
        let op = Operation::Link { path: "source.ecproj".into(), composition: comp, item: None, expected: None };
        let result = apply(s, &op, Prepared { catalog: None, sources: vec![(None, "source.ecproj".into(), comp, Ok(source(name)))] }).unwrap();
        ItemId(result["items"][0]["item"].as_u64().unwrap())
    }

    #[test]
    fn links_keep_distinct_composition_ids_through_save_reopen_and_undo() {
        let mut s = Session::default();
        let a = link(&mut s, 5, "First");
        let b = link(&mut s, 9, "Second");
        let media = |id| s.project.item(id).unwrap().as_media().unwrap();
        assert_ne!(crate::media_pool::media_key(media(a)), crate::media_pool::media_key(media(b)));
        let reopened = filmcraft_project::Project::from_json(&s.project.to_json()).unwrap();
        assert_eq!(reopened.item(a).unwrap().as_media().unwrap().interpret.linked_composition, Some(5));
        assert_eq!(reopened.item(b).unwrap().as_media().unwrap().interpret.linked_composition, Some(9));
        s.execute("edit.undo", json!({})).unwrap();
        assert!(s.project.item(b).is_none());
        s.execute("edit.redo", json!({})).unwrap();
        assert_eq!(s.project.item(b).unwrap().as_media().unwrap().interpret.linked_composition, Some(9));
    }

    #[test]
    fn one_deleted_composition_does_not_block_its_sibling_refresh() {
        let mut s = Session::default();
        let a = link(&mut s, 5, "First");
        let b = link(&mut s, 9, "Second");
        s.saved_revision = s.revision;
        let op = Operation::Refresh { items: links(&s) };
        apply(
            &mut s,
            &op,
            Prepared {
                catalog: None,
                sources: vec![
                    (Some(a), "source.ecproj".into(), 5, Err("Composition was deleted".into())),
                    (Some(b), "source.ecproj".into(), 9, Ok(source("Renamed in EffectCraft"))),
                ],
            },
        )
        .unwrap();
        assert!(s.media.offline_status(a).is_some());
        assert!(s.media.offline_status(b).is_none());
        assert_eq!(s.project.item(b).unwrap().name, "Second", "source rename must not rename the edit's item");
        assert_eq!(s.project.item(b).unwrap().as_media().unwrap().info.name, "Renamed in EffectCraft");
        assert!(!s.is_dirty(), "automatic refresh must not dirty a clean edit");
    }

    #[test]
    fn late_failed_refresh_cannot_poison_a_retargeted_item() {
        let mut s = Session::default();
        let a = link(&mut s, 5, "First");
        let op = Operation::Refresh { items: links(&s) };
        Arc::make_mut(&mut s.project).item_mut(a).unwrap().as_media_mut().unwrap().interpret.linked_composition = Some(8);
        apply(&mut s, &op, Prepared { catalog: None, sources: vec![(Some(a), "source.ecproj".into(), 5, Err("old worker failed".into()))] }).unwrap();
        assert!(s.media.offline_status(a).is_none());
        assert_eq!(s.project.item(a).unwrap().as_media().unwrap().interpret.linked_composition, Some(8));
    }

    #[test]
    fn unavailable_private_export_renderer_never_uses_cached_preview() {
        let mut s = Session::default();
        let a = link(&mut s, 5, "First");
        s.media.set_composition_renderer("this-application-does-not-exist.exe");
        let src = s.media.own_source(&s.project, a, &*s.services).expect("a linked export must get a source even on failure");
        assert!(src.video_frame(FrameRequest::full(Tick::ZERO)).is_err());
        assert!(s.media.cached(a).unwrap().video_frame(FrameRequest::full(Tick::ZERO)).is_ok(), "preview can remain independently usable");
    }

    #[test]
    fn real_export_reports_unavailable_link_instead_of_delivering_black_or_silence() {
        let mut s = Session::default();
        let item = link(&mut s, 5, "Linked title");
        s.execute("file.newSequence", json!({"name":"Linked edit","fromItem":item.0})).unwrap();
        s.media.set_composition_renderer("this-application-does-not-exist.exe");
        let dir = filmcraft_testkit::fixtures_dir("engine/linked-compositions");
        for (format, name) in [("png", "failed.png"), ("wav", "failed.wav")] {
            let result =
                s.execute("file.exportMedia", json!({"path":dir.join(name),"format":format,"wait":true,"range":"custom","startSeconds":0,"endSeconds":0.1}));
            let error = result.expect_err("unavailable linked rendering must fail the actual export command").to_string();
            assert!(error.contains("Linked composition"), "actionable source failure must survive the render/export pipeline: {error}");
        }
    }

    #[test]
    fn repair_rejects_present_invalid_composition_instead_of_falling_back() {
        let mut s = Session::default();
        let item = link(&mut s, 9, "Current");
        for bad_id in [json!(-1), json!(null), json!("9"), json!(0)] {
            let result = s.execute("composition.relink", json!({"item":item.0,"path":"new.ecproj","composition":bad_id}));
            assert!(result.is_err());
            assert_eq!(s.project.item(item).unwrap().as_media().unwrap().interpret.linked_composition, Some(9));
        }
    }

    #[test]
    fn automatic_refresh_keeps_document_relative_path_resolution() {
        let mut s = Session::default();
        let id = link(&mut s, 5, "Relative source");
        let document = std::env::temp_dir().join("linked-project-folder").join("edit.fcproj").to_string_lossy().into_owned();
        s.media.add_stored_paths(["source.ecproj".into()], Some(&document));
        let before = s.media.follow("source.ecproj").unwrap();
        let op = Operation::Refresh { items: links(&s) };
        apply(&mut s, &op, Prepared { catalog: None, sources: vec![(Some(id), "source.ecproj".into(), 5, Ok(source("Refreshed")))] }).unwrap();
        assert_eq!(s.media.follow("source.ecproj").unwrap(), before, "refresh must not turn a document-relative path into a CWD-relative explicit import");
    }

    #[test]
    fn invalid_link_parameters_do_not_change_the_project() {
        let mut s = Session::default();
        let before = s.project.to_json();
        for p in [json!({}), json!({"path":"a.ecproj","composition":0}), json!({"path":"a.mp4","composition":1}), json!({"path":"a.ecproj","composition":-4})] {
            assert!(s.execute("file.linkComposition", p).is_err());
            assert_eq!(s.project.to_json(), before);
        }
    }
}
