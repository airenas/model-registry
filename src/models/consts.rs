pub const LOCAL_DIR: &str = ".mr";
pub const VERSIONS_FILE: &str = "versions.json";
/// Bucket-level list of models.
pub const MODELS_FILE: &str = "models.json";
pub const ARTIFACTS_FILE: &str = "artifacts.json";
/// Version of this tool, recorded in versions.json and per version entry.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
