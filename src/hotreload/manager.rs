use crate::models::artifacts::Artifacts;
use crate::models::consts::ARTIFACTS_FILE;
use crate::models::versions::{self, validate_version, versions_key};
use crate::storage::utils::{hash_file, safe_relative};
use arc_swap::ArcSwap;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::interval;

use crate::LoadableModel;
use crate::storage::Storage;

/// Downloaded versions kept in the cache directory: the current one and two previous ones.
const STATE_FILE: &str = "version.json";

pub struct Options {
    pub interval_secs: u64,
    pub keep_versions: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self { interval_secs: 600, keep_versions: 2 }
    }
}

pub struct ModelManager<M> {
    storage: Arc<dyn Storage>,
    model_name: String,
    tag: String,
    base_dir: PathBuf,
    opts: Options,

    current_model: ArcSwap<M>,
    /// Version of `current_model`; other processes may move version.json ahead of it.
    loaded_version: Mutex<String>,
}

impl<M: LoadableModel> ModelManager<M> {
    pub async fn new(
        storage: Box<dyn Storage>,
        model_name: impl Into<String>,
        model_tag: impl Into<String>,
        base_dir: impl Into<PathBuf>,
        opts: Options,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let model_name = model_name.into();
        let model_tag = model_tag.into();
        log::info!(
            "Initializing ModelManager for model: {}, tag: {}",
            model_name,
            model_tag
        );
        if opts.interval_secs == 0 {
            return Err("hot-reload interval must be greater than zero".into());
        }
        let base_dir = base_dir.into();
        let storage: Arc<dyn Storage> = storage.into();
        let (model, version) = blocking({
            let (storage, base_dir) = (storage.clone(), base_dir.clone());
            let (name, tag) = (model_name.clone(), model_tag.clone());
            move || {
                let _lock = lock_dir(&base_dir)?;
                let version = match read_state(&base_dir)? {
                    Some(state) => {
                        log::info!("Found local version: {}", state.current);
                        state.current
                    }
                    None => {
                        log::info!("No local version found, downloading model from remote");
                        let version = download(&base_dir, storage.as_ref(), &name, &tag)?;
                        validate_version(&version)?;
                        version
                    }
                };
                let model = load_validated::<M>(&base_dir, &version)?;
                log::info!("Model {} loaded from disk successfully", version);
                record_version(&base_dir, &version, true, true, opts.keep_versions)?;
                Ok((model, version))
            }
        })
        .await?;

        let manager = Arc::new(Self {
            storage,
            model_name,
            tag: model_tag,
            base_dir,
            opts,

            current_model: ArcSwap::from(Arc::new(model)),
            loaded_version: Mutex::new(version),
        });

        let bg_manager = Arc::clone(&manager);
        tokio::spawn(async move {
            bg_manager.poll_loop().await;
        });

        Ok(manager)
    }

    pub fn get_model(&self) -> arc_swap::Guard<Arc<M>> {
        self.current_model.load()
    }

    async fn poll_loop(&self) {
        log::info!(
            "Starting model hot-reload poll loop. Tick interval: {} seconds",
            self.opts.interval_secs
        );
        let mut ticker = interval(Duration::from_secs(self.opts.interval_secs));
        loop {
            ticker.tick().await;
            if let Err(e) = self.check_and_reload().await {
                log::error!("Model hot-reload check failed: {}", e);
            }
        }
    }

