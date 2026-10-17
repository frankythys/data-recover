//! Removing a batch of files in the background, one at a time, so the
//! screen can show which file is being removed and how far along it is.
//! Callers check beforehand that every file may be removed.

use crate::largefiles::{self, LargeFile};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// To the Recycle Bin; space is only freed once the bin is emptied.
    RecycleBin,
    /// Removed for good; the space is free right away.
    Permanent,
}

#[derive(Clone, Debug, Default)]
pub struct DeleteProgress {
    pub done: usize,
    pub total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// Bytes of the files that were actually removed.
    pub bytes_removed: u64,
    /// Name of the file being removed right now.
    pub current: String,
    pub removed: Vec<PathBuf>,
    /// Files that could not be removed, with the reason.
    pub failed: Vec<(PathBuf, String)>,
    pub finished: bool,
    pub cancelled: bool,
}

pub struct DeleteJob {
    pub mode: Mode,
    progress: Arc<Mutex<DeleteProgress>>,
    cancel: Arc<AtomicBool>,
}

impl DeleteJob {
    pub fn start(files: Vec<LargeFile>, mode: Mode, ctx: &egui::Context) -> Self {
        let progress = Arc::new(Mutex::new(DeleteProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (thread_progress, thread_cancel, ctx) = (progress.clone(), cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            run(&files, mode, &thread_cancel, |p| {
                *thread_progress.lock().unwrap() = p.clone();
                ctx.request_repaint();
            });
        });
        Self { mode, progress, cancel }
    }

    pub fn progress(&self) -> DeleteProgress {
        self.progress.lock().unwrap().clone()
    }

    /// Stops after the file that is being removed now.
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Removes `files` one by one, reporting progress before and after each.
pub fn run(files: &[LargeFile], mode: Mode, cancel: &AtomicBool, mut report: impl FnMut(&DeleteProgress)) -> DeleteProgress {
    let mut p = DeleteProgress {
        total: files.len(),
        bytes_total: files.iter().map(|f| f.size).sum(),
        ..Default::default()
    };
    for file in files {
        if cancel.load(Ordering::Relaxed) {
            p.cancelled = true;
            break;
        }
        p.current = file.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        report(&p);
        let outcome = match mode {
            Mode::Permanent => std::fs::remove_file(&file.path).map(|()| true),
            Mode::RecycleBin => largefiles::recycle_unchecked(&file.path),
        };
        match outcome {
            Ok(true) => {
                p.removed.push(file.path.clone());
                p.bytes_removed += file.size;
            }
            Ok(false) => p.failed.push((file.path.clone(), "geannuleerd".to_string())),
            Err(e) => p.failed.push((file.path.clone(), e.to_string())),
        }
        p.done += 1;
        p.bytes_done += file.size;
    }
    p.current.clear();
    p.finished = true;
    report(&p);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("dr_deleter_{name}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str, size: u64) -> LargeFile {
            let path = self.0.join(name);
            std::fs::File::create(&path).unwrap().set_len(size).unwrap();
            LargeFile { size, path, modified: None }
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn removes_every_file_and_reports_progress() {
        let t = TempTree::new("all");
        let files = vec![t.file("a.bin", 10), t.file("b.bin", 20)];
        let mut seen = Vec::new();

        let p = run(&files, Mode::Permanent, &AtomicBool::new(false), |p| seen.push(p.current.clone()));

        assert_eq!((p.done, p.bytes_done, p.removed.len(), p.failed.len(), p.finished), (2, 30, 2, 0, true));
        assert!(files.iter().all(|f| !f.path.exists()));
        assert_eq!(seen, ["a.bin", "b.bin", ""]);
    }

    #[test]
    fn records_files_that_cannot_be_removed() {
        let t = TempTree::new("fail");
        let gone = LargeFile { size: 5, path: t.0.join("bestaat-niet.bin"), modified: None };
        let real = t.file("echt.bin", 10);

        let p = run(&[gone, real], Mode::Permanent, &AtomicBool::new(false), |_| {});

        assert_eq!((p.removed.len(), p.failed.len(), p.bytes_removed), (1, 1, 10));
        assert!(p.failed[0].0.ends_with("bestaat-niet.bin"));
    }

    #[test]
    fn stops_when_cancelled() {
        let t = TempTree::new("cancel");
        let file = t.file("blijft.bin", 10);

        let p = run(&[file.clone()], Mode::Permanent, &AtomicBool::new(true), |_| {});

        assert!(p.cancelled && p.finished);
        assert_eq!(p.done, 0);
        assert!(file.path.exists());
    }
}
