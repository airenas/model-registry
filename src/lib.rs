pub mod commands;
pub mod hotreload;
pub mod models;
pub mod parallel;
pub mod progress;
pub mod storage;

use std::path::Path;

pub use commands::ls::human_size;

pub trait LoadableModel: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    fn load_from_disk(path: &Path, version: &str) -> Result<Self, Self::Error>
    where
        Self: Sized;
    fn validate(&self) -> Result<(), Self::Error>;
}
