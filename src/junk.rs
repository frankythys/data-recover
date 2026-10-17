//! "Systeem opschonen": caches and temporary files that programs recreate on
//! their own, so they are safe to remove. Files that are in use are skipped.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Temporary files younger than this may belong to a running installer.
const TEMP_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    UserTemp,
    WindowsTemp,
    BrowserCache,
    UpdateCache,
    CrashDumps,
    RecycleBin,
}

pub const ALL: [Kind; 6] = [
    Kind::UserTemp,
    Kind::BrowserCache,
    Kind::RecycleBin,
    Kind::UpdateCache,
    Kind::WindowsTemp,
    Kind::CrashDumps,
];

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::UserTemp => "Tijdelijke bestanden",
            Kind::WindowsTemp => "Tijdelijke Windows-bestanden",
            Kind::BrowserCache => "Browsercache",
            Kind::UpdateCache => "Windows Update-downloads",
            Kind::CrashDumps => "Crashdumps en foutrapporten",
            Kind::RecycleBin => "Prullenbak",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Kind::UserTemp => "Restanten van installaties en programma's (ouder dan 24 uur)",
            Kind::WindowsTemp => "Tijdelijke bestanden van Windows zelf (ouder dan 24 uur)",
            Kind::BrowserCache => "Opgeslagen webpagina's van Chrome, Edge, Brave en Firefox",
            Kind::UpdateCache => "Al geïnstalleerde updates die nog op schijf staan",
            Kind::CrashDumps => "Geheugendumps en rapporten van vastgelopen programma's",
            Kind::RecycleBin => "Bestanden die je eerder hebt verwijderd",
        }
    }

    fn min_age(self) -> Option<Duration> {
        matches!(self, Kind::UserTemp | Kind::WindowsTemp).then_some(TEMP_MIN_AGE)
    }

    /// Folders whose contents this category removes (the folders stay).
    fn folders(self) -> Vec<PathBuf> {
        let windows = env_path("SystemRoot").unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let local = env_path("LOCALAPPDATA");
        match self {
            Kind::UserTemp => vec![std::env::temp_dir()],
            Kind::WindowsTemp => vec![windows.join("Temp")],
            Kind::BrowserCache => local.map(|l| browser_cache_folders(&l)).unwrap_or_default(),
            Kind::UpdateCache => vec![windows.join("SoftwareDistribution").join("Download")],
            Kind::CrashDumps => {
                let mut folders = vec![windows.join("Minidump")];
                if let Some(l) = local {
                    folders.push(l.join("CrashDumps"));
                }
                if let Some(data) = env_path("ProgramData") {
                    let wer = data.join("Microsoft").join("Windows").join("WER");
                    folders.push(wer.join("ReportArchive"));
                    folders.push(wer.join("ReportQueue"));
                }
                folders
            }
            Kind::RecycleBin => Vec::new(),
        }
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// Cache folders of every profile of the common browsers.
fn browser_cache_folders(local: &Path) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    let chromium = ["Google\\Chrome", "Microsoft\\Edge", "BraveSoftware\\Brave-Browser"];
    for browser in chromium {
        let Ok(profiles) = std::fs::read_dir(local.join(browser).join("User Data")) else {
            continue;
        };
        for profile in profiles.flatten() {
            for cache in ["Cache", "Code Cache", "GPUCache"] {
                let dir = profile.path().join(cache);
                if dir.is_dir() {
                    folders.push(dir);
                }
            }
        }
    }
    if let Ok(profiles) = std::fs::read_dir(local.join("Mozilla").join("Firefox").join("Profiles")) {
        for profile in profiles.flatten() {
            let dir = profile.path().join("cache2");
            if dir.is_dir() {
                folders.push(dir);
            }
        }
    }
    folders
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Measure {
    pub bytes: u64,
    pub files: u64,
    /// False when the folders exist but cannot be read (needs administrator).
    pub accessible: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CleanReport {
    pub freed: u64,
    pub removed: u64,
    /// Files that were in use, too new or could not be removed.
    pub skipped: u64,
}

pub fn measure(kind: Kind) -> Measure {
    match kind {
        Kind::RecycleBin => recycle_bin_size(),
        _ => measure_folders(&kind.folders(), kind.min_age()),
    }
}

pub fn clean(kind: Kind) -> CleanReport {
    match kind {
        Kind::RecycleBin => empty_recycle_bin(),
        _ => clean_folders(&kind.folders(), kind.min_age()),
    }
}

fn old_enough(meta: &std::fs::Metadata, min_age: Option<Duration>, now: SystemTime) -> bool {
    let Some(min_age) = min_age else { return true };
    meta.modified()
        .ok()
        .and_then(|m| now.duration_since(m).ok())
        .is_some_and(|age| age >= min_age)
}

/// Size of everything `clean_folders` would remove.
pub fn measure_folders(folders: &[PathBuf], min_age: Option<Duration>) -> Measure {
    let now = SystemTime::now();
    let mut m = Measure { accessible: true, ..Default::default() };
    let existing: Vec<&PathBuf> = folders.iter().filter(|f| f.is_dir()).collect();
    if !existing.is_empty() && existing.iter().all(|f| std::fs::read_dir(f).is_err()) {
        m.accessible = false;
        return m;
    }
    let mut pending: Vec<PathBuf> = existing.into_iter().cloned().collect();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                pending.push(entry.path());
            } else if let Ok(meta) = entry.metadata() {
                if old_enough(&meta, min_age, now) {
                    m.bytes += meta.len();
                    m.files += 1;
                }
            }
        }
    }
    m
}