    async fn check_and_reload(&self) -> Result<(), Box<dyn std::error::Error>> {
        log::info!("Checking for model updates...");
        let (storage, base_dir) = (self.storage.clone(), self.base_dir.clone());
        let (name, tag) = (self.model_name.clone(), self.tag.clone());
        let loaded_version = self
            .loaded_version
            .lock()
            .map_err(|e| e.to_string())?
            .clone();
        let keep_versions = self.opts.keep_versions;
        let update = blocking(move || {
            let remote_version = resolve_version(storage.as_ref(), &name, &tag)?;
            log::info!("Resolved remote version: {}", remote_version);
            if loaded_version == remote_version {
                log::info!("No new version available");
                return Ok(None);
            }
            // Held until version.json is saved, so processes sharing the directory
            // neither download the same version at once nor read a half-written file.
            let _lock = lock_dir(&base_dir)?;
            let known = read_state(&base_dir)?
                .and_then(|s| s.versions.into_iter().find(|v| v.version == remote_version));
            if known.as_ref().is_some_and(|v| !v.validated) {
                log::warn!(
                    "Version {} failed validation before, skipping",
                    remote_version
                );
                return Ok(None);
            }
            if known.is_none() {
                log::info!("New version available: {}", remote_version);
                download(&base_dir, storage.as_ref(), &name, &tag)?;
            }
            match load_validated::<M>(&base_dir, &remote_version) {
                Ok(model) => {
                    log::info!("Loaded new model from disk successfully");
                    record_version(&base_dir, &remote_version, true, true, keep_versions)?;
                    Ok(Some((model, remote_version)))
                }
                Err(e) => {
                    record_version(&base_dir, &remote_version, false, false, keep_versions)?;
                    Err(e.into())
                }
            }
        })
        .await?;
        if let Some((model, version)) = update {
            self.current_model.store(Arc::new(model));
            *self.loaded_version.lock().map_err(|e| e.to_string())? = version;
        }
        Ok(())
    }
}

/// Storage backends block on their own runtime, so they must not run on async worker threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, Box<dyn std::error::Error>> + Send + 'static,
) -> Result<T, Box<dyn std::error::Error>> {
    tokio::task::spawn_blocking(move || f().map_err(|e| e.to_string()))
        .await?
        .map_err(Into::into)
}

