//! Windows only: embed the app icon and version info (VERSIONINFO) into `designcraft.exe`, so it
//! shows in Explorer, the taskbar, the Start menu and Alt-Tab.
//!
//! On every other target this does nothing. A missing resource compiler is a warning, so a
//! cross-compile from macOS or Linux still links, unless `DESIGNCRAFT_REQUIRE_WINRES=1` turns it
//! into an error (for release builds).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/app-icon/designcraft.ico");
    println!("cargo:rerun-if-env-changed=DESIGNCRAFT_REQUIRE_WINRES");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let src_icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/app-icon/designcraft.ico");
    let out_dir = std::env::var_os("OUT_DIR").map(std::path::PathBuf::from);
    let icon_path = out_dir
        .as_ref()
        .and_then(|out| {
            let dst = out.join("designcraft.ico");
            std::fs::copy(&src_icon, &dst).ok().map(|_| dst)
        })
        .unwrap_or(src_icon);
    let icon_str = icon_path.to_string_lossy().replace('\\', "/");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(&icon_str)
        .set("ProductName", "Smart Design")
        .set("FileDescription", "Smart Design page layout and AI production")
        .set("CompanyName", "Smart Design contributors")
        .set("LegalCopyright", "Copyright (c) the DesignCraft authors. MIT OR Apache-2.0.")
        .set("OriginalFilename", "designcraft.exe")
        .set("InternalName", "designcraft");
    if let Err(e) = res.compile() {
        let gnu_ok =
            std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") && out_dir.as_deref().is_some_and(compile_gnu_rc_without_manifest_include);
        if !gnu_ok {
            if std::env::var_os("DESIGNCRAFT_REQUIRE_WINRES").is_some() {
                eprintln!("embedding Windows resources failed: {e}");
                std::process::exit(1);
            }
            println!("cargo:warning=designcraft.exe built without icon/version resources: {e}");
        }
    }
}

fn compile_gnu_rc_without_manifest_include(out_dir: &std::path::Path) -> bool {
    let rc_path = out_dir.join("resource.rc");
    if !rc_path.is_file() {
        return false;
    }
    let obj_path = out_dir.join("designcraft_res.o");
    for tool in ["x86_64-w64-mingw32-windres", "windres"] {
        let status = std::process::Command::new(tool)
            .current_dir(out_dir)
            .args(["--use-temp-file", "--input", "resource.rc", "--output", "designcraft_res.o"])
            .status();
        if status.is_ok_and(|s| s.success()) && obj_path.is_file() {
            println!("cargo:rustc-link-arg-bins={}", obj_path.display());
            return true;
        }
    }
    false
}
