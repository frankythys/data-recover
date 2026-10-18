//! Which deleted files a scan shows, and where files from an emptied
//! Recycle Bin originally came from.
//!
//! The file-system scanners report every deleted file on the drive. A
//! [`Collector`] moves them into the results while the scan runs, keeping
//! only those in the chosen [`Scope`]. Files in the Recycle Bin are stored
//! as `$R<id>.ext` next to a small `$I<id>.ext` that holds the original
//! path; the collector pairs them up so such files show with their own
//! name and folder (and so a Desktop scan also finds them).

use crate::ntfs::{self, DataLocation, RecoveredFile};
use std::collections::HashMap;

/// Largest `$I` file read; real ones are a few hundred bytes.
const MAX_INFO_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Scope {
    /// Every deleted file on the drive.
    Drive,
    /// Files that were in this folder or below, as a path on the volume
    /// without the drive letter (`\Users\frank\Desktop`).
    Folder(String),
    /// Only files that were in the Recycle Bin when it was emptied.
    RecycleBin,
}

impl Scope {
    fn includes(&self, rf: &RecoveredFile, from_bin: bool) -> bool {
        match self {
            Scope::Drive => true,
            Scope::Folder(dir) => is_under(&rf.path, dir),
            Scope::RecycleBin => from_bin,
        }
    }
}

/// Whether folder `path` is `dir` or inside it (NTFS ignores case).
fn is_under(path: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('\\').to_lowercase();
    let path = path.to_lowercase();
    dir.is_empty() || path == dir || path.strip_prefix(&dir).is_some_and(|rest| rest.starts_with('\\'))
}

#[derive(Debug, PartialEq)]
enum BinKind {
    /// `$I…`: holds the original path.
    Info,
    /// `$R…`: the deleted file itself.
    Data,
    /// A file inside a deleted folder `$R…`; `rest` is its folder below it.
    Inside { rest: String },
}

/// Where a file sits in the Recycle Bin, or `None` for any other file.
/// `key` pairs a `$R` with its `$I`: the user's bin folder plus the id.
fn bin_entry(rf: &RecoveredFile) -> Option<(String, BinKind)> {
    let parts: Vec<&str> = rf.path.trim_start_matches('\\').split('\\').collect();
    if parts.len() < 2 || !parts[0].eq_ignore_ascii_case("$recycle.bin") {
        return None;
    }
    let user = parts[1].to_lowercase();
    let id = |name: &str, prefix: &str| {
        let head = name.get(..2)?;
        head.eq_ignore_ascii_case(prefix).then(|| format!("{user}\\{}", name[2..].to_lowercase()))
    };
    if parts.len() == 2 {
        if let Some(key) = id(&rf.name, "$I") {
            return Some((key, BinKind::Info));
        }
        return id(&rf.name, "$R").map(|key| (key, BinKind::Data));
    }
    let key = id(parts[2], "$R")?;
    Some((key, BinKind::Inside { rest: parts[3..].join("\\") }))
}

/// The original full path stored in a `$I` file (`C:\Users\…\foto.jpg`).
/// Version 1 (Vista–8) has a fixed 260-character field, version 2
/// (Windows 10 and later) a length followed by the path.
fn parse_info(bytes: &[u8]) -> Option<String> {
    let version = u64::from_le_bytes(bytes.get(0..8)?.try_into().ok()?);
    let utf16 = match version {
        1 => bytes.get(24..)?,
        2 => {
            let chars = u32::from_le_bytes(bytes.get(24..28)?.try_into().ok()?) as usize;
            bytes.get(28..28 + chars.checked_mul(2)?)?
        }
        _ => return None,
    };
    let units: Vec<u16> =
        utf16.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).take(32_768).collect();
    String::from_utf16(&units).ok().filter(|s| !s.is_empty())
}

