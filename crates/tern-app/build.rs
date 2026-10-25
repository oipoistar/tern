//! On Windows, gives tern.exe Tern's icon (resource 1, which GPUI also puts
//! on the window and the taskbar) and the name Task Manager shows, and has
//! it find the libraries it imports in System32 only.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=resources/tern.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let var = |name: &str| std::env::var(name).expect("set by cargo");
    // Windows looks for a program's libraries in its own folder first, so
    // tern.exe run from Downloads would load a `dwmapi.dll` or `d3d11.dll`
    // waiting there. 0x800 is LOAD_LIBRARY_SEARCH_SYSTEM32; `main` does the
    // same for libraries loaded later.
    if var("CARGO_CFG_TARGET_ENV") == "msvc" {
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }
    let icon = PathBuf::from(var("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("tern.ico");
    let version = var("CARGO_PKG_VERSION");
    let mut numbers = version
        .split(['.', '-', '+'])
        .map(|part| part.parse::<u16>().unwrap_or(0));
    let mut next = || numbers.next().unwrap_or(0);
    let (major, minor, patch) = (next(), next(), next());
    let rc = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Tern"
      VALUE "ProductName", "Tern"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "tern.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = icon.display().to_string().replace('\\', "\\\\"),
    );
    let rc_file = PathBuf::from(var("OUT_DIR")).join("tern.rc");
    std::fs::write(&rc_file, rc).expect("the resource script");
    embed_resource::compile(&rc_file, embed_resource::NONE)
        .manifest_optional()
        .expect("tern.exe's icon and version");
}
