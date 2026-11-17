//! On Windows, `agentloft.exe` carries the app icon as resource 1: Explorer
//! and the taskbar show it, and GPUI gives it to every window it makes.

fn main() {
    let icon =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/icon/agentloft.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // Written with the icon's full path: the resource compiler runs on a
    // preprocessed copy elsewhere, where a relative path finds nothing.
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let rc = out.join("agentloft.rc");
    let path = icon.to_string_lossy().replace('\\', "\\\\");
    std::fs::write(&rc, format!("1 ICON \"{path}\"\n")).expect("write agentloft.rc");
    embed_resource::compile_for(&rc, ["agentloft"], embed_resource::NONE)
        .manifest_optional()
        .expect("compile agentloft.rc");
}
