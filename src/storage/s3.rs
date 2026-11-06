use super::utils::{MULTIPART_THRESHOLD, part_size_for, plan_parts};
use super::{OnBytes, Storage, StorageResult};
use aws_credential_types::provider::ProvideCredentials;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::primitives::{ByteStream, Length, SdkBody};
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier};
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use std::fmt;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::runtime::Runtime;

async fn load_config() -> aws_config::SdkConfig {
    aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await
}

/// With a custom endpoint (Garage, MinIO, ...) use path-style addressing like the
/// AWS CLI does; bucket subdomains usually do not resolve or have a valid certificate.
fn s3_client(cfg: &aws_config::SdkConfig) -> Client {
    let conf = aws_sdk_s3::config::Builder::from(cfg)
        .force_path_style(cfg.endpoint_url().is_some())
        .build();
    Client::from_conf(conf)
}

/// Resolved AWS connection settings (secrets are never included).
#[derive(Debug, Default, PartialEq)]
pub struct ConnectionInfo {
    pub region: Option<String>,
    pub profile: Option<String>,
    pub endpoint_url: Option<String>,
    pub access_key_id: Option<String>,
    pub credentials_error: Option<String>,
    pub bucket: Option<String>,
    pub bucket_check: Option<Result<(), String>>,
}

pub fn mask(s: &str) -> String {
    if s.len() <= 8 {
        return "*".repeat(s.len());
    }
    format!("{}...{}", &s[..4], &s[s.len() - 4..])
}

impl fmt::Display for ConnectionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = |o: &Option<String>| o.clone().unwrap_or_else(|| "<not set>".into());
        writeln!(f, "Region:        {}", v(&self.region))?;
        writeln!(f, "Profile:       {}", v(&self.profile))?;
        writeln!(
            f,
            "Endpoint:      {}",
            self.endpoint_url.clone().unwrap_or("<default>".into())
        )?;
        match (&self.access_key_id, &self.credentials_error) {
            (Some(k), _) => writeln!(f, "Access key id: {}", mask(k))?,
            (None, Some(e)) => writeln!(f, "Credentials:   not available ({e})")?,
            _ => writeln!(f, "Credentials:   not available")?,
        }
        let name = self.bucket.as_deref().unwrap_or("<unknown>");
        match &self.bucket_check {
            Some(Ok(())) => writeln!(f, "Bucket:        {name} (accessible)")?,
            Some(Err(e)) => writeln!(f, "Bucket:        {name} (not accessible: {e})")?,
            None => {}
        }
        Ok(())
    }
}

/// Loads the standard AWS config chain and reports what it resolved to.
/// If `bucket` is given, also checks that it can be accessed.
pub fn connection_info(bucket: Option<&str>) -> StorageResult<ConnectionInfo> {
    let rt = Runtime::new()?;
    rt.block_on(async {
        let cfg = load_config().await;
        let mut info = ConnectionInfo {
            region: cfg.region().map(|r| r.to_string()),
            profile: std::env::var("AWS_PROFILE").ok(),
            endpoint_url: cfg.endpoint_url().map(String::from),
            ..Default::default()
        };
        match cfg.credentials_provider() {
            Some(p) => match p.provide_credentials().await {
                Ok(c) => info.access_key_id = Some(c.access_key_id().to_string()),
                Err(e) => info.credentials_error = Some(DisplayErrorContext(&e).to_string()),
            },
            None => info.credentials_error = Some("no credentials provider".into()),
        }
        if let Some(b) = bucket {
            info.bucket = Some(b.to_string());
            let res = s3_client(&cfg).head_bucket().bucket(b).send().await;
            info.bucket_check = Some(
                res.map(|_| ())
                    .map_err(|e| DisplayErrorContext(e).to_string()),
            );
        }
        Ok(info)
    })
}

/// S3 storage for one bucket. Credentials/region come from the standard AWS
/// chain (env vars such as AWS_ACCESS_KEY_ID, ~/.aws/credentials, AWS_PROFILE, ...).
pub struct S3Storage {
    rt: Runtime,
    client: Client,
    bucket: String,
}

/// Parallel parts per large file; files themselves are parallelized by the commands.
const PART_CONCURRENCY: usize = 4;
const REPORT_INTERVAL: Duration = Duration::from_millis(100);

type TransferResult<T> = Result<T, String>;

fn err<E: std::error::Error + 'static>(e: E) -> String {
    DisplayErrorContext(e).to_string()
}

