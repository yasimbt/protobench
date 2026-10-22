//! Embeds the app icon in the Windows executable (Explorer, taskbar, shortcuts).

fn main() {
    let ico = "../../assets/icon/protobench.ico";
    println!("cargo:rerun-if-changed={ico}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico);
    // A missing resource compiler costs only the .exe icon, not the build.
    if let Err(e) = res.compile() {
        println!("cargo:warning=no icon embedded in the .exe: {e}");
    }
}
