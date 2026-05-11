//! Generates `assets/dubsync.ico` from the master `assets/logo.png` at build
//! time and embeds it into both binaries via winresource. The .ico packs every
//! size Windows shell consumes (16/20/24/32/40/48/64/96/128/256) so taskbar,
//! Alt+Tab, Start tile and Explorer each pick a crisp variant.
//!
//! No-op on non-Windows targets. If `assets/logo.png` is missing, the build
//! still succeeds and binaries get the default Windows icon.

#[cfg(target_os = "windows")]
fn main() {
    use std::fs::File;
    use std::path::Path;

    let logo_path = Path::new("assets/logo.png");
    println!("cargo:rerun-if-changed=assets/logo.png");

    if !logo_path.exists() {
        return;
    }

    let master = match image::open(logo_path) {
        Ok(img) => img.to_rgba8(),
        Err(e) => {
            println!("cargo:warning=failed to decode assets/logo.png: {e}");
            return;
        }
    };

    let ico_path = Path::new("assets/dubsync.ico");
    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
    for &size in &[16u32, 20, 24, 32, 40, 48, 64, 96, 128, 256] {
        let resized =
            image::imageops::resize(&master, size, size, image::imageops::FilterType::Lanczos3);
        let icon_image = ico::IconImage::from_rgba_data(size, size, resized.into_raw());
        match ico::IconDirEntry::encode(&icon_image) {
            Ok(entry) => icon_dir.add_entry(entry),
            Err(e) => {
                println!("cargo:warning=failed to encode {size}px .ico entry: {e}");
            }
        }
    }

    let ico_file = match File::create(ico_path) {
        Ok(f) => f,
        Err(e) => {
            println!("cargo:warning=failed to create {}: {e}", ico_path.display());
            return;
        }
    };
    if let Err(e) = icon_dir.write(ico_file) {
        println!("cargo:warning=failed to write {}: {e}", ico_path.display());
        return;
    }

    // winresource applies to every binary in the crate; both the GUI and CLI
    // get the icon. The CLI's terminal usage doesn't show it anyway, and
    // the installer/shortcut icons all pull from the GUI exe.
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().expect("ico path is valid UTF-8"));
    if let Err(e) = res.compile() {
        println!("cargo:warning=winresource compile failed: {e}");
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {}
