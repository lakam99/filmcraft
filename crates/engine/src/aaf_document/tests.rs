use super::*;
use crate::media_test_util::{make_movie, session_with, tmp_dir};
use filmcraft_media::DemoScene;
use filmcraft_time::{FrameRate, Tick};

fn fixture(tag: &str, embedded: bool) -> (Session, std::path::PathBuf) {
    let dir = tmp_dir(tag);
    let movie = dir.join("boom.mov");
    make_movie(&movie, DemoScene::Aurora, 64, 36, 48);
    let (mut original, _, sequence) = session_with(&[&movie]);
    let path = dir.join("Studio session.aaf");
    original
        .execute(
            "file.exportAaf",
            json!({"path":path.to_string_lossy(),"sequence":sequence.0,
        "audio":if embedded {"embedded"} else {"linked"},"preserveMix":false,"trimAudio":false}),
        )
        .unwrap();
    let mut opened = Session::default();
    opened.execute("file.open", json!({"path":path.to_string_lossy()})).unwrap();
    (opened, path)
}

fn align_audio(s: &mut Session, start: Tick) {
    let seq = s.state.active_sequence.unwrap();
    s.edit("Align lav", |project, _| {
        project.sequence_mut(seq).unwrap().audio_tracks[0].items[0].start = start;
        Ok(())
    })
    .unwrap();
}

#[test]
fn open_edit_save_reopen_aaf_without_an_export_command() {
    let (mut s, path) = fixture("aaf-document-save", false);
    assert_eq!(s.path.as_deref(), path.to_str());
    assert!(!s.is_dirty());
    let original = std::fs::read(&path).unwrap();
    let aligned = FrameRate::FPS_24.tick_of(12);
    align_audio(&mut s, aligned);
    let saved = s.execute("file.save", json!({})).unwrap();
    assert!(!s.is_dirty());
    let backup = saved["backup"].as_str().unwrap();
    assert_eq!(std::fs::read(backup).unwrap(), original);
    let bytes = std::fs::read(&path).unwrap();
    assert!(filmcraft_interchange::aaf::sniff(&bytes), "Save wrote real AAF, not JSON with an AAF extension");
    let mut reopened = Session::default();
    reopened.execute("file.open", json!({"path":path.to_string_lossy()})).unwrap();
    let seq = reopened.active_sequence().unwrap();
    assert_eq!(seq.audio_tracks[0].items[0].start, aligned);
    align_audio(&mut s, FrameRate::FPS_24.tick_of(24));
    assert_eq!(s.execute("file.save", json!({})).unwrap()["backup"], saved["backup"]);
    assert_eq!(std::fs::read(backup).unwrap(), original, "subsequent saves keep the original backup");
}

#[test]
fn embedded_documents_and_save_as_keep_embedded_audio() {
    let (mut s, path) = fixture("aaf-document-embedded", true);
    let (_, before, _) = filmcraft_interchange::aaf::import(&std::fs::read(&path).unwrap(), &Default::default()).unwrap();
    let copy = path.with_file_name("Aligned lav.AAF");
    align_audio(&mut s, FrameRate::FPS_24.tick_of(7));
    s.execute("file.saveAs", json!({"path":copy.to_string_lossy()})).unwrap();
    assert_eq!(s.path.as_deref(), copy.to_str());
    let (_, extracted, _) = filmcraft_interchange::aaf::import(&std::fs::read(&copy).unwrap(), &Default::default()).unwrap();
    assert!(!extracted.is_empty());
    assert_eq!(
        filmcraft_interchange::wav::parse_wav(&extracted[0].wav),
        filmcraft_interchange::wav::parse_wav(&before[0].wav),
        "ordinary edits preserve the embedded samples, rate and bit depth without re-encoding"
    );
    assert!(!s.is_dirty());
}

#[test]
fn reopening_embedded_audio_reuses_identical_files_and_preserves_modified_files() {
    fn extracted(s: &Session) -> String {
        s.project
            .items
            .values()
            .filter_map(|item| item.as_media())
            .find_map(|media| match &media.media {
                filmcraft_project::MediaRef::File { path } if path.ends_with(".wav") => Some(path.clone()),
                _ => None,
            })
            .unwrap()
    }
    let (mut s, path) = fixture("aaf-document-reopen", true);
    let first = extracted(&s);
    let original = std::fs::read(&first).unwrap();
    s.execute("file.open", json!({"path":path})).unwrap();
    assert_eq!(extracted(&s), first, "opening again must not accumulate duplicate audio");
    std::fs::write(&first, b"independent edits to extracted audio").unwrap();
    s.execute("file.open", json!({"path":path})).unwrap();
    let second = extracted(&s);
    assert_ne!(second, first);
    assert_eq!(std::fs::read(&first).unwrap(), b"independent edits to extracted audio");
    assert_eq!(std::fs::read(&second).unwrap(), original);
    s.execute("file.open", json!({"path":path})).unwrap();
    assert_eq!(extracted(&s), second);
}

