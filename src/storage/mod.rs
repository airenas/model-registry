pub mod lock;
pub mod s3;
pub mod utils;

use std::error::Error;
use std::path::Path;

pub type StorageResult<T> = Result<T, Box<dyn Error>>;

/// Called with the number of bytes transferred since the previous call.
pub type OnBytes<'a> = &'a (dyn Fn(u64) + Send + Sync);

/// Backend-agnostic blob storage (S3 now, filesystem possible later).
/// Implementations are shared between worker threads.
pub trait Storage: Send + Sync {
    fn put(&self, key: &str, data: &[u8]) -> StorageResult<()>;
    /// Uploads a local file without loading it fully into memory.
    fn put_file(&self, key: &str, path: &Path, on_bytes: OnBytes) -> StorageResult<()>;
    fn get(&self, key: &str) -> StorageResult<Vec<u8>>;
    /// Downloads an object to a local file, streaming it to disk.
    fn get_file(&self, key: &str, path: &Path, on_bytes: OnBytes) -> StorageResult<()>;
    fn exists(&self, key: &str) -> StorageResult<bool>;
    fn list(&self, prefix: &str) -> StorageResult<Vec<String>>;
    /// Names of the direct "sub folders" under the prefix (`""` for the top level), sorted,
    /// without the prefix and the trailing slash.
    fn list_dirs(&self, prefix: &str) -> StorageResult<Vec<String>>;
    /// Deletes all objects with the key prefix and returns how many were deleted.
    fn delete_prefix(&self, prefix: &str) -> StorageResult<usize>;
}

#[cfg(test)]
pub(crate) mod mem {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct MemStorage(pub Arc<Mutex<HashMap<String, Vec<u8>>>>);

    impl MemStorage {
        pub fn len(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    impl Storage for MemStorage {
        fn put(&self, key: &str, data: &[u8]) -> StorageResult<()> {
            self.0.lock().unwrap().insert(key.into(), data.to_vec());
            Ok(())
        }
        fn put_file(&self, key: &str, path: &Path, on_bytes: OnBytes) -> StorageResult<()> {
            let data = std::fs::read(path)?;
            self.put(key, &data)?;
            on_bytes(data.len() as u64);
            Ok(())
        }
        fn get(&self, key: &str) -> StorageResult<Vec<u8>> {
            self.0
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or("not found".into())
        }
        fn get_file(&self, key: &str, path: &Path, on_bytes: OnBytes) -> StorageResult<()> {
            let data = self.get(key)?;
            std::fs::write(path, &data)?;
            on_bytes(data.len() as u64);
            Ok(())
        }
        fn exists(&self, key: &str) -> StorageResult<bool> {
            Ok(self.0.lock().unwrap().contains_key(key))
        }
        fn list(&self, prefix: &str) -> StorageResult<Vec<String>> {
            let mut keys: Vec<String> = self
                .0
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect();
            keys.sort();
            Ok(keys)
        }
        fn list_dirs(&self, prefix: &str) -> StorageResult<Vec<String>> {
            let dirs: std::collections::BTreeSet<String> = self
                .0
                .lock()
                .unwrap()
                .keys()
                .filter_map(|k| k.strip_prefix(prefix))
                .filter_map(|rest| rest.split_once('/').map(|(dir, _)| dir.to_string()))
                .collect();
            Ok(dirs.into_iter().collect())
        }
        fn delete_prefix(&self, prefix: &str) -> StorageResult<usize> {
            let mut m = self.0.lock().unwrap();
            let before = m.len();
            m.retain(|k, _| !k.starts_with(prefix));
            Ok(before - m.len())
        }
    }
}
