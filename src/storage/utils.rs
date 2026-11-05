use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Hex encoded BLAKE3 hash of a file's content.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    hash_file_with(path, &|_| {})
}

/// Like `hash_file`, reporting the bytes read so far through `on_bytes` (as increments).
pub fn hash_file_with(path: &Path, on_bytes: &dyn Fn(u64)) -> std::io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        on_bytes(n as u64);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Checks that an artifact path from artifacts.json stays inside the target directory.
pub fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
        && !path.contains(':')
}

/// S3 limits: at most 10000 parts per multipart upload, parts of at least 5 MiB.
pub const MAX_PARTS: u64 = 10_000;
pub const PART_SIZE: u64 = 64 * 1024 * 1024;

/// Files of this size and larger are transferred in parallel parts.
pub const MULTIPART_THRESHOLD: u64 = PART_SIZE;

/// Part size for a file, grown when needed to stay within the part count limit.
pub fn part_size_for(size: u64) -> u64 {
    PART_SIZE.max(size.div_ceil(MAX_PARTS))
}

/// Splits `size` bytes into (offset, length) parts.
pub fn plan_parts(size: u64, part_size: u64) -> Vec<(u64, u64)> {
    (0..size.div_ceil(part_size))
        .map(|i| {
            let offset = i * part_size;
            (offset, part_size.min(size - offset))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_parts() {
        assert_eq!(plan_parts(10, 4), vec![(0, 4), (4, 4), (8, 2)]);
        assert_eq!(plan_parts(8, 4), vec![(0, 4), (4, 4)]);
        assert!(plan_parts(0, 4).is_empty());
    }

    #[test]
    fn part_size_stays_within_limit() {
        assert_eq!(part_size_for(1), PART_SIZE);
        let huge = 5 * 1024 * 1024 * 1024 * 1024; // 5 TiB
        let ps = part_size_for(huge);
        assert!(plan_parts(huge, ps).len() as u64 <= MAX_PARTS);
        assert!(ps <= 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn safe_paths() {
        assert!(safe_relative("a/b.txt"));
        assert!(!safe_relative("../a"));
        assert!(!safe_relative("/a"));
        assert!(!safe_relative("a//b"));
    }
}
