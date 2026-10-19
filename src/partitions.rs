//! Partition layout of the physical disks, and a search for lost
//! partitions: file systems whose entry in the partition table is gone
//! (deleted partition, new partition table, quick-formatted disk) while
//! their boot sector is still on the disk. Nothing is written; a found
//! partition can be scanned for files like any other.

use crate::imaging::BlockSource;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

const SECTOR: u64 = 512;
const CHUNK: u64 = 1 << 20;
/// Unpartitioned space up to this size is alignment slack or the room of
/// the partition table itself (the first MiB), not a gap.
pub const MIN_GAP: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Mbr,
    Gpt,
    /// No partition table.
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Efi,
    Reserved,
    Recovery,
    Data,
    Linux,
    LinuxSwap,
    Apple,
    Other,
}

impl PartKind {
    pub fn label(self) -> &'static str {
        match self {
            PartKind::Efi => "EFI-systeempartitie",
            PartKind::Reserved => "Gereserveerd (Microsoft)",
            PartKind::Recovery => "Herstelpartitie",
            PartKind::Data => "Gegevens",
            PartKind::Linux => "Linux",
            PartKind::LinuxSwap => "Linux-wisselgeheugen",
            PartKind::Apple => "Apple",
            PartKind::Other => "Overig",
        }
    }

    /// Partitions Windows and the boot process need; never user data.
    pub fn is_system(self) -> bool {
        matches!(self, PartKind::Efi | PartKind::Reserved | PartKind::Recovery)
    }

    pub fn from_gpt(guid: u128) -> Self {
        match guid {
            0xC12A7328_F81F_11D2_BA4B_00A0C93EC93B => PartKind::Efi,
            0xE3C9E316_0B5C_4DB8_817D_F92DF00215AE => PartKind::Reserved,
            0xDE94BBA4_06D1_4D40_A16A_BFD50179D6AC => PartKind::Recovery,
            0xEBD0A0A2_B9E5_4433_87C0_68B6B72699C7 => PartKind::Data,
            0x0FC63DAF_8483_4772_8E79_3D69D8477DE4 => PartKind::Linux,
            0x0657FD6D_A4AB_43C4_84E5_0933C84B4F4F => PartKind::LinuxSwap,
            0x7C3457EF_0000_11AA_AA11_00306543ECAC | 0x48465300_0000_11AA_AA11_00306543ECAC => PartKind::Apple,
            _ => PartKind::Other,
        }
    }

    pub fn from_mbr(kind: u8) -> Self {
        match kind {
            0x01 | 0x04 | 0x06 | 0x07 | 0x0B | 0x0C | 0x0E => PartKind::Data,
            0x27 => PartKind::Recovery,
            0xEF => PartKind::Efi,
            0x83 => PartKind::Linux,
            0x82 => PartKind::LinuxSwap,
            0xAF => PartKind::Apple,
            _ => PartKind::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PartInfo {
    pub number: u32,
    pub start: u64,
    pub size: u64,
    pub kind: PartKind,
    /// GPT partition name (often empty).
    pub name: String,
    /// MBR: the partition the BIOS starts from.
    pub active: bool,
    /// MBR: a logical partition inside the extended partition.
    pub logical: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub index: u32,
    pub style: Style,
    pub size: u64,
    /// Ordered by start.
    pub partitions: Vec<PartInfo>,
}

impl Layout {
    /// Unpartitioned stretches (start, end) larger than [`MIN_GAP`].
    pub fn gaps(&self) -> Vec<(u64, u64)> {
        let mut gaps = Vec::new();
        let mut pos = 0;
        for p in &self.partitions {
            if p.start > pos + MIN_GAP {
                gaps.push((pos, p.start));
            }
            pos = pos.max(p.start + p.size);
        }
        if self.size > pos + MIN_GAP {
            gaps.push((pos, self.size));
        }
        gaps
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    Ntfs,
    Fat32,
    /// FAT12 or FAT16.
    Fat,
    ExFat,
}

impl FsKind {
    pub fn label(self) -> &'static str {
        match self {
            FsKind::Ntfs => "NTFS",
            FsKind::Fat32 => "FAT32",
            FsKind::Fat => "FAT",
            FsKind::ExFat => "exFAT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoundPartition {
    pub start: u64,
    pub size: u64,
    pub fs: FsKind,
    /// Found through the NTFS backup boot sector at the end of the
    /// partition; the boot sector at the start is damaged.
    pub from_backup: bool,
}

/// What a boot sector says about its file system.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BootInfo {
    fs: FsKind,
    size: u64,
    bytes_per_sector: u64,
    /// Offset, from the partition start, of a structure that must look
    /// right for the boot sector to be real: the MFT or the first FAT.
    check_at: u64,
    /// First byte expected in a FAT (the media descriptor).
    media: u8,
}

fn u16le(b: &[u8], at: usize) -> u64 {
    u64::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn u32le(b: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes(b[at..at + 4].try_into().unwrap()))
}

fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn boot_info(s: &[u8]) -> Option<BootInfo> {
    if s.len() < 512 || s[510] != 0x55 || s[511] != 0xAA {
        return None;
    }
    let valid_bps = |bps: u64| matches!(bps, 512 | 1024 | 2048 | 4096);
    if &s[3..11] == b"NTFS    " {
        let bps = u16le(s, 11);
        // Large clusters are stored as a negative power of two.
        let spc = match s[13] {
            0 => return None,
            n if n > 0x80 => 1u64.checked_shl(256 - u32::from(n))?,
            n => u64::from(n),
        };
        let total = u64le(s, 0x28);
        if !valid_bps(bps) || total == 0 {
            return None;
        }
        let check_at = u64le(s, 0x30).checked_mul(spc * bps)?;
        return Some(BootInfo { fs: FsKind::Ntfs, size: total.checked_add(1)?.checked_mul(bps)?, bytes_per_sector: bps, check_at, media: 0 });
    }
    if &s[3..11] == b"EXFAT   " {
        let shift = u32::from(s[108]);
        if !(9..=12).contains(&shift) {
            return None;
        }
        let bps = 1u64 << shift;
        let size = u64le(s, 72).checked_mul(bps)?;
        return (size > 0).then_some(BootInfo { fs: FsKind::ExFat, size, bytes_per_sector: bps, check_at: u32le(s, 80) * bps, media: 0xF8 });
    }
    if crate::fat::is_fat_boot(s) {
        let bps = u16le(s, 11);
        let total = match u16le(s, 19) {
            0 => u32le(s, 32),
            n => n,
        };
        let fs = if &s[82..87] == b"FAT32" { FsKind::Fat32 } else { FsKind::Fat };
        return (total > 0).then_some(BootInfo { fs, size: total * bps, bytes_per_sector: bps, check_at: u16le(s, 14) * bps, media: s[21] });
    }
    None
}

/// Whether the structure at `check_at` looks like an MFT record or a FAT.
fn structure_ok(info: &BootInfo, b: &[u8]) -> bool {
    match info.fs {
        FsKind::Ntfs => b.starts_with(b"FILE"),
        FsKind::ExFat => b.starts_with(&[0xF8, 0xFF, 0xFF, 0xFF]),
        FsKind::Fat32 | FsKind::Fat => b.len() >= 3 && b[0] == info.media && b[1] == 0xFF && b[2] == 0xFF,
    }
}

/// The partition a boot sector at `at` belongs to, when its file-system
/// structures are where the boot sector says.
fn confirm(src: &mut impl BlockSource, at: u64, info: &BootInfo, disk_len: u64) -> Option<FoundPartition> {
    let mut fits_and_checks = |start: u64| {
        start.checked_add(info.size).is_some_and(|end| end <= disk_len)
            && src.read_block(start + info.check_at, SECTOR as usize).is_ok_and(|b| structure_ok(info, &b))
    };
    if fits_and_checks(at) {
        return Some(FoundPartition { start: at, size: info.size, fs: info.fs, from_backup: false });
    }
    // NTFS keeps a copy of its boot sector in the partition's last sector.
    if info.fs == FsKind::Ntfs {
        let start = (at + info.bytes_per_sector).checked_sub(info.size)?;
        if fits_and_checks(start) {
            return Some(FoundPartition { start, size: info.size, fs: info.fs, from_backup: true });
        }
    }
    None
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchProgress {
    pub done: u64,
    pub total: u64,
    pub found: usize,
    /// Bytes skipped because they could not be read.
    pub unreadable: u64,
}

/// Looks for boot sectors in `ranges` (start, end) of a disk of `disk_len`
/// bytes. After each confirmed partition the search jumps past its end.
pub fn search(
    src: &mut impl BlockSource,
    ranges: &[(u64, u64)],
    disk_len: u64,
    cancel: &AtomicBool,
    mut report: impl FnMut(&SearchProgress),
) -> io::Result<Vec<FoundPartition>> {
    let mut found: Vec<FoundPartition> = Vec::new();
    let mut p = SearchProgress { total: ranges.iter().map(|(s, e)| e.saturating_sub(*s)).sum(), ..Default::default() };
    let mut last_report = Instant::now();
    for &(start, end) in ranges {
        let mut pos = start.div_ceil(SECTOR) * SECTOR;
        p.done += pos - start;
        while pos + SECTOR <= end {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "Zoeken gestopt"));
            }
            let len = (end - pos).min(CHUNK) / SECTOR * SECTOR;
            let mut next = pos + len;
            match src.read_block(pos, len as usize) {
                Err(_) => p.unreadable += len,
                Ok(chunk) => {
                    for i in (0..chunk.len().saturating_sub(SECTOR as usize - 1)).step_by(SECTOR as usize) {
                        let Some(info) = boot_info(&chunk[i..i + SECTOR as usize]) else { continue };
                        let at = pos + i as u64;
                        let Some(part) = confirm(src, at, &info, disk_len) else { continue };
                        if !found.iter().any(|f| f.start == part.start) {
                            found.push(part);
                        }
                        if part.start + part.size > at + SECTOR {
                            next = (part.start + part.size).div_ceil(SECTOR) * SECTOR;
                            break;
                        }
                    }
                }
            }
            let next = next.min(end);
            p.done += next - pos;
            p.found = found.len();
            pos = next;
            if last_report.elapsed().as_millis() > 150 {
                report(&p);
                last_report = Instant::now();
            }
        }
        p.done += end.saturating_sub(pos);
    }
    found.sort_by_key(|f| f.start);
    p.found = found.len();
    report(&p);
    Ok(found)
}

#[cfg(windows)]
pub use win::read_layouts;

#[cfg(not(windows))]
pub fn read_layouts() -> Vec<Layout> {
    Vec::new()
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::ffi::c_void;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{DRIVE_LAYOUT_INFORMATION_EX, PARTITION_INFORMATION_EX};
    use windows::core::PCWSTR;

    const IOCTL_DISK_GET_DRIVE_LAYOUT_EX: u32 = 0x0007_0050;
    const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x0007_00A0;
    const MBR_EXTENDED: [u8; 3] = [0x05, 0x0F, 0x85];
    const MAX_DISKS: u32 = 32;

    fn ioctl(h: HANDLE, code: u32, out: &mut [u64]) -> Option<usize> {
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                h,
                code,
                None,
                0,
                Some(out.as_mut_ptr() as *mut c_void),
                std::mem::size_of_val(out) as u32,
                Some(&mut returned),
                None,
            )
        }
        .ok()?;
        Some(returned as usize)
    }

    fn read_layout(index: u32) -> Option<Layout> {
        let path: Vec<u16> = format!(r"\\.\PhysicalDrive{index}").encode_utf16().chain([0]).collect();
        // Both questions are answered without read access.
        let h = unsafe {
            CreateFileW(PCWSTR(path.as_ptr()), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None)
        }
        .ok()?;
        let mut geometry = [0u64; 32];
        let size = ioctl(h, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &mut geometry).map(|_| geometry[3]);
        // Room for 128 GPT entries; u64 keeps the buffer aligned for the cast.
        let mut buf = vec![0u64; 4096];
        let returned = ioctl(h, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &mut buf);
        unsafe {
            let _ = CloseHandle(h);
        }
        let (size, returned) = (size?, returned?);

        let info = unsafe { &*(buf.as_ptr() as *const DRIVE_LAYOUT_INFORMATION_EX) };
        let header = std::mem::offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry);
        let fits = (returned.saturating_sub(header)) / std::mem::size_of::<PARTITION_INFORMATION_EX>();
        let count = (info.PartitionCount as usize).min(fits);
        let entries = unsafe { std::slice::from_raw_parts(info.PartitionEntry.as_ptr(), count) };
        let style = match info.PartitionStyle {
            0 => Style::Mbr,
            1 => Style::Gpt,
            _ => Style::Raw,
        };
        // Logical partitions lie inside an extended container.
        let extended: Vec<(i64, i64)> = entries
            .iter()
            .filter(|e| style == Style::Mbr && MBR_EXTENDED.contains(&unsafe { e.Anonymous.Mbr.PartitionType }))
            .map(|e| (e.StartingOffset, e.StartingOffset + e.PartitionLength))
            .collect();
        let mut partitions: Vec<PartInfo> = entries
            .iter()
            .filter(|e| e.PartitionLength > 0)
            .filter_map(|e| {
                let (mut active, mut logical) = (false, false);
                let (kind, name) = match style {
                    Style::Gpt => {
                        let gpt = unsafe { &e.Anonymous.Gpt };
                        let end = gpt.Name.iter().position(|&c| c == 0).unwrap_or(gpt.Name.len());
                        (PartKind::from_gpt(gpt.PartitionType.to_u128()), String::from_utf16_lossy(&gpt.Name[..end]))
                    }
                    _ => {
                        let mbr = unsafe { &e.Anonymous.Mbr };
                        // The extended container only holds the logical partitions.
                        if mbr.PartitionType == 0 || MBR_EXTENDED.contains(&mbr.PartitionType) {
                            return None;
                        }
                        active = mbr.BootIndicator.as_bool();
                        logical = extended.iter().any(|&(s, end)| e.StartingOffset >= s && e.StartingOffset < end);
                        (PartKind::from_mbr(mbr.PartitionType), String::new())
                    }
                };
                Some(PartInfo { number: e.PartitionNumber, start: e.StartingOffset as u64, size: e.PartitionLength as u64, kind, name, active, logical })
            })
            .collect();
        partitions.sort_by_key(|p| p.start);
        Some(Layout { index, style, size, partitions })
    }

    /// The partition layout of every physical disk.
    pub fn read_layouts() -> Vec<Layout> {
        (0..MAX_DISKS).filter_map(read_layout).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory disk.
    struct Disk(Vec<u8>);

    impl BlockSource for Disk {
        fn len(&mut self) -> io::Result<u64> {
            Ok(self.0.len() as u64)
        }

        fn read_block(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            let start = offset as usize;
            self.0.get(start..start + len).map(<[u8]>::to_vec).ok_or_else(|| io::Error::other("buiten de schijf"))
        }
    }

    const MIB: usize = 1 << 20;

    /// Writes an NTFS boot sector at `at` for a partition of `sectors`
    /// sectors with its MFT at cluster 4 (8 sectors per cluster).
    fn ntfs_boot(disk: &mut [u8], at: usize, sectors: u64) {
        let s = &mut disk[at..at + 512];
        s[3..11].copy_from_slice(b"NTFS    ");
        s[11..13].copy_from_slice(&512u16.to_le_bytes());
        s[13] = 8;
        s[0x28..0x30].copy_from_slice(&(sectors - 1).to_le_bytes());
        s[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        s[510] = 0x55;
        s[511] = 0xAA;
    }

    fn mft_at(disk: &mut [u8], start: usize) {
        disk[start + 4 * 8 * 512..][..4].copy_from_slice(b"FILE");
    }

    fn fat32_boot(disk: &mut [u8], at: usize, sectors: u32) {
        let s = &mut disk[at..at + 512];
        s[11..13].copy_from_slice(&512u16.to_le_bytes());
        s[13] = 8;
        s[14..16].copy_from_slice(&32u16.to_le_bytes());
        s[21] = 0xF8;
        s[32..36].copy_from_slice(&sectors.to_le_bytes());
        s[82..87].copy_from_slice(b"FAT32");
        s[510] = 0x55;
        s[511] = 0xAA;
        disk[at + 32 * 512..][..4].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0x0F]);
    }

    fn find(disk: Vec<u8>) -> Vec<FoundPartition> {
        let len = disk.len() as u64;
        search(&mut Disk(disk), &[(0, len)], len, &AtomicBool::new(false), |_| {}).unwrap()
    }

    #[test]
    fn finds_ntfs_and_fat32_partitions_on_a_disk_without_table() {
        let mut disk = vec![0u8; 8 * MIB];
        ntfs_boot(&mut disk, MIB, 4096);
        mft_at(&mut disk, MIB);
        fat32_boot(&mut disk, 4 * MIB, 4096);

        let found = find(disk);

        assert_eq!(
            found,
            [
                FoundPartition { start: MIB as u64, size: 2 * MIB as u64, fs: FsKind::Ntfs, from_backup: false },
                FoundPartition { start: 4 * MIB as u64, size: 2 * MIB as u64, fs: FsKind::Fat32, from_backup: false },
            ]
        );
    }

    #[test]
    fn ntfs_with_damaged_start_is_found_through_its_backup() {
        let mut disk = vec![0u8; 4 * MIB];
        ntfs_boot(&mut disk, 3 * MIB - 512, 4096);
        mft_at(&mut disk, MIB);

        let found = find(disk);

        assert_eq!(found, [FoundPartition { start: MIB as u64, size: 2 * MIB as u64, fs: FsKind::Ntfs, from_backup: true }]);
    }

    #[test]
    fn boot_sector_without_matching_structures_is_ignored() {
        let mut disk = vec![0u8; 4 * MIB];
        ntfs_boot(&mut disk, MIB, 2048);
        let mut too_big = vec![0u8; 4 * MIB];
        ntfs_boot(&mut too_big, MIB, 1 << 30);
        mft_at(&mut too_big, MIB);

        assert!(find(disk).is_empty());
        assert!(find(too_big).is_empty());
    }

    #[test]
    fn reports_progress_to_the_end_and_stops_on_cancel() {
        let disk = vec![0u8; 3 * MIB];
        let mut last = SearchProgress::default();
        search(&mut Disk(disk.clone()), &[(0, MIB as u64), (2 * MIB as u64, 3 * MIB as u64)], 3 * MIB as u64, &AtomicBool::new(false), |p| {
            last = p.clone()
        })
        .unwrap();

        assert_eq!((last.done, last.total), (2 * MIB as u64, 2 * MIB as u64));
        let err = search(&mut Disk(disk), &[(0, MIB as u64)], MIB as u64, &AtomicBool::new(true), |_| {}).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn gaps_skip_alignment_slack() {
        let part = |start: u64, size: u64| PartInfo { number: 1, start, size, kind: PartKind::Data, name: String::new(), active: false, logical: false };
        let m = MIB as u64;
        let layout = Layout {
            index: 0,
            style: Style::Gpt,
            size: 100 * m,
            partitions: vec![part(m, 10 * m), part(11 * m + 4096, 20 * m), part(50 * m, 40 * m)],
        };

        assert_eq!(layout.gaps(), [(31 * m + 4096, 50 * m), (90 * m, 100 * m)]);
    }

    #[test]
    fn classifies_partition_types() {
        assert_eq!(PartKind::from_gpt(0xC12A7328_F81F_11D2_BA4B_00A0C93EC93B), PartKind::Efi);
        assert_eq!(PartKind::from_gpt(0xEBD0A0A2_B9E5_4433_87C0_68B6B72699C7), PartKind::Data);
        assert_eq!(PartKind::from_mbr(0x07), PartKind::Data);
        assert_eq!(PartKind::from_mbr(0x27), PartKind::Recovery);
        assert!(PartKind::Recovery.is_system() && !PartKind::Data.is_system());
    }

    /// Prints the layout of this computer's disks:
    /// `cargo test print_layouts -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn print_layouts() {
        for l in read_layouts() {
            println!("{l:#?}\ngaps: {:?}", l.gaps());
        }
    }
}
