// Embeds the application manifest with the MSVC linker (no resource
// compiler needed).
fn main() {
    let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("kibotos.manifest");
    println!("cargo:rerun-if-changed=kibotos.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
        // These DLLs are only needed after the window is visible (icons,
        // dialogs, COM). Delay-loading them shortens process start.
        for dll in ["shell32.dll", "comdlg32.dll", "ole32.dll", "oleaut32.dll", "imm32.dll"] {
            println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}
