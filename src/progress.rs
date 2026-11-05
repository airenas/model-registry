use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Bars are drawn on stderr and hidden automatically when it is not a terminal.
const OVERALL_TEMPLATE: &str =
    "{prefix:>10} [{bar:40}] {bytes}/{total_bytes} {binary_bytes_per_sec} ETA {eta} ({msg})";
const FILE_TEMPLATE: &str =
    "           {spinner:.green} {msg:<42} [{bar:20.green/white}] {bytes}/{total_bytes}";

/// Progress of one stage (hashing, uploading, ...): an overall bar in bytes with speed and
/// ETA, and one short-lived bar per file being processed.
pub struct Progress {
    multi: MultiProgress,
    overall: ProgressBar,
    files_total: u64,
    files_done: Arc<AtomicU64>,
}

impl Progress {
    pub fn new(label: &str, files: u64, bytes: u64) -> Self {
        Self::with_target(label, files, bytes, ProgressDrawTarget::stderr_with_hz(10))
    }

    /// Never draws anything.
    pub fn hidden() -> Self {
        Self::with_target("", 0, 0, ProgressDrawTarget::hidden())
    }

    fn with_target(label: &str, files: u64, bytes: u64, target: ProgressDrawTarget) -> Self {
        let multi = MultiProgress::with_draw_target(target);
        let overall = multi.add(ProgressBar::new(bytes));
        overall.set_style(
            ProgressStyle::with_template(OVERALL_TEMPLATE)
                .expect("valid template")
                .progress_chars("=> "),
        );
        overall.set_prefix(
            console::Style::new()
                .italic()
                .for_stderr()
                .apply_to(label)
                .to_string(),
        );
        overall.set_message(format!("0/{files} files"));
        Self {
            multi,
            overall,
            files_total: files,
            files_done: Arc::new(AtomicU64::new(0)),
        }
    }

    /// True if bars are drawn; commands then skip their per-file text lines.
    pub fn visible(&self) -> bool {
        !self.overall.is_hidden()
    }

    /// Starts tracking one file; call `inc` as bytes are processed and `done` at the end.
    pub fn file(&self, name: &str, size: u64) -> FileProgress {
        let bar = self.multi.add(ProgressBar::new(size));
        bar.set_style(
            ProgressStyle::with_template(FILE_TEMPLATE)
                .expect("valid template")
                .progress_chars("━╸ "),
        );
        bar.set_message(shorten(name, 42));
        bar.enable_steady_tick(Duration::from_millis(100));
        FileProgress {
            bar,
            overall: self.overall.clone(),
            files_done: self.files_done.clone(),
            files_total: self.files_total,
        }
    }

    pub fn finish(&self) {
        self.overall.finish();
    }

    pub fn elapsed(&self) -> Duration {
        self.overall.elapsed()
    }
}

pub struct FileProgress {
    bar: ProgressBar,
    overall: ProgressBar,
    files_done: Arc<AtomicU64>,
    files_total: u64,
}

impl FileProgress {
    pub fn inc(&self, bytes: u64) {
        self.bar.inc(bytes);
        self.overall.inc(bytes);
    }

    pub fn set_note(&self, note: &str) {
        self.bar.set_message(note.to_string());
    }

    pub fn done(self) {
        let n = self.files_done.fetch_add(1, Ordering::SeqCst) + 1;
        self.overall
            .set_message(format!("{n}/{} files", self.files_total));
        self.bar.finish_and_clear();
    }

    /// Drops the bar of a failed file without counting it.
    pub fn abandon(self) {
        self.bar.finish_and_clear();
    }
}

/// Keeps the end of long names, e.g. `…/deep/model.safetensors`.
fn shorten(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max {
        return name.to_string();
    }
    format!(
        "…{}",
        chars[chars.len() - (max - 1)..].iter().collect::<String>()
    )
}

/// Average speed for the summary line, e.g. `46.5 MiB/s`.
pub fn speed(bytes: u64, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return String::new();
    }
    format!("{}/s", crate::human_size((bytes as f64 / secs) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_long_names() {
        assert_eq!(shorten("a.bin", 10), "a.bin");
        assert_eq!(shorten("abcdefghij", 5), "…ghij");
        assert_eq!(shorten("ąčęėįšųū", 4), "…šųū");
    }

    #[test]
    fn hidden_progress_counts() {
        let p = Progress::hidden();
        assert!(!p.visible());
        let f = p.file("a", 10);
        f.inc(10);
        f.done();
        assert_eq!(p.files_done.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn speed_text() {
        assert_eq!(speed(2 * 1024 * 1024, Duration::from_secs(2)), "1.0 MiB/s");
        assert_eq!(speed(5, Duration::ZERO), "");
    }
}
