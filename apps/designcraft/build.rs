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
    let icon_path = std::env::var_os("OUT_DIR")
        .map(std::path::PathBuf::from)
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
        if std::env::var_os("DESIGNCRAFT_REQUIRE_WINRES").is_some() {
            eprintln!("embedding Windows resources failed: {e}");
            std::process::exit(1);
        }
        println!("cargo:warning=designcraft.exe built without icon/version resources: {e}");
    }
}
