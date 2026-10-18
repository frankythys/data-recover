//! Read-only walk of a drive or folder that finds the user's own largest
//! files and folders, plus the two actions the cleanup screen offers on a
//! result: show it in Explorer and move it to the Recycle Bin.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

pub const MAX_FILE_RESULTS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LargeFile {
    pub size: u64,
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone, Default)]
pub struct CleanupProgress {
    pub files_scanned: u64,
    pub bytes_counted: u64,
    pub current_folder: String,
}

/// One of the user's own folders. Sizes include subfolders but only count
/// the user's own files.
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

#[derive(Debug, Default)]
pub struct CleanupResult {
    /// The user's own files, largest first.
    pub files: Vec<LargeFile>,
    /// The user's own folders; index 0 is the scanned root. Windows and
    /// program folders are left out.
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

    /// Folders to show at the top level: the root's subfolders, with
    /// container folders (`C:\Users`, `C:\Users\<naam>`) replaced by what is
    /// inside them. Largest first.
    pub fn top_folders(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut pending: Vec<usize> = self.dirs.first().map(|d| d.children.clone()).unwrap_or_default();
        while let Some(i) = pending.pop() {
            if is_container(&self.dir_path(i)) {
                pending.extend(self.dirs[i].children.iter().copied());
            } else {
                out.push(i);
            }
        }
        out.sort_by(|&a, &b| self.dirs[b].size.cmp(&self.dirs[a].size));
        out
    }

    /// Takes folder `index` out of the tree after it was removed from disk,
    /// lowering the totals of its ancestors, and drops its large files.
    pub fn remove_dir(&mut self, index: usize) {
        let path = self.dir_path(index);
        let (size, files) = (self.dirs[index].size, self.dirs[index].files);
        if let Some(parent) = self.dirs[index].parent {
            self.dirs[parent].children.retain(|&c| c != index);
        }
        self.subtract(self.dirs[index].parent, size, files);
        self.dirs[index].size = 0;
        self.dirs[index].files = 0;
        self.files.retain(|f| !f.path.starts_with(&path));
    }

    /// Takes a removed file out of the totals of `folder` and its ancestors.
    pub fn remove_file(&mut self, folder: Option<usize>, file: &LargeFile) {
        self.subtract(folder, file.size, 1);
        self.files.retain(|f| f.path != file.path);
    }

    fn subtract(&mut self, from: Option<usize>, size: u64, files: u64) {
        let mut cur = from;
        while let Some(i) = cur {
            let node = &mut self.dirs[i];
            node.size = node.size.saturating_sub(size);
            node.files = node.files.saturating_sub(files);
            cur = node.parent;
        }
        self.total_bytes = self.total_bytes.saturating_sub(size);
        self.file_count = self.file_count.saturating_sub(files);
    }

    /// Index of the folder at `path`, found by walking down from the root.
    pub fn find_dir(&self, path: &Path) -> Option<usize> {
        let rest = path.strip_prefix(self.dir_path(0)).ok()?;
        let mut node = 0;
        for part in rest.components() {
            node = *self.dirs[node].children.iter().find(|&&c| self.dirs[c].name == part.as_os_str())?;
        }
        Some(node)
    }
}

/// Walks `root`, totals the user's own folders and keeps the user's largest
/// files of at least `min_size` bytes (see [`is_system_or_app`]). Never
/// modifies anything on disk.
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
    // (folder, its node; None inside Windows and program folders)
    let mut pending: Vec<(PathBuf, Option<usize>)> = vec![(root.to_path_buf(), Some(0))];
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
                let path = entry.path();
                // Everything below a Windows or program folder stays out of
                // the tree, but is still walked for the totals.
                let child = node.filter(|_| !is_system_or_app(&path)).map(|parent| {
                    let index = dirs.len();
                    dirs.push(DirNode { name: entry.file_name(), parent: Some(parent), size: 0, files: 0, children: Vec::new() });
                    dirs[parent].children.push(index);
                    index
                });
                pending.push((path, child));
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !takes_local_space(&meta) {
                continue;
            }
            let size = meta.len();
            result.file_count += 1;
            result.total_bytes += size;
            let Some(node) = node else { continue };
            if is_protected_file_name(&entry.file_name().to_string_lossy()) {
                continue;
            }
            dirs[node].size += size;
            dirs[node].files += 1;
            if size >= min_size {
                keep_largest(&mut largest, max_files, || LargeFile {
                    size,
                    path: entry.path(),
                    modified: meta.modified().ok(),
                });
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

/// The user's own files directly inside `dir`, largest first.
pub fn list_files(dir: &Path) -> Vec<LargeFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<LargeFile> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|k| k.is_file()))
        .filter(|e| !is_system_or_app(&e.path()))
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
pub(crate) fn takes_local_space(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x40000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x400000;
    meta.file_attributes() & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) == 0
}

