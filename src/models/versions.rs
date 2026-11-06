use crate::models::{consts::*, model_prefix};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One entry of a model's versions.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Version {
    pub version: String,
    pub tool_version: String,
    pub path: String,
    pub created_at: String,
    pub file_count: usize,
    pub total_size: u64,
    /// Copy of the note in the version's artifacts.json, so listing needs no extra reads.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// A named pointer to a version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "TagRepr")]
pub struct Tag {
    pub version: String,
    /// When the tag was set or last moved; empty for tags written by older versions.
    #[serde(default)]
    pub created_at: String,
}

/// Accepts the old plain-string form (`"prod": "v1"`) as well.
#[derive(Deserialize)]
#[serde(untagged)]
enum TagRepr {
    Full {
        version: String,
        #[serde(default)]
        created_at: String,
    },
    Plain(String),
}

impl From<TagRepr> for Tag {
    fn from(r: TagRepr) -> Self {
        match r {
            TagRepr::Full {
                version,
                created_at,
            } => Tag {
                version,
                created_at,
            },
            TagRepr::Plain(version) => Tag {
                version,
                created_at: String::new(),
            },
        }
    }
}

/// Content of a model's versions.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Versions {
    pub tool_version: String,
    pub created_at: String,
    pub latest_model_version: String,
    /// Free text describing the model itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Named pointers (e.g. `production`) to a version name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, Tag>,
    pub versions: Vec<Version>,
}

impl Versions {
    /// An empty registry; `created_at` is an RFC 3339 timestamp.
    pub fn new(created_at: String) -> Self {
        Self {
            tool_version: TOOL_VERSION.to_string(),
            created_at,
            latest_model_version: String::new(),
            description: String::new(),
            tags: BTreeMap::new(),
            versions: Vec::new(),
        }
    }

    pub fn find(&self, version: &str) -> Option<&Version> {
        self.versions.iter().find(|v| v.version == version)
    }

    /// Maps a version name, an alias or the built-in `latest` to a registered version name.
    pub fn resolve(&self, name: &str) -> Option<&str> {
        if let Some(v) = self.find(name) {
            return Some(&v.version);
        }
        let target = match self.tags.get(name) {
            Some(t) => &t.version,
            None if name == LATEST_ALIAS && !self.latest_model_version.is_empty() => {
                &self.latest_model_version
            }
            None => return None,
        };
        self.find(target).map(|v| v.version.as_str())
    }

    /// True if the name is taken by an alias (including the built-in `latest`).
    pub fn is_alias(&self, name: &str) -> bool {
        name == LATEST_ALIAS || self.tags.contains_key(name)
    }

    /// Names of the aliases pointing to a version.
    pub fn aliases_of(&self, version: &str) -> Vec<&str> {
        self.tags
            .iter()
            .filter(|(_, t)| t.version == version)
            .map(|(a, _)| a.as_str())
            .collect()
    }

    /// The name for an automatically numbered push: the latest version with its last number
    /// increased (`v1.2.3` -> `v1.2.4`), or `v0.0.1` for a model without versions.
    pub fn next_version(&self) -> Result<String, String> {
        let latest = &self.latest_model_version;
        if latest.is_empty() {
            return Ok(FIRST_VERSION.to_string());
        }
        let (prefix, numbers) = match latest.strip_prefix('v') {
            Some(rest) => ("v", rest),
            None => ("", latest.as_str()),
        };
        let mut parts = numbers
            .split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                format!("cannot increase version '{latest}', please provide the version name")
            })?;
        let last = parts.last_mut().ok_or("empty version")?;
        *last += 1;
        let numbers: Vec<String> = parts.iter().map(u64::to_string).collect();
        Ok(format!("{prefix}{}", numbers.join(".")))
    }

    /// Advice for a manually chosen version name; empty if it follows `vX.Y.Z` numbering
    /// relative to the latest version.
    pub fn version_warnings(&self, new: &str) -> Vec<String> {
        let Some(n) = parse_semver(new) else {
            return vec![format!("version '{new}' is not of the format vX.Y.Z")];
        };
        // an older, non-vX.Y.Z latest gives nothing to compare with
        let Some(l) = parse_semver(&self.latest_model_version) else {
            return Vec::new();
        };
        let latest = &self.latest_model_version;
        let mut w = Vec::new();
        if n.0 > l.0 {
            if n.0 != l.0 + 1 {
                w.push(format!(
                    "major version jumps by more than 1 ({latest} -> {new})"
                ));
            }
            if n.1 != 0 || n.2 != 0 {
                w.push(format!(
                    "major version increased ({latest} -> {new}), y and z are expected to be 0"
                ));
            }
        } else if n.0 < l.0 {
            w.push(format!("version {new} is lower than the latest {latest}"));
        } else if n.1 > l.1 {
            if n.1 != l.1 + 1 {
                w.push(format!(
                    "minor version jumps by more than 1 ({latest} -> {new})"
                ));
            }
            if n.2 != 0 {
                w.push(format!(
                    "minor version increased ({latest} -> {new}), z is expected to be 0"
                ));
            }
        } else if n.1 < l.1 {
            w.push(format!("version {new} is lower than the latest {latest}"));
        } else if n.2 != l.2 + 1 {
            w.push(format!(
                "patch version is expected to be {} after {latest}, got {}",
                l.2 + 1,
                n.2
            ));
        }
        w
    }

    /// Appends a version and marks it as the latest.
    pub fn add(&mut self, entry: Version) {
        self.latest_model_version = entry.version.clone();
        self.versions.push(entry);
    }

    /// Removes a version; if it was the latest, the most recently added remaining one becomes latest.
    pub fn remove(&mut self, version: &str) -> Option<Version> {
        let idx = self.versions.iter().position(|v| v.version == version)?;
        let removed = self.versions.remove(idx);
        if self.latest_model_version == version {
            self.latest_model_version = self
                .versions
                .last()
                .map(|v| v.version.clone())
                .unwrap_or_default();
        }
        Some(removed)
    }
}

