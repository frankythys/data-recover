//! Read-only health check of the physical disks: model and type plus the
//! SMART values a disk keeps about itself (temperature, wear, bad sectors,
//! hours in use), turned into a plain verdict. NVMe disks report one
//! standard health log; SATA disks a table of numbered attributes. Reading
//! SMART needs administrator rights; without them only model, type, size
//! and (when Windows knows it) the temperature are shown.

use crate::i18n::t;
use crate::tf;

/// Bytes per NVMe "data unit" (1000 sectors of 512 bytes).
const NVME_DATA_UNIT: u64 = 512_000;
/// From these temperatures on a disk is reported as warm.
const WARM_SSD_C: i32 = 70;
const WARM_HDD_C: i32 = 55;
/// At or below this much remaining life an SSD is reported.
const LOW_LIFE_PERCENT: u8 = 10;

/// Verdict, from least to most serious.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Good,
    Caution,
    Bad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bus {
    Nvme,
    Sata,
    Usb,
    Sd,
    Other,
}

impl Bus {
    fn from_storage_bus_type(value: u32) -> Self {
        match value {
            17 => Bus::Nvme,
            3 | 11 => Bus::Sata,
            7 => Bus::Usb,
            12 | 13 => Bus::Sd,
            _ => Bus::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Smart {
    /// Not read: Windows only allows it to administrators.
    NeedsAdmin,
    /// The disk or its connection (often USB) does not report SMART.
    Unavailable,
    Nvme(NvmeLog),
    Ata(Vec<AtaAttribute>),
}

/// The NVMe "SMART / Health Information" log page (02h).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NvmeLog {
    pub critical_warning: u8,
    pub temperature_kelvin: u16,
    pub available_spare: u8,
    pub spare_threshold: u8,
    pub percentage_used: u8,
    pub data_units_read: u64,
    pub data_units_written: u64,
    pub power_cycles: u64,
    pub power_on_hours: u64,
    pub unsafe_shutdowns: u64,
    pub media_errors: u64,
    pub error_log_entries: u64,
}

/// One row of a SATA disk's SMART table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtaAttribute {
    pub id: u8,
    /// Normalised value (usually 100 or 200 when new, lower is worse).
    pub current: u8,
    pub worst: u8,
    /// The value at or below which the maker considers the disk failing.
    pub threshold: u8,
    /// Raw 48-bit counter; its meaning depends on the attribute.
    pub raw: u64,
}

impl AtaAttribute {
    /// The low 32 bits, where counters and hours are kept.
    fn count(&self) -> u64 {
        self.raw & 0xFFFF_FFFF
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiskHealth {
    /// Physical disk number (`\\.\PhysicalDrive<n>`).
    pub index: u32,
    pub model: String,
    pub serial: String,
    pub bus: Bus,
    /// `None` when Windows does not say.
    pub ssd: Option<bool>,
    pub size: u64,
    /// Temperature reported by Windows itself, used when SMART has none.
    pub os_temperature: Option<i32>,
    pub smart: Smart,
}

/// What the screen shows for a disk.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub status: Option<Status>,
    pub temperature: Option<i32>,
    /// Estimated remaining life of an SSD, in percent.
    pub life_left: Option<u8>,
    pub power_on_hours: Option<u64>,
    pub power_cycles: Option<u64>,
    pub bytes_read: Option<u64>,
    pub bytes_written: Option<u64>,
    /// Plain-language findings, most serious first.
    pub issues: Vec<(Status, String)>,
}

impl DiskHealth {
    pub fn is_ssd(&self) -> bool {
        self.bus == Bus::Nvme || self.ssd == Some(true)
    }

