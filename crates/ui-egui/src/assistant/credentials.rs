//! A separate OS credential per editor preferences profile. No key enters preferences JSON.
use std::sync::{Arc, mpsc};

use sha2::{Digest, Sha256};

pub(super) enum Action {
    Load,
    Save(String),
    Delete,
}
enum Completed {
    Loaded(Option<String>),
    Saved([u8; 32]),
    Deleted,
}

trait Store: Send + Sync {
    fn load(&self) -> Result<Option<String>, String>;
    fn save(&self, key: &str) -> Result<(), String>;
    fn delete(&self) -> Result<(), String>;
}
struct OsStore {
    profile: std::path::PathBuf,
}
fn account(profile: &std::path::Path) -> Result<String, String> {
    let parent = profile.parent().ok_or_else(unavailable)?;
    // Resolve the directory, not the preferences file: first launch may not have saved it yet.
    std::fs::create_dir_all(parent).map_err(|_| unavailable())?;
    let directory = std::fs::canonicalize(parent).map_err(|_| unavailable())?;
    let path = directory.join(profile.file_name().ok_or_else(unavailable)?);
    Ok(format!("openai-{:x}", Sha256::digest(path.to_string_lossy().as_bytes())))
}
impl OsStore {
    fn entry(&self) -> Result<keyring::Entry, String> {
        // Never use keyring's mock fallback on a platform without an OS backend.
        if !cfg!(any(target_os = "windows", target_os = "macos", target_os = "linux")) {
            return Err("Secure key storage is unavailable on this operating system".into());
        }
        keyring::Entry::new("FilmCraft Fork / OpenAI", &account(&self.profile)?).map_err(|_| unavailable())
    }
}
fn unavailable() -> String {
    "The OS credential store is unavailable or locked. The key has not been saved to a file.".into()
}
impl Store for OsStore {
    fn load(&self) -> Result<Option<String>, String> {
        match self.entry()?.get_password() {
            Ok(key) => {
                super::api::validate_key(&key).map_err(|_| "The saved OpenAI key is invalid. Enter it again.".to_string())?;
                Ok(Some(key))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(unavailable()),
        }
    }
    fn save(&self, key: &str) -> Result<(), String> {
        self.entry()?.set_password(key).map_err(|_| unavailable())
    }
    fn delete(&self) -> Result<(), String> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(unavailable()),
        }
    }
}

#[derive(Default)]
pub(super) struct Vault {
    initialized: bool,
    store: Option<Arc<dyn Store>>,
    pending: Option<mpsc::Receiver<Result<Completed, String>>>,
    saved: Option<[u8; 32]>,
    pub message: String,
}
impl Vault {
    pub fn configured(&self) -> bool {
        self.store.is_some()
    }
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }
    pub fn saved(&self) -> bool {
        self.saved.is_some()
    }
    pub fn matches(&self, key: &str) -> bool {
        self.saved == Some(Sha256::digest(key.as_bytes()).into())
    }
    pub fn initialize(&mut self, path: Option<&std::path::Path>, remember: bool, ctx: &egui::Context) {
        if self.initialized {
            return;
        }
        self.initialized = true;
        let Some(path) = path else {
            return;
        };
        let Ok(profile) = std::path::absolute(path) else {
            self.message = unavailable();
            return;
        };
        self.store = Some(Arc::new(OsStore { profile }));
        let action = if remember { Action::Load } else { Action::Delete };
        if let Err(error) = self.start(action, ctx) {
            self.message = error;
        }
    }
    pub fn start(&mut self, action: Action, ctx: &egui::Context) -> Result<(), String> {
        if self.busy() {
            return Err("Wait for the OS credential operation to finish".into());
        }
        let store = self.store.clone().ok_or("This session has no preferences profile; the API key can only be kept in memory")?;
        if let Action::Save(key) = &action {
            super::api::validate_key(key)?;
        }
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("ai-credential-store".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match action {
                    Action::Load => store.load().map(Completed::Loaded),
                    Action::Save(key) => store.save(&key).map(|()| Completed::Saved(Sha256::digest(key.as_bytes()).into())),
                    Action::Delete => store.delete().map(|()| Completed::Deleted),
                }))
                .unwrap_or_else(|_| Err(unavailable()));
                let _ = sender.send(result);
                ctx.request_repaint();
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
            })
            .map_err(|_| "Cannot start the credential-store worker".to_string())?;
        self.pending = Some(receiver);
        self.message = "Accessing the OS credential store…".into();
        Ok(())
    }
    pub fn poll(&mut self, key: &mut String) {
        let Some(receiver) = &self.pending else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(unavailable()),
        };
        self.pending = None;
        match result {
            Ok(Completed::Loaded(Some(saved))) => {
                self.saved = Some(Sha256::digest(saved.as_bytes()).into());
                *key = saved;
                self.message = "API key loaded from the OS credential store".into();
            }
            Ok(Completed::Loaded(None)) => {
                self.saved = None;
                self.message = "No API key saved for this editor profile".into();
            }
            Ok(Completed::Saved(hash)) => {
                self.saved = Some(hash);
                self.message = "API key saved in the OS credential store".into();
            }
            Ok(Completed::Deleted) => {
                self.saved = None;
                self.message = "Saved API key removed".into();
            }
            Err(error) => self.message = error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, PoisonError};
    #[derive(Default)]
    struct MemoryStore(Mutex<Option<String>>);
    impl Store for MemoryStore {
        fn load(&self) -> Result<Option<String>, String> {
            Ok(self.0.lock().unwrap_or_else(PoisonError::into_inner).clone())
        }
        fn save(&self, key: &str) -> Result<(), String> {
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(key.into());
            Ok(())
        }
        fn delete(&self) -> Result<(), String> {
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
            Ok(())
        }
    }
    fn complete(vault: &mut Vault, key: &mut String) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while vault.busy() && std::time::Instant::now() < deadline {
            vault.poll(key);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!vault.busy());
    }
    #[test]
    fn save_reload_and_delete_use_the_store_without_serializing_secrets() {
        let store = Arc::new(MemoryStore::default());
        let mut vault = Vault { initialized: true, store: Some(store.clone()), ..Default::default() };
        let ctx = egui::Context::default();
        let secret = "sk-private-test-credential";
        let mut key = String::new();
        vault.start(Action::Save(secret.into()), &ctx).unwrap();
        complete(&mut vault, &mut key);
        assert!(vault.saved());
        assert!(!vault.message.contains(secret));
        let mut reopened = Vault { initialized: true, store: Some(store), ..Default::default() };
        reopened.start(Action::Load, &ctx).unwrap();
        complete(&mut reopened, &mut key);
        assert_eq!(key, secret);
        assert!(reopened.matches(secret));
        reopened.start(Action::Delete, &ctx).unwrap();
        complete(&mut reopened, &mut key);
        assert!(!reopened.saved());
        key.clear();
        reopened.start(Action::Load, &ctx).unwrap();
        complete(&mut reopened, &mut key);
        assert!(key.is_empty());
    }
    #[test]
    fn profile_identity_survives_the_first_preferences_save() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("preferences.json");
        let before = account(&path).unwrap();
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(before, account(&path).unwrap());
        assert_ne!(before, account(&root.path().join("other.json")).unwrap());
    }
}