/// `rf` as it was before it went to the Recycle Bin. `original` is the
/// full path of the `$R` file or folder it belongs to.
fn restore(mut rf: RecoveredFile, kind: &BinKind, original: &str) -> Option<RecoveredFile> {
    // Drop the drive letter: paths in the results are relative to the volume.
    let on_volume = &original[original.find('\\')?..];
    match kind {
        BinKind::Data => {
            let split = on_volume.rfind('\\')?;
            rf.name = on_volume[split + 1..].to_string();
            rf.path = if split == 0 { "\\".to_string() } else { on_volume[..split].to_string() };
        }
        BinKind::Inside { rest } => {
            rf.path = if rest.is_empty() { on_volume.to_string() } else { format!("{on_volume}\\{rest}") };
        }
        BinKind::Info => return None,
    }
    (!rf.name.is_empty()).then_some(rf)
}

pub struct Collector {
    scope: Scope,
    /// Scan source, to read `$I` files whose data is not in the MFT record.
    source: String,
    /// Original path per bin key, from the `$I` files seen so far.
    originals: HashMap<String, String>,
    /// Bin files whose `$I` has not been seen yet.
    waiting: Vec<(String, BinKind, RecoveredFile)>,
}

impl Collector {
    pub fn new(scope: Scope, source: String) -> Self {
        Self { scope, source, originals: HashMap::new(), waiting: Vec::new() }
    }

    /// Moves the scanner's new files from `raw` into `found`, keeping only
    /// what is in scope. Bin files wait until their `$I` turns up.
    pub fn drain(&mut self, raw: &mut Vec<RecoveredFile>, found: &mut Vec<RecoveredFile>) {
        for rf in raw.drain(..) {
            match bin_entry(&rf) {
                None => {
                    if self.scope.includes(&rf, false) {
                        found.push(rf);
                    }
                }
                Some((key, BinKind::Info)) => {
                    if let Some(original) = self.read_info(&rf) {
                        self.originals.insert(key, original);
                    }
                }
                Some((key, kind)) => self.waiting.push((key, kind, rf)),
            }
        }
        for (key, kind, rf) in std::mem::take(&mut self.waiting) {
            let Some(original) = self.originals.get(&key) else {
                self.waiting.push((key, kind, rf));
                continue;
            };
            if let Some(rf) = restore(rf, &kind, original).filter(|rf| self.scope.includes(rf, true)) {
                found.push(rf);
            }
        }
    }

    /// Call when the scan is done: bin files whose `$I` was not found are
    /// shown under their bin name, unless only a folder is shown.
    pub fn finish(&mut self, found: &mut Vec<RecoveredFile>) {
        for (_, _, rf) in std::mem::take(&mut self.waiting) {
            if self.scope.includes(&rf, true) {
                found.push(rf);
            }
        }
    }