/// Removes the contents of `folders` (not the folders themselves). Never
/// follows links; files in use or younger than `min_age` are skipped.
pub fn clean_folders(folders: &[PathBuf], min_age: Option<Duration>) -> CleanReport {
    let now = SystemTime::now();
    let mut report = CleanReport::default();
    for folder in folders {
        clean_dir(folder, min_age, now, &mut report);
    }
    report
}

fn clean_dir(dir: &Path, min_age: Option<Duration>, now: SystemTime, report: &mut CleanReport) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        if kind.is_dir() {
            clean_dir(&path, min_age, now, report);
            // Only succeeds when everything inside could be removed.
            let _ = std::fs::remove_dir(&path);
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !old_enough(&meta, min_age, now) {
            report.skipped += 1;
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                report.freed += meta.len();
                report.removed += 1;
            }
            Err(_) => report.skipped += 1,
        }
    }
}

#[cfg(windows)]
fn recycle_bin_size() -> Measure {
    use windows::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};
    use windows::core::PCWSTR;
    let mut info = SHQUERYRBINFO { cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32, ..Default::default() };
    match unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut info) } {
        Ok(()) => Measure { bytes: info.i64Size.max(0) as u64, files: info.i64NumItems.max(0) as u64, accessible: true },
        Err(_) => Measure { accessible: false, ..Default::default() },
    }
}

#[cfg(windows)]
fn empty_recycle_bin() -> CleanReport {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHEmptyRecycleBinW};
    use windows::core::PCWSTR;
    let before = recycle_bin_size();
    let flags = SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND;
    // Fails harmlessly when the bin is already empty.
    let _ = unsafe { SHEmptyRecycleBinW(HWND::default(), PCWSTR::null(), flags) };
    let after = recycle_bin_size();
    CleanReport {
        freed: before.bytes.saturating_sub(after.bytes),
        removed: before.files.saturating_sub(after.files),
        skipped: after.files,
    }
}

#[cfg(not(windows))]
fn recycle_bin_size() -> Measure {
    Measure { accessible: false, ..Default::default() }
}

#[cfg(not(windows))]
fn empty_recycle_bin() -> CleanReport {
    CleanReport::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("dr_junk_{name}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, rel: &str, size: u64) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::File::create(&path).unwrap().set_len(size).unwrap();
            path
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn measures_all_files_below_the_folders() {
        let t = TempTree::new("measure");
        t.file("a.tmp", 100);
        t.file("sub/b.tmp", 50);

        let m = measure_folders(&[t.0.clone()], None);

        assert_eq!(m, Measure { bytes: 150, files: 2, accessible: true });
    }

    #[test]
    fn missing_folders_count_as_empty() {
        let t = TempTree::new("missing");

        let m = measure_folders(&[t.0.join("bestaat-niet")], None);

        assert_eq!(m, Measure { bytes: 0, files: 0, accessible: true });
    }

    #[test]
    fn clean_removes_contents_but_keeps_the_folder() {
        let t = TempTree::new("clean");
        t.file("a.tmp", 100);
        t.file("sub/deeper/b.tmp", 50);

        let report = clean_folders(&[t.0.clone()], None);

        assert_eq!(report, CleanReport { freed: 150, removed: 2, skipped: 0 });
        assert!(t.0.is_dir());
        assert_eq!(std::fs::read_dir(&t.0).unwrap().count(), 0);
    }

    #[test]
    fn new_files_are_kept_when_a_minimum_age_is_set() {
        let t = TempTree::new("age");
        let fresh = t.file("nieuw.tmp", 100);

        let m = measure_folders(&[t.0.clone()], Some(TEMP_MIN_AGE));
        let report = clean_folders(&[t.0.clone()], Some(TEMP_MIN_AGE));

        assert_eq!((m.bytes, report.removed, report.skipped), (0, 0, 1));
        assert!(fresh.exists());
    }

    #[cfg(windows)]
    #[test]
    fn files_in_use_are_skipped() {
        let t = TempTree::new("inuse");
        let path = t.file("open.tmp", 10);
        // Opening without delete sharing locks the file, like a running program.
        use std::os::windows::fs::OpenOptionsExt;
        let _lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();

        let report = clean_folders(&[t.0.clone()], None);

        assert_eq!((report.removed, report.skipped), (0, 1));
        assert!(path.exists());
    }
}
