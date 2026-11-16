use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn read_mod_version() -> String {
    let ui_rs = Path::new("../coop-dll/src/ui.rs");
    println!("cargo:rerun-if-changed={}", ui_rs.display());
    let Ok(src) = fs::read_to_string(ui_rs) else {
        return "0.0.1".into();
    };
    for line in src.lines() {
        if let Some(rest) = line.trim().strip_prefix("pub const MOD_VERSION: &str =")
            && let Some(start) = rest.find('"')
            && let Some(end) = rest[start + 1..].find('"')
        {
            return rest[start + 1..start + 1 + end].to_string();
        }
    }
    "0.0.1".into()
}

fn parse_win_version(ver: &str) -> (u16, u16, u16, u16) {
    let core = ver.split('-').next().unwrap_or(ver);
    let mut parts = core.split('.').map(|s| s.parse::<u16>().unwrap_or(0));
    (
        parts.next().unwrap_or(1),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

fn find_rc_exe() -> Option<PathBuf> {
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return Some(PathBuf::from("rc.exe"));
    }
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let entries = fs::read_dir(kits).ok()?;
    let mut candidates: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path().join("x64").join("rc.exe"))
        .filter(|p| p.is_file())
        .collect();
    candidates.sort();
    candidates.pop()
}

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let version = read_mod_version();
    let (maj, min, pat, bld) = parse_win_version(&version);
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let rc_path = out_dir.join("launcher.rc");
    let res_path = out_dir.join("launcher.res");

    let rc = format!(
        r#"#pragma code_page(65001)
1 VERSIONINFO
FILEVERSION {maj},{min},{pat},{bld}
PRODUCTVERSION {maj},{min},{pat},{bld}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "xCløuD (based on Tihvert)"
      VALUE "FileDescription", "CooP Sekiro Launcher"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "coop_sekiro_launcher"
      VALUE "LegalCopyright", "MIT, Copyright (c) 2026 xCløuD & Tihvert"
      VALUE "OriginalFilename", "coop_sekiro_launcher.exe"
      VALUE "ProductName", "CooP Sekiro"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );

    if fs::write(&rc_path, rc).is_err() {
        return;
    }
    let Some(rc_exe) = find_rc_exe() else {
        println!("cargo:warning=rc.exe not found; skipping launcher VERSIONINFO");
        return;
    };
    let ok = Command::new(rc_exe)
        .arg("/nologo")
        .arg("/c65001")
        .arg("/fo")
        .arg(&res_path)
        .arg(&rc_path)
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        println!(
            "cargo:rustc-link-arg-bin=coop_sekiro_launcher={}",
            res_path.display()
        );
    }
}
