use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

#[derive(Debug, Clone)]
pub struct DriveInfo {
    pub path: String,
    pub index: u32,
    pub size_bytes: u64,
}

/// Raw physical drive handles on Windows reject reads whose offset or
/// length isn't a multiple of the sector size, regardless of buffering
/// flags. 4096 covers both 512 and 4096 byte sectors.
const SECTOR_ALIGN: u64 = 4096;

/// Reads `len` bytes starting at an arbitrary `offset`, transparently
/// rounding the underlying seek/read to sector boundaries.
pub fn read_at(file: &mut File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let aligned_offset = offset - (offset % SECTOR_ALIGN);
    let front_pad = (offset - aligned_offset) as usize;
    let aligned_len = (front_pad + len).div_ceil(SECTOR_ALIGN as usize) * SECTOR_ALIGN as usize;

    file.seek(SeekFrom::Start(aligned_offset))?;
    let mut buffer = vec![0u8; aligned_len];
    let read = file.read(&mut buffer)?;
    buffer.truncate(read);

    if front_pad >= buffer.len() {
        return Ok(Vec::new());
    }
    buffer.drain(..front_pad);
    buffer.truncate(len);
    Ok(buffer)
}

#[cfg(windows)]
pub fn list_physical_drives() -> Vec<DriveInfo> {
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::core::PCWSTR;

    const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;

    #[repr(C)]
    struct GetLengthInformation {
        length: i64,
    }

    let mut drives = Vec::new();

    for index in 0..16u32 {
        let path = format!(r"\\.\PhysicalDrive{index}");
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

        let handle: Result<HANDLE, _> = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        };

        let Ok(handle) = handle else {
            continue;
        };

        let mut length_info = GetLengthInformation { length: 0 };
        let mut bytes_returned: u32 = 0;

        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_DISK_GET_LENGTH_INFO,
                None,
                0,
                Some(&mut length_info as *mut _ as *mut c_void),
                std::mem::size_of::<GetLengthInformation>() as u32,
                Some(&mut bytes_returned),
                None,
            )
        };

        if ok.is_ok() {
            drives.push(DriveInfo {
                path,
                index,
                size_bytes: length_info.length as u64,
            });
        }

        unsafe {
            let _ = CloseHandle(handle);
        }
    }

    drives
}

#[cfg(not(windows))]
pub fn list_physical_drives() -> Vec<DriveInfo> {
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
    format!("{size:.2} {}", UNITS[unit])
}
