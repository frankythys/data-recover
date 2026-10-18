//! Read-only search for duplicate files: the user's own files with exactly
//! the same contents. Files are grouped by size first, then by a hash of
//! their first block and only then by a hash of the whole file, so most
//! files are never read in full. Never modifies anything on disk.

use crate::largefiles::{self, LargeFile};
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Bytes hashed in the first round; files that differ here are not read further.
const HEAD_BYTES: u64 = 64 * 1024;
const READ_CHUNK: usize = 1 << 20;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    /// Walking the folders.
    #[default]
    Listing,
    /// Reading files of the same size to compare them.
    Comparing,
}

#[derive(Debug, Clone, Default)]
pub struct DupProgress {
    pub phase: Phase,
    pub files_scanned: u64,
    /// Total size of the files that share their size with another file.
    pub bytes_to_compare: u64,
    pub bytes_compared: u64,
    pub current: String,
}

/// Files with the same contents.
#[derive(Debug, Clone)]
pub struct DupGroup {
    pub size: u64,
    /// Oldest first.
    pub files: Vec<LargeFile>,
}

impl DupGroup {
    /// Space freed when every copy but one is removed.
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len() as u64).saturating_sub(1)
    }
}

#[derive(Debug, Default)]
pub struct DupResult {
    /// Most wasted space first.
    pub groups: Vec<DupGroup>,
    pub files_scanned: u64,
    pub skipped_folders: u64,
}

impl DupResult {
    pub fn wasted(&self) -> u64 {
        self.groups.iter().map(DupGroup::wasted).sum()
    }

    /// Takes removed files out of their groups and drops groups that no
    /// longer have a duplicate.
    pub fn remove_files(&mut self, removed: &[PathBuf]) {
        for group in &mut self.groups {
            group.files.retain(|f| !removed.contains(&f.path));
        }
        self.groups.retain(|g| g.files.len() > 1);
    }
}

/// Walks `root` and returns the user's own files of at least `min_size`
/// bytes (and never empty files) that have a duplicate. Windows and program
/// folders are skipped (see [`largefiles::is_system_or_app`]), as are hard
/// links to the same file, since removing one of those frees nothing.
pub fn scan(
    root: &Path,
    min_size: u64,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(DupProgress),
) -> io::Result<DupResult> {
    if !root.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("Map niet gevonden: {}", root.display())));
    }
    let mut result = DupResult::default();
    let mut progress = DupProgress::default();
    let mut last_report = Instant::now();
    let mut report = |p: &DupProgress, force: bool| {
        if force || last_report.elapsed().as_millis() > 150 {
            on_progress(p.clone());
            last_report = Instant::now();
        }
    };

    let by_size = list_files(root, min_size.max(1), cancel, &mut result, &mut progress, &mut report)?;
    let candidates: Vec<Vec<LargeFile>> = by_size.into_values().filter(|g| g.len() > 1).collect();

    progress.phase = Phase::Comparing;
    progress.bytes_to_compare = candidates.iter().flatten().map(|f| f.size).sum();
    report(&progress, true);

    let keys = (RandomState::new(), RandomState::new());
    for same_size in candidates {
        let size = same_size[0].size;
        let heads = split_by_hash(same_size, Some(HEAD_BYTES), &keys, cancel, &mut progress, &mut report)?;
        for same_head in heads {
            let same = if size <= HEAD_BYTES {
                // The first block was the whole file.
                progress.bytes_compared += size * same_head.len() as u64;
                vec![same_head]
            } else {
                split_by_hash(same_head, None, &keys, cancel, &mut progress, &mut report)?
            };
            for mut files in same {
                // Unknown dates last.
                files.sort_by(|a, b| {
                    let age = |f: &LargeFile| (f.modified.is_none(), f.modified);
                    age(a).cmp(&age(b)).then_with(|| a.path.cmp(&b.path))
                });
                result.groups.push(DupGroup { size, files });
            }
        }
    }
    progress.bytes_compared = progress.bytes_to_compare;
    progress.current.clear();
    report(&progress, true);

    result.groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then_with(|| a.files[0].path.cmp(&b.files[0].path)));
    Ok(result)
}

