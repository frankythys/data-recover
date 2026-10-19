//! Puts the DiskTools icon and name in the Windows executable, so Explorer
//! and the taskbar show them. Release builds also get a manifest that makes
//! Windows always start DiskTools as administrator (most disk tools need
//! it); debug builds do not, so `cargo run` and the tests work in a normal
//! terminal. The resource compiler runs inside OUT_DIR with short relative
//! paths: windres (GNU toolchain) breaks on paths with spaces.

use std::path::{Path, PathBuf};
use std::process::Command;

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
"#;

fn main() {
    println!("cargo:rerun-if-changed=assets/disktools.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // Without a resource compiler the app still builds, only without an
    // icon on the exe.
    if let Err(e) = embed_resources() {
        println!("cargo:warning=Icoon niet in de exe gezet: {e}");
    }
}

fn embed_resources() -> Result<(), String> {
    let out = PathBuf::from(std::env::var("OUT_DIR").map_err(|e| e.to_string())?);
    let release = std::env::var("PROFILE").as_deref() == Ok("release");
    let gnu = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
    std::fs::copy("assets/disktools.ico", out.join("disktools.ico")).map_err(|e| e.to_string())?;
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let numbers = version.split('.').chain(std::iter::repeat("0")).take(4).collect::<Vec<_>>().join(",");
    let mut rc = format!(
        "1 ICON \"disktools.ico\"\n\
         1 VERSIONINFO\nFILEVERSION {numbers}\nPRODUCTVERSION {numbers}\nFILEOS 0x40004\nFILETYPE 0x1\n\
         {{\n BLOCK \"StringFileInfo\"\n {{\n  BLOCK \"041304b0\"\n  {{\n\
           VALUE \"FileDescription\", \"DiskTools\"\n   VALUE \"ProductName\", \"DiskTools\"\n\
           VALUE \"FileVersion\", \"{version}\"\n   VALUE \"ProductVersion\", \"{version}\"\n  }}\n }}\n\
          BLOCK \"VarFileInfo\"\n {{\n  VALUE \"Translation\", 0x413, 1200\n }}\n}}\n"
    );
    if release {
        std::fs::write(out.join("app.manifest"), MANIFEST).map_err(|e| e.to_string())?;
        // 24 = RT_MANIFEST. With the GNU toolchain ld warns "multiple
        // non-default manifests" (MinGW adds one of its own); this one is the
        // one Windows uses, which `disktools.exe` refusing to start without
        // elevation (error 740) confirms.
        rc.push_str("1 24 \"app.manifest\"\n");
        if !gnu {
            // The MSVC linker would otherwise add a manifest of its own.
            println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
        }
    }
    std::fs::write(out.join("app.rc"), rc).map_err(|e| e.to_string())?;

    let (tool, args, object): (PathBuf, &[&str], &str) = if gnu {
        ("windres".into(), &["--input", "app.rc", "--output", "app.o", "--output-format=coff"], "app.o")
    } else {
        (newest_rc().ok_or("rc.exe niet gevonden")?, &["/nologo", "/fo", "app.res", "app.rc"], "app.res")
    };
    let status = Command::new(&tool).args(args).current_dir(&out).status().map_err(|e| format!("{}: {e}", tool.display()))?;
    if !status.success() {
        return Err(format!("{} gaf {status}", tool.display()));
    }
    println!("cargo:rustc-link-arg-bins={}", out.join(object).display());
    Ok(())
}

/// `rc.exe` from the newest Windows SDK that has one.
fn newest_rc() -> Option<PathBuf> {
    let program_files = std::env::var_os("ProgramFiles(x86)")?;
    let bin = Path::new(&program_files).join("Windows Kits").join("10").join("bin");
    let mut found: Vec<PathBuf> = std::fs::read_dir(bin)
        .ok()?
        .flatten()
        .map(|e| e.path().join("x64").join("rc.exe"))
        .filter(|rc| rc.is_file())
        .collect();
    // Folder names are versions like 10.0.26100.0; compare them as numbers.
    found.sort_by_key(|rc| {
        rc.ancestors()
            .nth(2)
            .and_then(Path::file_name)
            .map(|v| v.to_string_lossy().split('.').map(|n| n.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>())
    });
    found.pop()
}
