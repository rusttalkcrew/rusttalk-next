//! Keeps WebRTC's Objective-C categories when linking on macOS (see the client's build.rs).

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-ObjC");
    }
}
