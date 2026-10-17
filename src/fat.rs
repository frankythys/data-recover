//! Deleted-file recovery for FAT32 and exFAT volumes (common on USB sticks
//! and SD cards). Both walk the directory tree, collect entries whose
//! "deleted" marker is set, and reconstruct each file's name and data run
//! (assuming contiguous allocation, since the cluster chain is usually
//! cleared on deletion).

use crate::disk;
use crate::ntfs::{DataLocation, RecoveredFile, ScanProgress};
use std::collections::HashSet;
use std::fs::File;
use std::io;

const MAX_DIR_CLUSTERS: usize = 100_000;
const MAX_ENTRIES: u64 = 20_000_000;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
    ])
}

/// Recognises a FAT32 boot sector (used so a bare `\\.\D:` volume handle is
/// scanned correctly). exFAT and NTFS are matched by their 8-byte OEM tag.
pub fn is_fat32_boot(boot: &[u8]) -> bool {
    if boot.len() < 512 || boot[510] != 0x55 || boot[511] != 0xAA {
        return false;
    }
    let bytes_per_sector = u16le(boot, 11);
    if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
        return false;
    }
    &boot[82..87] == b"FAT32"
}

// ---------------------------------------------------------------------------
// FAT32
// ---------------------------------------------------------------------------

struct Fat32 {
    part_off: u64,
    bytes_per_sector: u64,
    sectors_per_cluster: u64,
    fat_off: u64,
    data_start_sector: u64,
}

impl Fat32 {
    fn cluster_offset(&self, cluster: u32) -> u64 {
        self.part_off
            + (self.data_start_sector + (cluster as u64 - 2) * self.sectors_per_cluster)
                * self.bytes_per_sector
    }
    fn cluster_size(&self) -> u64 {
        self.bytes_per_sector * self.sectors_per_cluster
    }
    fn next_cluster(&self, file: &mut File, cluster: u32) -> io::Result<u32> {
        let entry = disk::read_at(file, self.fat_off + cluster as u64 * 4, 4)?;
        if entry.len() < 4 {
            return Ok(0x0FFF_FFFF);
        }
        Ok(u32le(&entry, 0) & 0x0FFF_FFFF)
    }
}

pub fn scan_fat32(
    file: &mut File,
    part_off: u64,
    cancel: &std::sync::atomic::AtomicBool,
    found: &std::sync::Mutex<Vec<RecoveredFile>>,
    on_progress: &mut dyn FnMut(ScanProgress),
) -> io::Result<()> {
    use std::sync::atomic::Ordering;

    let boot = disk::read_at(file, part_off, 512)?;
    if boot.len() < 512 {
        return Ok(());
    }
    let bps = u16le(&boot, 11) as u64;
    let spc = boot[13] as u64;
    let reserved = u16le(&boot, 14) as u64;
    let num_fats = boot[16] as u64;
    let fat_size = u32le(&boot, 36) as u64;
    let root_cluster = u32le(&boot, 44);
    if bps == 0 || spc == 0 || root_cluster < 2 {
        return Ok(());
    }

    let fs = Fat32 {
        part_off,
        bytes_per_sector: bps,
        sectors_per_cluster: spc,
        fat_off: part_off + reserved * bps,
        data_start_sector: reserved + num_fats * fat_size,
    };

    let mut stack = vec![root_cluster];
    let mut visited: HashSet<u32> = HashSet::new();
    let mut scanned: u64 = 0;

    while let Some(start) = stack.pop() {
        if !visited.insert(start) {
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let buf = read_fat32_chain(file, &fs, start)?;
        let mut lfn: Vec<Vec<u16>> = Vec::new();
        let mut i = 0usize;
        while i + 32 <= buf.len() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let e = &buf[i..i + 32];
            i += 32;
            scanned += 1;
            if scanned % 4096 == 0 {
                on_progress(ScanProgress {
                    records_scanned: scanned,
                    total_records: scanned,
                });
            }
            let first = e[0];
            let attr = e[11];
            if first == 0x00 {
                lfn.clear();
                continue;
            }
            if attr == 0x0F {
                lfn.push(lfn_chars(e));
                continue;
            }
            if first == 0xE5 {
                let name = assemble_name(&lfn, e);
                let (start_cl, size) = short_entry(e);
                if size > 0 && start_cl >= 2 && !name.is_empty() {
                    found.lock().unwrap().push(RecoveredFile {
                        name,
                        size: size as u64,
                        data: DataLocation::NonResident(vec![(
                            fs.cluster_offset(start_cl),
                            size as u64,
                        )]),
                    });
                }
            } else if attr & 0x10 != 0 {
                // live subdirectory: recurse, skipping "." and ".."
                let (start_cl, _) = short_entry(e);
                if start_cl >= 2 && e[0] != b'.' && stack.len() < MAX_DIR_CLUSTERS {
                    stack.push(start_cl);
                }
            }
            lfn.clear();
        }
        if scanned > MAX_ENTRIES {
            break;
        }
    }

    on_progress(ScanProgress {
        records_scanned: scanned,
        total_records: scanned,
    });
    Ok(())
}

