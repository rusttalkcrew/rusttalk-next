//! Embeds the app icon in the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=rusttalk.rc");
    println!("cargo:rerun-if-changed=assets/rusttalk.ico");
    // Windows release builds run on a Windows host; other hosts skip the icon.
    #[cfg(windows)]
    embed_resource::compile("rusttalk.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("embed the app icon");
}