/// Wraps a request body and counts the bytes the HTTP client takes from it.
struct CountingBody {
    inner: SdkBody,
    sent: Arc<AtomicU64>,
}

impl http_body::Body for CountingBody {
    type Data = Bytes;
    type Error = <SdkBody as http_body::Body>::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        let poll = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &poll
            && let Some(data) = frame.data_ref()
        {
            self.sent.fetch_add(data.len() as u64, Ordering::Relaxed);
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Makes the stream count its bytes in `sent`; retries get a fresh body.
fn counted(stream: ByteStream, sent: &Arc<AtomicU64>) -> ByteStream {
    let inner = stream.into_inner();
    let sent = sent.clone();
    ByteStream::new(SdkBody::retryable(move || {
        let body = inner.try_clone().expect("file bodies are retryable");
        SdkBody::from_body_1_x(CountingBody {
            inner: body,
            sent: sent.clone(),
        })
    }))
}

/// Drives `fut` and reports the growth of `sent` to `on_bytes` while it runs.
async fn report_progress<F: std::future::Future>(
    fut: F,
    sent: &AtomicU64,
    on_bytes: OnBytes<'_>,
) -> F::Output {
    tokio::pin!(fut);
    let mut tick = tokio::time::interval(REPORT_INTERVAL);
    let mut reported = 0;
    let flush = |reported: &mut u64| {
        let now = sent.load(Ordering::Relaxed);
        if now > *reported {
            on_bytes(now - *reported);
            *reported = now;
        }
    };
    loop {
        tokio::select! {
            out = &mut fut => {
                flush(&mut reported);
                return out;
            }
            _ = tick.tick() => flush(&mut reported),
        }
    }
}

/// Uploads a file with a single request, or as a parallel multipart upload when large.
/// Parts are streamed from disk, so memory use stays small.
async fn upload_file(
    client: &Client,
    bucket: &str,
    key: &str,
    path: &Path,
    on_bytes: OnBytes<'_>,
) -> TransferResult<()> {
    let size = tokio::fs::metadata(path).await.map_err(err)?.len();
    let sent = Arc::new(AtomicU64::new(0));
    if size < MULTIPART_THRESHOLD {
        let upload = async {
            let body = counted(ByteStream::from_path(path).await.map_err(err)?, &sent);
            client
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .send()
                .await
                .map_err(err)
        };
        report_progress(upload, &sent, on_bytes).await?;
        return Ok(());
    }

    let upload_id = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(err)?
        .upload_id()
        .ok_or("no upload id returned")?
        .to_string();

    let upload = async {
        let parts = upload_parts(client, bucket, key, path, &upload_id, size, &sent).await?;
        client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .map_err(err)?;
        Ok(())
    };
    let result = report_progress(upload, &sent, on_bytes).await;
    if result.is_err() {
        // do not leave billable incomplete parts behind
        let _ = client
            .abort_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .send()
            .await;
    }
    result
}

async fn upload_parts(
    client: &Client,
    bucket: &str,
    key: &str,
    path: &Path,
    upload_id: &str,
    size: u64,
    sent: &Arc<AtomicU64>,
) -> TransferResult<Vec<CompletedPart>> {
    let parts = plan_parts(size, part_size_for(size));
    let mut done: Vec<CompletedPart> = stream::iter(parts.into_iter().enumerate())
        .map(|(i, (offset, len))| async move {
            let part_number = i as i32 + 1;
            let body = ByteStream::read_from()
                .path(path)
                .offset(offset)
                .length(Length::Exact(len))
                .build()
                .await
                .map_err(err)?;
            let resp = client
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(counted(body, sent))
                .send()
                .await
                .map_err(err)?;
            Ok::<_, String>(
                CompletedPart::builder()
                    .part_number(part_number)
                    .set_e_tag(resp.e_tag().map(String::from))
                    .build(),
            )
        })
        .buffer_unordered(PART_CONCURRENCY)
        .try_collect()
        .await?;
    done.sort_by_key(|p| p.part_number());
    Ok(done)
}

async fn download_range(
    client: &Client,
    bucket: &str,
    key: &str,
    path: &Path,
    offset: u64,
    len: u64,
    on_bytes: OnBytes<'_>,
) -> TransferResult<()> {
    let mut resp = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .range(format!("bytes={}-{}", offset, offset + len - 1))
        .send()
        .await
        .map_err(err)?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .await
        .map_err(err)?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(err)?;
    while let Some(chunk) = resp.body.try_next().await.map_err(err)? {
        file.write_all(&chunk).await.map_err(err)?;
        on_bytes(chunk.len() as u64);
    }
    file.flush().await.map_err(err)
}

/// Downloads an object into a file, streaming; large objects are fetched as parallel ranges.
async fn download_file(
    client: &Client,
    bucket: &str,
    key: &str,
    path: &Path,
    on_bytes: OnBytes<'_>,
) -> TransferResult<()> {
    let size = client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(err)?
        .content_length()
        .unwrap_or(0)
        .max(0) as u64;

    let file = tokio::fs::File::create(path).await.map_err(err)?;
    if size < MULTIPART_THRESHOLD {
        let mut file = file;
        let mut resp = client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(err)?;
        while let Some(chunk) = resp.body.try_next().await.map_err(err)? {
            file.write_all(&chunk).await.map_err(err)?;
            on_bytes(chunk.len() as u64);
        }
        return file.flush().await.map_err(err);
    }

    file.set_len(size).await.map_err(err)?;
    drop(file);
    stream::iter(plan_parts(size, part_size_for(size)))
        .map(|(offset, len)| download_range(client, bucket, key, path, offset, len, on_bytes))
        .buffer_unordered(PART_CONCURRENCY)
        .try_collect::<Vec<()>>()
        .await?;
    Ok(())
}

impl S3Storage {
    pub fn new(bucket: &str) -> StorageResult<Self> {
        let rt = Runtime::new()?;
        let cfg = rt.block_on(load_config());
        Ok(Self {
            rt,
            client: s3_client(&cfg),
            bucket: bucket.to_string(),
        })
    }
}

impl Storage for S3Storage {
    fn put(&self, key: &str, data: &[u8]) -> StorageResult<()> {
        self.rt
            .block_on(
                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .body(ByteStream::from(data.to_vec()))
                    .send(),
            )
            .map_err(|e| DisplayErrorContext(e).to_string())?;
        Ok(())
    }

    fn put_file(&self, key: &str, path: &Path, on_bytes: OnBytes) -> StorageResult<()> {
        self.rt
            .block_on(upload_file(&self.client, &self.bucket, key, path, on_bytes))
            .map_err(Into::into)
    }

    fn get(&self, key: &str) -> StorageResult<Vec<u8>> {
        self.rt.block_on(async {
            let resp = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await?;
            Ok(resp.body.collect().await?.into_bytes().to_vec())
        })
    }

    fn get_file(&self, key: &str, path: &Path, on_bytes: OnBytes) -> StorageResult<()> {
        self.rt
            .block_on(download_file(
                &self.client,
                &self.bucket,
                key,
                path,
                on_bytes,
            ))
            .map_err(Into::into)
    }

    fn exists(&self, key: &str) -> StorageResult<bool> {
        let res = self.rt.block_on(
            self.client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .send(),
        );
        match res {
            Ok(_) => Ok(true),
            Err(e) if e.as_service_error().is_some_and(|s| s.is_not_found()) => Ok(false),
            Err(e) => Err(DisplayErrorContext(e).to_string().into()),
        }
    }

    fn delete_prefix(&self, prefix: &str) -> StorageResult<usize> {
        let keys = self.list(prefix)?;
        self.rt.block_on(async {
            for chunk in keys.chunks(1000) {
                let objects = chunk
                    .iter()
                    .map(|k| ObjectIdentifier::builder().key(k).build())
                    .collect::<Result<Vec<_>, _>>()?;
                let resp = self
                    .client
                    .delete_objects()
                    .bucket(&self.bucket)
                    .delete(
                        Delete::builder()
                            .set_objects(Some(objects))
                            .quiet(true)
                            .build()?,
                    )
                    .send()
                    .await
                    .map_err(|e| DisplayErrorContext(e).to_string())?;
                if let Some(e) = resp.errors().first() {
                    return Err(format!(
                        "deleting {}: {}",
                        e.key().unwrap_or("?"),
                        e.message().unwrap_or("unknown error")
                    )
                    .into());
                }
            }
            Ok(keys.len())
        })
    }

    fn list(&self, prefix: &str) -> StorageResult<Vec<String>> {
        self.rt.block_on(async {
            let mut keys = Vec::new();
            let mut pages = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .into_paginator()
                .send();
            while let Some(page) = pages.next().await {
                keys.extend(
                    page?
                        .contents()
                        .iter()
                        .filter_map(|o| o.key().map(String::from)),
                );
            }
            Ok(keys)
        })
    }
}
