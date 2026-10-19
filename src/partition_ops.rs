//! Changing partitions: resize, create, drive letter, label, mark active,
//! hide or show, check, optimise, format, wipe and delete. The work is done by Windows' own storage
//! management (the PowerShell Storage cmdlets, as Disk Management uses), so
//! the app never writes partition tables or file systems itself. Values are
//! passed through environment variables, never pasted into the script, so a
//! label cannot inject commands. Checking and optimising run `chkdsk` and
//! `defrag` directly, because those report their progress. Everything needs
//! administrator rights.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::process::{Command, Stdio};

/// Exit code the script uses when it does not run as administrator.
const EXIT_NOT_ADMIN: i32 = 3;
const NEEDS_ADMIN: &str = "Hiervoor zijn administratorrechten nodig. Start DiskTools als administrator.";
/// Last output lines of a tool kept for an error message.
const TAIL_LINES: usize = 6;
const MAX_NTFS_LABEL: usize = 32;
const MAX_FAT_LABEL: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fs {
    Ntfs,
    ExFat,
    Fat32,
}

impl Fs {
    pub const ALL: [Fs; 3] = [Fs::Ntfs, Fs::ExFat, Fs::Fat32];

    pub fn label(self) -> &'static str {
        match self {
            Fs::Ntfs => "NTFS",
            Fs::ExFat => "exFAT",
            Fs::Fat32 => "FAT32",
        }
    }
}

/// One partition, by disk and partition number, with the name shown to the
/// user (`D: Data` or `Partitie 3`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub disk: u32,
    pub part: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PartOp {
    Resize { target: Target, from: u64, to: u64 },
    Create { disk: u32, offset: u64, size: u64, fs: Fs, label: String, letter: Option<char> },
    SetLetter { target: Target, letter: char },
    SetLabel { target: Target, label: String },
    /// Mark an MBR partition as the one the BIOS starts from.
    SetActive { target: Target },
    /// Remove the drive letter, so the partition no longer shows in Explorer.
    Hide { target: Target, letter: char },
    /// Give a hidden partition a drive letter again.
    Show { target: Target },
    /// `chkdsk X: /scan`: an online check that changes nothing.
    Check { target: Target, letter: char },
    /// `defrag X: /O`: defragments a hard disk, trims an SSD.
    Optimize { target: Target, letter: char },
    Format { target: Target, fs: Fs, label: String },
    /// Full format: every sector of the partition is overwritten with zeros.
    Wipe { target: Target, fs: Fs },
    Delete { target: Target },
}

impl PartOp {
    /// Dutch one-line description for the queue.
    pub fn describe(&self) -> String {
        let size = crate::disk::format_size;
        match self {
            PartOp::Resize { target, from, to } => {
                let verb = if to > from { "vergroten" } else { "verkleinen" };
                format!("{} {verb} van {} naar {}", target.name, size(*from), size(*to))
            }
            PartOp::Create { disk, size: s, fs, letter, .. } => {
                let letter = letter.map(|l| format!(" als {l}:")).unwrap_or_default();
                format!("Nieuwe {}-partitie van {} op schijf {disk}{letter}", fs.label(), size(*s))
            }
            PartOp::SetLetter { target, letter } => format!("{}: letter wordt {letter}:", target.name),
            PartOp::SetLabel { target, label } => format!("{}: label wordt \"{label}\"", target.name),
            PartOp::SetActive { target } => format!("{} actief maken", target.name),
            PartOp::Hide { target, .. } => format!("{} verbergen (letter weghalen)", target.name),
            PartOp::Show { target } => format!("{} zichtbaar maken", target.name),
            PartOp::Wipe { target, .. } => format!("{} wissen: alles overschrijven met nullen", target.name),
            PartOp::Check { target, .. } => format!("{} controleren op fouten", target.name),
            PartOp::Optimize { target, .. } => format!("{} optimaliseren", target.name),
            PartOp::Format { target, fs, .. } => format!("{} formatteren als {}", target.name, fs.label()),
            PartOp::Delete { target } => format!("{} verwijderen", target.name),
        }
    }

    /// Whether the operation erases data.
    pub fn is_destructive(&self) -> bool {
        matches!(self, PartOp::Format { .. } | PartOp::Wipe { .. } | PartOp::Delete { .. })
    }