/// Checks that a version name is usable as a folder name in the bucket.
/// Version argument of `push` asking for the next version number.
pub const AUTO_VERSION: &str = "auto";
pub const FIRST_VERSION: &str = "v0.0.1";

pub const LATEST_ALIAS: &str = "latest";

/// `vX.Y.Z` with numeric parts.
fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.strip_prefix('v')?.split('.');
    let mut num = || {
        let p = it.next()?;
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        p.parse::<u64>().ok()
    };
    let r = (num()?, num()?, num()?);
    it.next().is_none().then_some(r)
}

pub fn validate_version(version: &str) -> Result<(), String> {
    if version.is_empty()
        || version.contains(['/', ':'])
        || matches!(
            version,
            "." | ".." | VERSIONS_FILE | LOCK_FILE | AUTO_VERSION
        )
    {
        return Err(format!("invalid version name '{version}'"));
    }
    Ok(())
}

/// Returns the key of the model's versions file.
pub fn versions_key(model: &str) -> Result<String, String> {
    Ok(format!("{}/{VERSIONS_FILE}", model_prefix(model)?))
}

pub fn parse(data: &[u8]) -> Result<Versions, serde_json::Error> {
    serde_json::from_slice(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_latest(latest: &str) -> Versions {
        let mut v = Versions::new("2026-11-05T09:00:00Z".into());
        v.latest_model_version = latest.into();
        v
    }

    #[test]
    fn version_warnings() {
        let w = |latest: &str, new: &str| with_latest(latest).version_warnings(new).len();
        // follows the numbering
        assert_eq!(w("", "v0.0.1"), 0);
        assert_eq!(w("v1.2.3", "v1.2.4"), 0);
        assert_eq!(w("v1.2.3", "v1.3.0"), 0);
        assert_eq!(w("v1.2.3", "v2.0.0"), 0);
        // format
        assert_eq!(w("v1.2.3", "1.2.4"), 1);
        assert_eq!(w("v1.2.3", "v1.2"), 1);
        assert_eq!(w("v1.2.3", "v1.2.x"), 1);
        assert_eq!(w("v1.2.3", "v1.2.4.5"), 1);
        assert_eq!(w("", "model-a"), 1);
        // patch must be +1
        assert_eq!(w("v1.2.3", "v1.2.5"), 1);
        assert_eq!(w("v1.2.3", "v1.2.3"), 1);
        // minor +1 needs z = 0
        assert_eq!(w("v1.2.3", "v1.3.1"), 1);
        // major increase needs y = 0 and z = 0
        assert_eq!(w("v1.2.3", "v2.0.1"), 1);
        assert_eq!(w("v1.2.3", "v2.1.0"), 1);
        // going back or skipping
        assert_eq!(w("v1.2.3", "v1.1.0"), 1);
        assert_eq!(w("v1.2.3", "v0.9.9"), 1);
        assert_eq!(w("v1.2.3", "v1.4.0"), 1);
        assert_eq!(w("v1.2.3", "v3.0.0"), 1);
        assert_eq!(w("v1.2.3", "v3.1.0"), 2);
        // nothing to compare with
        assert_eq!(w("best", "v1.0.0"), 0);
    }

    #[test]
    fn next_version() {
        assert_eq!(with_latest("").next_version().unwrap(), "v0.0.1");
        assert_eq!(with_latest("v0.0.1").next_version().unwrap(), "v0.0.2");
        assert_eq!(with_latest("v1.2.9").next_version().unwrap(), "v1.2.10");
        assert_eq!(with_latest("v3").next_version().unwrap(), "v4");
        assert_eq!(with_latest("1.0").next_version().unwrap(), "1.1");
        assert!(with_latest("best").next_version().is_err());
        assert!(with_latest("v1.x").next_version().is_err());
    }
}
