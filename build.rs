use winres::{VersionInfo, WindowsResource};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icons/icon.ico");

    // Build scripts run on the host. `winres` needs a Windows resource compiler,
    // which is only available on Windows hosts. Skipping it elsewhere lets
    // `cargo check --target x86_64-pc-windows-msvc` (and clippy) run from Linux/macOS
    // CI without a Windows machine. Release binaries are always built on Windows
    // (see .github/workflows/release.yml), so shipped executables keep their icon
    // and version metadata.
    if !cfg!(windows) {
        println!(
            "cargo:warning=winres skipped on a non-Windows host; no icon/version resource embedded"
        );
        return;
    }

    let version = env!("CARGO_PKG_VERSION");

    // Embed the icon and richer PE version metadata into the executable.
    let mut res = WindowsResource::new();
    let numeric_version = pack_version(version);

    res.set_icon("src/icons/icon.ico")
        .set("FileVersion", version)
        .set("ProductVersion", version)
        .set_version_info(VersionInfo::FILEVERSION, numeric_version)
        .set_version_info(VersionInfo::PRODUCTVERSION, numeric_version);

    res.compile().expect("Failed to compile Windows resources");
}

fn pack_version(version: &str) -> u64 {
    let core = version.split('-').next().unwrap_or(version);
    let mut parts = core.split('.').map(|part| part.parse::<u64>().unwrap_or(0));

    let major = parts.next().unwrap_or(0).min(u16::MAX as u64);
    let minor = parts.next().unwrap_or(0).min(u16::MAX as u64);
    let patch = parts.next().unwrap_or(0).min(u16::MAX as u64);

    (major << 48) | (minor << 32) | (patch << 16)
}