    pub fn summary(&self) -> Summary {
        let mut s = match &self.smart {
            Smart::Nvme(log) => nvme_summary(log),
            Smart::Ata(attrs) => ata_summary(attrs),
            Smart::NeedsAdmin | Smart::Unavailable => Summary::default(),
        };
        s.temperature = s.temperature.or(self.os_temperature);
        if let Some(t) = s.temperature {
            let warm = if self.is_ssd() { WARM_SSD_C } else { WARM_HDD_C };
            if t >= warm {
                s.issues.push((Status::Caution, tf!("De schijf is warm ({t} °C). Zorg voor betere koeling.", "The disk is warm ({t} °C). Improve the cooling.")));
            }
        }
        if matches!(self.smart, Smart::Nvme(_) | Smart::Ata(_)) {
            let worst = s.issues.iter().map(|(st, _)| *st).max().unwrap_or(Status::Good);
            s.status = Some(worst.max(Status::Good));
        }
        s.issues.sort_by_key(|(status, _)| std::cmp::Reverse(*status));
        s
    }
}

fn nvme_summary(log: &NvmeLog) -> Summary {
    let mut issues = Vec::new();
    let warnings = [
        (0x01, t("De reserve-opslag is bijna op.", "The spare capacity is nearly used up.")),
        (0x02, t("De temperatuur is buiten het veilige bereik geweest.", "The temperature has been outside the safe range.")),
        (0x04, t("De schijf meldt zelf dat hij onbetrouwbaar wordt.", "The disk itself reports it is becoming unreliable.")),
        (0x08, t("De schijf staat in alleen-lezen-modus.", "The disk is in read-only mode.")),
        (0x10, t("De back-up van het werkgeheugen van de schijf werkt niet.", "The backup of the disk's volatile memory has failed.")),
    ];
    for (bit, text) in warnings {
        if log.critical_warning & bit != 0 {
            issues.push((Status::Bad, text.to_string()));
        }
    }
    if log.available_spare < log.spare_threshold && log.critical_warning & 0x01 == 0 {
        issues.push((Status::Bad, tf!("Nog maar {}% reserve-opslag over.", "Only {}% spare capacity left.", log.available_spare)));
    }
    let used = log.percentage_used.min(100);
    let life_left = 100 - used;
    if used >= 100 {
        issues.push((Status::Bad, end_of_life()));
    } else if life_left <= LOW_LIFE_PERCENT {
        issues.push((Status::Caution, tf!("Nog ongeveer {life_left}% levensduur over.", "About {life_left}% of its life left.")));
    }
    if log.media_errors > 0 {
        issues.push((Status::Caution, tf!("{} keer kon de schijf gegevens niet goed lezen of schrijven.", "{} times the disk could not read or write data correctly.", log.media_errors)));
    }
    let temperature = (log.temperature_kelvin > 0).then(|| i32::from(log.temperature_kelvin) - 273);
    Summary {
        status: None,
        temperature,
        life_left: Some(life_left),
        power_on_hours: Some(log.power_on_hours),
        power_cycles: Some(log.power_cycles),
        bytes_read: Some(log.data_units_read.saturating_mul(NVME_DATA_UNIT)),
        bytes_written: Some(log.data_units_written.saturating_mul(NVME_DATA_UNIT)),
        issues,
    }
}

fn ata_summary(attrs: &[AtaAttribute]) -> Summary {
    let find = |id: u8| attrs.iter().find(|a| a.id == id);
    let mut issues = Vec::new();
    for a in attrs {
        if a.threshold > 0 && a.current > 0 && a.current <= a.threshold {
            issues.push((Status::Bad, tf!("{} is onder de grens van de fabrikant.", "{} is below the maker's threshold.", attribute_name(a.id))));
        }
    }
    let counters = [
        (5, t("sectoren zijn al vervangen door reserve-sectoren.", "sectors have already been replaced by spare sectors.")),
        (197, t("sectoren zijn onbetrouwbaar en wachten op vervanging.", "sectors are unstable and waiting to be replaced.")),
        (198, t("sectoren konden niet meer gelezen worden.", "sectors could no longer be read.")),
    ];
    for (id, text) in counters {
        if let Some(n) = find(id).map(AtaAttribute::count).filter(|&n| n > 0) {
            issues.push((Status::Caution, format!("{n} {text}")));
        }
    }
    // SSD wear: makers use different attributes, all with a normalised
    // value that counts down from 100.
    let life_left = [231, 233, 177, 202, 169].iter().find_map(|&id| find(id)).map(|a| a.current.min(100));
    if let Some(left) = life_left {
        if left == 0 {
            issues.push((Status::Bad, end_of_life()));
        } else if left <= LOW_LIFE_PERCENT {
            issues.push((Status::Caution, tf!("Nog ongeveer {left}% levensduur over.", "About {left}% of its life left.")));
        }
    }
    let temperature = find(194).or_else(|| find(190)).map(|a| (a.raw & 0xFF) as i32).filter(|&t| t > 0);
    Summary {
        status: None,
        temperature,
        life_left,
        power_on_hours: find(9).map(AtaAttribute::count),
        power_cycles: find(12).map(AtaAttribute::count),
        bytes_read: None,
        bytes_written: None,
        issues,
    }
}

fn end_of_life() -> String {
    t(
        "De opgegeven levensduur is bereikt. Vervang de schijf binnenkort.",
        "The rated life has been reached. Replace the disk soon.",
    )
    .to_string()
}

/// Name of a SATA SMART attribute.
pub fn attribute_name(id: u8) -> &'static str {
    match id {
        1 => t("Leesfouten", "Read errors"),
        2 => t("Doorvoer", "Throughput"),
        3 => t("Opstarttijd", "Spin-up time"),
        4 => t("Aantal keer gestart", "Start/stop count"),
        5 => t("Vervangen sectoren", "Reallocated sectors"),
        7 => t("Zoekfouten", "Seek errors"),
        8 => t("Zoeksnelheid", "Seek time"),
        9 => t("Uren in gebruik", "Power-on hours"),
        10 => t("Herhaalde opstartpogingen", "Spin retries"),
        11 => t("Herhaalde kalibraties", "Calibration retries"),
        12 => t("Aantal keer aangezet", "Power cycles"),
        13 => t("Zachte leesfouten", "Soft read errors"),
        169 => t("Resterende levensduur", "Remaining life"),
        170 => t("Beschikbare reserveblokken", "Available reserved blocks"),
        171 => t("Programmeerfouten", "Program fails"),
        172 => t("Wisfouten", "Erase fails"),
        173 => t("Slijtage", "Wear"),
        174 => t("Onverwacht uitgeschakeld", "Unexpected power loss"),
        177 => t("Slijtage-egalisatie", "Wear leveling"),
        179 => t("Gebruikte reserveblokken", "Used reserved blocks"),
        181 => t("Programmeerfouten", "Program fails"),
        182 => t("Wisfouten", "Erase fails"),
        183 => t("Teruggeschakelde snelheid", "Runtime bad blocks / downshifts"),
        184 => t("Fouten van begin tot eind", "End-to-end errors"),
        187 => t("Gemelde onherstelbare fouten", "Reported uncorrectable errors"),
        188 => t("Time-outs van commando's", "Command timeouts"),
        189 => t("Hoog vliegende kop", "High fly writes"),
        190 => t("Luchtstroomtemperatuur", "Airflow temperature"),
        191 => t("Schokken", "G-sense errors"),
        192 => t("Noodparkeringen", "Emergency retracts"),
        193 => t("Parkeercycli", "Load cycles"),
        194 => t("Temperatuur", "Temperature"),
        195 => t("Gecorrigeerde fouten (ECC)", "Corrected errors (ECC)"),
        196 => t("Vervangingen", "Reallocation events"),
        197 => t("Wachtende sectoren", "Pending sectors"),
        198 => t("Onherstelbare sectoren", "Uncorrectable sectors"),
        199 => t("Overdrachtsfouten (kabel)", "Transfer errors (cable)"),
        200 => t("Schrijffouten", "Write errors"),
        202 => t("Gebruikte levensduur", "Life used"),
        206 => t("Kop-afstand", "Fly height"),
        231 => t("Resterende levensduur", "Remaining life"),
        232 => t("Beschikbare reserve", "Available reserve"),
        233 => t("Slijtage-indicator", "Wearout indicator"),
        235 => t("Stroomstoringen", "Power failures"),
        240 => t("Uren kop in gebruik", "Head flying hours"),
        241 => t("Totaal geschreven", "Total written"),
        242 => t("Totaal gelezen", "Total read"),
        _ => t("Fabrikantspecifiek", "Vendor specific"),
    }
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    bytes.get(at..at + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()))
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    bytes.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
}

