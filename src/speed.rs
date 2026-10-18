//! Speed test of a drive, measured on a temporary test file the way
//! CrystalDiskMark does: sequential reading and writing in 1 MiB blocks and
//! random reading and writing of 4 KiB blocks. Windows' file cache is
//! bypassed so the disk itself is measured. The test file is always removed
//! afterwards.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const ALIGN: usize = 4096;
const SEQ_BLOCK: usize = 1 << 20;
const RANDOM_BLOCK: usize = 4096;
pub const TEST_FILE_NAME: &str = "DataRecoverer-snelheidstest.tmp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    SeqWrite,
    SeqRead,
    RandomRead,
    RandomWrite,
}

/// Result of one measurement.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rate {
    pub bytes_per_sec: f64,
    pub ops_per_sec: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpeedProgress {
    pub phase: Option<Phase>,
    /// Progress within the current phase, 0 to 1.
    pub fraction: f32,
    pub seq_write: Option<Rate>,
    pub seq_read: Option<Rate>,
    pub random_read: Option<Rate>,
    pub random_write: Option<Rate>,
}

/// A buffer whose start is aligned to [`ALIGN`], as unbuffered I/O needs.
struct AlignedBuf {
    raw: Vec<u8>,
    offset: usize,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize) -> Self {
        let mut raw = vec![0u8; len + ALIGN];
        let offset = raw.as_ptr().align_offset(ALIGN);
        // Pseudo-random contents, so compressing controllers cannot cheat.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for b in &mut raw[offset..offset + len] {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        Self { raw, offset, len }
    }

    fn bytes(&mut self, len: usize) -> &mut [u8] {
        &mut self.raw[self.offset..self.offset + len.min(self.len)]
    }
}

/// Removes the test file when the test ends, however it ends.
struct TestFile(PathBuf);

impl Drop for TestFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn open_unbuffered(path: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create {
        options.create_new(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
        const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
        options.custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH);
    }
    options.open(path)
}

fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "Test gestopt")
}

/// Runs the four measurements on a `size`-byte test file in `folder`; each
/// random test runs for `random_time`.
pub fn run(
    folder: &Path,
    size: u64,
    random_time: Duration,
    cancel: &AtomicBool,
    mut report: impl FnMut(&SpeedProgress),
) -> io::Result<SpeedProgress> {
    let size = size / SEQ_BLOCK as u64 * SEQ_BLOCK as u64;
    if size == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "testbestand te klein"));
    }
    let guard = TestFile(folder.join(TEST_FILE_NAME));
    // Left behind by a test that was killed halfway.
    let _ = std::fs::remove_file(&guard.0);
    let mut file = open_unbuffered(&guard.0, true)?;
    let mut buf = AlignedBuf::new(SEQ_BLOCK);
    let mut p = SpeedProgress::default();

    p.phase = Some(Phase::SeqWrite);
    p.seq_write = Some(sequential(&mut file, size, &mut buf, true, cancel, &mut p, &mut report)?);
    p.phase = Some(Phase::SeqRead);
    p.seq_read = Some(sequential(&mut file, size, &mut buf, false, cancel, &mut p, &mut report)?);
    p.phase = Some(Phase::RandomRead);
    p.random_read = Some(random(&mut file, size, &mut buf, false, random_time, cancel, &mut p, &mut report)?);
    p.phase = Some(Phase::RandomWrite);
    p.random_write = Some(random(&mut file, size, &mut buf, true, random_time, cancel, &mut p, &mut report)?);

    drop(file);
    drop(guard);
    p.phase = None;
    p.fraction = 1.0;
    report(&p);
    Ok(p)
}

fn sequential(
    file: &mut File,
    size: u64,
    buf: &mut AlignedBuf,
    write: bool,
    cancel: &AtomicBool,
    p: &mut SpeedProgress,
    report: &mut impl FnMut(&SpeedProgress),
) -> io::Result<Rate> {
    file.seek(SeekFrom::Start(0))?;
    let blocks = size / SEQ_BLOCK as u64;
    let started = Instant::now();
    for i in 0..blocks {
        if cancel.load(Ordering::Relaxed) {
            return Err(interrupted());
        }
        let block = buf.bytes(SEQ_BLOCK);
        if write {
            file.write_all(block)?;
        } else {
            file.read_exact(block)?;
        }
        if i.is_multiple_of(16) {
            p.fraction = i as f32 / blocks as f32;
            report(p);
        }
    }
    Ok(rate(size, blocks, started.elapsed()))
}

#[allow(clippy::too_many_arguments)]
fn random(
    file: &mut File,
    size: u64,
    buf: &mut AlignedBuf,
    write: bool,
    time: Duration,
    cancel: &AtomicBool,
    p: &mut SpeedProgress,
    report: &mut impl FnMut(&SpeedProgress),
) -> io::Result<Rate> {
    let blocks = size / RANDOM_BLOCK as u64;
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let started = Instant::now();
    let mut ops = 0u64;
    while started.elapsed() < time {
        if cancel.load(Ordering::Relaxed) {
            return Err(interrupted());
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        file.seek(SeekFrom::Start(x % blocks * RANDOM_BLOCK as u64))?;
        let block = buf.bytes(RANDOM_BLOCK);
        if write {
            file.write_all(block)?;
        } else {
            file.read_exact(block)?;
        }
        ops += 1;
        if ops.is_multiple_of(256) {
            p.fraction = (started.elapsed().as_secs_f32() / time.as_secs_f32()).min(1.0);
            report(p);
        }
    }
    Ok(rate(ops * RANDOM_BLOCK as u64, ops, started.elapsed()))
}

fn rate(bytes: u64, ops: u64, took: Duration) -> Rate {
    let secs = took.as_secs_f64().max(1e-6);
    Rate { bytes_per_sec: bytes as f64 / secs, ops_per_sec: ops as f64 / secs }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dr_speed_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn measures_all_four_and_removes_the_test_file() {
        let dir = folder("all");
        let mut phases = Vec::new();

        let p = run(&dir, 8 << 20, Duration::from_millis(100), &AtomicBool::new(false), |p| {
            if phases.last() != Some(&p.phase) {
                phases.push(p.phase);
            }
        })
        .unwrap();

        for rate in [p.seq_write, p.seq_read, p.random_read, p.random_write] {
            assert!(rate.is_some_and(|r| r.bytes_per_sec > 0.0 && r.ops_per_sec > 0.0));
        }
        assert_eq!(phases, [Some(Phase::SeqWrite), Some(Phase::SeqRead), Some(Phase::RandomRead), Some(Phase::RandomWrite), None]);
        assert!(!dir.join(TEST_FILE_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelled_test_also_removes_the_file() {
        let dir = folder("cancel");

        let err = run(&dir, 8 << 20, Duration::from_millis(100), &AtomicBool::new(true), |_| {}).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(!dir.join(TEST_FILE_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn buffer_is_aligned_and_not_all_zero() {
        let mut buf = AlignedBuf::new(SEQ_BLOCK);

        let bytes = buf.bytes(SEQ_BLOCK);

        assert_eq!(bytes.as_ptr() as usize % ALIGN, 0);
        assert!(bytes.iter().any(|&b| b != 0));
    }
}
