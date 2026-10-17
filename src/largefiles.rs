//! Read-only walk of a folder tree that finds the largest files and folders,
//! plus the two actions the cleanup screen offers on a result: show it in
//! Explorer and move it to the Recycle Bin.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

pub const MAX_FILE_RESULTS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LargeFile {
    pub size: u64,
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
}

/// One folder of the scanned tree. Sizes include all subfolders.
#[derive(Debug, Clone)]
pub struct DirNode {
    /// Folder name; for the root node the full root path.
    pub name: OsString,
    pub parent: Option<usize>,
    pub size: u64,
    pub files: u64,
    /// Indexes into [`CleanupResult::dirs`], largest first.
    pub children: Vec<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct CleanupProgress {
    pub files_scanned: u64,
    pub bytes_counted: u64,
    pub current_folder: String,
}

#[derive(Debug, Default)]
pub struct CleanupResult {
    /// Largest first.
    pub files: Vec<LargeFile>,
    /// Every scanned folder; index 0 is the root.
    pub dirs: Vec<DirNode>,
    pub file_count: u64,
    pub total_bytes: u64,
    pub skipped_folders: u64,
}

impl CleanupResult {
    /// Full path of folder `index`, rebuilt from the parent chain.
    pub fn dir_path(&self, index: usize) -> PathBuf {
        let mut names = Vec::new();
        let mut cur = Some(index);
        while let Some(i) = cur {
            names.push(&self.dirs[i].name);
            cur = self.dirs[i].parent;
        }
        names.iter().rev().collect()
    }

    /// Lowers the totals of folder `index` and its ancestors after a file
    /// of `size` bytes was removed from it.
    pub fn subtract_file(&mut self, index: usize, size: u64) {
        let mut cur = Some(index);
        while let Some(i) = cur {
            let node = &mut self.dirs[i];
            node.size = node.size.saturating_sub(size);
            node.files = node.files.saturating_sub(1);
            cur = node.parent;
        }
        self.total_bytes = self.total_bytes.saturating_sub(size);
        self.file_count = self.file_count.saturating_sub(1);
    }
}

/// Walks `root`, totals every folder and keeps the largest files of at
/// least `min_size` bytes. Never modifies anything on disk.
pub fn scan(
    root: &Path,
    min_size: u64,
    max_files: usize,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(CleanupProgress),
) -> io::Result<CleanupResult> {
    if !root.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("Map niet gevonden: {}", root.display())));
    }
    let mut result = CleanupResult::default();
    let mut largest: BinaryHeap<Reverse<LargeFile>> = BinaryHeap::new();
    let mut dirs = vec![DirNode {
        name: root.as_os_str().to_owned(),
        parent: None,
        size: 0,
        files: 0,
        children: Vec::new(),
    }];
    let mut pending: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut last_report = Instant::now();

    while let Some((dir, node)) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Scan gestopt"));
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            result.skipped_folders += 1;
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            // Junctions and symlinks point at folders that are counted elsewhere.
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                let child = dirs.len();
                dirs.push(DirNode { name: entry.file_name(), parent: Some(node), size: 0, files: 0, children: Vec::new() });
                dirs[node].children.push(child);
                pending.push((entry.path(), child));
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !takes_local_space(&meta) {
                continue;
            }
            let size = meta.len();
            result.file_count += 1;
            result.total_bytes += size;
            dirs[node].size += size;
            dirs[node].files += 1;
            if size >= min_size {
                let path = entry.path();
                // Files Windows needs stay out of the "largest files" list.
                if !is_protected(&path) {
                    keep_largest(&mut largest, max_files, || LargeFile {
                        size,
                        path,
                        modified: meta.modified().ok(),
                    });
                }
            }
        }
        if last_report.elapsed().as_millis() > 150 {
            on_progress(CleanupProgress {
                files_scanned: result.file_count,
                bytes_counted: result.total_bytes,
                current_folder: dir.display().to_string(),
            });
            last_report = Instant::now();
        }
    }

    // A child always comes after its parent, so walking backwards adds each
    // folder's finished total to its parent.
    for i in (1..dirs.len()).rev() {
        let (size, files, parent) = (dirs[i].size, dirs[i].files, dirs[i].parent.unwrap_or(0));
        dirs[parent].size += size;
        dirs[parent].files += files;
    }
    for i in 0..dirs.len() {
        let mut children = std::mem::take(&mut dirs[i].children);
        children.sort_by(|&a, &b| dirs[b].size.cmp(&dirs[a].size));
        dirs[i].children = children;
    }
    result.files = largest.into_sorted_vec().into_iter().map(|Reverse(f)| f).collect();
    result.dirs = dirs;
    Ok(result)
}

