use crate::disk;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const READ_CHUNK: usize = 4 * 1024 * 1024;
const FILE_SIGNATURE: &[u8; 4] = b"FILE";
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const FLAG_IN_USE: u16 = 0x01;
const FLAG_DIRECTORY: u16 = 0x02;

#[derive(Debug, Clone)]
pub enum DataLocation {
    Resident(Vec<u8>),
    /// Absolute (byte_offset, byte_length) runs on the source drive.
    NonResident(Vec<(u64, u64)>),
}

#[derive(Debug, Clone)]
pub struct RecoveredFile {
    pub name: String,
    pub size: u64,
    pub data: DataLocation,
}

pub struct ScanProgress {
    pub records_scanned: u64,
    pub total_records: u64,
}

struct Partition {
    start: u64,
}

struct NtfsVolume {
    partition_start: u64,
    cluster_size: u64,
    mft_offset: u64,
    record_size: u64,
}

struct Attr {
    type_id: u32,
    name_len: u8,
    non_resident: bool,
    resident_data: Vec<u8>,
    runs: Vec<(i64, u64)>,
    real_size: u64,
}

/// Scans every NTFS partition on the drive by walking its Master File
/// Table, pushing each deleted file it finds into `found` as soon as it is
/// located so the UI can display results live.
pub fn scan_drive(
    path: &str,
    cancel: &Arc<AtomicBool>,
    found: &Arc<Mutex<Vec<RecoveredFile>>>,
    mut on_progress: impl FnMut(ScanProgress),
) -> io::Result<()> {
    let mut file = File::open(path)?;
    let partitions = read_partitions(&mut file)?;

    'partitions: for part in &partitions {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Some(vol) = read_ntfs_boot_sector(&mut file, part.start)? else {
            continue;
        };

        let mut record0 = disk::read_at(&mut file, vol.mft_offset, vol.record_size as usize)?;
        if record0.len() < vol.record_size as usize || &record0[0..4] != FILE_SIGNATURE {
            continue;
        }
        apply_fixup(&mut record0);
        let attrs0 = parse_attributes(&record0);

        // The $MFT's own $DATA attribute is itself usually fragmented. On a
        // long-lived volume its full run list no longer fits in the base
        // record and spills into an $ATTRIBUTE_LIST pointing at extension
        // records, which must be followed to find every fragment.
        let mut raw_runs: Vec<(i64, u64)> = attrs0
            .iter()
            .find(|a| a.type_id == ATTR_DATA && a.name_len == 0)
            .map(|a| a.runs.clone())
            .unwrap_or_default();

        if let Some(attr_list) = attrs0.iter().find(|a| a.type_id == ATTR_ATTRIBUTE_LIST) {
            let content = attribute_content(&mut file, &vol, attr_list)?;
            for entry in parse_attribute_list(&content) {
                if entry.type_id != ATTR_DATA || entry.record_number == 0 {
                    continue;
                }
                let ext_offset = vol.mft_offset + entry.record_number * vol.record_size;
                let mut ext_record = disk::read_at(&mut file, ext_offset, vol.record_size as usize)?;
                if ext_record.len() < vol.record_size as usize || &ext_record[0..4] != FILE_SIGNATURE {
                    continue;
                }
                apply_fixup(&mut ext_record);
                for a in parse_attributes(&ext_record)
                    .into_iter()
                    .filter(|a| a.type_id == ATTR_DATA && a.non_resident)
                {
                    raw_runs.extend(a.runs);
                }
            }
        }

        let mft_runs: Vec<(u64, u64)> = if !raw_runs.is_empty() {
            resolve_runs(&raw_runs, vol.cluster_size, vol.partition_start)
        } else {
            vec![(vol.mft_offset, vol.record_size * 4096)]
        };
        let total_records: u64 =
            mft_runs.iter().map(|(_, len)| len).sum::<u64>() / vol.record_size;

        let mut record_index: u64 = 0;
        for (run_offset, run_len) in &mft_runs {
            let mut pos = *run_offset;
            let end = run_offset + run_len;

            while pos < end {
                if cancel.load(Ordering::Relaxed) {
                    break 'partitions;
                }

                let chunk_len = READ_CHUNK.min((end - pos) as usize);
                let chunk = disk::read_at(&mut file, pos, chunk_len)?;
                if chunk.is_empty() {
                    break;
                }

                let mut record_start = 0usize;
                while record_start + vol.record_size as usize <= chunk.len() {
                    if cancel.load(Ordering::Relaxed) {
                        break 'partitions;
                    }

                    let mut record =
                        chunk[record_start..record_start + vol.record_size as usize].to_vec();
                    record_start += vol.record_size as usize;
                    record_index += 1;

                    if &record[0..4] == FILE_SIGNATURE && apply_fixup(&mut record) {
                        process_record(&record, &vol, found);
                    }
                }

                pos += chunk.len() as u64;
                on_progress(ScanProgress {
                    records_scanned: record_index,
                    total_records,
                });
            }
        }
    }

    Ok(())
}

