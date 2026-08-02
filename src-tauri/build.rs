fn main() {
    // WKWebView's getUserMedia can only read the mic once the HOST app holds a
    // TCC microphone grant. wry auto-grants at the WebKit layer but never
    // triggers the OS prompt, so we request access natively via AVFoundation
    // (see request_microphone_access in lib.rs) — which needs this framework.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=AVFoundation");
    }
    tauri_build::build()
}