/// The user's own files of at least `min_size`, by size.
fn list_files(
    root: &Path,
    min_size: u64,
    cancel: &AtomicBool,
    result: &mut DupResult,
    progress: &mut DupProgress,
    report: &mut impl FnMut(&DupProgress, bool),
) -> io::Result<HashMap<u64, Vec<LargeFile>>> {
    let mut by_size: HashMap<u64, Vec<LargeFile>> = HashMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(interrupted());
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            result.skipped_folders += 1;
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            // Junctions and symlinks point at folders that are walked elsewhere.
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if largefiles::is_system_or_app(&path) {
                continue;
            }
            if kind.is_dir() {
                pending.push(path);
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !largefiles::takes_local_space(&meta) {
                continue;
            }
            result.files_scanned += 1;
            let size = meta.len();
            if size >= min_size {
                by_size.entry(size).or_default().push(LargeFile { size, path, modified: meta.modified().ok() });
            }
        }
        progress.files_scanned = result.files_scanned;
        progress.current = dir.display().to_string();
        report(progress, false);
    }
    Ok(by_size)
}

/// Splits same-size `files` into groups of two or more with the same hash
/// of their first `limit` bytes (`None` = the whole file). Files that cannot
/// be read, or that are hard links to a file already in the list, drop out.
fn split_by_hash(
    files: Vec<LargeFile>,
    limit: Option<u64>,
    keys: &(RandomState, RandomState),
    cancel: &AtomicBool,
    progress: &mut DupProgress,
    report: &mut impl FnMut(&DupProgress, bool),
) -> io::Result<Vec<Vec<LargeFile>>> {
    let mut by_hash: HashMap<(u64, u64), Vec<LargeFile>> = HashMap::new();
    let mut seen_ids = Vec::new();
    let mut buf = vec![0u8; READ_CHUNK];
    for file in files {
        if cancel.load(Ordering::Relaxed) {
            return Err(interrupted());
        }
        progress.current = file.path.display().to_string();
        report(progress, false);
        let Ok(mut handle) = File::open(&file.path) else {
            progress.bytes_compared += file.size;
            continue;
        };
        if let Some(id) = file_id(&handle) {
            if seen_ids.contains(&id) {
                progress.bytes_compared += file.size;
                continue;
            }
            seen_ids.push(id);
        }
        let mut a = keys.0.build_hasher();
        let mut b = keys.1.build_hasher();
        let mut left = limit.unwrap_or(u64::MAX).min(file.size);
        let mut ok = true;
        while left > 0 {
            if cancel.load(Ordering::Relaxed) {
                return Err(interrupted());
            }
            let want = left.min(buf.len() as u64) as usize;
            match handle.read(&mut buf[..want]) {
                Ok(0) => break,
                Ok(n) => {
                    a.write(&buf[..n]);
                    b.write(&buf[..n]);
                    left -= n as u64;
                    if limit.is_none() {
                        progress.bytes_compared += n as u64;
                        report(progress, false);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        // Unreadable, or shorter than when it was listed: no longer comparable.
        if !ok || left > 0 {
            progress.bytes_compared += if limit.is_none() { left } else { file.size };
            continue;
        }
        by_hash.entry((a.finish(), b.finish())).or_default().push(file);
    }
    let (keep, unique): (Vec<_>, Vec<_>) = by_hash.into_values().partition(|g| g.len() > 1);
    if limit.is_some() {
        // Files without a match after the first block are done.
        progress.bytes_compared += unique.iter().flatten().map(|f| f.size).sum::<u64>();
    }
    Ok(keep)
}

fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "Scan gestopt")
}

/// (volume, file index): equal for hard links to the same file.
#[cfg(windows)]
fn file_id(file: &File) -> Option<(u32, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }.ok()?;
    Some((info.dwVolumeSerialNumber, (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)))
}

#[cfg(not(windows))]
fn file_id(file: &File) -> Option<(u32, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().ok()?;
    Some((meta.dev() as u32, meta.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test tree at the root of the system drive; the temp folder lives in
    /// AppData, which the scan skips.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let base = std::env::var_os("SystemDrive").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:"));
            let dir = base.join("\\").join(format!("dr_duplicates_{name}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, rel: &str, contents: &[u8]) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }

        fn scan(&self, min_size: u64) -> DupResult {
            scan(&self.0, min_size, &AtomicBool::new(false), |_| {}).unwrap()
        }

        /// Groups as sorted relative paths with `/`.
        fn groups(&self, r: &DupResult) -> Vec<Vec<String>> {
            r.groups
                .iter()
                .map(|g| {
                    let mut names: Vec<String> = g
                        .files
                        .iter()
                        .map(|f| f.path.strip_prefix(&self.0).unwrap().to_string_lossy().replace('\\', "/"))
                        .collect();
                    names.sort();
                    names
                })
                .collect()
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn finds_identical_files_in_different_folders() {
        let t = TempTree::new("same");
        t.file("a/foto.jpg", b"hetzelfde");
        t.file("b/kopie.jpg", b"hetzelfde");
        t.file("c/anders.jpg", b"iets anders");
        t.file("d/zelfde-grootte.jpg", b"HETZELFDE");

        let r = t.scan(1);

        assert_eq!(t.groups(&r), [["a/foto.jpg", "b/kopie.jpg"]]);
        assert_eq!((r.files_scanned, r.wasted()), (4, 9));
    }

    #[test]
    fn same_start_but_different_end_is_no_duplicate() {
        let t = TempTree::new("tail");
        let mut one = vec![7u8; (HEAD_BYTES as usize) * 3];
        let two = one.clone();
        *one.last_mut().unwrap() = 8;
        t.file("een.bin", &one);
        t.file("twee.bin", &two);
        t.file("drie.bin", &two);

        let r = t.scan(1);

        assert_eq!(t.groups(&r), [["drie.bin", "twee.bin"]]);
    }

    #[test]
    fn skips_small_empty_and_program_files() {
        let t = TempTree::new("skip");
        t.file("leeg1.txt", b"");
        t.file("leeg2.txt", b"");
        t.file("klein1.txt", b"ab");
        t.file("klein2.txt", b"ab");
        t.file("AppData/x.bin", b"groot genoeg");
        t.file("Program Files/x.bin", b"groot genoeg");
        t.file("eigen.bin", b"groot genoeg");

        let r = t.scan(5);

        assert!(r.groups.is_empty());
        assert_eq!(r.files_scanned, 5);
    }

    #[test]
    fn orders_groups_by_wasted_space_and_files_by_age() {
        let t = TempTree::new("order");
        t.file("klein1.bin", b"12345");
        t.file("klein2.bin", b"12345");
        t.file("groot1.bin", b"0123456789");
        t.file("groot2.bin", b"0123456789");
        t.file("groot3.bin", b"0123456789");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
        File::options().write(true).open(t.0.join("groot3.bin")).unwrap().set_modified(old).unwrap();

        let r = t.scan(1);

        assert_eq!(r.groups.iter().map(DupGroup::wasted).collect::<Vec<_>>(), [20, 5]);
        assert!(r.groups[0].files[0].path.ends_with("groot3.bin"), "oudste eerst");
    }

    #[test]
    fn hard_links_are_not_duplicates() {
        let t = TempTree::new("links");
        t.file("origineel.bin", b"inhoud");
        std::fs::hard_link(t.0.join("origineel.bin"), t.0.join("link.bin")).unwrap();

        assert!(t.scan(1).groups.is_empty());

        t.file("echte-kopie.bin", b"inhoud");
        assert_eq!(t.scan(1).groups[0].files.len(), 2);
    }

    #[test]
    fn remove_files_drops_groups_without_duplicates() {
        let t = TempTree::new("remove");
        t.file("a.bin", b"aaaa");
        t.file("b.bin", b"aaaa");
        t.file("c.bin", b"cccc");
        t.file("d.bin", b"cccc");
        t.file("e.bin", b"cccc");
        let mut r = t.scan(1);

        r.remove_files(&[t.0.join("a.bin"), t.0.join("c.bin")]);

        assert_eq!(t.groups(&r), [["d.bin", "e.bin"]]);
    }

    #[test]
    fn reports_compare_progress_to_the_end() {
        let t = TempTree::new("progress");
        t.file("a.bin", b"xxxx");
        t.file("b.bin", b"xxxx");
        t.file("c.bin", b"yyyy");
        let mut last = DupProgress::default();

        scan(&t.0, 1, &AtomicBool::new(false), |p| last = p).unwrap();

        assert_eq!((last.phase, last.bytes_to_compare, last.bytes_compared), (Phase::Comparing, 12, 12));
    }

    #[test]
    fn cancelled_scan_returns_interrupted() {
        let t = TempTree::new("cancel");
        t.file("a.bin", b"x");

        let err = scan(&t.0, 1, &AtomicBool::new(true), |_| {}).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }
}
