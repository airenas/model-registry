use anyhow::{Context, anyhow, bail};
use model_registry::LoadableModel;
use model_registry::hotreload::manager::{ModelManager, Options};
use model_registry::storage::s3::S3Storage;
use std::io;
use std::path::Path;
use std::time::Duration;

struct SampleModel {
    version: String,
    message: String,
}

impl LoadableModel for SampleModel {
    type Error = io::Error;

    fn load_from_disk(path: &Path, version: &str) -> Result<Self, Self::Error> {
        log::info!("Loading model from disk at path: {:?}", path);
        let contents = std::fs::read(path.join("model"))?;
        Ok(Self {
            version: version.to_string(),
            message: String::from_utf8(contents)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        })
    }

    fn validate(&self) -> Result<(), Self::Error> {
        log::info!("Validating model with version: {}", self.version);
        if self.message.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "model message must not be empty",
            ));
        }
        Ok(())
    }
}

const USAGE: &str = "usage: mr-simple-test <bucket> <model> <tag> [interval-secs] [cache-dir]";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("Starting mr-simple-test");

    if let Err(e) = main_int().await {
        log::error!("{}", e);
        return Err(e);
    }
    Ok(())
}

async fn main_int() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let bucket = args.next().ok_or_else(|| anyhow!(USAGE))?;
    let model = args.next().ok_or_else(|| anyhow!(USAGE))?;
    let tag = args.next().ok_or_else(|| anyhow!(USAGE))?;
    let interval_secs = args
        .next()
        .map_or(Ok(5), |value| value.parse())
        .context("invalid interval-secs")?;
    let cache_dir = args
        .next()
        .unwrap_or_else(|| ".model-registry-cache".into());
    if args.next().is_some() {
        bail!(USAGE);
    }

    // S3Storage blocks on its own runtime, which is not allowed on an async thread
    let storage =
        tokio::task::spawn_blocking(move || S3Storage::new(&bucket).map_err(|e| e.to_string()))
            .await?
            .map_err(|e| anyhow!(e))?;
    let manager = ModelManager::<SampleModel>::new(
        Box::new(storage),
        model,
        tag,
        cache_dir,
        Options { interval_secs, keep_versions: 2 },
    )
    .await
    .map_err(|e| anyhow!(e.to_string()))?;

    let mut displayed = String::new();
    loop {
        let state = {
            let current = manager.get_model();
            format!("{}: {}", current.version, current.message)
        };
        if state != displayed {
            log::info!("Loaded model {state}");
            displayed = state;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