fn read_fat32_chain(file: &mut File, fs: &Fat32, start: u32) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut cluster = start;
    let mut guard = 0usize;
    while cluster >= 2 && cluster < 0x0FFF_FFF8 && guard < MAX_DIR_CLUSTERS {
        let chunk = disk::read_at(file, fs.cluster_offset(cluster), fs.cluster_size() as usize)?;
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk);
        cluster = fs.next_cluster(file, cluster)?;
        guard += 1;
    }
    Ok(out)
}

/// Starting cluster and size from a short (8.3) directory entry.
fn short_entry(e: &[u8]) -> (u32, u32) {
    let high = u16le(e, 20) as u32;
    let low = u16le(e, 26) as u32;
    ((high << 16) | low, u32le(e, 28))
}

/// The 13 UTF-16 code units a long-file-name entry carries.
fn lfn_chars(e: &[u8]) -> Vec<u16> {
    let mut chars = Vec::with_capacity(13);
    for &(start, count) in &[(1usize, 5usize), (14, 6), (28, 2)] {
        for k in 0..count {
            chars.push(u16le(e, start + k * 2));
        }
    }
    chars
}

/// Reconstructs a name from buffered LFN entries (stored in reverse order),
/// falling back to the 8.3 short name with the lost first character marked.
fn assemble_name(lfn: &[Vec<u16>], short: &[u8]) -> String {
    if !lfn.is_empty() {
        let mut units: Vec<u16> = Vec::new();
        for chunk in lfn.iter().rev() {
            units.extend_from_slice(chunk);
        }
        if let Some(end) = units.iter().position(|&c| c == 0x0000 || c == 0xFFFF) {
            units.truncate(end);
        }
        let name = String::from_utf16_lossy(&units);
        if !name.is_empty() {
            return name;
        }
    }
    short_name_8_3(short)
}