#[cfg(not(windows))]
pub(crate) fn takes_local_space(_meta: &std::fs::Metadata) -> bool {
    true
}

/// Folder names that hold Windows or installed programs and their data.
/// Removing files from these by hand breaks Windows or the program.
const SYSTEM_AND_APP_FOLDERS: [&str; 9] = [
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "appdata",
    "$recycle.bin",
    "system volume information",
    "recovery",
    "$windows.~bt",
];

fn is_protected_file_name(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "pagefile.sys" | "hiberfil.sys" | "swapfile.sys" | "dumpstack.log.tmp"
    )
}

/// Lower-case names of the normal path components (no drive or root).
fn lower_parts(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect()
}

/// Whether `path` (a file or a folder) belongs to Windows or an installed
/// program rather than to the user: the page/hibernation/swap files,
/// anything inside or named like one of [`SYSTEM_AND_APP_FOLDERS`], and the
/// program folders in a user profile whose name starts with a dot
/// (`C:\Users\<naam>\.ollama`, `.android`, `.gradle`, …). Such paths are
/// never listed and never removed.
pub fn is_system_or_app(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    if is_protected_file_name(&name) {
        return true;
    }
    let parts = lower_parts(path);
    parts.iter().enumerate().any(|(i, part)| {
        SYSTEM_AND_APP_FOLDERS.contains(&part.as_str()) || (part.starts_with('.') && i == 2 && parts[0] == "users")
    })
}

/// Folders that only hold other folders and may never be removed as a
/// whole: a drive root, `C:\Users` and each user profile.
pub fn is_container(path: &Path) -> bool {
    let parts = lower_parts(path);
    parts.is_empty() || (parts[0] == "users" && parts.len() <= 2)
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
    if is_system_or_app(path) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "systeem- of programmabestand, niet verwijderd"));
    }
    if path.is_dir() && is_container(path) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "deze map bevat andere mappen en kan niet in één keer weg"));
    }
    recycle_unchecked(path)
}

/// [`move_to_recycle_bin`] without the safety checks; callers check first.
#[cfg(windows)]
pub fn recycle_unchecked(path: &Path) -> io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_SILENT, FOF_WANTNUKEWARNING, SHFILEOPSTRUCTW, SHFileOperationW,
    };
    use windows::core::PCWSTR;

    // pFrom is a list of paths ending with an extra NUL.
    let from: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    // FOF_WANTNUKEWARNING: without it Windows silently deletes files that are
    // too big for the Recycle Bin — exactly the files this screen is about.
    // FOF_SILENT: the app shows its own progress.
    let flags = (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_WANTNUKEWARNING.0 | FOF_SILENT.0) as u16;
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

