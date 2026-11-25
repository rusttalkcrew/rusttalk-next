//! Embeds the app icon in the Windows executable, and keeps WebRTC's Objective-C
//! categories when linking on macOS and iOS.

fn main() {
    println!("cargo:rerun-if-changed=rusttalk.rc");
    println!("cargo:rerun-if-changed=assets/rusttalk.ico");
    // WebRTC adds methods such as `+[NSString stringForStdString:]` in Objective-C
    // categories, which the linker drops from a static library without `-ObjC`. The
    // flag webrtc-sys prints applies only to its own targets, so the binary sets it.
    if matches!(
        std::env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("macos" | "ios")
    ) {
        println!("cargo:rustc-link-arg=-ObjC");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        let camera = "src/camera/mobile/ios.m";
        println!("cargo:rerun-if-changed={camera}");
        cc::Build::new()
            .file(camera)
            .flag("-fobjc-arc")
            .compile("call_camera");
        for framework in ["AVFoundation", "CallKit", "CoreMedia", "CoreVideo", "UIKit"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }
    // Windows release builds run on a Windows host; other hosts skip the icon.
    #[cfg(windows)]
    embed_resource::compile("rusttalk.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("embed the app icon");
}
