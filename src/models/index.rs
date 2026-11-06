use crate::models::consts::{MODELS_FILE, TOOL_VERSION};
use serde::{Deserialize, Serialize};

/// One model in the bucket-level models.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub name: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// Content of the bucket-level models.json: the list of initialized models, so listing
/// models does not need to scan the bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelsIndex {
    pub tool_version: String,
    pub created_at: String,
    pub models: Vec<ModelEntry>,
}

impl ModelsIndex {
    pub fn new(created_at: String) -> Self {
        Self {
            tool_version: TOOL_VERSION.to_string(),
            created_at,
            models: Vec::new(),
        }
    }

    /// Adds or replaces the entry with the same name, keeping the list sorted.
    pub fn upsert(&mut self, entry: ModelEntry) {
        self.models.retain(|m| m.name != entry.name);
        self.models.push(entry);
        self.models.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

pub fn parse(data: &[u8]) -> Result<ModelsIndex, serde_json::Error> {
    serde_json::from_slice(data)
}

pub const fn models_key() -> &'static str {
    MODELS_FILE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_keeps_sorted_and_unique() {
        let mut i = ModelsIndex::new("t".into());
        for n in ["b", "a", "b"] {
            i.upsert(ModelEntry {
                name: n.into(),
                created_at: "t".into(),
                description: String::new(),
            });
        }
        let names: Vec<_> = i.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(parse(&serde_json::to_vec(&i).unwrap()).unwrap(), i);
    }
}