#[test]
fn save_copy_leaves_the_document_dirty_and_native_save_as_changes_format() {
    let (mut s, path) = fixture("aaf-document-copy", false);
    let original = std::fs::read(&path).unwrap();
    align_audio(&mut s, FrameRate::FPS_24.tick_of(9));
    let copy = path.with_file_name("Aligned copy.aaf");
    s.execute("file.saveCopy", json!({"path":copy.to_string_lossy()})).unwrap();
    assert!(s.is_dirty());
    assert_eq!(s.path.as_deref(), path.to_str());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let native = path.with_extension("fcproj");
    s.execute("file.saveAs", json!({"path":native.to_string_lossy()})).unwrap();
    assert_eq!(s.path.as_deref(), native.to_str());
    assert!(filmcraft_format::decode(&std::fs::read(native).unwrap()).is_ok());
}

#[test]
fn external_changes_and_corrupt_opens_leave_the_current_document_intact() {
    let (mut s, path) = fixture("aaf-document-external", false);
    align_audio(&mut s, FrameRate::FPS_24.tick_of(8));
    let project = s.project.clone();
    let revision = s.revision;
    std::fs::write(&path, b"external edits must not be overwritten").unwrap();
    assert!(s.execute("file.save", json!({})).unwrap_err().to_string().contains("outside FilmCraft"));
    assert!(s.execute("file.open", json!({"path":path.to_string_lossy()})).is_err());
    assert!(Arc::ptr_eq(&project, &s.project));
    assert_eq!(s.revision, revision);
    assert!(s.is_dirty());
    assert_eq!(std::fs::read(&path).unwrap(), b"external edits must not be overwritten");
}

#[test]
fn aaf_save_refuses_to_discard_another_independent_sequence() {
    let (mut s, path) = fixture("aaf-document-multiple", false);
    let original = std::fs::read(&path).unwrap();
    s.execute("file.newSequence", json!({"name":"Another independent sequence"})).unwrap();
    assert!(s.execute("file.save", json!({})).unwrap_err().to_string().contains("top-level"));
    assert_eq!(std::fs::read(path).unwrap(), original);
    assert!(s.is_dirty());
}

#[test]
fn background_completion_never_marks_a_new_document_or_new_edits_clean() {
    let (mut s, path) = fixture("aaf-document-completion", false);
    align_audio(&mut s, FrameRate::FPS_24.tick_of(8));
    let pending = Pending {
        job: 1,
        epoch: s.document_epoch,
        revision: s.revision,
        previous_path: s.path.clone(),
        path: path.to_string_lossy().into_owned(),
        adopt: true,
        embedded: false,
        backup: None,
    };
    align_audio(&mut s, FrameRate::FPS_24.tick_of(10));
    complete(&mut s, &pending, &json!({"documentFingerprint":42}));
    assert!(s.is_dirty());
    assert_eq!(s.saved_revision, pending.revision);
    s.execute("file.newProject", json!({"name":"Other work"})).unwrap();
    let revision = s.saved_revision;
    complete(&mut s, &pending, &json!({"documentFingerprint":42}));
    assert_eq!(s.path, None);
    assert_eq!(s.saved_revision, revision);
}

#[test]
fn running_save_keeps_the_document_available_until_acknowledged() {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s.aaf_document.pending.push(Pending {
        job: 1,
        epoch: s.document_epoch,
        revision: s.revision,
        previous_path: s.path.clone(),
        path: "pending.aaf".into(),
        adopt: true,
        embedded: false,
        backup: None,
    });
    s.jobs.push(crate::Job { id: 1, label: "Save AAF".into(), progress: Arc::default(), result: Arc::default() });
    let project = s.project.clone();
    for command in ["file.open", "file.closeProject", "file.newProject", "file.revert", "file.recover"] {
        assert!(s.execute(command, json!({"path":"other.fcproj"})).unwrap_err().to_string().contains("still running"));
        assert!(Arc::ptr_eq(&project, &s.project));
    }
    assert_eq!(s.execute("file.saveAs", json!({"path":"pending.aaf"})).unwrap()["job"], 1);
    assert!(s.execute("file.saveAs", json!({"path":"another.aaf"})).is_err());
}

#[test]
fn unsupported_imports_and_mixer_edits_do_not_overwrite_the_source() {
    let (mut s, path) = fixture("aaf-document-loss-guard", false);
    let original = std::fs::read(&path).unwrap();
    s.aaf_document.origin.as_mut().unwrap().warnings.push("unknown foreign operation".into());
    align_audio(&mut s, FrameRate::FPS_24.tick_of(6));
    assert!(s.execute("file.save", json!({})).unwrap_err().to_string().contains("unsupported"));
    s.aaf_document.origin.as_mut().unwrap().warnings.clear();
    let seq = s.state.active_sequence.unwrap();
    s.edit("Track gain", |project, _| {
        project.sequence_mut(seq).unwrap().audio_tracks[0].volume_db = -6.0;
        Ok(())
    })
    .unwrap();
    assert!(s.execute("file.save", json!({})).unwrap_err().to_string().contains("mixer processing"));
    assert_eq!(std::fs::read(path).unwrap(), original);
    assert!(s.is_dirty());
}

