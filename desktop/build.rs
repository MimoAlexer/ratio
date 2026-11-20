fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var_os("CARGO_CFG_TARGET_OS").as_deref() == Some(std::ffi::OsStr::new("windows")) {
        let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon.ico");
        tauri_winres::WindowsResource::new()
            .set_icon(icon.to_str().expect("UTF-8 icon path"))
            .set("ProductName", "Ratio")
            .set("FileDescription", "Ratio desktop")
            .set("OriginalFilename", "ratio.exe")
            .compile_for(&["ratio"])
            .expect("embed Windows icon and version information");
    }
}
