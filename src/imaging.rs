//! Copies a whole disk or partition into an image file, sector by sector, so
//! files can be recovered from the copy instead of from a disk that may be
//! failing. Like ddrescue, unreadable sectors are filled with zeros and
//! counted instead of stopping the copy. The source is only ever read.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Bytes read per step while the disk reads fine.
const CHUNK: usize = 1 << 20;
/// Blocks retried one at a time when a chunk cannot be read.
const RETRY_BLOCK: usize = 4096;

/// Something an image can be made of: a disk, a partition or (in tests)
/// a file.
pub trait BlockSource {
    fn len(&mut self) -> io::Result<u64>;
    /// Exactly `len` bytes at `offset`, or an error.
    fn read_block(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>>;
}

impl BlockSource for File {
    fn len(&mut self) -> io::Result<u64> {
        crate::disk::device_len(self)
    }

    fn read_block(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            match self.read(&mut buf[filled..])? {
                0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "einde van de schijf")),
                n => filled += n,
            }
        }
        Ok(buf)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageProgress {
    pub done: u64,
    pub total: u64,
    /// Bytes that could not be read and were written as zeros.
    pub unreadable: u64,
    pub finished: bool,
    pub cancelled: bool,
}

/// Copies all of `source` to `out`, reporting progress about every 150 ms
/// and once at the end. Blocks while `paused` is set.
pub fn copy(
    source: &mut impl BlockSource,
    out: &mut impl Write,
    cancel: &AtomicBool,
    paused: &AtomicBool,
    mut report: impl FnMut(&ImageProgress),
) -> io::Result<ImageProgress> {
    let mut p = ImageProgress { total: source.len()?, ..Default::default() };
    let mut last_report = Instant::now();
    report(&p);
    while p.done < p.total {
        while paused.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(60));
        }
        if cancel.load(Ordering::Relaxed) {
            p.cancelled = true;
            break;
        }
        let want = (p.total - p.done).min(CHUNK as u64) as usize;
        let block = match source.read_block(p.done, want) {
            Ok(b) if b.len() == want => b,
            _ => read_carefully(source, p.done, want, &mut p.unreadable),
        };
        out.write_all(&block)?;
        p.done += want as u64;
        if last_report.elapsed().as_millis() > 150 {
            report(&p);
            last_report = Instant::now();
        }
    }
    out.flush()?;
    p.finished = true;
    report(&p);
    Ok(p)
}

/// Reads a chunk that failed block by block; blocks that still fail
/// become zeros.
fn read_carefully(source: &mut impl BlockSource, offset: u64, len: usize, unreadable: &mut u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let n = (len - out.len()).min(RETRY_BLOCK);
        match source.read_block(offset + out.len() as u64, n) {
            Ok(b) if b.len() == n => out.extend_from_slice(&b),
            _ => {
                out.resize(out.len() + n, 0);
                *unreadable += n as u64;
            }
        }
    }
    out
}

/// A copy running in the background.
pub struct ImageJob {
    pub source_label: String,
    pub dest: PathBuf,
    pub cancel: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    pub progress: Arc<Mutex<ImageProgress>>,
    pub started_at: Instant,
    done_rx: Receiver<io::Result<ImageProgress>>,
    /// Set once the copy has ended.
    pub result: Option<Result<ImageProgress, String>>,
    pub finished_after: Option<Duration>,
}

