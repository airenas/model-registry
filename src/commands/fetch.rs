use super::{BUCKET_REQUIRED, CmdResult, Command, StorageFactory, jobs_arg, s3_factory};
use crate::models::ModelRef;
use crate::models::artifacts::{Artifact, Artifacts};
use crate::models::consts::{ARTIFACTS_FILE, LOCAL_DIR};
use crate::models::versions::{self, validate_version, versions_key};
use crate::parallel;
use crate::progress::{FileProgress, Progress, speed};
use crate::storage::Storage;
use crate::storage::utils::{hash_file, safe_relative};
use clap::{Arg, ArgAction, ArgMatches, Command as ClapCommand};
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

pub struct Fetch {
    factory: StorageFactory,
}

impl Fetch {
    pub fn new() -> Self {
        Self::with_factory(s3_factory())
    }

    pub fn with_factory(factory: StorageFactory) -> Self {
        Self { factory }
    }
}

impl Default for Fetch {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Fetch {
    fn definition(&self) -> ClapCommand {
        ClapCommand::new("fetch")
            .about("Download a version of a model into a directory")
            .arg(
                Arg::new("model")
                    .help("Model name (folder in the bucket)")
                    .required(true),
            )
            .arg(
                Arg::new("dir")
                    .help("Target directory (created if missing)")
                    .value_parser(clap::value_parser!(PathBuf))
                    .required(true),
            )
            .arg(
                Arg::new("force")
                    .long("force")
                    .action(ArgAction::SetTrue)
                    .help("Overwrite local files that differ from the registry"),
            )
            .arg(jobs_arg())
    }