fn download(
    base_dir: impl AsRef<Path>,
    storage: &dyn Storage,
    model_name: &str,
    model_tag: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let version = resolve_version(storage, model_name, model_tag)?;
    let versions_key = versions_key(model_name)?;
    let list = versions::parse(&storage.get(&versions_key)?)
        .map_err(|e| format!("invalid {versions_key}: {e}"))?;
    let entry = list
        .find(&version)
        .ok_or_else(|| format!("version '{version}' not found"))?;
    if !safe_relative(&entry.path) {
        return Err(format!("unsafe version path in {versions_key}: '{}'", entry.path).into());
    }

    let artifacts_key = format!("{}/{ARTIFACTS_FILE}", entry.path);
    let artifacts: Artifacts = serde_json::from_slice(&storage.get(&artifacts_key)?)
        .map_err(|e| format!("invalid {artifacts_key}: {e}"))?;
    if artifacts.version != version {
        return Err(format!(
            "{artifacts_key} is for version '{}', expected '{version}'",
            artifacts.version
        )
        .into());
    }

    let version_dir = base_dir.as_ref().join(&version);
    std::fs::create_dir_all(&version_dir)?;
    for artifact in artifacts.files {
        if !safe_relative(&artifact.path) {
            return Err(format!(
                "unsafe artifact path in {artifacts_key}: '{}'",
                artifact.path
            )
            .into());
        }
        let target = version_dir.join(&artifact.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        storage.get_file(
            &format!("{}/{}", entry.path, artifact.path),
            &target,
            &|_| {},
        )?;
        let actual_size = std::fs::metadata(&target)?.len();
        if actual_size != artifact.size {
            return Err(format!(
                "size mismatch for '{}': expected {}, got {actual_size}",
                artifact.path, artifact.size
            )
            .into());
        }
        let actual_hash = hash_file(&target)?;
        if actual_hash != artifact.blake3 {
            return Err(format!(
                "BLAKE3 mismatch for '{}': expected {}, got {actual_hash}",
                artifact.path, artifact.blake3
            )
            .into());
        }
    }
    Ok(version)
}

fn resolve_version(
    storage: &dyn Storage,
    model_name: &str,
    model_tag: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let key = versions_key(model_name)?;
    let list = versions::parse(&storage.get(&key)?).map_err(|e| format!("invalid {key}: {e}"))?;
    let version = list
        .resolve(model_tag)
        .ok_or_else(|| format!("version or tag '{model_tag}' not found"))?
        .to_string();
    validate_version(&version)?;
    Ok(version)
}

/// Content of the cache directory's version.json.
#[derive(Debug, Serialize, Deserialize)]
struct LocalState {
    current: String,
    /// Oldest first.
    versions: Vec<LocalVersion>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LocalVersion {
    version: String,
    downloaded_at: String,
    validated: bool,
}

fn read_state(base_dir: &Path) -> Result<Option<LocalState>, Box<dyn std::error::Error>> {
    let file = base_dir.join(STATE_FILE);
    if !file.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&file)?;
    if let Ok(state) = serde_json::from_str::<LocalState>(&content) {
        return Ok(Some(state));
    }
    // older format: {"version": "..."}
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(version) = json.get("version").and_then(|v| v.as_str())
    {
        return Ok(Some(LocalState {
            current: version.to_string(),
            versions: vec![LocalVersion {
                version: version.to_string(),
                downloaded_at: String::new(),
                validated: true,
            }],
        }));
    }
    Err(format!("invalid {}", file.display()).into())
}

fn load_validated<M: LoadableModel>(base_dir: &Path, version: &str) -> Result<M, M::Error> {
    let model = M::load_from_disk(&base_dir.join(version), version)?;
    model.validate()?;
    Ok(model)
}

/// Adds (or refreshes) a version in version.json, optionally making it the current one,
/// and deletes the oldest versions beyond `KEEP_VERSIONS`. Call with the directory locked.
fn record_version(
    base_dir: &Path,
    version: &str,
    validated: bool,
    make_current: bool,
    keep_versions: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    log::info!(
        "Saving local version: {} (validated: {})",
        version,
        validated
    );
    let mut state = read_state(base_dir)?.unwrap_or(LocalState {
        current: String::new(),
        versions: Vec::new(),
    });
    let downloaded_at = state
        .versions
        .iter()
        .find(|v| v.version == version)
        .map(|v| v.downloaded_at.clone())
        .unwrap_or_else(|| Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
    state.versions.retain(|v| v.version != version);
    state.versions.push(LocalVersion {
        version: version.to_string(),
        downloaded_at,
        validated,
    });
    if make_current {
        state.current = version.to_string();
    }

    while state.versions.len() > keep_versions {
        let Some(idx) = state
            .versions
            .iter()
            .position(|v| v.version != state.current)
        else {
            break;
        };
        let old = state.versions.remove(idx);
        log::info!("Deleting old local version {}", old.version);
        if validate_version(&old.version).is_ok()
            && let Err(e) = std::fs::remove_dir_all(base_dir.join(&old.version))
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("Failed to delete old version {}: {}", old.version, e);
        }
    }

    std::fs::create_dir_all(base_dir)?;
    // write and rename, so readers never see a partially written file
    let file = base_dir.join(STATE_FILE);
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&state)?)?;
    std::fs::rename(tmp, file)?;
    Ok(())
}

