// Embeds the app icon in kerf.exe, so Explorer, the taskbar and shortcuts show it.
fn main() {
    println!("cargo::rerun-if-changed=../../packaging/icons/kerf.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../packaging/icons/kerf.ico");
        res.compile().expect("embedding the Windows icon");
    }
}