    fn run(&self, args: &ArgMatches, out: &mut dyn Write) -> CmdResult {
        let (model, version) = ModelRef::parse_with_version(
            args.get_one::<String>("model").ok_or("model is required")?,
        )?;
        let version = version.as_str();
        let dir = args.get_one::<PathBuf>("dir").ok_or("dir is required")?;
        let bucket = args.get_one::<String>("bucket").ok_or(BUCKET_REQUIRED)?;

        validate_version(version)?;
        let storage = (self.factory)(bucket)?;
        let jobs = (*args.get_one::<usize>("jobs").ok_or("jobs is required")?).clamp(1, 64);
        fetch(
            storage.as_ref(),
            &model,
            version,
            dir,
            args.get_flag("force"),
            jobs,
            out,
        )
    }
}

fn local_artifacts_path(dir: &Path) -> PathBuf {
    dir.join(LOCAL_DIR).join(ARTIFACTS_FILE)
}

fn fetch(
    storage: &dyn Storage,
    model: &str,
    version: &str,
    dir: &Path,
    force: bool,
    jobs: usize,
    out: &mut dyn Write,
) -> CmdResult {
    let vkey = versions_key(model)?;
    if !storage.exists(&vkey)? {
        return Err(format!("model not initialized: {vkey} (run `mr init`)").into());
    }
    let list = versions::parse(&storage.get(&vkey)?).map_err(|e| format!("invalid {vkey}: {e}"))?;
    let version = list
        .resolve(version)
        .ok_or_else(|| format!("version or alias '{version}' not found"))?;
    let entry = list.find(version).ok_or("version not found")?;

    let akey = format!("{}/{ARTIFACTS_FILE}", entry.path);
    let remote: Artifacts =
        serde_json::from_slice(&storage.get(&akey)?).map_err(|e| format!("invalid {akey}: {e}"))?;
    if remote.version != version {
        return Err(format!(
            "{akey} is for version '{}', expected '{version}'",
            remote.version
        )
        .into());
    }

    let local_path = local_artifacts_path(dir);
    let mut previous: Option<Artifacts> = None;
    let local = if local_path.exists() {
        let l: Artifacts = serde_json::from_slice(&fs::read(&local_path)?)
            .map_err(|e| format!("invalid {}: {e}", local_path.display()))?;
        if l.version == version {
            Some(l)
        } else if force {
            // overwrite: treat the directory as not fetched yet, files are compared below;
            // files of the old version that are not in the new one are removed afterwards
            previous = Some(l);
            None
        } else {
            return Err(format!(
                "{} contains version '{}', requested '{version}'; use another directory or --force to overwrite",
                dir.display(),
                l.version
            )
            .into());
        }
    } else {
        None
    };

    // Plan first, so nothing is downloaded if a local file would be overwritten.
    for a in &remote.files {
        if !safe_relative(&a.path) {
            return Err(format!("unsafe path in {akey}: '{}'", a.path).into());
        }
    }
    let status = parallel::run(
        &remote.files,
        jobs,
        |a| {
            let target = dir.join(&a.path);
            Ok(if !target.exists() {
                Status::Missing
            } else if verify(&target, a).is_ok() {
                Status::Same
            } else {
                Status::Differs
            })
        },
        |_, _| Ok(()),
    )?;
    let mut todo = Vec::new();
    for (a, st) in remote.files.iter().zip(status) {
        match st {
            Status::Same => {}
            Status::Missing => todo.push(a),
            Status::Differs if force => todo.push(a),
            Status::Differs => {
                return Err(format!(
                    "{} differs from the registry, use --force to overwrite",
                    dir.join(&a.path).display()
                )
                .into());
            }
        }
    }

    let progress = if !todo.is_empty() && std::io::stderr().is_terminal() {
        let bytes = todo.iter().map(|a| a.size).sum();
        Progress::new("fetching", todo.len() as u64, bytes)
    } else {
        Progress::hidden()
    };
    parallel::run(
        &todo,
        jobs,
        |a| {
            let fp = progress.file(&a.path, a.size);
            let res = download(
                storage,
                &format!("{}/{}", entry.path, a.path),
                &dir.join(&a.path),
                a,
                &fp,
            );
            match res {
                Ok(()) => fp.done(),
                Err(_) => fp.abandon(),
            }
            res
        },
        |a, _| {
            if progress.visible() {
                return Ok(());
            }
            writeln!(out, "downloaded {} ({} bytes)", a.path, a.size)
        },
    )?;
    progress.finish();

    let removed = match &previous {
        Some(old) => remove_stale(dir, old, &remote)?,
        None => 0,
    };
    if local.as_ref() != Some(&remote) {
        fs::create_dir_all(local_path.parent().ok_or("invalid dir")?)?;
        fs::write(&local_path, serde_json::to_vec_pretty(&remote)?)?;
    }
    if todo.is_empty() {
        writeln!(
            out,
            "Up to date: {version}, {} files in {}",
            remote.files.len(),
            dir.display()
        )?;
    } else {
        write!(
            out,
            "Fetched {version}: {} of {} files to {}",
            todo.len(),
            remote.files.len(),
            dir.display()
        )?;
        if removed > 0 {
            write!(
                out,
                ", removed {removed} files of '{}'",
                previous.as_ref().map_or("", |p| &p.version)
            )?;
        }
        if progress.visible() {
            let bytes = todo.iter().map(|a| a.size).sum();
            write!(
                out,
                " in {:.1?} ({})",
                progress.elapsed(),
                speed(bytes, progress.elapsed())
            )?;
        }
        writeln!(out)?;
    }
    Ok(())
}

/// Deletes files that the previous version listed but the new one does not, then prunes
/// directories left empty. Files not tracked by the old artifacts are never touched.
fn remove_stale(dir: &Path, old: &Artifacts, new: &Artifacts) -> Result<usize, String> {
    let keep: std::collections::HashSet<&str> = new.files.iter().map(|a| a.path.as_str()).collect();
    let mut removed = 0;
    for a in old.files.iter().filter(|a| !keep.contains(a.path.as_str())) {
        if !safe_relative(&a.path) {
            continue;
        }
        let path = dir.join(&a.path);
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("remove {}: {e}", path.display())),
        }
        let mut parent = path.parent();
        while let Some(p) = parent.filter(|p| *p != dir) {
            // fails (and stops) as soon as a directory is not empty
            if fs::remove_dir(p).is_err() {
                break;
            }
            parent = p.parent();
        }
    }
    Ok(removed)
}

enum Status {
    Missing,
    Same,
    Differs,
}

/// Downloads to a temp name so an interrupted or corrupt file is never left under the final name.
fn download(
    storage: &dyn Storage,
    key: &str,
    target: &Path,
    a: &Artifact,
    fp: &FileProgress,
) -> Result<(), String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", a.path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| fail(&e))?;
    }
    let part = target.with_file_name(format!(
        "{}.part",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
    ));
    let result = storage.get_file(key, &part, &|n| fp.inc(n)).and_then(|()| {
        fp.set_note("verifying");
        verify(&part, a)
    });
    if let Err(e) = result {
        let _ = fs::remove_file(&part);
        return Err(fail(&e));
    }
    fs::rename(&part, target).map_err(|e| fail(&e))
}

