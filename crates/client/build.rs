//! Stamps the Windows executable with its icon and version information.
//!
//! Nothing else needs a build script: on every other target this is a no-op.

fn main() {
    // Icons are committed, so a change to the SVG only matters once
    // `assets/generate-icons.sh` has been run.
    println!("cargo:rerun-if-changed=../../assets/lynxrdp.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // `cfg!(windows)` here would describe the machine doing the building.
    // What matters is what we are building for.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("../../assets/lynxrdp.ico");
        resource.set("ProductName", "LynxRDP");
        resource.set("FileDescription", "LynxRDP remote desktop client");
        resource.set("LegalCopyright", "MIT licensed");
        // A missing rc.exe should not stop the build; the result is an
        // executable with the default icon, which still runs.
        if let Err(e) = resource.compile() {
            println!("cargo:warning=could not embed the Windows icon: {e}");
        }
    }
}
