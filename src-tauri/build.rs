fn main() {
    // ScreenCaptureKit's Swift bridge links the concurrency runtime by rpath.
    // Cargo does not propagate a dependency's linker rpath to this crate's
    // unit-test harness, so add the system Swift runtime location explicitly.
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    tauri_build::build()
}