#[cfg(not(windows))]
pub fn recycle_unchecked(_path: &Path) -> io::Result<bool> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Alleen op Windows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test tree in a fresh folder at the root of the system drive. The temp
    /// folder lives in AppData, which `is_system_or_app` hides, so the tests
    /// cannot use it.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let base = std::env::var_os("SystemDrive").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:"));
            let dir = base.join("\\").join(format!("dr_largefiles_{name}_{}", std::process::id()));
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
    fn system_and_program_files_are_not_listed_but_counted() {
        let t = TempTree::new("hidden");
        t.file("film.mkv", 5000);
        t.file("AppData/Local/model.bin", 9000);
        t.file("Program Files/App/app.bin", 8000);
        t.file("pagefile.sys", 7000);

        let r = t.scan(1000, MAX_FILE_RESULTS);

        assert_eq!(names(&r), ["film.mkv"]);
        assert_eq!(r.total_bytes, 29000);
    }

    #[test]
    fn recognises_system_and_program_locations() {
        let hidden = [
            r"C:\pagefile.sys",
            r"D:\HIBERFIL.SYS",
            r"C:\Windows\System32\ntoskrnl.exe",
            r"C:\Program Files\App\app.exe",
            r"C:\ProgramData\Docker\x.vhdx",
            r"C:\Users\frank\AppData\Local\Docker\wsl\disk\docker_data.vhdx",
            r"C:\Users\frank\.ollama\models\blobs\sha256-1",
            r"C:\$Recycle.Bin\S-1-5\$R123.mkv",
            r"C:\Users\frank\AppData",
            r"C:\Users\frank\.ollama",
        ];
        let shown = [
            r"C:\Users\frank\Downloads\film.mkv",
            r"C:\models\Qwen.gguf",
            r"C:\Users\frank\Documents\WindowsBackup.zip",
            r"C:\Programming\project\.git\pack.pack",
            r"C:\Users\frank\Documents",
        ];
        for p in hidden {
            assert!(is_system_or_app(Path::new(p)), "{p} hoort verborgen te zijn");
        }
        for p in shown {
            assert!(!is_system_or_app(Path::new(p)), "{p} hoort zichtbaar te zijn");
        }
    }

    #[test]
    fn folder_sizes_count_only_own_files() {
        let t = TempTree::new("folders");
        t.file("x/one.bin", 1000);
        t.file("x/y/two.bin", 2000);
        t.file("x/AppData/prog.bin", 9000);
        t.file("z/three.bin", 500);

        let r = t.scan(0, MAX_FILE_RESULTS);

        let root = &r.dirs[0];
        assert_eq!((root.size, root.files), (3500, 3));
        let kids: Vec<(String, u64)> = root
            .children
            .iter()
            .map(|&i| (r.dirs[i].name.to_string_lossy().into_owned(), r.dirs[i].size))
            .collect();
        assert_eq!(kids, [("x".to_string(), 3000), ("z".to_string(), 500)]);
        let x = root.children[0];
        assert_eq!(r.dirs[x].children.len(), 1, "AppData hoort niet in de boom");
        assert_eq!(r.dir_path(r.dirs[x].children[0]), t.0.join("x").join("y"));
        assert_eq!(r.find_dir(&t.0.join("x").join("y")), Some(r.dirs[x].children[0]));
    }

    #[test]
    fn remove_dir_updates_totals_and_large_files() {
        let t = TempTree::new("removedir");
        t.file("x/y/two.bin", 2000);
        t.file("z/one.bin", 1000);
        let mut r = t.scan(0, MAX_FILE_RESULTS);
        let x = r.find_dir(&t.0.join("x")).unwrap();
        let y = r.find_dir(&t.0.join("x").join("y")).unwrap();

        r.remove_dir(y);

        assert_eq!((r.dirs[x].size, r.dirs[0].size, r.total_bytes), (0, 1000, 1000));
        assert!(r.dirs[x].children.is_empty());
        assert_eq!(names(&r), ["one.bin"]);
    }

    #[test]
    fn containers_are_drive_roots_users_and_profiles() {
        for p in [r"C:\", r"C:\Users", r"C:\Users\frank"] {
            assert!(is_container(Path::new(p)), "{p}");
        }
        for p in [r"C:\Programming", r"C:\Users\frank\Downloads"] {
            assert!(!is_container(Path::new(p)), "{p}");
        }
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

    #[cfg(windows)]
    #[test]
    fn refuses_to_recycle_program_files() {
        let err = move_to_recycle_bin(Path::new(r"C:\Users\x\AppData\Local\iets.bin")).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
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