#[test]
fn oversized_input_is_refused_before_reading_it() {
    struct Huge;
    impl Services for Huge {
        fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
            panic!("must not allocate the oversized file")
        }
        fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
            panic!("must not write")
        }
        fn file_size(&self, _: &str) -> std::io::Result<u64> {
            Ok(MAX_DOCUMENT_BYTES + 1)
        }
    }
    let mut s = Session::new(Arc::new(Huge));
    let before = s.project.clone();
    assert!(s.execute("file.open", json!({"path":"huge.aaf"})).unwrap_err().to_string().contains("256 MiB"));
    assert!(Arc::ptr_eq(&before, &s.project));
}

#[test]
fn an_added_lav_returns_as_a_separate_aligned_editable_aaf_track() {
    let (mut s, path) = fixture("aaf-document-new-lav", false);
    let lav = path.with_file_name("lav.mov");
    make_movie(&lav, DemoScene::Plasma, 64, 36, 24);
    let imported = s.execute("file.import", json!({"paths":[lav.to_string_lossy()]})).unwrap();
    let item = imported["items"][0].as_u64().unwrap();
    s.execute("sequence.addTracks", json!({"video":0,"audio":1})).unwrap();
    s.execute("timeline.place", json!({"item":item,"track":"A2","frame":12,"video":false})).unwrap();
    s.execute("file.save", json!({})).unwrap();
    let mut reopened = Session::default();
    reopened.execute("file.open", json!({"path":path.to_string_lossy()})).unwrap();
    let q = reopened.active_sequence().unwrap();
    assert_eq!(q.audio_tracks.len(), 2);
    let clip = &q.audio_tracks[1].items[0];
    assert_eq!(clip.start, FrameRate::FPS_24.tick_of(12));
    assert_eq!(clip.duration, FrameRate::FPS_24.tick_of(24));
    let media = reopened.project.item(clip.item).unwrap().as_media().unwrap();
    assert!(matches!(&media.media, filmcraft_project::MediaRef::File { path } if path.ends_with("lav.mov")));
}

#[test]
fn recovery_and_autosave_never_replace_the_aaf_with_native_json() {
    let (mut s, path) = fixture("aaf-document-autosave", false);
    let original = std::fs::read(&path).unwrap();
    s.start_autosave(crate::autosave::AutosaveConfig::new(path.with_file_name("editor-data"))).unwrap();
    let mut prefs = s.prefs.clone();
    prefs.auto_save.save_current_project = true;
    s.set_prefs(prefs).unwrap();
    align_audio(&mut s, FrameRate::FPS_24.tick_of(12));
    s.sync_persistence();
    let saved = s.execute("file.autoSaveNow", json!({})).unwrap();
    assert!(filmcraft_format::decode(&std::fs::read(saved["path"].as_str().unwrap()).unwrap()).is_ok());
    s.poll_persistence();
    assert_eq!(std::fs::read(path).unwrap(), original);
    assert!(s.is_dirty(), "a native recovery copy is not an acknowledged AAF Save");
    s.shutdown();
}

#[test]
fn recovery_without_the_original_checkpoint_cannot_overwrite_the_aaf() {
    let (mut s, path) = fixture("aaf-document-recovery-origin", true);
    let original = std::fs::read(&path).unwrap();
    align_audio(&mut s, FrameRate::FPS_24.tick_of(12));
    // install_project restores native recovery data without a live AAF origin checkpoint.
    let project = (*s.project).clone();
    crate::commands::install_project(&mut s, project, Some(path.to_string_lossy().into_owned()), false);
    assert!(s.execute("file.save", json!({})).unwrap_err().to_string().contains("checkpoint"));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(s.is_dirty());
    let native = path.with_file_name("Recovered.fcproj");
    s.execute("file.saveAs", json!({"path":native})).unwrap();
    assert!(!s.is_dirty());
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
fn failed_disk_write_keeps_the_source_and_dirty_revision() {
    struct Refuse;
    impl Services for Refuse {
        fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
            crate::FsServices.read_file(path)
        }
        fn file_size(&self, path: &str) -> std::io::Result<u64> {
            crate::FsServices.file_size(path)
        }
        fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
            Err(std::io::ErrorKind::PermissionDenied.into())
        }
        fn write_file_new(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
            crate::FsServices.write_file_new(path, data)
        }
    }
    let (_, path) = fixture("aaf-document-write-failure", false);
    let original = std::fs::read(&path).unwrap();
    let mut s = Session::new(Arc::new(Refuse));
    s.execute("file.open", json!({"path":path.to_string_lossy()})).unwrap();
    align_audio(&mut s, FrameRate::FPS_24.tick_of(12));
    let before = s.saved_revision;
    assert!(s.execute("file.save", json!({})).is_err());
    assert_eq!(s.saved_revision, before);
    assert!(s.is_dirty());
    assert_eq!(std::fs::read(path).unwrap(), original);
}