/// Parses the 512-byte NVMe health log. The 128-bit counters are read as
/// their low 64 bits, which is more than any disk will reach.
pub fn parse_nvme_log(b: &[u8]) -> Option<NvmeLog> {
    if b.len() < 192 {
        return None;
    }
    Some(NvmeLog {
        critical_warning: b[0],
        temperature_kelvin: u16::from_le_bytes([b[1], b[2]]),
        available_spare: b[3],
        spare_threshold: b[4],
        percentage_used: b[5],
        data_units_read: le_u64(b, 32),
        data_units_written: le_u64(b, 48),
        power_cycles: le_u64(b, 112),
        power_on_hours: le_u64(b, 128),
        unsafe_shutdowns: le_u64(b, 144),
        media_errors: le_u64(b, 160),
        error_log_entries: le_u64(b, 176),
    })
}

/// Parses ATA "SMART READ DATA" and "READ THRESHOLDS" (512 bytes each):
/// after a 2-byte revision, 30 entries of 12 bytes.
pub fn parse_ata(data: &[u8], thresholds: &[u8]) -> Vec<AtaAttribute> {
    let entry = |b: &[u8], i: usize| b.get(2 + i * 12..2 + i * 12 + 12).map(<[u8]>::to_vec);
    let threshold_of = |id: u8| {
        (0..30).filter_map(|i| entry(thresholds, i)).find(|e| e[0] == id).map_or(0, |e| e[1])
    };
    (0..30)
        .filter_map(|i| entry(data, i))
        .filter(|e| e[0] != 0)
        .map(|e| AtaAttribute {
            id: e[0],
            current: e[3],
            worst: e[4],
            threshold: threshold_of(e[0]),
            raw: e[5..11].iter().rev().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)),
        })
        .collect()
}