fn process_record(record: &[u8], vol: &NtfsVolume, found: &Arc<Mutex<Vec<RecoveredFile>>>) {
    let flags = u16::from_le_bytes(record[0x16..0x18].try_into().unwrap());
    let in_use = flags & FLAG_IN_USE != 0;
    let is_directory = flags & FLAG_DIRECTORY != 0;
    if in_use || is_directory {
        return;
    }

    let attrs = parse_attributes(record);

    let name = attrs
        .iter()
        .filter(|a| a.type_id == ATTR_FILE_NAME)
        .filter_map(|a| parse_file_name(&a.resident_data))
        .max_by_key(|f| if f.namespace == 2 { 0 } else { 1 })
        .map(|f| f.name);

    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return;
    };

    let Some(data_attr) = attrs
        .iter()
        .find(|a| a.type_id == ATTR_DATA && a.name_len == 0)
    else {
        return;
    };

    let size = data_attr.real_size;
    if size == 0 {
        return;
    }

    let data = if data_attr.non_resident {
        DataLocation::NonResident(resolve_runs(
            &data_attr.runs,
            vol.cluster_size,
            vol.partition_start,
        ))
    } else {
        DataLocation::Resident(data_attr.resident_data.clone())
    };

    found.lock().unwrap().push(RecoveredFile { name, size, data });
}

fn read_partitions(file: &mut File) -> io::Result<Vec<Partition>> {
    let mbr = disk::read_at(file, 0, 512)?;
    if mbr.len() < 512 || mbr[510] != 0x55 || mbr[511] != 0xAA {
        return Ok(Vec::new());
    }

    let is_gpt = (0..4).any(|i| mbr[0x1BE + i * 16 + 4] == 0xEE);
    if is_gpt {
        return read_gpt_partitions(file);
    }

    let mut partitions = Vec::new();
    for i in 0..4 {
        let entry = &mbr[0x1BE + i * 16..0x1BE + i * 16 + 16];
        if entry[4] == 0 {
            continue;
        }
        let start_lba = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as u64;
        partitions.push(Partition {
            start: start_lba * 512,
        });
    }
    Ok(partitions)
}

fn read_gpt_partitions(file: &mut File) -> io::Result<Vec<Partition>> {
    let header = disk::read_at(file, 512, 512)?;
    if header.len() < 512 || &header[0..8] != b"EFI PART" {
        return Ok(Vec::new());
    }

    let entry_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
    let num_entries = u32::from_le_bytes(header[80..84].try_into().unwrap()) as usize;
    let entry_size = u32::from_le_bytes(header[84..88].try_into().unwrap()) as usize;
    if entry_size == 0 || num_entries == 0 {
        return Ok(Vec::new());
    }

    let table = disk::read_at(file, entry_lba * 512, num_entries * entry_size)?;
    let mut partitions = Vec::new();
    for i in 0..num_entries {
        let off = i * entry_size;
        if off + entry_size > table.len() {
            break;
        }
        let entry = &table[off..off + entry_size];
        if entry[0..16].iter().all(|b| *b == 0) {
            continue;
        }
        let first_lba = u64::from_le_bytes(entry[32..40].try_into().unwrap());
        partitions.push(Partition {
            start: first_lba * 512,
        });
    }
    Ok(partitions)
}

fn read_ntfs_boot_sector(file: &mut File, partition_start: u64) -> io::Result<Option<NtfsVolume>> {
    let boot = disk::read_at(file, partition_start, 512)?;
    if boot.len() < 512 || &boot[3..11] != b"NTFS    " {
        return Ok(None);
    }

    let bytes_per_sector = u16::from_le_bytes(boot[11..13].try_into().unwrap()) as u64;
    let sectors_per_cluster = boot[13] as u64;
    if bytes_per_sector == 0 || sectors_per_cluster == 0 {
        return Ok(None);
    }
    let cluster_size = bytes_per_sector * sectors_per_cluster;

    let mft_cluster = u64::from_le_bytes(boot[48..56].try_into().unwrap());
    let clusters_per_record_raw = boot[0x40] as i8;
    let record_size = if clusters_per_record_raw >= 0 {
        clusters_per_record_raw as u64 * cluster_size
    } else {
        1u64 << (-(clusters_per_record_raw as i32)) as u32
    };
    if record_size == 0 || record_size > 1024 * 1024 {
        return Ok(None);
    }

    Ok(Some(NtfsVolume {
        partition_start,
        cluster_size,
        mft_offset: partition_start + mft_cluster * cluster_size,
        record_size,
    }))
}

