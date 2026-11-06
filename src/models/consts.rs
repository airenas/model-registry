pub const LOCAL_DIR: &str = ".mr";
pub const VERSIONS_FILE: &str = "versions.json";
/// Bucket-level list of models.
pub const MODELS_FILE: &str = "models.json";
/// Name of the lock object inside a model folder.
pub const LOCK_FILE: &str = ".lock";
/// Lock object guarding the bucket-level models.json.
pub const MODELS_LOCK: &str = ".models.lock";
pub const ARTIFACTS_FILE: &str = "artifacts.json";
/// Version of this tool, recorded in versions.json and per version entry.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