fn verify(path: &Path, a: &Artifact) -> Result<(), Box<dyn std::error::Error>> {
    let size = fs::metadata(path)?.len();
    if size != a.size {
        return Err(format!("size mismatch: expected {}, got {size}", a.size).into());
    }
    if hash_file(path)? != a.blake3 {
        return Err("blake3 checksum mismatch".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mem::MemStorage;

    fn pushed() -> MemStorage {
        let mem = MemStorage::default();
        let h = |d: &[u8]| blake3::hash(d).to_hex().to_string();
        mem.put(
            "m/versions.json",
            br#"{"tool_version":"0.1.0","created_at":"x","latest_model_version":"v1","versions":[
                {"version":"v1","tool_version":"0.1.0","path":"m/v1","created_at":"2026-11-05T10:00:00Z","file_count":2,"total_size":7}]}"#,
        )
        .unwrap();
        let artifacts = format!(
            r#"{{"version":"v1","files":[{{"path":"a.bin","size":5,"modified_at":"x","blake3":"{}"}},
                {{"path":"sub/b.txt","size":2,"modified_at":"x","blake3":"{}"}}]}}"#,
            h(b"hello"),
            h(b"hi")
        );
        mem.put("m/v1/artifacts.json", artifacts.as_bytes())
            .unwrap();
        mem.put("m/v1/a.bin", b"hello").unwrap();
        mem.put("m/v1/sub/b.txt", b"hi").unwrap();
        mem
    }

    fn run(
        mem: &MemStorage,
        version: &str,
        dir: &Path,
    ) -> Result<String, Box<dyn std::error::Error>> {
        run_force(mem, version, dir, false)
    }

    fn run_force(
        mem: &MemStorage,
        version: &str,
        dir: &Path,
        force: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut out = Vec::new();
        fetch(mem, "m", version, dir, force, 4, &mut out)?;
        Ok(String::from_utf8(out)?)
    }

    #[test]
    fn downloads_files() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("out");
        let out = run(&pushed(), "v1", &target).unwrap();
        assert_eq!(fs::read(target.join("a.bin")).unwrap(), b"hello");
        assert_eq!(fs::read(target.join("sub/b.txt")).unwrap(), b"hi");
        assert!(out.contains("Fetched v1: 2 of 2 files"));
        let local: Artifacts =
            serde_json::from_slice(&fs::read(target.join(".mr/artifacts.json")).unwrap()).unwrap();
        assert_eq!(local.version, "v1");
        assert_eq!(local.files.len(), 2);
    }

    #[test]
    fn fetches_by_alias_and_latest() {
        let mem = pushed();
        let mut v = versions::parse(&mem.get("m/versions.json").unwrap()).unwrap();
        v.tags.insert(
            "production".into(),
            crate::models::versions::Tag {
                version: "v1".into(),
                created_at: String::new(),
            },
        );
        mem.put("m/versions.json", &serde_json::to_vec(&v).unwrap())
            .unwrap();
        for name in ["production", "latest"] {
            let d = tempfile::tempdir().unwrap();
            let out = run(&mem, name, d.path()).unwrap();
            assert!(out.contains("Fetched v1:"), "{name}: {out}");
            assert!(d.path().join("a.bin").exists());
        }
    }

    #[test]
    fn other_version_in_dir_needs_force() {
        let mem = pushed();
        let d = tempfile::tempdir().unwrap();
        run(&mem, "v1", d.path()).unwrap();
        fs::write(d.path().join("a.bin"), b"changed").unwrap();
        let mut v = versions::parse(&mem.get("m/versions.json").unwrap()).unwrap();
        v.tags.insert(
            "prod".into(),
            crate::models::versions::Tag {
                version: "v1".into(),
                created_at: String::new(),
            },
        );
        let local: Artifacts =
            serde_json::from_slice(&fs::read(d.path().join(".mr/artifacts.json")).unwrap())
                .unwrap();
        let mut other = local;
        other.version = "v0".into();
        fs::write(
            d.path().join(".mr/artifacts.json"),
            serde_json::to_vec(&other).unwrap(),
        )
        .unwrap();

        let mut old = other.clone();
        old.files.push(Artifact {
            path: "gone/old.bin".into(),
            size: 1,
            modified_at: String::new(),
            blake3: String::new(),
        });
        fs::write(
            d.path().join(".mr/artifacts.json"),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(d.path().join("gone")).unwrap();
        fs::write(d.path().join("gone/old.bin"), b"x").unwrap();
        fs::write(d.path().join("mine.txt"), b"keep").unwrap();

        let err = run(&mem, "v1", d.path()).unwrap_err().to_string();
        assert!(err.contains("--force"), "{err}");
        let mut out = Vec::new();
        fetch(&mem, "m", "v1", d.path(), true, 1, &mut out).unwrap();
        assert_eq!(fs::read(d.path().join("a.bin")).unwrap(), b"hello");
        assert!(!d.path().join("gone").exists());
        assert!(d.path().join("mine.txt").exists());
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("removed 1 files of 'v0'")
        );
        let local: Artifacts =
            serde_json::from_slice(&fs::read(d.path().join(".mr/artifacts.json")).unwrap())
                .unwrap();
        assert_eq!(local.version, "v1");
    }

    #[test]
    fn unknown_version() {
        let d = tempfile::tempdir().unwrap();
        assert!(
            run(&pushed(), "v9", d.path())
                .unwrap_err()
                .to_string()
                .contains("not found")
        );
    }

    #[test]
    fn detects_corruption_and_cleans_up() {
        let mem = pushed();
        mem.put("m/v1/a.bin", b"HELLO").unwrap();
        let d = tempfile::tempdir().unwrap();
        let err = run(&mem, "v1", d.path()).unwrap_err().to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(!d.path().join("a.bin").exists());
        assert!(!d.path().join("a.bin.part").exists());
    }

    #[test]
    fn second_fetch_does_nothing() {
        let d = tempfile::tempdir().unwrap();
        run(&pushed(), "v1", d.path()).unwrap();
        let out = run(&pushed(), "v1", d.path()).unwrap();
        assert!(out.starts_with("Up to date: v1, 2 files"), "{out}");
        assert!(!out.contains("downloaded"));
    }

    #[test]
    fn restores_missing_file() {
        let d = tempfile::tempdir().unwrap();
        run(&pushed(), "v1", d.path()).unwrap();
        fs::remove_file(d.path().join("sub/b.txt")).unwrap();
        let out = run(&pushed(), "v1", d.path()).unwrap();
        assert!(out.contains("downloaded sub/b.txt") && !out.contains("downloaded a.bin"));
        assert!(out.contains("Fetched v1: 1 of 2 files"));
        assert_eq!(fs::read(d.path().join("sub/b.txt")).unwrap(), b"hi");
    }

    #[test]
    fn version_mismatch_with_local() {
        let d = tempfile::tempdir().unwrap();
        run(&pushed(), "v1", d.path()).unwrap();
        let mem = pushed();
        mem.put(
            "m/versions.json",
            br#"{"tool_version":"0.1.0","created_at":"x","latest_model_version":"v2","versions":[
                {"version":"v1","tool_version":"0.1.0","path":"m/v1","created_at":"x","file_count":2,"total_size":7},
                {"version":"v2","tool_version":"0.1.0","path":"m/v2","created_at":"x","file_count":0,"total_size":0}]}"#,
        )
        .unwrap();
        mem.put("m/v2/artifacts.json", br#"{"version":"v2","files":[]}"#)
            .unwrap();
        let err = run(&mem, "v2", d.path()).unwrap_err().to_string();
        assert!(
            err.contains("contains version 'v1', requested 'v2'"),
            "{err}"
        );
    }

    #[test]
    fn remote_artifacts_version_mismatch() {
        let mem = pushed();
        mem.put("m/v1/artifacts.json", br#"{"version":"v7","files":[]}"#)
            .unwrap();
        let d = tempfile::tempdir().unwrap();
        assert!(
            run(&mem, "v1", d.path())
                .unwrap_err()
                .to_string()
                .contains("expected 'v1'")
        );
    }

    #[test]
    fn modified_file_needs_force() {
        let d = tempfile::tempdir().unwrap();
        run(&pushed(), "v1", d.path()).unwrap();
        fs::write(d.path().join("a.bin"), b"mine").unwrap();
        let err = run(&pushed(), "v1", d.path()).unwrap_err().to_string();
        assert!(err.contains("--force"), "{err}");
        assert_eq!(fs::read(d.path().join("a.bin")).unwrap(), b"mine");
        run_force(&pushed(), "v1", d.path(), true).unwrap();
        assert_eq!(fs::read(d.path().join("a.bin")).unwrap(), b"hello");
    }

    #[test]
    fn rejects_path_traversal() {
        let mem = pushed();
        mem.put(
            "m/v1/artifacts.json",
            br#"{"version":"v1","files":[{"path":"../evil","size":1,"modified_at":"x","blake3":"x"}]}"#,
        )
        .unwrap();
        let d = tempfile::tempdir().unwrap();
        assert!(
            run(&mem, "v1", d.path())
                .unwrap_err()
                .to_string()
                .contains("unsafe")
        );
    }
}