/// Files directly inside `dir`, largest first (for the folder browser).
pub fn list_files(dir: &Path) -> Vec<LargeFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<LargeFile> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|k| k.is_file()))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            takes_local_space(&meta).then(|| LargeFile { size: meta.len(), path: e.path(), modified: meta.modified().ok() })
        })
        .collect();
    files.sort_by(|a, b| b.size.cmp(&a.size));
    files
}

fn keep_largest(heap: &mut BinaryHeap<Reverse<LargeFile>>, max: usize, make: impl FnOnce() -> LargeFile) {
    if max == 0 {
        return;
    }
    let file = make();
    if heap.len() < max {
        heap.push(Reverse(file));
    } else if heap.peek().is_some_and(|Reverse(smallest)| file.size > smallest.size) {
        heap.pop();
        heap.push(Reverse(file));
    }
}

/// Cloud placeholders (OneDrive "alleen online") report a size but use no
/// space on this disk.
#[cfg(windows)]
fn takes_local_space(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x40000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x400000;
    meta.file_attributes() & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) == 0
}

#[cfg(not(windows))]
fn takes_local_space(_meta: &std::fs::Metadata) -> bool {
    true
}

/// Files Windows needs to run: the page, hibernation and swap files at a
/// drive root, and everything in the Windows folder and the
/// "System Volume Information" folder. These may never be removed.
pub fn is_protected(path: &Path) -> bool {
    let text = path.to_string_lossy().to_lowercase();
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let windows_dir = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()).to_lowercase();
    let in_folder = |folder: &str| text.starts_with(&format!("{}\\", folder.trim_end_matches('\\')));
    matches!(name.as_str(), "pagefile.sys" | "hiberfil.sys" | "swapfile.sys" | "dumpstack.log.tmp")
        || in_folder(&windows_dir)
        || text.contains("\\system volume information\\")
}

/// A warning for files that should be cleaned up another way than by
/// deleting them, shown in the confirmation dialog.
pub fn removal_warning(path: &Path) -> Option<&'static str> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if matches!(ext.as_str(), "vhdx" | "vhd") {
        Some("Dit is een virtuele schijf (bijvoorbeeld van Docker of WSL). Ruim die op via het programma zelf, anders raakt het beschadigd.")
    } else {
        None
    }
}

/// Opens Explorer with the file selected, or opens the folder itself.
#[cfg(windows)]
pub fn show_in_explorer(path: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new("explorer.exe");
    if path.is_dir() {
        cmd.arg(path);
    } else {
        // Explorer wants `/select,"path"` as one raw argument.
        cmd.raw_arg(format!("/select,\"{}\"", path.display()));
    }
    cmd.spawn().map(|_| ())
}

#[cfg(not(windows))]
pub fn show_in_explorer(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Alleen op Windows"))
}

/// Moves a file to the Recycle Bin. Returns `Ok(false)` when the user
/// cancelled Windows' "te groot voor de prullenbak" warning.
#[cfg(windows)]
pub fn move_to_recycle_bin(path: &Path) -> io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_WANTNUKEWARNING, SHFILEOPSTRUCTW, SHFileOperationW,
    };
    use windows::core::PCWSTR;

    // pFrom is a list of paths ending with an extra NUL.
    let from: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    // FOF_WANTNUKEWARNING: without it Windows silently deletes files that are
    // too big for the Recycle Bin — exactly the files this screen is about.
    let flags = (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_WANTNUKEWARNING.0) as u16;
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: flags,
        ..Default::default()
    };
    let code = unsafe { SHFileOperationW(&mut op) };
    if op.fAnyOperationsAborted.as_bool() {
        return Ok(false);
    }
    if code != 0 || path.exists() {
        return Err(io::Error::other(format!(
            "verwijderen mislukt (code 0x{code:X}); het bestand is mogelijk in gebruik of beveiligd"
        )));
    }
    Ok(true)
}