    fn read_info(&self, rf: &RecoveredFile) -> Option<String> {
        match &rf.data {
            DataLocation::Resident(bytes) => parse_info(bytes),
            DataLocation::NonResident(_) => parse_info(&ntfs::read_file_bytes(&self.source, rf, MAX_INFO_BYTES).ok()?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIN: &str = r"\$Recycle.Bin\S-1-5-21-1";

    fn file(path: &str, name: &str) -> RecoveredFile {
        RecoveredFile { name: name.into(), size: 10, data: DataLocation::Resident(vec![1; 10]), path: path.into(), year: 2024, month: 5 }
    }

    /// A Windows 10 `$I` file for `original`.
    fn info(name: &str, original: &str) -> RecoveredFile {
        let units: Vec<u16> = original.encode_utf16().chain([0]).collect();
        let mut bytes = Vec::new();
        bytes.extend(2u64.to_le_bytes());
        bytes.extend(10u64.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        bytes.extend((units.len() as u32).to_le_bytes());
        bytes.extend(units.iter().flat_map(|u| u.to_le_bytes()));
        RecoveredFile { data: DataLocation::Resident(bytes), ..file(BIN, name) }
    }

    fn collect(scope: Scope, files: Vec<RecoveredFile>) -> Vec<(String, String)> {
        let mut c = Collector::new(scope, String::new());
        let mut found = Vec::new();
        for f in files {
            c.drain(&mut vec![f], &mut found);
        }
        c.finish(&mut found);
        found.into_iter().map(|f| (f.path, f.name)).collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(p, n)| (p.to_string(), n.to_string())).collect()
    }

    #[test]
    fn bin_files_get_their_original_name_and_folder() {
        let found = collect(
            Scope::Drive,
            vec![file(BIN, "$RAB12CD.jpg"), info("$IAB12CD.jpg", r"C:\Users\frank\Desktop\vakantie.jpg")],
        );

        assert_eq!(found, pairs(&[(r"\Users\frank\Desktop", "vakantie.jpg")]));
    }

    #[test]
    fn files_inside_a_deleted_folder_keep_their_subfolder() {
        let found = collect(
            Scope::Drive,
            vec![
                info("$IXY9", r"C:\Users\frank\Documents\Project"),
                file(&format!(r"{BIN}\$RXY9\src"), "main.rs"),
                file(&format!(r"{BIN}\$RXY9"), "readme.md"),
            ],
        );

        assert_eq!(
            found,
            pairs(&[(r"\Users\frank\Documents\Project\src", "main.rs"), (r"\Users\frank\Documents\Project", "readme.md")])
        );
    }

    #[test]
    fn folder_scope_keeps_only_that_folder_and_below_ignoring_case() {
        let found = collect(
            Scope::Folder(r"\Users\Frank\Desktop".into()),
            vec![
                file(r"\Users\frank\Desktop", "a.txt"),
                file(r"\Users\frank\Desktop\sub", "b.txt"),
                file(r"\Users\frank\DesktopOud", "c.txt"),
                file(r"\Users\frank", "d.txt"),
                file(BIN, "$R1.txt"),
                info("$I1.txt", r"C:\Users\frank\Desktop\e.txt"),
                file(BIN, "$R2.txt"),
            ],
        );

        assert_eq!(
            found,
            pairs(&[(r"\Users\frank\Desktop", "a.txt"), (r"\Users\frank\Desktop\sub", "b.txt"), (r"\Users\frank\Desktop", "e.txt")])
        );
    }

    #[test]
    fn recycle_bin_scope_shows_only_bin_files_also_without_info() {
        let found = collect(
            Scope::RecycleBin,
            vec![file(r"\Users\frank", "gewoon.txt"), file(BIN, "$R1.txt"), info("$I1.txt", r"C:\x\terug.txt"), file(BIN, "$R2.txt")],
        );

        assert_eq!(found, pairs(&[(r"\x", "terug.txt"), (BIN, "$R2.txt")]));
    }

    #[test]
    fn info_files_and_other_bin_files_are_not_results() {
        let found = collect(Scope::Drive, vec![info("$I1.txt", r"C:\a.txt"), file(BIN, "desktop.ini")]);

        assert_eq!(found, pairs(&[(BIN, "desktop.ini")]));
    }

    #[test]
    fn file_from_the_drive_root_goes_back_to_the_root() {
        let found = collect(Scope::Drive, vec![file(BIN, "$R1.txt"), info("$I1.txt", r"D:\los.txt")]);

        assert_eq!(found, pairs(&[("\\", "los.txt")]));
    }

    #[test]
    fn reads_version_1_info() {
        let mut bytes = vec![0u8; 24 + 520];
        bytes[0] = 1;
        for (i, u) in r"C:\oud.doc".encode_utf16().enumerate() {
            bytes[24 + i * 2..26 + i * 2].copy_from_slice(&u.to_le_bytes());
        }

        assert_eq!(parse_info(&bytes).as_deref(), Some(r"C:\oud.doc"));
        assert_eq!(parse_info(&[9; 40]), None);
        assert_eq!(parse_info(&[2, 0, 0, 0, 0, 0, 0, 0]), None);
    }
}