    fn target(&self) -> Option<&Target> {
        match self {
            PartOp::Resize { target, .. }
            | PartOp::SetLetter { target, .. }
            | PartOp::SetLabel { target, .. }
            | PartOp::SetActive { target }
            | PartOp::Hide { target, .. }
            | PartOp::Show { target }
            | PartOp::Check { target, .. }
            | PartOp::Optimize { target, .. }
            | PartOp::Format { target, .. }
            | PartOp::Wipe { target, .. }
            | PartOp::Delete { target } => Some(target),
            PartOp::Create { .. } => None,
        }
    }

    /// The script body and the values it reads from `$env:DR_*`; `None` for
    /// the operations that run a tool instead.
    fn script(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        let mut env = Vec::new();
        if let Some(t) = self.target() {
            env.push(("DR_DISK", t.disk.to_string()));
            env.push(("DR_PART", t.part.to_string()));
        }
        let body = match self {
            PartOp::Resize { to, .. } => {
                env.push(("DR_SIZE", to.to_string()));
                "Resize-Partition -DiskNumber $env:DR_DISK -PartitionNumber $env:DR_PART -Size ([UInt64]$env:DR_SIZE)"
            }
            PartOp::Create { disk, offset, size, fs, label, letter } => {
                env.push(("DR_DISK", disk.to_string()));
                env.push(("DR_OFFSET", offset.to_string()));
                env.push(("DR_SIZE", size.to_string()));
                env.push(("DR_FS", fs.label().to_string()));
                env.push(("DR_LABEL", label.clone()));
                env.push(("DR_LETTER", letter.map(String::from).unwrap_or_default()));
                "$size = [UInt64]$env:DR_SIZE; $offset = [UInt64]$env:DR_OFFSET\n\
                 $p = if ($env:DR_LETTER) { New-Partition -DiskNumber $env:DR_DISK -Offset $offset -Size $size -DriveLetter $env:DR_LETTER }\n\
                 else { New-Partition -DiskNumber $env:DR_DISK -Offset $offset -Size $size -AssignDriveLetter }\n\
                 $p | Format-Volume -FileSystem $env:DR_FS -NewFileSystemLabel $env:DR_LABEL -Confirm:$false | Out-Null"
            }
            PartOp::SetLetter { letter, .. } => {
                env.push(("DR_LETTER", letter.to_string()));
                "Set-Partition -DiskNumber $env:DR_DISK -PartitionNumber $env:DR_PART -NewDriveLetter $env:DR_LETTER"
            }
            PartOp::SetLabel { label, .. } => {
                env.push(("DR_LABEL", label.clone()));
                "Get-Partition -DiskNumber $env:DR_DISK -PartitionNumber $env:DR_PART | Get-Volume | Set-Volume -NewFileSystemLabel $env:DR_LABEL"
            }
            PartOp::SetActive { .. } => "Set-Partition -DiskNumber $env:DR_DISK -PartitionNumber $env:DR_PART -IsActive $true",
            PartOp::Hide { letter, .. } => {
                env.push(("DR_LETTER", letter.to_string()));
                "Remove-PartitionAccessPath -DiskNumber $env:DR_DISK -PartitionNumber $env:DR_PART -AccessPath \"$($env:DR_LETTER):\\\""
            }
            PartOp::Show { .. } => "Add-PartitionAccessPath -DiskNumber $env:DR_DISK -PartitionNumber $env:DR_PART -AssignDriveLetter",
            PartOp::Wipe { fs, .. } => {
                env.push(("DR_FS", fs.label().to_string()));
                "Get-Partition -DiskNumber $env:DR_DISK -PartitionNumber $env:DR_PART | \
                 Format-Volume -FileSystem $env:DR_FS -Full -Confirm:$false -Force | Out-Null"
            }
            PartOp::Check { .. } | PartOp::Optimize { .. } => return None,
            PartOp::Format { fs, label, .. } => {
                env.push(("DR_FS", fs.label().to_string()));
                env.push(("DR_LABEL", label.clone()));
                "Get-Partition -DiskNumber $env:DR_DISK -PartitionNumber $env:DR_PART | \
                 Format-Volume -FileSystem $env:DR_FS -NewFileSystemLabel $env:DR_LABEL -Confirm:$false -Force | Out-Null"
            }
            PartOp::Delete { .. } => "Remove-Partition -DiskNumber $env:DR_DISK -PartitionNumber $env:DR_PART -Confirm:$false",
        };
        Some((body, env))
    }