#[cfg(not(windows))]
pub fn move_to_recycle_bin(_path: &Path) -> io::Result<bool> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Alleen op Windows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("dr_largefiles_{name}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, rel: &str, size: u64) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::File::create(path).unwrap().set_len(size).unwrap();
        }

        fn scan(&self, min_size: u64, max_files: usize) -> CleanupResult {
            scan(&self.0, min_size, max_files, &AtomicBool::new(false), |_| {}).unwrap()
        }

        fn rel(&self, path: &Path) -> String {
            path.strip_prefix(&self.0).unwrap().display().to_string()
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn names(r: &CleanupResult) -> Vec<String> {
        r.files.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn returns_files_above_minimum_largest_first() {
        let t = TempTree::new("order");
        t.file("a.bin", 3000);
        t.file("sub/b.bin", 5000);
        t.file("sub/deeper/c.bin", 4000);
        t.file("small.bin", 100);

        let r = t.scan(1000, MAX_FILE_RESULTS);

        assert_eq!(names(&r), ["b.bin", "c.bin", "a.bin"]);
    }

    #[test]
    fn keeps_only_the_largest_files() {
        let t = TempTree::new("limit");
        for (name, size) in [("a", 1000), ("b", 4000), ("c", 3000), ("d", 2000)] {
            t.file(name, size);
        }

        let r = t.scan(0, 2);

        assert_eq!(names(&r), ["b", "c"]);
    }

    #[test]
    fn folder_sizes_include_subfolders() {
        let t = TempTree::new("folders");
        t.file("x/one.bin", 1000);
        t.file("x/y/two.bin", 2000);
        t.file("z/three.bin", 500);
        t.file("rootfile.bin", 9000);

        let r = t.scan(0, MAX_FILE_RESULTS);

        let root = &r.dirs[0];
        assert_eq!((root.size, root.files), (12500, 4));
        let kids: Vec<(String, u64, u64)> = root
            .children
            .iter()
            .map(|&i| (t.rel(&r.dir_path(i)), r.dirs[i].size, r.dirs[i].files))
            .collect();
        assert_eq!(kids, [("x".to_string(), 3000, 2), ("z".to_string(), 500, 1)]);
        let y = r.dirs[root.children[0]].children[0];
        let y_rel = Path::new("x").join("y").display().to_string();
        assert_eq!((t.rel(&r.dir_path(y)), r.dirs[y].size), (y_rel, 2000));
    }

    #[test]
    fn subtract_file_updates_folder_and_ancestors() {
        let t = TempTree::new("subtract");
        t.file("x/y/two.bin", 2000);
        let mut r = t.scan(0, MAX_FILE_RESULTS);
        let x = r.dirs[0].children[0];
        let y = r.dirs[x].children[0];

        r.subtract_file(y, 2000);

        assert_eq!((r.dirs[y].size, r.dirs[x].size, r.dirs[0].size, r.total_bytes), (0, 0, 0, 0));
    }

    #[test]
    fn list_files_returns_direct_files_largest_first() {
        let t = TempTree::new("list");
        t.file("a.bin", 10);
        t.file("b.bin", 30);
        t.file("sub/c.bin", 99);

        let files = list_files(&t.0);

        let got: Vec<(String, u64)> = files.iter().map(|f| (t.rel(&f.path), f.size)).collect();
        assert_eq!(got, [("b.bin".to_string(), 30), ("a.bin".to_string(), 10)]);
    }

    #[test]
    fn windows_system_files_are_protected() {
        assert!(is_protected(Path::new(r"C:\pagefile.sys")));
        assert!(is_protected(Path::new(r"D:\HIBERFIL.SYS")));
        assert!(is_protected(Path::new(r"C:\Windows\System32\ntoskrnl.exe")));
        assert!(is_protected(Path::new(r"C:\System Volume Information\x.dat")));
        assert!(!is_protected(Path::new(r"C:\Users\frank\Downloads\film.mkv")));
        assert!(!is_protected(Path::new(r"C:\WindowsApps\x.bin")));
    }

    #[test]
    fn counts_every_file_in_totals() {
        let t = TempTree::new("totals");
        t.file("a.bin", 100);
        t.file("sub/b.bin", 200);

        let r = t.scan(1_000_000, MAX_FILE_RESULTS);

        assert_eq!((r.file_count, r.total_bytes, r.files.len()), (2, 300, 0));
    }

    #[test]
    fn cancelled_scan_returns_interrupted() {
        let t = TempTree::new("cancel");
        t.file("a.bin", 100);

        let err = scan(&t.0, 0, 10, &AtomicBool::new(true), |_| {}).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn missing_root_is_an_error() {
        let t = TempTree::new("missing");

        let err = scan(&t.0.join("bestaat-niet"), 0, 10, &AtomicBool::new(false), |_| {}).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    /// Leaves a 10-byte file in the real Recycle Bin, so only on request:
    /// `cargo test -- --ignored`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn moves_file_to_recycle_bin() {
        let t = TempTree::new("recycle");
        t.file("weg.bin", 10);
        let path = t.0.join("weg.bin");

        assert!(move_to_recycle_bin(&path).unwrap());
        assert!(!path.exists());
    }
}
