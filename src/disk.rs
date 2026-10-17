use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

/// A fixed or removable volume the user can scan (e.g. `C:`), shown as a
/// card on the home screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveKind {
    Internal,
    External,
}

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    /// Path handed to the scanner, e.g. `\\.\C:`.
    pub scan_path: String,
    /// Drive letter with colon, e.g. `C:`.
    pub letter: String,
    /// Volume label, e.g. `Windows-SSD` (may be empty).
    pub label: String,
    /// File system name, e.g. `NTFS`.
    pub filesystem: String,
    /// Physical disk number backing this volume, for the "Schijf0" badge.
    pub disk_index: Option<u32>,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub kind: DriveKind,
}

impl VolumeInfo {
    /// Label + letter the way the card shows it, e.g. `Windows-SSD (C:)`.
    pub fn display_name(&self) -> String {
        if self.label.is_empty() {
            format!("Lokale schijf ({})", self.letter)
        } else {
            format!("{} ({})", self.label, self.letter)
        }
    }
}

/// Raw physical/volume handles on Windows reject reads whose offset or
/// length isn't a multiple of the sector size. 4096 covers both 512 and
/// 4096 byte sectors.
const SECTOR_ALIGN: u64 = 4096;

/// Reads `len` bytes starting at an arbitrary `offset`, transparently
/// rounding the underlying seek/read to sector boundaries.
///
/// Raw physical-drive handles on Windows often satisfy a single `ReadFile`
/// with FEWER bytes than requested (a short read), and reject very large
/// requests. So we seek once, then loop `read`, capping each call to 1 MiB
/// and stopping only at a genuine end-of-data. Relying on one `read` call
/// truncated large scans to the first chunk.
pub fn read_at(file: &mut File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let aligned_offset = offset - (offset % SECTOR_ALIGN);
    let front_pad = (offset - aligned_offset) as usize;
    let aligned_len = (front_pad + len).div_ceil(SECTOR_ALIGN as usize) * SECTOR_ALIGN as usize;

    file.seek(SeekFrom::Start(aligned_offset))?;

    const MAX_READ: usize = 1024 * 1024; // Windows raw devices dislike huge reads
    let mut buffer = vec![0u8; aligned_len];
    let mut filled = 0usize;
    while filled < aligned_len {
        let want = (aligned_len - filled).min(MAX_READ);
        let n = file.read(&mut buffer[filled..filled + want])?;
        if n == 0 {
            break; // genuine end of device/data
        }
        filled += n;
    }
    buffer.truncate(filled);

    if front_pad >= buffer.len() {
        return Ok(Vec::new());
    }
    buffer.drain(..front_pad);
    buffer.truncate(len);
    Ok(buffer)
}

#[cfg(windows)]
pub fn list_volumes() -> Vec<VolumeInfo> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::core::PCWSTR;

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

    #[repr(C)]
    struct DiskExtent {
        disk_number: u32,
        starting_offset: i64,
        extent_length: i64,
    }
    #[repr(C)]
    struct VolumeDiskExtents {
        number_of_disk_extents: u32,
        extents: [DiskExtent; 4],
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn physical_disk_index(volume_path: &str) -> Option<u32> {
        let wpath = wide(volume_path);
        let handle: HANDLE = unsafe {
            CreateFileW(
                PCWSTR(wpath.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .ok()?;

        let mut extents = VolumeDiskExtents {
            number_of_disk_extents: 0,
            extents: [
                DiskExtent { disk_number: 0, starting_offset: 0, extent_length: 0 },
                DiskExtent { disk_number: 0, starting_offset: 0, extent_length: 0 },
                DiskExtent { disk_number: 0, starting_offset: 0, extent_length: 0 },
                DiskExtent { disk_number: 0, starting_offset: 0, extent_length: 0 },
            ],
        };
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                None,
                0,
                Some(&mut extents as *mut _ as *mut c_void),
                std::mem::size_of::<VolumeDiskExtents>() as u32,
                Some(&mut returned),
                None,
            )
        };
        unsafe {
            let _ = CloseHandle(handle);
        }
        if ok.is_ok() && extents.number_of_disk_extents >= 1 {
            Some(extents.extents[0].disk_number)
        } else {
            None
        }
    }

    let mut volumes = Vec::new();
    let mask = unsafe { GetLogicalDrives() };

    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter_char = (b'A' + i as u8) as char;
        let root = format!("{letter_char}:\\");
        let wroot = wide(&root);

        let kind = match unsafe { GetDriveTypeW(PCWSTR(wroot.as_ptr())) } {
            DRIVE_FIXED => DriveKind::Internal,
            DRIVE_REMOVABLE => DriveKind::External,
            _ => continue, // skip network, CD-ROM, RAM disks
        };

        let mut label_buf = [0u16; 256];
        let mut fs_buf = [0u16; 64];
        let info = unsafe {
            GetVolumeInformationW(
                PCWSTR(wroot.as_ptr()),
                Some(&mut label_buf),
                None,
                None,
                None,
                Some(&mut fs_buf),
            )
        };
        if info.is_err() {
            continue; // volume not ready (empty card reader, etc.)
        }
        let label = wide_to_string(&label_buf);
        let filesystem = wide_to_string(&fs_buf);

        let mut total: u64 = 0;
        let mut free: u64 = 0;
        let _ = unsafe {
            GetDiskFreeSpaceExW(
                PCWSTR(wroot.as_ptr()),
                None,
                Some(&mut total),
                Some(&mut free),
            )
        };

        let scan_path = format!(r"\\.\{letter_char}:");
        volumes.push(VolumeInfo {
            disk_index: physical_disk_index(&scan_path),
            scan_path,
            letter: format!("{letter_char}:"),
            label,
            filesystem,
            total_bytes: total,
            used_bytes: total.saturating_sub(free),
            kind,
        });
    }

    volumes
}

#[cfg(windows)]
fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(not(windows))]
pub fn list_volumes() -> Vec<VolumeInfo> {
    Vec::new()
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    // Dutch decimal comma to match the UI.
    format!("{size:.2} {}", UNITS[unit]).replace('.', ",")
}