impl ImageJob {
    /// Starts copying `source` (e.g. `\\.\PhysicalDrive1`) to `dest`.
    pub fn start(source: String, source_label: String, dest: &Path, ctx: &egui::Context) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(ImageProgress::default()));
        let (tx, rx) = channel();
        let (thread_cancel, thread_paused, thread_progress) = (cancel.clone(), paused.clone(), progress.clone());
        let (thread_dest, ctx) = (dest.to_path_buf(), ctx.clone());
        std::thread::spawn(move || {
            let result = (|| {
                let mut input = File::open(&source)?;
                let mut output = File::create(&thread_dest)?;
                copy(&mut input, &mut output, &thread_cancel, &thread_paused, |p| {
                    *thread_progress.lock().unwrap() = p.clone();
                    ctx.request_repaint();
                })
            })();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        Self {
            source_label,
            dest: dest.to_path_buf(),
            cancel,
            paused,
            progress,
            started_at: Instant::now(),
            done_rx: rx,
            result: None,
            finished_after: None,
        }
    }

    /// Picks up the result once the copy has ended.
    pub fn poll(&mut self) {
        if self.result.is_some() {
            return;
        }
        if let Ok(result) = self.done_rx.try_recv() {
            self.finished_after = Some(self.started_at.elapsed());
            self.result = Some(result.map_err(|e| {
                if e.kind() == io::ErrorKind::PermissionDenied {
                    "Toegang geweigerd. Start DiskTools als administrator.".to_string()
                } else {
                    e.to_string()
                }
            }));
        }
    }

    pub fn running(&self) -> bool {
        self.result.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory disk whose blocks at `bad` offsets cannot be read.
    struct FakeDisk {
        data: Vec<u8>,
        bad: Vec<u64>,
        short_reads: bool,
    }

    impl BlockSource for FakeDisk {
        fn len(&mut self) -> io::Result<u64> {
            Ok(self.data.len() as u64)
        }

        fn read_block(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            let range = offset..offset + len as u64;
            if self.bad.iter().any(|b| range.contains(b)) {
                return Err(io::Error::other("CRC-fout"));
            }
            let start = offset as usize;
            let end = if self.short_reads && len > RETRY_BLOCK { start + len / 2 } else { start + len };
            Ok(self.data[start..end].to_vec())
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8 + 1).collect()
    }

    fn run(disk: &mut FakeDisk, cancel: bool) -> (Vec<u8>, ImageProgress) {
        let mut out = Vec::new();
        let p = copy(disk, &mut out, &AtomicBool::new(cancel), &AtomicBool::new(false), |_| {}).unwrap();
        (out, p)
    }

    #[test]
    fn copies_every_byte_including_a_partial_last_chunk() {
        let data = pattern(CHUNK * 2 + 5000);
        let mut disk = FakeDisk { data: data.clone(), bad: vec![], short_reads: false };

        let (out, p) = run(&mut disk, false);

        assert_eq!(out, data);
        assert_eq!((p.done, p.total, p.unreadable, p.finished, p.cancelled), (data.len() as u64, data.len() as u64, 0, true, false));
    }

    #[test]
    fn unreadable_blocks_become_zeros_and_are_counted() {
        let data = pattern(CHUNK * 2);
        let bad = vec![CHUNK as u64 + 8192 + 10, 3 * 4096];
        let mut disk = FakeDisk { data: data.clone(), bad, short_reads: false };

        let (out, p) = run(&mut disk, false);

        assert_eq!(p.unreadable, 2 * RETRY_BLOCK as u64);
        assert_eq!(out.len(), data.len());
        let zero = |from: usize| out[from..from + RETRY_BLOCK].iter().all(|&b| b == 0);
        assert!(zero(3 * 4096) && zero(CHUNK + 8192));
        assert_eq!(out[..3 * 4096], data[..3 * 4096]);
        assert_eq!(out[4 * 4096..CHUNK + 8192], data[4 * 4096..CHUNK + 8192]);
        assert_eq!(out[CHUNK + 3 * 4096..], data[CHUNK + 3 * 4096..]);
    }

    #[test]
    fn short_reads_are_retried_block_by_block() {
        let data = pattern(CHUNK + 4096);
        let mut disk = FakeDisk { data: data.clone(), bad: vec![], short_reads: true };

        let (out, p) = run(&mut disk, false);

        assert_eq!((out, p.unreadable), (data, 0));
    }

    #[test]
    fn stops_when_cancelled() {
        let mut disk = FakeDisk { data: pattern(CHUNK), bad: vec![], short_reads: false };

        let (out, p) = run(&mut disk, true);

        assert!(out.is_empty());
        assert!(p.cancelled && p.finished);
    }

    #[test]
    fn copies_a_real_file() {
        let dir = std::env::temp_dir().join(format!("dr_imaging_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (src, dst) = (dir.join("bron.bin"), dir.join("kopie.img"));
        let data = pattern(CHUNK + 777);
        std::fs::write(&src, &data).unwrap();

        let mut input = File::open(&src).unwrap();
        let mut output = File::create(&dst).unwrap();
        copy(&mut input, &mut output, &AtomicBool::new(false), &AtomicBool::new(false), |_| {}).unwrap();
        drop(output);

        assert_eq!(std::fs::read(&dst).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
