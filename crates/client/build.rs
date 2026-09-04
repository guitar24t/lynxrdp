//! Stamps the Windows executable with its icon and version information, and
//! records which release this build belongs to.

fn main() {
    // Icons are committed, so a change to the SVG only matters once
    // `assets/generate-icons.sh` has been run.
    println!("cargo:rerun-if-changed=../../assets/lynxrdp.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // The release tag this build is part of, which only the release workflow
    // knows. It cannot be derived from the Cargo version: that stays at
    // 0.1.0 across every candidate, so a client built at v0.1.0-rc.5 would
    // read its own version as 0.1.0 and conclude that v0.1.0-rc.6 was a
    // downgrade. An empty value is the honest answer for every other build,
    // and `update::current_tag` turns it into "not a release build" rather
    // than guessing -- which is also what stops a working copy from
    // replacing itself with a download.
    println!("cargo:rerun-if-env-changed=LYNXRDP_RELEASE_TAG");
    let tag = std::env::var("LYNXRDP_RELEASE_TAG").unwrap_or_default();
    println!("cargo:rustc-env=LYNXRDP_RELEASE_TAG={}", tag.trim());

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