/// (model, serial, bus) from a `STORAGE_DEVICE_DESCRIPTOR`.
fn parse_descriptor(b: &[u8]) -> Option<(String, String, Bus)> {
    if b.len() < 32 {
        return None;
    }
    let text = |offset_at: usize| {
        let offset = le_u32(b, offset_at) as usize;
        if offset == 0 || offset >= b.len() {
            return String::new();
        }
        let end = b[offset..].iter().position(|&c| c == 0).map_or(b.len(), |p| offset + p);
        String::from_utf8_lossy(&b[offset..end]).trim().to_string()
    };
    let (vendor, product) = (text(12), text(16));
    let model = if vendor.is_empty() || product.starts_with(&vendor) { product } else { format!("{vendor} {product}") };
    Some((model, text(24), Bus::from_storage_bus_type(le_u32(b, 28))))
}

#[cfg(windows)]
pub use win::read_all;

#[cfg(not(windows))]
pub fn read_all() -> Vec<DiskHealth> {
    Vec::new()
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::ffi::c_void;
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::core::PCWSTR;

    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
    const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x0007_00A0;
    const SMART_RCV_DRIVE_DATA: u32 = 0x0007_C088;
    const STORAGE_DEVICE_PROPERTY: u32 = 0;
    const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: u32 = 7;
    const STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY: u32 = 50;
    const STORAGE_DEVICE_TEMPERATURE_PROPERTY: u32 = 52;
    const PROTOCOL_TYPE_NVME: u32 = 3;
    const NVME_DATA_TYPE_LOG_PAGE: u32 = 2;
    const NVME_LOG_HEALTH: u32 = 0x02;
    /// `STORAGE_PROTOCOL_SPECIFIC_DATA`: ten 32-bit fields.
    const PROTOCOL_DATA_SIZE: usize = 40;
    const SECTOR: usize = 512;
    const MAX_DISKS: u32 = 32;

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn open(index: u32, access: u32) -> windows::core::Result<Handle> {
        let path: Vec<u16> = format!(r"\\.\PhysicalDrive{index}").encode_utf16().chain([0]).collect();
        unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .map(Handle)
    }

    /// Runs `code` with `buf` as both input (its first `input_len` bytes) and
    /// output. Returns the number of bytes written back.
    fn ioctl(h: &Handle, code: u32, buf: &mut [u8], input_len: usize) -> Option<usize> {
        let mut returned = 0u32;
        let ptr = buf.as_mut_ptr() as *mut c_void;
        unsafe {
            DeviceIoControl(h.0, code, Some(ptr), input_len as u32, Some(ptr), buf.len() as u32, Some(&mut returned), None)
        }
        .ok()?;
        Some(returned as usize)
    }

    /// `IOCTL_STORAGE_QUERY_PROPERTY` (standard query) for `property`.
    fn query_property(h: &Handle, property: u32, out_len: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; out_len.max(12)];
        buf[0..4].copy_from_slice(&property.to_le_bytes());
        let n = ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, &mut buf, 12)?;
        buf.truncate(n);
        Some(buf)
    }

    fn nvme_log(h: &Handle) -> Option<NvmeLog> {
        // STORAGE_PROPERTY_QUERY header (8 bytes), then the protocol request,
        // then room for the log page.
        let mut buf = vec![0u8; 8 + PROTOCOL_DATA_SIZE + SECTOR];
        let fields = [
            STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY,
            0,
            PROTOCOL_TYPE_NVME,
            NVME_DATA_TYPE_LOG_PAGE,
            NVME_LOG_HEALTH,
            0,
            PROTOCOL_DATA_SIZE as u32,
            SECTOR as u32,
        ];
        for (i, v) in fields.iter().enumerate() {
            buf[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        let len = buf.len();
        ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, &mut buf, len)?;
        // The answer is a STORAGE_PROTOCOL_DATA_DESCRIPTOR: Version, Size and
        // the protocol data, whose offset field is relative to itself.
        let offset = 8 + le_u32(&buf, 8 + 16) as usize;
        parse_nvme_log(buf.get(offset..offset + SECTOR)?)
    }

    /// One 512-byte SMART sector via the legacy `SMART_RCV_DRIVE_DATA`.
    fn ata_sector(h: &Handle, feature: u8) -> Option<Vec<u8>> {
        // SENDCMDINPARAMS (packed): cBufferSize, IDEREGS, bDriveNumber, …
        // The answer, SENDCMDOUTPARAMS, has the sector after 16 bytes.
        let mut buf = vec![0u8; 16 + SECTOR];
        buf[0..4].copy_from_slice(&(SECTOR as u32).to_le_bytes());
        buf[4..12].copy_from_slice(&[feature, 1, 1, 0x4F, 0xC2, 0xA0, 0xB0, 0]);
        let n = ioctl(h, SMART_RCV_DRIVE_DATA, &mut buf, 33)?;
        (n >= 16 + SECTOR).then(|| buf[16..16 + SECTOR].to_vec())
    }

    fn ata_smart(h: &Handle) -> Option<Vec<AtaAttribute>> {
        let data = ata_sector(h, 0xD0)?;
        let thresholds = ata_sector(h, 0xD1).unwrap_or_default();
        let attrs = parse_ata(&data, &thresholds);
        (!attrs.is_empty()).then_some(attrs)
    }

    fn read_disk(index: u32) -> Option<DiskHealth> {
        // Without read access Windows still answers questions about the disk.
        let info = open(index, 0).ok()?;
        let (model, serial, bus) = parse_descriptor(&query_property(&info, STORAGE_DEVICE_PROPERTY, 1024)?)?;
        let ssd = query_property(&info, STORAGE_DEVICE_SEEK_PENALTY_PROPERTY, 12).and_then(|b| b.get(8).map(|&p| p == 0));
        let size = {
            let mut buf = vec![0u8; 256];
            ioctl(&info, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &mut buf, 0).map_or(0, |_| le_u64(&buf, 24))
        };
        let os_temperature = query_property(&info, STORAGE_DEVICE_TEMPERATURE_PROPERTY, 64)
            // Header of 24 bytes, then the first STORAGE_TEMPERATURE_INFO.
            .filter(|b| b.len() >= 28 && u16::from_le_bytes([b[12], b[13]]) > 0)
            .map(|b| i32::from(i16::from_le_bytes([b[26], b[27]])))
            .filter(|&t| t > 0);

        let smart = match bus {
            Bus::Usb | Bus::Sd => Smart::Unavailable,
            _ => match open(index, GENERIC_READ.0 | GENERIC_WRITE.0) {
                Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                    // Some NVMe drivers answer without administrator rights.
                    match (bus == Bus::Nvme).then(|| nvme_log(&info)).flatten() {
                        Some(log) => Smart::Nvme(log),
                        None => Smart::NeedsAdmin,
                    }
                }
                Err(_) => Smart::Unavailable,
                Ok(rw) => {
                    let read = if bus == Bus::Nvme {
                        nvme_log(&rw).map(Smart::Nvme)
                    } else {
                        ata_smart(&rw).map(Smart::Ata)
                    };
                    read.unwrap_or(Smart::Unavailable)
                }
            },
        };
        Some(DiskHealth { index, model, serial, bus, ssd, size, os_temperature, smart })
    }

    /// Every physical disk Windows knows about. Slow-ish (a sleeping hard
    /// disk may have to spin up), so call it from a background thread.
    pub fn read_all() -> Vec<DiskHealth> {
        (0..MAX_DISKS).filter_map(read_disk).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(smart: Smart) -> DiskHealth {
        DiskHealth {
            index: 0,
            model: "Test".into(),
            serial: String::new(),
            bus: Bus::Nvme,
            ssd: Some(true),
            size: 0,
            os_temperature: None,
            smart,
        }
    }

    fn nvme_bytes(edit: impl FnOnce(&mut [u8])) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[1..3].copy_from_slice(&310u16.to_le_bytes()); // 37 °C
        b[3] = 100;
        b[4] = 10;
        b[5] = 3;
        b[32..40].copy_from_slice(&2_000u64.to_le_bytes());
        b[112..120].copy_from_slice(&42u64.to_le_bytes());
        b[128..136].copy_from_slice(&1234u64.to_le_bytes());
        edit(&mut b);
        b
    }

    fn ata(id: u8, current: u8, threshold: u8, raw: u64) -> AtaAttribute {
        AtaAttribute { id, current, worst: current, threshold, raw }
    }

    #[test]
    fn healthy_nvme_disk() {
        let log = parse_nvme_log(&nvme_bytes(|_| {})).unwrap();

        let s = disk(Smart::Nvme(log)).summary();

        assert_eq!(s.status, Some(Status::Good));
        assert_eq!((s.temperature, s.life_left, s.power_on_hours, s.power_cycles), (Some(37), Some(97), Some(1234), Some(42)));
        assert_eq!(s.bytes_read, Some(2_000 * 512_000));
        assert!(s.issues.is_empty());
    }

    #[test]
    fn nvme_critical_warning_and_wear_are_reported() {
        let log = parse_nvme_log(&nvme_bytes(|b| {
            b[0] = 0x04;
            b[5] = 95;
            b[160] = 3;
        }))
        .unwrap();

        let s = disk(Smart::Nvme(log)).summary();

        assert_eq!(s.status, Some(Status::Bad));
        assert_eq!(s.life_left, Some(5));
        let levels: Vec<Status> = s.issues.iter().map(|(st, _)| *st).collect();
        assert_eq!(levels, [Status::Bad, Status::Caution, Status::Caution]);
        assert!(s.issues[0].1.contains("onbetrouwbaar"));
    }

    #[test]
    fn parses_ata_table_with_thresholds() {
        let mut data = vec![0u8; 512];
        let mut thr = vec![0u8; 512];
        // Entry 0: attribute 5, current 100, worst 99, raw 0x0102 (258).
        data[2..14].copy_from_slice(&[5, 0x33, 0, 100, 99, 0x02, 0x01, 0, 0, 0, 0, 0]);
        // Entry 1: attribute 194, raw low byte 41 °C.
        data[14..26].copy_from_slice(&[194, 0x22, 0, 59, 40, 41, 0, 20, 0, 50, 0, 0]);
        thr[2..4].copy_from_slice(&[5, 10]);

        let attrs = parse_ata(&data, &thr);

        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0], AtaAttribute { id: 5, current: 100, worst: 99, threshold: 10, raw: 258 });
        assert_eq!(attrs[1].threshold, 0);
        assert_eq!(ata_summary(&attrs).temperature, Some(41));
    }

    #[test]
    fn ata_attribute_below_threshold_is_bad() {
        let attrs = vec![ata(5, 9, 10, 1500), ata(9, 90, 0, 20_000), ata(12, 99, 0, 800)];

        let s = DiskHealth { bus: Bus::Sata, ssd: Some(false), ..disk(Smart::Ata(attrs)) }.summary();

        assert_eq!(s.status, Some(Status::Bad));
        assert_eq!((s.power_on_hours, s.power_cycles), (Some(20_000), Some(800)));
        assert!(s.issues[0].1.starts_with("Vervangen sectoren"));
        assert!(s.issues.iter().any(|(st, t)| *st == Status::Caution && t.starts_with("1500 ")));
    }

    #[test]
    fn warm_hard_disk_is_a_caution_but_the_same_ssd_is_fine() {
        let attrs = vec![ata(194, 50, 0, 58)];
        let hdd = DiskHealth { bus: Bus::Sata, ssd: Some(false), ..disk(Smart::Ata(attrs.clone())) };
        let ssd = DiskHealth { bus: Bus::Sata, ssd: Some(true), ..disk(Smart::Ata(attrs)) };

        assert_eq!(hdd.summary().status, Some(Status::Caution));
        assert_eq!(ssd.summary().status, Some(Status::Good));
    }

    #[test]
    fn ssd_wear_attribute_gives_life_left() {
        let s = ata_summary(&[ata(231, 8, 0, 0)]);

        assert_eq!(s.life_left, Some(8));
        assert_eq!(s.issues[0].0, Status::Caution);
    }

    #[test]
    fn without_smart_there_is_no_verdict_but_windows_temperature_is_used() {
        let d = DiskHealth { os_temperature: Some(44), ..disk(Smart::NeedsAdmin) };

        let s = d.summary();

        assert_eq!((s.status, s.temperature), (None, Some(44)));
    }

    #[test]
    fn parses_device_descriptor_strings() {
        let mut b = vec![0u8; 128];
        b[12..16].copy_from_slice(&64u32.to_le_bytes());
        b[16..20].copy_from_slice(&72u32.to_le_bytes());
        b[24..28].copy_from_slice(&100u32.to_le_bytes());
        b[28..32].copy_from_slice(&17u32.to_le_bytes());
        b[64..69].copy_from_slice(b"WDC  ");
        b[72..90].copy_from_slice(b"WDS100T2B0C-00PXH0");
        b[100..106].copy_from_slice(b"  A1B2");

        let (model, serial, bus) = parse_descriptor(&b).unwrap();

        assert_eq!((model.as_str(), serial.as_str(), bus), ("WDC WDS100T2B0C-00PXH0", "A1B2", Bus::Nvme));
    }

    /// Prints what this computer reports: `cargo test print_disks -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn print_disks() {
        for d in read_all() {
            println!("{d:#?}\n{:#?}", d.summary());
        }
    }
}
