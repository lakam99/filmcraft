use super::*;
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

const TOKEN: &str = "filmcraft-public-integration-test-token";
struct Host {
    child: Child,
    address: String,
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn host(project: &Path, root: &Path) -> Host {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/network-relay");
    assert!(directory.join("node_modules/redweb").exists(), "Run npm ci in tools/network-relay before this integration test");
    let script = "require('./relay.cjs').createRelay({project:process.env.TEST_PROJECT,root:process.env.TEST_ROOT,token:process.env.TEST_TOKEN,port:0,bind:'127.0.0.1'}).then(async a=>{await a.run();console.log('READY:'+a.server.address().port)}).catch(e=>{console.error(e);process.exit(1)})";
    let mut command = Command::new("node");
    command
        .current_dir(directory)
        .args(["-e", script])
        .env("TEST_PROJECT", project)
        .env("TEST_ROOT", root)
        .env("TEST_TOKEN", TOKEN)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut host = Host { child: command.spawn().expect("Node.js must be installed"), address: String::new() };
    let stdout = host.child.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(port) = line.strip_prefix("READY:") {
                let _ = sender.send(format!("http://127.0.0.1:{port}"));
                break;
            }
        }
    });
    host.address = receiver.recv_timeout(Duration::from_secs(10)).expect("RedWeb relay did not start");
    host
}
fn wave() -> Vec<u8> {
    let samples: Vec<u8> = (0..4800).flat_map(|n| if n % 100 < 50 { 8000_i16 } else { -8000_i16 }.to_le_bytes()).collect();
    let mut bytes = Vec::new();
    bytes.extend(b"RIFF");
    bytes.extend((36 + samples.len() as u32).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16_u32.to_le_bytes());
    bytes.extend(1_u16.to_le_bytes());
    bytes.extend(1_u16.to_le_bytes());
    bytes.extend(48000_u32.to_le_bytes());
    bytes.extend(96000_u32.to_le_bytes());
    bytes.extend(2_u16.to_le_bytes());
    bytes.extend(16_u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend((samples.len() as u32).to_le_bytes());
    bytes.extend(samples);
    bytes
}

#[test]
fn hostile_paths_and_addresses_are_rejected_without_io() {
    for path in ["../outside", "/etc/passwd", "C:\\outside", "a:b", "\\\\host\\share"] {
        assert!(key(Path::new(path)).is_err(), "{path}");
    }
    for path in ["../../secret", "/secret", "C:/secret", "file:///secret", "a\\b"] {
        assert!(resolve_key(Path::new("nested"), path).is_err(), "{path}");
    }
    assert_eq!(resolve_key(Path::new("nested"), "../media/take.wav").unwrap(), "media/take.wav");
    assert_eq!(relative_key(Path::new("nested"), Path::new("media/take.wav")).unwrap(), "../media/take.wav");
    let cache = tempfile::tempdir().unwrap();
    for address in ["file:///tmp", "http://user:pass@localhost/", "http://localhost/?token=secret", "http://localhost/path"] {
        assert!(Relay::connect(address, TOKEN.into(), cache.path(), Arc::new(filmcraft_engine::FsServices)).is_err());
    }
}

#[test]
#[ignore = "requires Node.js and npm ci in tools/network-relay; run with --include-ignored"]
fn real_redweb_open_seek_decode_edit_save_upload_conflict_autosave_and_disconnect() {
    let directory = tempfile::tempdir().unwrap();
    let wav = directory.path().join("take.wav");
    let wave = wave();
    std::fs::write(&wav, &wave).unwrap();
    let large: Vec<u8> = (0..CHUNK * 3 + 31).map(|i| (i % 251) as u8).collect();
    std::fs::write(directory.path().join("large.bin"), &large).unwrap();
    std::fs::create_dir(directory.path().join("edits")).unwrap();
    let project = directory.path().join("edits/cut.fcproj");
    let mut original = filmcraft_engine::Session::default();
    original.execute("file.import", json!({"paths":[wav.to_string_lossy()]})).unwrap();
    original.execute("file.saveAs", json!({"path":project.to_string_lossy()})).unwrap();
    let mut host = host(&project, directory.path());
    let client_a = tempfile::tempdir().unwrap();
    let client_b = tempfile::tempdir().unwrap();
    let connect = |cache: &Path| Relay::connect(&host.address, TOKEN.into(), cache, Arc::new(filmcraft_engine::FsServices)).unwrap();
    let a = connect(client_a.path());
    let b = connect(client_b.path());
    assert_ne!(a.mount(), b.mount());
    let services = RelayServices(a.clone());
    let remote = a.mount().join("take.wav").to_string_lossy().into_owned();
    assert_eq!(services.read_file(&remote).unwrap(), wave);
    assert_eq!(services.read_range(&remote, 45, 200).unwrap(), wave[45..245]);
    assert!(services.read_range(&remote, 0, usize::MAX).is_err(), "hostile read lengths must be refused before allocating");
    let reader = services.reader(&remote).unwrap().unwrap();
    let mut tail = [0; 4];
    reader.read_at(reader.len() - 4, &mut tail).unwrap();
    assert_eq!(&tail, &wave[wave.len() - 4..]);
    assert!(reader.read_at(u64::MAX, &mut tail).is_err());
    let big = services.reader(&a.mount().join("large.bin").to_string_lossy()).unwrap().unwrap();
    let mut crossing = vec![0; CHUNK + 30];
    big.read_at(CHUNK as u64 - 17, &mut crossing).unwrap();
    assert_eq!(crossing, large[CHUNK - 17..CHUNK * 2 + 13]);
    let mut session = filmcraft_engine::Session::new(Arc::new(RelayServices(a.clone())));
    session.start_autosave(filmcraft_engine::autosave::AutosaveConfig::new(client_a.path().join("recovery"))).unwrap();
    session.execute("file.open", json!({"path":a.project_path().to_string_lossy()})).unwrap();
    assert!(session.offline.missing.is_empty());
    let item = session.project.items.values().find(|i| i.as_media().is_some()).unwrap().id;
    let audio = session.source(item).unwrap().audio(0, 100, 48000).unwrap();
    assert!(audio.channels.iter().flatten().any(|sample| sample.abs() > 0.1), "the normal decoder must receive actual audio");
    session.execute("project.setNotes", json!({"text":"Edited on the other machine"})).unwrap();
    session.execute("file.save", json!({})).unwrap();
    assert!(!session.is_dirty());
    let host_bytes = std::fs::read(&project).unwrap();
    let saved = filmcraft_format::decode(&host_bytes).unwrap().project;
    assert_eq!(saved.notes, "Edited on the other machine");
    for media in saved.items.values().filter_map(|i| i.as_media()) {
        if let filmcraft_project::MediaRef::File { path } = &media.media {
            assert_eq!(path, "../take.wav");
        }
    }
    assert!(!String::from_utf8_lossy(&host_bytes).contains(TOKEN));
    let bpath = b.project_path();
    let mut stale = filmcraft_engine::Session::new(Arc::new(RelayServices(b)));
    stale.execute("file.open", json!({"path":bpath.to_string_lossy()})).unwrap();
    stale.execute("project.setNotes", json!({"text":"Stale editor"})).unwrap();
    assert!(stale.execute("file.save", json!({})).unwrap_err().to_string().contains("changed on the host"));
    assert!(stale.is_dirty());
    assert_eq!(std::fs::read(&project).unwrap(), host_bytes);
    let cache_before = std::fs::read(a.project_path()).unwrap();
    let mut prefs = session.prefs.clone();
    prefs.auto_save.save_current_project = true;
    session.set_prefs(prefs).unwrap();
    session.execute("project.setNotes", json!({"text":"Local recovery only"})).unwrap();
    session.execute("file.autoSaveNow", json!({})).unwrap();
    session.poll_persistence();
    assert!(session.is_dirty());
    assert_eq!(std::fs::read(&project).unwrap(), host_bytes);
    assert_eq!(std::fs::read(a.project_path()).unwrap(), cache_before);
    let added = client_a.path().join("new take.wav");
    std::fs::write(&added, &wave).unwrap();
    session.execute("file.import", json!({"paths":[added.to_string_lossy()]})).unwrap();
    session.execute("file.save", json!({})).unwrap();
    let final_bytes = std::fs::read(&project).unwrap();
    let final_project = filmcraft_format::decode(&final_bytes).unwrap().project;
    assert!(
        final_project
            .items
            .values()
            .filter_map(|i| i.as_media())
            .any(|m| matches!(&m.media, filmcraft_project::MediaRef::File { path } if path.contains("FilmCraft Incoming/")))
    );
    assert!(!String::from_utf8_lossy(&final_bytes).contains(&client_a.path().to_string_lossy().replace('\\', "\\\\")));
    host.child.kill().unwrap();
    host.child.wait().unwrap();
    assert!(services.file_size(&remote).is_err());
    session.execute("project.setNotes", json!({"text":"Unsaved after disconnect"})).unwrap();
    assert!(session.execute("file.save", json!({})).is_err());
    assert!(session.is_dirty());
    session.shutdown();
}
