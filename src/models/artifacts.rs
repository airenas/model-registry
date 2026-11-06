use crate::models::{consts::*, model_prefix};
use serde::{Deserialize, Serialize};

/// One file of a version, stored in the version's artifacts.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    /// Path relative to the pushed directory, always with `/` separators.
    pub path: String,
    pub size: u64,
    /// File modification time, RFC 3339.
    pub modified_at: String,
    /// Hex encoded BLAKE3 hash of the content.
    pub blake3: String,
}

/// Content of a version's artifacts.json (also stored locally in `<dir>/.mr/`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifacts {
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Free text describing what changed in this version.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    pub files: Vec<Artifact>,
}

/// Content of `<model>/<alias>/artifacts.json`: only points to the real version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AliasLink {
    pub version: String,
    /// Folder of that version in the bucket.
    pub path: String,
}

/// Returns the key of the model's artifacts file.
pub fn artifacts_key(model: &str) -> Result<String, String> {
    Ok(format!("{}/{ARTIFACTS_FILE}", model_prefix(model)?))
}

pub fn parse(data: &[u8]) -> Result<Artifacts, serde_json::Error> {
    serde_json::from_slice(data)
}