    /// Carries out the operation, reporting a percentage when the tool gives
    /// one. Returns what Windows reported, in Dutch where it is known.
    pub fn run(&self, progress: &mut dyn FnMut(u8)) -> Result<String, String> {
        match self {
            PartOp::Check { letter, .. } => check(*letter, progress),
            PartOp::Optimize { letter, .. } => optimize(*letter, progress),
            _ => {
                let (body, env) = self.script().ok_or("onbekende bewerking")?;
                run_script(body, &env).map(|_| "Gelukt".to_string())
            }
        }
    }
}

fn check(letter: char, progress: &mut dyn FnMut(u8)) -> Result<String, String> {
    require_admin()?;
    let drive = format!("{letter}:");
    let (code, tail) = run_tool("chkdsk.exe", &[&drive, "/scan"], progress).map_err(|e| format!("chkdsk kon niet starten: {e}"))?;
    check_result(code, letter, &tail)
}

/// What `chkdsk /scan` means by its exit code.
fn check_result(code: i32, letter: char, tail: &str) -> Result<String, String> {
    match code {
        0 => Ok("Geen fouten gevonden".to_string()),
        1 | 2 => Ok("Kleine problemen gevonden en meteen opgelost".to_string()),
        3 => Err(format!("Fouten gevonden die nu niet hersteld kunnen worden. Herstel ze met chkdsk {letter}: /f (vraagt soms een herstart).")),
        _ => Err(if tail.is_empty() { format!("chkdsk stopte met code {code}") } else { tail.to_string() }),
    }
}

fn optimize(letter: char, progress: &mut dyn FnMut(u8)) -> Result<String, String> {
    require_admin()?;
    let drive = format!("{letter}:");
    match run_tool("defrag.exe", &[&drive, "/O", "/U"], progress) {
        Ok((0, _)) => Ok("Gelukt".to_string()),
        Ok((code, tail)) => Err(if tail.is_empty() { format!("defrag stopte met code {code}") } else { tail }),
        Err(e) => Err(format!("defrag kon niet starten: {e}")),
    }
}

#[cfg(windows)]
fn require_admin() -> Result<(), String> {
    use windows::Win32::UI::Shell::IsUserAnAdmin;
    if unsafe { IsUserAnAdmin() }.as_bool() { Ok(()) } else { Err(NEEDS_ADMIN.to_string()) }
}

#[cfg(not(windows))]
fn require_admin() -> Result<(), String> {
    Err(NEEDS_ADMIN.to_string())
}

/// The last percentage in a line of tool output, e.g. 31 in
/// "Voortgang: 5 van 9 voltooid; Fase: 100%; Totaal: 31%; ETA: 0:00:05".
fn percent_in(line: &str) -> Option<u8> {
    let bytes = line.as_bytes();
    let mut last = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'%' {
            continue;
        }
        let mut end = i;
        while end > 0 && bytes[end - 1] == b' ' {
            end -= 1;
        }
        let start = bytes[..end].iter().rposition(|c| !c.is_ascii_digit()).map_or(0, |p| p + 1);
        if let Some(n) = line.get(start..end).and_then(|d| d.parse::<u8>().ok()).filter(|&n| n <= 100) {
            last = Some(n);
        }
    }
    last
}

