//! Advisory lock kept as a small object in the storage, for backends without conditional
//! writes. Writers put their own id, wait a moment and read it back; only the one whose id
//! is still there continues. A heartbeat keeps the lock fresh during long operations, and a
//! lock that was not refreshed for `ttl` (crashed client) is taken over.

use super::{Storage, StorageResult};
use serde::{Deserialize, Serialize};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy)]
pub struct LockConfig {
    /// A lock not refreshed for this long is considered abandoned.
    pub ttl: Duration,
    pub heartbeat: Duration,
    /// Time given to a concurrent writer to overwrite our lock before we read it back.
    pub settle: Duration,
}

impl Default for LockConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(300),
            heartbeat: Duration::from_secs(10),
            settle: if cfg!(test) {
                Duration::ZERO
            } else {
                Duration::from_millis(500)
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct LockFile {
    id: String,
    owner: String,
    /// Unix seconds of the last refresh.
    acquired_at: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let h = blake3::hash(format!("{nanos}-{}", std::process::id()).as_bytes());
    h.to_hex()[..16].to_string()
}

fn owner() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
        .unwrap_or_else(|_| "unknown".into());
    format!("{user}@{host} (pid {})", std::process::id())
}

fn read(storage: &dyn Storage, key: &str) -> Option<LockFile> {
    if !storage.exists(key).ok()? {
        return None;
    }
    serde_json::from_slice(&storage.get(key).ok()?).ok()
}

fn write(storage: &dyn Storage, key: &str, lock: &LockFile) -> StorageResult<()> {
    storage.put(key, &serde_json::to_vec(lock)?)
}

/// Runs `f` while holding the lock object `key`; fails at once if somebody else holds it.
pub fn with_lock<T>(
    storage: &dyn Storage,
    key: &str,
    f: impl FnOnce() -> Result<T, Box<dyn std::error::Error>>,
) -> Result<T, Box<dyn std::error::Error>> {
    with_lock_cfg(storage, key, LockConfig::default(), f)
}

pub fn with_lock_cfg<T>(
    storage: &dyn Storage,
    key: &str,
    cfg: LockConfig,
    f: impl FnOnce() -> Result<T, Box<dyn std::error::Error>>,
) -> Result<T, Box<dyn std::error::Error>> {
    let mut mine = LockFile {
        id: new_id(),
        owner: owner(),
        acquired_at: now_secs(),
    };
    if let Some(other) = read(storage, key) {
        let age = now_secs().saturating_sub(other.acquired_at);
        if age < cfg.ttl.as_secs() {
            return Err(busy(key, &other, age, cfg));
        }
    }
    write(storage, key, &mine)?;
    std::thread::sleep(cfg.settle);
    match read(storage, key) {
        Some(l) if l.id == mine.id => {}
        Some(other) => return Err(busy(key, &other, 0, cfg)),
        None => return Err(format!("lock {key} disappeared while acquiring it").into()),
    }

    let (stop, stopped) = mpsc::channel::<()>();
    let result = std::thread::scope(|s| {
        let (id, owner) = (mine.id.clone(), mine.owner.clone());
        s.spawn(move || {
            let mut beat = LockFile {
                id,
                owner,
                acquired_at: 0,
            };
            while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(cfg.heartbeat) {
                if read(storage, key).is_some_and(|l| l.id == beat.id) {
                    beat.acquired_at = now_secs();
                    let _ = write(storage, key, &beat);
                }
            }
        });
        let r = f();
        let _ = stop.send(());
        r
    });
    mine.acquired_at = 0;
    if read(storage, key).is_some_and(|l| l.id == mine.id) {
        let _ = storage.delete_prefix(key);
    }
    result
}

fn busy(key: &str, other: &LockFile, age: u64, cfg: LockConfig) -> Box<dyn std::error::Error> {
    format!(
        "{key} is locked by {} ({age}s ago); another mr command is changing it. \
         A lock of a crashed process expires after {}s",
        other.owner,
        cfg.ttl.as_secs()
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mem::MemStorage;

    fn put_lock(mem: &MemStorage, id: &str, age: u64) {
        let l = LockFile {
            id: id.into(),
            owner: "bob@pc".into(),
            acquired_at: now_secs() - age,
        };
        write(mem, "m/.lock", &l).unwrap();
    }

    #[test]
    fn runs_and_releases() {
        let mem = MemStorage::default();
        let r = with_lock(&mem, "m/.lock", || {
            assert!(mem.exists("m/.lock").unwrap());
            Ok(7)
        })
        .unwrap();
        assert_eq!(r, 7);
        assert!(!mem.exists("m/.lock").unwrap());
    }

    #[test]
    fn releases_on_error() {
        let mem = MemStorage::default();
        let r: Result<(), _> = with_lock(&mem, "m/.lock", || Err("boom".into()));
        assert_eq!(r.unwrap_err().to_string(), "boom");
        assert!(!mem.exists("m/.lock").unwrap());
    }

    #[test]
    fn fails_when_locked_by_another() {
        let mem = MemStorage::default();
        put_lock(&mem, "other", 5);
        let mut ran = false;
        let err = with_lock(&mem, "m/.lock", || {
            ran = true;
            Ok(())
        })
        .unwrap_err()
        .to_string();
        assert!(!ran);
        assert!(err.contains("locked by bob@pc"), "{err}");
        // the other lock is untouched
        assert_eq!(read(&mem, "m/.lock").unwrap().id, "other");
    }

    #[test]
    fn takes_over_stale_lock() {
        let mem = MemStorage::default();
        put_lock(&mem, "other", 3600);
        with_lock(&mem, "m/.lock", || Ok(())).unwrap();
        assert!(!mem.exists("m/.lock").unwrap());
    }

    #[test]
    fn invalid_lock_file_is_taken_over() {
        let mem = MemStorage::default();
        mem.put("m/.lock", b"garbage").unwrap();
        with_lock(&mem, "m/.lock", || Ok(())).unwrap();
    }

    #[test]
    fn heartbeat_refreshes() {
        let mem = MemStorage::default();
        let cfg = LockConfig {
            heartbeat: Duration::from_millis(20),
            ..LockConfig::default()
        };
        with_lock_cfg(&mem, "m/.lock", cfg, || {
            let id = read(&mem, "m/.lock").unwrap().id;
            // make the lock look old; the heartbeat must refresh it
            let old = LockFile {
                id,
                owner: "me".into(),
                acquired_at: 1,
            };
            write(&mem, "m/.lock", &old).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            assert!(read(&mem, "m/.lock").unwrap().acquired_at > 1);
            Ok(())
        })
        .unwrap();
    }
}