/// Applies the NTFS fixup (update sequence array): the last two bytes of
/// each on-disk sector in a record are placeholders restored from the USA
/// so a torn/incomplete multi-sector write can be detected.
fn apply_fixup(record: &mut [u8]) -> bool {
    if record.len() < 8 {
        return false;
    }
    let usa_offset = u16::from_le_bytes(record[4..6].try_into().unwrap()) as usize;
    let usa_count = u16::from_le_bytes(record[6..8].try_into().unwrap()) as usize;
    if usa_count == 0 || usa_offset + usa_count * 2 > record.len() {
        return false;
    }

    let usa = record[usa_offset..usa_offset + usa_count * 2].to_vec();
    for i in 1..usa_count {
        let sector_end = i * 512;
        if sector_end > record.len() {
            break;
        }
        let check_pos = sector_end - 2;
        record[check_pos] = usa[i * 2];
        record[check_pos + 1] = usa[i * 2 + 1];
    }
    true
}

struct AttrListEntry {
    type_id: u32,
    record_number: u64,
}

/// Reads an attribute's content regardless of residency, following its data
/// runs when non-resident (used for `$ATTRIBUTE_LIST`, which can outgrow the
/// base record on heavily fragmented files).
fn attribute_content(file: &mut File, vol: &NtfsVolume, attr: &Attr) -> io::Result<Vec<u8>> {
    if !attr.non_resident {
        return Ok(attr.resident_data.clone());
    }
    let runs = resolve_runs(&attr.runs, vol.cluster_size, vol.partition_start);
    let mut content = Vec::with_capacity(attr.real_size as usize);
    for (offset, len) in runs {
        if content.len() as u64 >= attr.real_size {
            break;
        }
        content.extend(disk::read_at(file, offset, len as usize)?);
    }
    content.truncate(attr.real_size as usize);
    Ok(content)
}

/// Decodes `$ATTRIBUTE_LIST` entries (26-byte fixed header + optional name)
/// to find which MFT record holds each continuation of a fragmented
/// attribute.
fn parse_attribute_list(data: &[u8]) -> Vec<AttrListEntry> {
    let mut entries = Vec::new();
    let mut idx = 0usize;
    while idx + 26 <= data.len() {
        let type_id = u32::from_le_bytes(data[idx..idx + 4].try_into().unwrap());
        let entry_len = u16::from_le_bytes(data[idx + 4..idx + 6].try_into().unwrap()) as usize;
        if entry_len < 26 {
            break;
        }
        let file_ref = u64::from_le_bytes(data[idx + 16..idx + 24].try_into().unwrap());
        let record_number = file_ref & 0x0000_FFFF_FFFF_FFFF;
        entries.push(AttrListEntry {
            type_id,
            record_number,
        });
        idx += entry_len;
    }
    entries
}

fn parse_attributes(record: &[u8]) -> Vec<Attr> {
    let mut attrs = Vec::new();
    if record.len() < 0x16 {
        return attrs;
    }
    let mut offset = u16::from_le_bytes(record[0x14..0x16].try_into().unwrap()) as usize;

    while offset + 8 <= record.len() {
        let type_id = u32::from_le_bytes(record[offset..offset + 4].try_into().unwrap());
        if type_id == 0xFFFF_FFFF {
            break;
        }
        let length = u32::from_le_bytes(record[offset + 4..offset + 8].try_into().unwrap()) as usize;
        if length < 8 || offset + length > record.len() {
            break;
        }
        let non_resident = record[offset + 8] != 0;
        let name_len = record[offset + 9];

        if non_resident {
            if offset + 0x40 <= record.len() {
                let data_runs_offset =
                    u16::from_le_bytes(record[offset + 0x20..offset + 0x22].try_into().unwrap())
                        as usize;
                let real_size =
                    u64::from_le_bytes(record[offset + 0x30..offset + 0x38].try_into().unwrap());
                let runs_start = offset + data_runs_offset;
                let runs_end = offset + length;
                let runs = if runs_start < runs_end && runs_end <= record.len() {
                    parse_data_runs(&record[runs_start..runs_end])
                } else {
                    Vec::new()
                };
                attrs.push(Attr {
                    type_id,
                    name_len,
                    non_resident: true,
                    resident_data: Vec::new(),
                    runs,
                    real_size,
                });
            }
        } else if offset + 0x16 <= record.len() {
            let content_len =
                u32::from_le_bytes(record[offset + 0x10..offset + 0x14].try_into().unwrap())
                    as usize;
            let content_offset =
                u16::from_le_bytes(record[offset + 0x14..offset + 0x16].try_into().unwrap())
                    as usize;
            let start = offset + content_offset;
            let end = start + content_len;
            let data = if start <= end && end <= record.len() {
                record[start..end].to_vec()
            } else {
                Vec::new()
            };
            let len = data.len() as u64;
            attrs.push(Attr {
                type_id,
                name_len,
                non_resident: false,
                resident_data: data,
                runs: Vec::new(),
                real_size: len,
            });
        }

        offset += length;
    }

    attrs
}