/// Runs a console tool, passing every percentage it prints to `progress`
/// as it appears. Returns its exit code and its last lines of output.
fn run_tool(exe: &str, args: &[&str], progress: &mut dyn FnMut(u8)) -> io::Result<(i32, String)> {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    let mut out = child.stdout.take().ok_or_else(|| io::Error::other("geen uitvoer"))?;
    let mut tail: VecDeque<String> = VecDeque::new();
    let mut line = Vec::new();
    let mut buf = [0u8; 4096];
    let mut finish_line = |line: &mut Vec<u8>, tail: &mut VecDeque<String>| {
        let text = String::from_utf8_lossy(line).trim().to_string();
        line.clear();
        if text.is_empty() {
            return;
        }
        if let Some(p) = percent_in(&text) {
            progress(p);
        }
        if tail.len() == TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(text);
    };
    loop {
        let n = out.read(&mut buf)?;
        if n == 0 {
            break;
        }
        // Progress lines are rewritten with a carriage return.
        for &b in &buf[..n] {
            if b == b'\r' || b == b'\n' {
                finish_line(&mut line, &mut tail);
            } else {
                line.push(b);
            }
        }
    }
    finish_line(&mut line, &mut tail);
    let status = child.wait()?;
    Ok((status.code().unwrap_or(-1), Vec::from(tail).join("\n")))
}

/// What may not be changed on this computer.
#[derive(Debug, Clone, Default)]
pub struct Guard {
    /// Disk that holds the Windows drive; `None` when unknown, and then no
    /// disk may be formatted or have partitions deleted.
    pub windows_disk: Option<u32>,
    /// (disk, partition) of EFI, Microsoft-reserved and recovery partitions.
    pub system_parts: Vec<(u32, u32)>,
    /// Drive letters in use.
    pub used_letters: Vec<char>,
    /// Letter of the Windows drive, which may never be hidden.
    pub windows_letter: Option<char>,
}

impl Guard {
    /// Why `op` is not allowed, if it is not.
    pub fn refuse(&self, op: &PartOp) -> Option<String> {
        if let Some(t) = op.target() {
            if self.system_parts.contains(&(t.disk, t.part)) {
                return Some("Dit is een systeempartitie die Windows nodig heeft om op te starten.".to_string());
            }
            if op.is_destructive() && self.windows_disk.is_none_or(|d| d == t.disk) {
                return Some("Verwijderen, formatteren en wissen mag niet op de schijf waar Windows op staat.".to_string());
            }
            if matches!(op, PartOp::SetActive { .. }) && self.windows_disk.is_none_or(|d| d == t.disk) {
                return Some("Een andere partitie actief maken op de Windows-schijf kan het opstarten breken.".to_string());
            }
        }
        if let PartOp::Hide { letter, .. } = op {
            if self.windows_letter.is_none_or(|w| w == *letter) {
                return Some("De Windows-schijf kan niet verborgen worden.".to_string());
            }
        }
        let letter = match op {
            PartOp::SetLetter { letter, .. } | PartOp::Create { letter: Some(letter), .. } => Some(*letter),
            _ => None,
        };
        if let Some(l) = letter {
            if !('D'..='Z').contains(&l) {
                return Some("Kies een letter van D tot en met Z.".to_string());
            }
            if self.used_letters.contains(&l) {
                return Some(format!("Letter {l}: is al in gebruik."));
            }
        }
        let label = match op {
            PartOp::SetLabel { label, .. } => Some((label, None)),
            PartOp::Format { label, fs, .. } | PartOp::Create { label, fs, .. } => Some((label, Some(*fs))),
            _ => None,
        };
        if let Some((label, fs)) = label {
            let max = if matches!(fs, Some(Fs::Fat32 | Fs::ExFat)) { MAX_FAT_LABEL } else { MAX_NTFS_LABEL };
            if label.chars().count() > max {
                return Some(format!("Het label mag hoogstens {max} tekens hebben."));
            }
        }
        if let PartOp::Resize { from, to, .. } = op {
            if from == to {
                return Some("Kies een andere grootte.".to_string());
            }
        }
        None
    }
}

/// Smallest and largest size Windows allows for a partition.
pub fn supported_size(disk: u32, part: u32) -> Result<(u64, u64), String> {
    let out = run_script(
        "$s = Get-PartitionSupportedSize -DiskNumber $env:DR_DISK -PartitionNumber $env:DR_PART\n\
         Write-Output \"$($s.SizeMin) $($s.SizeMax)\"",
        &[("DR_DISK", disk.to_string()), ("DR_PART", part.to_string())],
    )?;
    parse_sizes(&out).ok_or_else(|| format!("Onverwacht antwoord van Windows: {out}"))
}

