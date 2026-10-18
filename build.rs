// Embeds the app icon in the Windows executable, so Explorer and the taskbar show it.
fn main() {
    println!("cargo:rerun-if-changed=assets/logos/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/logos/icon.ico")
            .compile()
            .expect("failed to embed the app icon");
    }
}
