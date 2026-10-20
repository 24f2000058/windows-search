//! Embeds the application manifest (common controls v6, per-monitor DPI,
//! long paths, asInvoker) and version information into the Windows
//! executable. MinGW builds use `windres`; on other toolchains a warning
//! is printed and the executable is built without them.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=res/flint.manifest");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("res").join("flint.manifest");
    let v = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<u32> = v.split(['.', '-']).filter_map(|p| p.parse().ok()).collect();
    parts.resize(4, 0);
    let comma = parts.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let rc = format!(
        r#"#include <winver.h>
1 24 "{manifest}"
VS_VERSION_INFO VERSIONINFO
FILEVERSION {comma}
PRODUCTVERSION {comma}
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Flint contributors"
      VALUE "FileDescription", "Flint launcher"
      VALUE "FileVersion", "{v}"
      VALUE "InternalName", "flint"
      VALUE "LegalCopyright", "MIT OR Apache-2.0"
      VALUE "OriginalFilename", "flint.exe"
      VALUE "ProductName", "Flint"
      VALUE "ProductVersion", "{v}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        manifest = manifest.display().to_string().replace('\\', "/")
    );
    let rc_path = out.join("flint.rc");
    std::fs::write(&rc_path, rc).unwrap();
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("gnu") {
        println!("cargo:warning=manifest/version resources are embedded only for MinGW builds");
        return;
    }
    let windres = std::env::var("WINDRES").unwrap_or_else(|_| "x86_64-w64-mingw32-windres".into());
    let obj = out.join("flint-res.o");
    match Command::new(&windres).arg(&rc_path).arg("-O").arg("coff").arg("-o").arg(&obj).status() {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", obj.display()),
        r => println!("cargo:warning=windres failed ({r:?}); built without manifest/version resources"),
    }
}