fn parse_sizes(out: &str) -> Option<(u64, u64)> {
    let mut parts = out.split_whitespace().map(str::parse::<u64>);
    match (parts.next(), parts.next()) {
        (Some(Ok(min)), Some(Ok(max))) if min <= max => Some((min, max)),
        _ => None,
    }
}

/// Runs `body` in PowerShell with `env` set, as administrator only. Returns
/// its output, or the error message Windows gave.
fn run_script(body: &str, env: &[(&str, String)]) -> Result<String, String> {
    let script = format!(
        "$ErrorActionPreference = 'Stop'\n\
         [Console]::OutputEncoding = [Text.Encoding]::UTF8\n\
         $id = [Security.Principal.WindowsIdentity]::GetCurrent()\n\
         if (-not ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {{ exit {EXIT_NOT_ADMIN} }}\n\
         try {{\n{body}\n}} catch {{ [Console]::Error.WriteLine($_.Exception.Message); exit 1 }}"
    );
    let output = powershell(&script, env).map_err(|e| format!("PowerShell kon niet starten: {e}"))?;
    match output.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&output.stdout).trim().to_string()),
        Some(EXIT_NOT_ADMIN) => Err(NEEDS_ADMIN.to_string()),
        _ => {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if err.is_empty() { "Windows meldde een onbekende fout.".to_string() } else { err })
        }
    }
}