fn short_name_8_3(e: &[u8]) -> String {
    let mut name: Vec<u8> = e[0..8].to_vec();
    if name[0] == 0xE5 {
        name[0] = b'_'; // first char is overwritten on deletion
    }
    let base = String::from_utf8_lossy(&name).trim_end().to_string();
    let ext = String::from_utf8_lossy(&e[8..11]).trim_end().to_string();
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

// ---------------------------------------------------------------------------
// exFAT
// ---------------------------------------------------------------------------

struct Exfat {
    part_off: u64,
    bytes_per_sector: u64,
    sectors_per_cluster: u64,
    fat_off: u64,
    cluster_heap_off_sector: u64,
}

impl Exfat {
    fn cluster_offset(&self, cluster: u32) -> u64 {
        self.part_off
            + (self.cluster_heap_off_sector + (cluster as u64 - 2) * self.sectors_per_cluster)
                * self.bytes_per_sector
    }
    fn cluster_size(&self) -> u64 {
        self.bytes_per_sector * self.sectors_per_cluster
    }
    fn next_cluster(&self, file: &mut File, cluster: u32) -> io::Result<u32> {
        let entry = disk::read_at(file, self.fat_off + cluster as u64 * 4, 4)?;
        if entry.len() < 4 {
            return Ok(0xFFFF_FFFF);
        }
        Ok(u32le(&entry, 0))
    }
}

pub fn scan_exfat(
    file: &mut File,
    part_off: u64,
    cancel: &std::sync::atomic::AtomicBool,
    found: &std::sync::Mutex<Vec<RecoveredFile>>,
    on_progress: &mut dyn FnMut(ScanProgress),
) -> io::Result<()> {
    use std::sync::atomic::Ordering;

    let boot = disk::read_at(file, part_off, 512)?;
    if boot.len() < 512 || &boot[3..11] != b"EXFAT   " {
        return Ok(());
    }
    let bps = 1u64 << boot[108];
    let spc = 1u64 << boot[109];
    let fat_off_sector = u32le(&boot, 80) as u64;
    let cluster_heap_off_sector = u32le(&boot, 88) as u64;
    let root_cluster = u32le(&boot, 96);
    if boot[108] == 0 || root_cluster < 2 {
        return Ok(());
    }

    let fs = Exfat {
        part_off,
        bytes_per_sector: bps,
        sectors_per_cluster: spc,
        fat_off: part_off + fat_off_sector * bps,
        cluster_heap_off_sector,
    };

    let mut stack = vec![root_cluster];
    let mut visited: HashSet<u32> = HashSet::new();
    let mut scanned: u64 = 0;

    while let Some(start) = stack.pop() {
        if !visited.insert(start) {
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let buf = read_exfat_chain(file, &fs, start)?;
        let mut i = 0usize;
        while i + 32 <= buf.len() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let t = buf[i];
            scanned += 1;
            if scanned % 4096 == 0 {
                on_progress(ScanProgress {
                    records_scanned: scanned,
                    total_records: scanned,
                });
            }
            if t == 0x00 {
                break;
            }
            // File directory entry: 0x85 in use, 0x05 deleted.
            if t == 0x85 || t == 0x05 {
                let secondary = buf[i + 1] as usize;
                let file_attr = u16le(&buf, i + 4);
                let is_dir = file_attr & 0x10 != 0;
                let is_deleted = t & 0x80 == 0;

                // Stream extension entry follows (0xC0 / 0x40).
                let stream_at = i + 32;
                if stream_at + 32 <= buf.len() && matches!(buf[stream_at], 0xC0 | 0x40) {
                    let s = &buf[stream_at..stream_at + 32];
                    let flags = s[1];
                    let no_fat_chain = flags & 0x02 != 0;
                    let name_len = s[3] as usize;
                    let first_cluster = u32le(s, 20);
                    let data_length = u64le(s, 24);

                    let name = exfat_name(&buf, stream_at + 32, secondary, name_len);

                    if is_deleted && data_length > 0 && first_cluster >= 2 && !name.is_empty() {
                        let runs = if no_fat_chain {
                            vec![(fs.cluster_offset(first_cluster), data_length)]
                        } else {
                            exfat_runs(file, &fs, first_cluster, data_length)?
                        };
                        found.lock().unwrap().push(RecoveredFile {
                            name,
                            size: data_length,
                            data: DataLocation::NonResident(runs),
                        });
                    } else if is_dir && !is_deleted && first_cluster >= 2 {
                        if stack.len() < MAX_DIR_CLUSTERS {
                            stack.push(first_cluster);
                        }
                    }
                }
                i += 32 * (secondary + 1);
            } else {
                i += 32;
            }
        }
        if scanned > MAX_ENTRIES {
            break;
        }
    }

    on_progress(ScanProgress {
        records_scanned: scanned,
        total_records: scanned,
    });
    Ok(())
}

fn read_exfat_chain(file: &mut File, fs: &Exfat, start: u32) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut cluster = start;
    let mut guard = 0usize;
    while cluster >= 2 && cluster != 0xFFFF_FFFF && guard < MAX_DIR_CLUSTERS {
        let chunk = disk::read_at(file, fs.cluster_offset(cluster), fs.cluster_size() as usize)?;
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk);
        let next = fs.next_cluster(file, cluster)?;
        if next < 2 || next == 0xFFFF_FFFF || next == cluster {
            break;
        }
        cluster = next;
        guard += 1;
    }
    Ok(out)
}

/// Builds data runs by following an exFAT cluster chain up to `data_length`.
fn exfat_runs(
    file: &mut File,
    fs: &Exfat,
    first_cluster: u32,
    data_length: u64,
) -> io::Result<Vec<(u64, u64)>> {
    let mut runs = Vec::new();
    let mut cluster = first_cluster;
    let mut remaining = data_length;
    let mut guard = 0usize;
    while cluster >= 2 && cluster != 0xFFFF_FFFF && remaining > 0 && guard < MAX_DIR_CLUSTERS {
        let take = remaining.min(fs.cluster_size());
        runs.push((fs.cluster_offset(cluster), take));
        remaining -= take;
        let next = fs.next_cluster(file, cluster)?;
        if next < 2 || next == 0xFFFF_FFFF {
            break;
        }
        cluster = next;
        guard += 1;
    }
    // Fall back to a single contiguous run if the chain was already cleared.
    if runs.is_empty() {
        runs.push((fs.cluster_offset(first_cluster), data_length));
    }
    Ok(runs)
}

/// Concatenates the exFAT file-name entries (0xC1 / 0x41), 15 UTF-16 units
/// each, into the file name.
fn exfat_name(buf: &[u8], mut pos: usize, secondary: usize, name_len: usize) -> String {
    let mut units: Vec<u16> = Vec::new();
    // secondary counts the stream-extension entry plus the name entries.
    let name_entries = secondary.saturating_sub(1);
    for _ in 0..name_entries {
        if pos + 32 > buf.len() {
            break;
        }
        if !matches!(buf[pos], 0xC1 | 0x41) {
            break;
        }
        for k in 0..15 {
            units.push(u16le(buf, pos + 2 + k * 2));
        }
        pos += 32;
    }
    units.truncate(name_len);
    String::from_utf16_lossy(&units)
}
