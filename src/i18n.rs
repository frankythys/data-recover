//! Dutch and English. Every text is written in both languages where it is
//! used: `t("Opslaan", "Save")`, or `tf!("{n} bestanden", "{n} files")` for
//! text with values. The language comes from `disktools.ini` next to the exe
//! (written by the installer, changed in the app), otherwise from the Windows
//! display language.

use std::path::PathBuf;

#[cfg(not(test))]
static ENGLISH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// Tests run in parallel threads; each keeps its own language.
#[cfg(test)]
thread_local! {
    static ENGLISH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const INI_NAME: &str = "disktools.ini";
const INI_SECTION: &str = "[DiskTools]";
const INI_KEY: &str = "taal";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Nl,
    En,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::Nl => "nl",
            Lang::En => "en",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code.trim().to_ascii_lowercase().as_str() {
            "nl" | "dutch" | "nederlands" => Some(Lang::Nl),
            "en" | "english" | "engels" => Some(Lang::En),
            _ => None,
        }
    }
}

#[cfg(not(test))]
pub fn english() -> bool {
    ENGLISH.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
pub fn english() -> bool {
    ENGLISH.with(std::cell::Cell::get)
}

pub fn lang() -> Lang {
    if english() { Lang::En } else { Lang::Nl }
}

/// The text in the current language.
pub fn t(nl: &'static str, en: &'static str) -> &'static str {
    if english() { en } else { nl }
}

/// Formats text in the current language: `tf!("{n} bestanden", "{n} files")`.
#[macro_export]
macro_rules! tf {
    ($nl:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::english() { format!($en $(, $arg)*) } else { format!($nl $(, $arg)*) }
    };
}

#[cfg(not(test))]
pub fn set(lang: Lang) {
    ENGLISH.store(lang == Lang::En, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
pub fn set(lang: Lang) {
    ENGLISH.with(|e| e.set(lang == Lang::En));
}

fn ini_path() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join(INI_NAME))
}

/// The language in `ini` text, if it names one.
fn parse_ini(ini: &str) -> Option<Lang> {
    ini.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim().eq_ignore_ascii_case(INI_KEY)).then(|| Lang::from_code(value)).flatten()
    })
}

/// Picks the language at start: the ini file, else Windows' language.
pub fn load() {
    let from_ini = ini_path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| parse_ini(&s));
    set(from_ini.unwrap_or_else(system_lang));
}

/// Switches language and remembers it in the ini file.
pub fn save(lang: Lang) -> std::io::Result<()> {
    set(lang);
    let path = ini_path().ok_or_else(|| std::io::Error::other("map van de app onbekend"))?;
    std::fs::write(path, format!("{INI_SECTION}\r\n{INI_KEY}={}\r\n", lang.code()))
}

/// Dutch when Windows is in Dutch, otherwise English.
#[cfg(windows)]
fn system_lang() -> Lang {
    use windows::Win32::Globalization::GetUserDefaultUILanguage;
    const LANG_DUTCH: u16 = 0x13;
    if unsafe { GetUserDefaultUILanguage() } & 0x3FF == LANG_DUTCH { Lang::Nl } else { Lang::En }
}

#[cfg(not(windows))]
fn system_lang() -> Lang {
    Lang::En
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_language_the_installer_wrote() {
        assert_eq!(parse_ini("[DiskTools]\r\ntaal=en\r\n"), Some(Lang::En));
        assert_eq!(parse_ini("[DiskTools]\ntaal = nl"), Some(Lang::Nl));
        assert_eq!(parse_ini("[DiskTools]\ntaal=fr"), None);
        assert_eq!(parse_ini(""), None);
    }

    #[test]
    fn texts_follow_the_language() {
        set(Lang::En);
        let n = 3;
        assert_eq!((t("Opslaan", "Save"), tf!("{n} bestanden", "{n} files")), ("Save", "3 files".to_string()));
        set(Lang::Nl);
        assert_eq!((t("Opslaan", "Save"), tf!("{n} bestanden", "{n} files")), ("Opslaan", "3 bestanden".to_string()));
    }
}