/// Takes an exclusive lock on the cache directory shared by processes using it;
/// the lock is released when the returned file is dropped.
fn lock_dir(base_dir: &Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(base_dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(base_dir.join(".lock"))?;
    file.lock()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::artifacts::{Artifact, Artifacts};
    use crate::models::versions::{Tag, Version, Versions};
    use crate::storage::Storage;
    use crate::storage::mem::MemStorage;
    use std::io;

    struct TestModel {
        version: String,
        content: String,
    }

    impl LoadableModel for TestModel {
        type Error = io::Error;

        fn load_from_disk(path: &Path, version: &str) -> Result<Self, Self::Error> {
            Ok(Self {
                version: version.to_string(),
                content: std::fs::read_to_string(path.join("model.txt"))?,
            })
        }

        fn validate(&self) -> Result<(), Self::Error> {
            if self.content.is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "empty model"));
            }
            Ok(())
        }
    }

    fn publish(storage: &MemStorage, versions: &[(&str, &str)], tagged_version: &str) {
        let mut registry = Versions::new("2026-01-01T00:00:00Z".into());
        for (version, content) in versions {
            let bytes = content.as_bytes();
            registry.add(Version {
                version: (*version).into(),
                tool_version: "test".into(),
                path: format!("example/{version}"),
                created_at: "2026-01-01T00:00:00Z".into(),
                file_count: 1,
                total_size: bytes.len() as u64,
                message: String::new(),
            });
            let artifacts = Artifacts {
                version: (*version).into(),
                model: "example".into(),
                message: String::new(),
                files: vec![Artifact {
                    path: "model.txt".into(),
                    size: bytes.len() as u64,
                    modified_at: "2026-01-01T00:00:00Z".into(),
                    blake3: blake3::hash(bytes).to_hex().to_string(),
                }],
            };
            storage
                .put(
                    &format!("example/{version}/{ARTIFACTS_FILE}"),
                    &serde_json::to_vec(&artifacts).unwrap(),
                )
                .unwrap();
            storage
                .put(&format!("example/{version}/model.txt"), bytes)
                .unwrap();
        }
        registry.tags.insert(
            "stable".into(),
            Tag {
                version: tagged_version.into(),
                created_at: "2026-01-01T00:00:00Z".into(),
            },
        );
        storage
            .put(
                "example/versions.json",
                &serde_json::to_vec(&registry).unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn dir_lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let held = lock_dir(dir.path()).unwrap();
        let other = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join(".lock"))
            .unwrap();
        assert!(other.try_lock().is_err());
        drop(held);
        assert!(other.try_lock().is_ok());
    }

    #[test]
    fn keeps_only_recent_versions_and_deletes_oldest() {
        let dir = tempfile::tempdir().unwrap();
        for v in ["v1", "v2", "v3", "v4"] {
            std::fs::create_dir_all(dir.path().join(v)).unwrap();
            record_version(dir.path(), v, true, true, 2).unwrap();
        }
        let state = read_state(dir.path()).unwrap().unwrap();
        let names: Vec<_> = state.versions.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(names, ["v2", "v3", "v4"]);
        assert_eq!(state.current, "v4");
        assert!(!dir.path().join("v1").exists());
        assert!(dir.path().join("v2").exists());
    }

    #[test]
    fn never_deletes_current_and_records_failed_validation() {
        let dir = tempfile::tempdir().unwrap();
        record_version(dir.path(), "v1", true, true, 2).unwrap();
        for v in ["v2", "v3", "v4"] {
            std::fs::create_dir_all(dir.path().join(v)).unwrap();
            record_version(dir.path(), v, false, false, 2).unwrap();
        }
        let state = read_state(dir.path()).unwrap().unwrap();
        let names: Vec<_> = state.versions.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(names, ["v1", "v3", "v4"]);
        assert_eq!(state.current, "v1");
        assert!(!state.versions[1].validated);
    }

    #[test]
    fn reads_old_version_file_format() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("version.json"), r#"{"version":"v9"}"#).unwrap();
        assert_eq!(read_state(dir.path()).unwrap().unwrap().current, "v9");
    }

    #[tokio::test]
    async fn manager_reloads_when_tag_moves() {
        let storage = MemStorage::default();
        publish(&storage, &[("v1", "first")], "v1");
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::<TestModel>::new(
            Box::new(storage.clone()),
            "example",
            "stable",
            dir.path(),
            Options { interval_secs: 60, keep_versions: 2 },
        )
        .await
        .unwrap();
        assert_eq!(manager.get_model().version, "v1");

        publish(&storage, &[("v1", "first"), ("v2", "second")], "v2");
        manager.check_and_reload().await.unwrap();
        let current = manager.get_model();
        assert_eq!(current.version, "v2");
        assert_eq!(current.content, "second");
    }
}
