// Release builds are GUI-only (no console window); debug builds keep the console for panics.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
mod index;
mod indexer;
mod mft;
mod preview;
mod search;
mod util;
mod walk;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FileCraft")
            .with_inner_size([1200.0, 760.0])
            .with_min_inner_size([640.0, 400.0])
            .with_icon(app_icon()),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native(
        "FileCraft",
        options,
        Box::new(|cc| {
            #[cfg(windows)]
            set_taskbar_icon(cc);
            Ok(Box::new(app::FileCraftApp::new(cc)))
        }),
    )
}

/// The window and taskbar icon, decoded from the PNG baked into the binary.
fn app_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/logos/icon.png"))
        .expect("bundled icon is a valid PNG")
        .into_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData { rgba: img.into_raw(), width, height }
}

/// egui only sets the small (title bar) icon; the taskbar shows the big one, so load that from the
/// icon resource `build.rs` embeds in the exe.
#[cfg(windows)]
fn set_taskbar_icon(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, ICON_BIG, IMAGE_ICON, LoadImageW, SM_CXICON, SM_CYICON, SendMessageW, WM_SETICON,
    };

    let Ok(handle) = cc.window_handle() else { return };
    let RawWindowHandle::Win32(win) = handle.as_raw() else { return };
    // SAFETY: plain Win32 calls on our own window; resource id 1 is the app icon (winresource's default).
    unsafe {
        let size = (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON));
        let icon = LoadImageW(GetModuleHandleW(std::ptr::null()), 1 as _, IMAGE_ICON, size.0, size.1, 0);
        if !icon.is_null() {
            SendMessageW(win.hwnd.get() as _, WM_SETICON, ICON_BIG as usize, icon as isize);
        }
    }
}
