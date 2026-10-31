// Windows: embed the app icon (rendered from src/icon.rs) and a manifest declaring per-monitor
// DPI awareness, so the window and tray look sharp on scaled displays.

#[allow(dead_code)]
mod icon {
    include!("src/icon.rs");
}

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 256] {
        let image = ico::IconImage::from_rgba_data(size, size, icon::icon_rgba(size));
        dir.add_entry(ico::IconDirEntry::encode(&image).expect("encode icon"));
    }
    let path = out.join("soundsync.ico");
    dir.write(std::fs::File::create(&path).expect("create icon")).expect("write icon");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(path.to_str().expect("utf-8 path"));
        res.set_manifest(MANIFEST);
        res.set("FileDescription", "SoundSync");
        res.set("ProductName", "SoundSync");
        res.compile().expect("compile Windows resources");
    }
}
