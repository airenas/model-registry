pub mod artifacts;
pub mod consts;
pub mod index;
pub mod versions;

pub fn model_prefix(model: &str) -> Result<String, String> {
    let model = model.trim_matches('/');
    if model.is_empty() {
        return Err("model must not be empty".into());
    }
    if model.contains(':') {
        return Err(format!(
            "invalid model name '{model}': ':' separates model and version"
        ));
    }
    Ok(model.to_string())
}

/// A command line reference: `model` or `model:version` (the version may be a tag or `latest`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRef {
    pub model: String,
    pub version: Option<String>,
}

impl ModelRef {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.split_once(':') {
            None => Ok(Self {
                model: model_prefix(s)?,
                version: None,
            }),
            Some((_, "")) => Err(format!("empty version in '{s}', expected model:version")),
            Some((model, version)) => Ok(Self {
                model: model_prefix(model)?,
                version: Some(version.to_string()),
            }),
        }
    }

    /// For commands that always act on one version.
    pub fn parse_with_version(s: &str) -> Result<(String, String), String> {
        let r = Self::parse(s)?;
        match r.version {
            Some(v) => Ok((r.model, v)),
            None => Err(format!(
                "version is required, use model:version (got '{s}')"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_refs() {
        let r = ModelRef::parse("m1").unwrap();
        assert_eq!((r.model.as_str(), r.version), ("m1", None));
        let r = ModelRef::parse("m1:v0.0.1").unwrap();
        assert_eq!(
            (r.model.as_str(), r.version.as_deref()),
            ("m1", Some("v0.0.1"))
        );
        assert_eq!(
            ModelRef::parse_with_version("/m1/:auto").unwrap(),
            ("m1".to_string(), "auto".to_string())
        );
        assert!(ModelRef::parse_with_version("m1").is_err());
        assert!(ModelRef::parse("m1:").is_err());
        assert!(ModelRef::parse(":v1").is_err());
        assert!(ModelRef::parse("").is_err());
    }

    #[test]
    fn colon_is_not_allowed_in_names() {
        assert!(model_prefix("a:b").is_err());
        assert!(super::versions::validate_version("a:b").is_err());
    }
}