/// Decodes an NTFS data-run list: a sequence of (length, LCN-delta) pairs
/// with variable-width little-endian fields, each delta accumulating onto
/// the previous cluster number.
fn parse_data_runs(data: &[u8]) -> Vec<(i64, u64)> {
    let mut runs = Vec::new();
    let mut current_lcn: i64 = 0;
    let mut idx = 0usize;

    while idx < data.len() {
        let header = data[idx];
        if header == 0 {
            break;
        }
        let length_size = (header & 0x0F) as usize;
        let offset_size = ((header >> 4) & 0x0F) as usize;
        idx += 1;
        if idx + length_size + offset_size > data.len() {
            break;
        }

        let length = read_le_uint(&data[idx..idx + length_size]);
        idx += length_size;
        let delta = read_le_int(&data[idx..idx + offset_size]);
        idx += offset_size;

        current_lcn += delta;
        runs.push((current_lcn, length));
    }

    runs
}

fn read_le_uint(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .enumerate()
        .fold(0u64, |acc, (i, b)| acc | ((*b as u64) << (8 * i)))
}

fn read_le_int(bytes: &[u8]) -> i64 {
    if bytes.is_empty() {
        return 0;
    }
    let mut v = read_le_uint(bytes) as i64;
    let bits = bytes.len() * 8;
    if bits < 64 {
        let sign_bit = 1i64 << (bits - 1);
        if v & sign_bit != 0 {
            v -= 1i64 << bits;
        }
    }
    v
}

fn resolve_runs(raw_runs: &[(i64, u64)], cluster_size: u64, partition_start: u64) -> Vec<(u64, u64)> {
    raw_runs
        .iter()
        .filter(|(lcn, _)| *lcn >= 0)
        .map(|(lcn, len)| (partition_start + (*lcn as u64) * cluster_size, len * cluster_size))
        .collect()
}

struct FileNameAttr {
    name: String,
    namespace: u8,
}

fn parse_file_name(data: &[u8]) -> Option<FileNameAttr> {
    if data.len() < 66 {
        return None;
    }
    let name_len = data[64] as usize;
    let namespace = data[65];
    let name_bytes = data.get(66..66 + name_len * 2)?;
    let utf16: Vec<u16> = name_bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(FileNameAttr {
        name: String::from_utf16_lossy(&utf16),
        namespace,
    })
}

/// Category used to group files in the results view and as the subfolder
/// name when recovering files to disk.
pub fn category_for_name(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "heic" | "webp" | "raw" | "cr2" | "nef" | "arw" => {
            "Fotos"
        }
        "mp4" | "mov" | "avi" | "mkv" | "wmv" => "Videos",
        "mp3" | "wav" | "flac" | "aac" => "Audio",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "rtf" => "Documenten",
        "zip" | "rar" | "7z" | "tar" | "gz" => "Archieven",
        _ => "Overig",
    }
}

pub fn recover_file(source_path: &str, rec: &RecoveredFile, out_dir: &Path) -> io::Result<()> {
    let category_dir = out_dir.join(category_for_name(&rec.name));
    fs::create_dir_all(&category_dir)?;
    let out_path = unique_path(&category_dir, &sanitize_filename(&rec.name));
    let mut out_file = File::create(out_path)?;

    match &rec.data {
        DataLocation::Resident(bytes) => out_file.write_all(bytes)?,
        DataLocation::NonResident(runs) => {
            let mut file = File::open(source_path)?;
            let mut remaining = rec.size;
            for (run_offset, run_len) in runs {
                if remaining == 0 {
                    break;
                }
                let mut pos = *run_offset;
                let run_end = run_offset + run_len;
                while pos < run_end && remaining > 0 {
                    let chunk_len = (READ_CHUNK as u64).min(run_end - pos).min(remaining) as usize;
                    let chunk = disk::read_at(&mut file, pos, chunk_len)?;
                    if chunk.is_empty() {
                        break;
                    }
                    out_file.write_all(&chunk)?;
                    pos += chunk.len() as u64;
                    remaining -= chunk.len() as u64;
                }
            }
        }
    }

    Ok(())
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect()
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let ext = Path::new(name).extension().and_then(|s| s.to_str()).unwrap_or("");
    for i in 1.. {
        let candidate = dir.join(if ext.is_empty() {
            format!("{stem}_{i}")
        } else {
            format!("{stem}_{i}.{ext}")
        });
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}
