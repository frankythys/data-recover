//! Making deleted data unrecoverable:
//! - free-space wipe: a temporary file fills all free space of a drive with
//!   zeros and is then removed, so the leftovers of deleted files are
//!   overwritten (like `cipher /w`). Existing files are not touched.
//! - shredding: chosen files are overwritten with zeros before they are
//!   deleted.
//!
//! One pass of zeros is enough for current disks. On SSDs wear levelling can
//! keep old copies out of reach; there TRIM usually clears deleted blocks.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

const ALIGN: usize = 4096;
const BLOCK: usize = 1 << 20;
pub const WIPE_FILE_NAME: &str = "DataRecoverer-vrije-ruimte.tmp";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WipeProgress {
    pub written: u64,
    /// Free space when the wipe started, as an estimate of the total.
    pub expected: u64,
    pub finished: bool,
}

fn is_disk_full(e: &io::Error) -> bool {
    // ERROR_HANDLE_DISK_FULL and ERROR_DISK_FULL.
    e.kind() == io::ErrorKind::StorageFull || matches!(e.raw_os_error(), Some(39 | 112))
}

/// Writes zeros to `out` until the disk is full (or `limit` bytes, for
/// tests): first in 1 MiB blocks, then in 4 KiB blocks to fill the rest.
fn fill(
    out: &mut impl Write,
    limit: Option<u64>,
    expected: u64,
    cancel: &AtomicBool,
    mut report: impl FnMut(&WipeProgress),
) -> io::Result<WipeProgress> {
    // Unbuffered writes need an aligned buffer.
    let mut raw = vec![0u8; BLOCK + ALIGN];
    let offset = raw.as_ptr().align_offset(ALIGN);
    let zeros = &mut raw[offset..offset + BLOCK];
    let mut p = WipeProgress { expected, ..Default::default() };
    let mut last_report = Instant::now();
    for block in [BLOCK, ALIGN] {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "Wissen gestopt"));
            }
            if limit.is_some_and(|l| p.written + block as u64 > l) {
                break;
            }
            match out.write_all(&zeros[..block]) {
                Ok(()) => p.written += block as u64,
                Err(e) if is_disk_full(&e) => break,
                Err(e) => return Err(e),
            }
            if last_report.elapsed().as_millis() > 150 {
                report(&p);
                last_report = Instant::now();
            }
        }
    }
    p.finished = true;
    report(&p);
    Ok(p)
}

/// Removes the wipe file however the wipe ends.
struct WipeFile(PathBuf);

impl Drop for WipeFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn open_for_wipe(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
        const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
        options.custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH);
    }
    options.open(path)
}

/// Fills the free space of the drive `folder` is on, then frees it again.
/// `expected` is the free space at the start, for the progress bar.
pub fn wipe_free_space(folder: &Path, expected: u64, cancel: &AtomicBool, report: impl FnMut(&WipeProgress)) -> io::Result<WipeProgress> {
    wipe_free_space_up_to(folder, None, expected, cancel, report)
}

fn wipe_free_space_up_to(
    folder: &Path,
    limit: Option<u64>,
    expected: u64,
    cancel: &AtomicBool,
    report: impl FnMut(&WipeProgress),
) -> io::Result<WipeProgress> {
    let guard = WipeFile(folder.join(WIPE_FILE_NAME));
    // Left behind by a wipe that was killed halfway.
    let _ = std::fs::remove_file(&guard.0);
    let mut file = open_for_wipe(&guard.0)?;
    let p = fill(&mut file, limit, expected, cancel, report)?;
    file.sync_all()?;
    drop(file);
    drop(guard);
    Ok(p)
}

/// Overwrites a file's contents with zeros and deletes it. Refuses Windows
/// and program files.
pub fn shred(path: &Path) -> io::Result<u64> {
    if crate::largefiles::is_system_or_app(path) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "systeem- of programmabestand, niet gewist"));
    }
    let len = overwrite(path)?;
    // A new name hides the old one in the folder's records.
    let hidden = path.with_file_name(format!("{:016x}.tmp", len ^ 0x5A5A_5A5A_5A5A_5A5A));
    let target = if std::fs::rename(path, &hidden).is_ok() { hidden } else { path.to_path_buf() };
    std::fs::remove_file(target)?;
    Ok(len)
}

/// Writes zeros over the whole of `path` in place. Returns its length.
fn overwrite(path: &Path) -> io::Result<u64> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    let len = file.metadata()?.len();
    let zeros = vec![0u8; BLOCK];
    let mut left = len;
    while left > 0 {
        let n = left.min(BLOCK as u64) as usize;
        file.write_all(&zeros[..n])?;
        left -= n as u64;
    }
    file.sync_all()?;
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disk that is full after `room` bytes.
    struct SmallDisk {
        room: usize,
        data: Vec<u8>,
    }

    impl Write for SmallDisk {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.data.len() + buf.len() > self.room {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            self.data.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dr_wipe_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fills_until_the_disk_is_full_also_the_last_small_blocks() {
        let mut disk = SmallDisk { room: 3 * BLOCK + 5 * ALIGN + 100, data: Vec::new() };

        let p = fill(&mut disk, None, 0, &AtomicBool::new(false), |_| {}).unwrap();

        assert_eq!(p.written as usize, 3 * BLOCK + 5 * ALIGN);
        assert!(p.finished && disk.data.iter().all(|&b| b == 0));
    }

    #[test]
    fn other_write_errors_stop_the_wipe() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("schijf weg"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        assert!(fill(&mut Broken, None, 0, &AtomicBool::new(false), |_| {}).is_err());
    }

    #[test]
    fn wipe_file_is_removed_afterwards_and_when_cancelled() {
        let dir = temp("free");

        let p = wipe_free_space_up_to(&dir, Some(4 * BLOCK as u64), 0, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(p.written, 4 * BLOCK as u64);
        assert!(!dir.join(WIPE_FILE_NAME).exists());

        let err = wipe_free_space_up_to(&dir, Some(BLOCK as u64), 0, &AtomicBool::new(true), |_| {}).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(!dir.join(WIPE_FILE_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrite_replaces_contents_with_zeros_and_shred_removes_the_file() {
        // Not in the temp folder: that is in AppData, which shred refuses.
        let base = std::env::var_os("SystemDrive").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:"));
        let dir = base.join("\\").join(format!("dr_wipe_shred_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("geheim.txt");
        std::fs::write(&path, vec![b'x'; BLOCK + 10]).unwrap();

        assert_eq!(overwrite(&path).unwrap(), BLOCK as u64 + 10);
        assert!(std::fs::read(&path).unwrap().iter().all(|&b| b == 0));

        assert_eq!(shred(&path).unwrap(), BLOCK as u64 + 10);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_to_shred_program_files() {
        let err = shred(Path::new(r"C:\Program Files\App\app.exe")).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }
}