fn powershell(script: &str, env: &[(&str, String)]) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script]);
    for (key, value) in env {
        cmd.env(key, value);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.output()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(disk: u32, part: u32) -> Target {
        Target { disk, part, name: format!("Partitie {part}") }
    }

    fn guard() -> Guard {
        Guard { windows_disk: Some(0), system_parts: vec![(0, 1), (0, 4)], used_letters: vec!['C', 'D'], windows_letter: Some('C') }
    }

    #[test]
    fn destructive_ops_are_refused_on_the_windows_disk_but_allowed_elsewhere() {
        let g = guard();
        let delete = |disk| PartOp::Delete { target: target(disk, 3) };
        let format = |disk| PartOp::Format { target: target(disk, 3), fs: Fs::Ntfs, label: String::new() };

        assert!(g.refuse(&delete(0)).is_some());
        assert!(g.refuse(&format(0)).is_some());
        assert_eq!(g.refuse(&delete(1)), None);
        assert_eq!(g.refuse(&PartOp::Resize { target: target(0, 3), from: 10, to: 20 }), None);
        let unknown = Guard { windows_disk: None, ..guard() };
        assert!(unknown.refuse(&delete(1)).is_some(), "zonder bekende Windows-schijf nergens verwijderen");
    }

    #[test]
    fn windows_disk_cannot_be_wiped_hidden_or_have_its_active_partition_changed() {
        let g = guard();

        assert!(g.refuse(&PartOp::Wipe { target: target(0, 3), fs: Fs::Ntfs }).is_some());
        assert!(g.refuse(&PartOp::SetActive { target: target(0, 3) }).is_some());
        assert!(g.refuse(&PartOp::Hide { target: target(0, 3), letter: 'C' }).is_some());
        assert_eq!(g.refuse(&PartOp::Hide { target: target(1, 1), letter: 'E' }), None);
        assert_eq!(g.refuse(&PartOp::SetActive { target: target(1, 1) }), None);
        assert!(PartOp::Wipe { target: target(1, 1), fs: Fs::Ntfs }.is_destructive());
    }

    #[test]
    fn hide_passes_the_letter_through_the_environment() {
        let (body, env) = PartOp::Hide { target: target(1, 2), letter: 'E' }.script().unwrap();

        assert!(body.contains("$env:DR_LETTER") && !body.contains("E:"));
        assert!(env.contains(&("DR_LETTER", "E".to_string())));
    }

    #[test]
    fn system_partitions_are_never_changed() {
        let op = PartOp::SetLabel { target: target(0, 4), label: "x".into() };

        assert!(guard().refuse(&op).unwrap().contains("systeempartitie"));
    }

    #[test]
    fn letters_must_be_free_and_from_d_to_z() {
        let g = guard();
        let set = |letter| PartOp::SetLetter { target: target(1, 1), letter };

        assert!(g.refuse(&set('D')).unwrap().contains("in gebruik"));
        assert!(g.refuse(&set('A')).is_some());
        assert_eq!(g.refuse(&set('E')), None);
    }

    #[test]
    fn labels_have_a_maximum_length_per_file_system() {
        let g = guard();
        let format = |fs, label: &str| PartOp::Format { target: target(1, 1), fs, label: label.into() };

        assert!(g.refuse(&format(Fs::Fat32, "TWAALF TEKEN")).is_some());
        assert_eq!(g.refuse(&format(Fs::Ntfs, "TWAALF TEKEN")), None);
    }

    #[test]
    fn values_go_through_the_environment_not_the_script() {
        let op = PartOp::SetLabel { target: target(2, 5), label: "'; Remove-Item C:\\ -Recurse; '".into() };

        let (body, env) = op.script().unwrap();

        assert!(!body.contains("Remove-Item"));
        assert!(env.contains(&("DR_LABEL", "'; Remove-Item C:\\ -Recurse; '".to_string())));
        assert!(env.contains(&("DR_DISK", "2".to_string())) && env.contains(&("DR_PART", "5".to_string())));
    }

    #[test]
    fn describes_operations_in_dutch() {
        let grow = PartOp::Resize { target: Target { disk: 0, part: 3, name: "C:".into() }, from: 1 << 30, to: 2 << 30 };

        assert_eq!(grow.describe(), "C: vergroten van 1,00 GB naar 2,00 GB");
        assert!(PartOp::Delete { target: target(1, 1) }.is_destructive());
    }

    #[test]
    fn parses_supported_sizes_and_check_results() {
        assert_eq!(parse_sizes("1048576 2097152\r\n"), Some((1_048_576, 2_097_152)));
        assert_eq!(parse_sizes("Fout"), None);
        assert_eq!(check_result(0, 'D', ""), Ok("Geen fouten gevonden".to_string()));
        assert!(check_result(3, 'D', "").unwrap_err().contains("chkdsk D: /f"));
        assert_eq!(check_result(7, 'D', "Toegang geweigerd"), Err("Toegang geweigerd".to_string()));
    }

    #[test]
    fn finds_the_total_percentage_in_tool_output() {
        assert_eq!(percent_in("Progress: 5 of 9 done; Stage: 100%; Total: 31%; ETA:   0:00:05 .."), Some(31));
        assert_eq!(percent_in("Voortgang: 5 van 9 voltooid; Fase: 100 %; Totaal: 7 %"), Some(7));
        assert_eq!(percent_in("Optimization: 45% complete..."), Some(45));
        assert_eq!(percent_in("Stage 1: Examining basic file system structure ..."), None);
        assert_eq!(percent_in("250% klopt niet"), None);
    }

    /// Runs a real process: progress printed with carriage returns arrives
    /// as it is printed, and the exit code and last lines come back.
    #[cfg(windows)]
    #[test]
    fn tool_progress_is_read_while_it_runs() {
        let script = "[Console]::Out.Write(\"Stage: 50%; Total: 10%`r\"); [Console]::Out.Write(\"Stage: 90%; Total: 60%`r\"); \
                      Write-Output 'Klaar'; exit 4";
        let mut seen = Vec::new();

        let (code, tail) = run_tool("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", script], &mut |p| seen.push(p)).unwrap();

        assert_eq!(seen, [10, 60]);
        assert_eq!(code, 4);
        assert!(tail.ends_with("Klaar"), "{tail}");
    }

    /// Runs PowerShell for real: without administrator rights the script
    /// stops before doing anything, with a clear message.
    #[cfg(windows)]
    #[test]
    fn scripts_only_run_as_administrator() {
        match run_script("Write-Output klaar", &[]) {
            Ok(out) => assert_eq!(out, "klaar"),
            Err(e) => assert!(e.contains("administrator"), "{e}"),
        }
    }

    /// Runs PowerShell for real; the value with quotes must come back
    /// unchanged. Without administrator rights the script stops first.
    #[cfg(windows)]
    #[test]
    fn powershell_receives_values_literally() {
        let value = "'; Write-Output gehackt; '";
        let out = powershell("[Console]::OutputEncoding = [Text.Encoding]::UTF8; Write-Output $env:DR_VALUE", &[("DR_VALUE", value.to_string())])
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), value);
    }
}
