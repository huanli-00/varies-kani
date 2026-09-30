// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! First-time setup for `cargo varies-kani setup`.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cmd::AutoRun;
use crate::os_hacks;

/// Comes from our Cargo.toml manifest file. Must correspond to our release verion.
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Set by our `build.rs`, reflects the Rust target triple we're building for
const TARGET: &str = env!("TARGET");

/// Varies Kani uses its own installation root and ignores the upstream KANI_HOME.
/// Defaults to `${HOME}/.varies-kani/varies-kani-<VERSION>`.
pub fn kani_dir() -> Result<PathBuf> {
    let kani_dir = match env::var("VARIES_KANI_HOME") {
        Ok(val) => custom_kani_dir(val),
        Err(_) => default_kani_dir()?,
    };
    let kani_dir = installation_dir(&kani_dir);
    Ok(kani_dir)
}

fn installation_dir(base: &Path) -> PathBuf {
    base.join(format!("varies-kani-{VERSION}"))
}

/// Returns the custom Varies Kani home directory: `${VARIES_KANI_HOME}`
fn custom_kani_dir(path: String) -> PathBuf {
    // We don't check if it doesn't exist since we create it later
    PathBuf::from(path)
}

/// Returns the default Varies Kani home directory: `${HOME}/.varies-kani`
fn default_kani_dir() -> Result<PathBuf> {
    let home_dir = home::home_dir().expect("couldn't find home directory");
    if !home_dir.is_dir() {
        bail!("got home directory `{}` which isn't a directory", home_dir.display());
    }
    let kani_dir = home_dir.join(".varies-kani");
    Ok(kani_dir)
}

/// Fast check to see if we look setup already
pub fn appears_setup() -> bool {
    validate_bundle(&kani_dir().expect("couldn't find kani directory")).is_ok()
}

// Ensure that the tar file does not exist, essentially using its presence
// to detect setup completion as if it were a lock file.
pub fn appears_incomplete() -> Option<PathBuf> {
    let kani_dir = kani_dir().expect("couldn't find kani directory");
    let kani_dir_parent = kani_dir.parent().unwrap();

    for entry in std::fs::read_dir(kani_dir_parent).ok()?.flatten() {
        // Don't collapse if as let_chains have only been stabilized in 1.88, which the target host
        // installing Kani need not have just yet.
        #[allow(clippy::collapsible_if)]
        if let Some(file_name) = entry.file_name().to_str() {
            if file_name.ends_with(".tar.gz") {
                return Some(kani_dir_parent.join(file_name));
            }
        }
    }
    None
}

/// Sets up Varies Kani in its separate installation directory.
pub fn setup(
    use_local_bundle: Option<OsString>,
    use_local_toolchain: Option<OsString>,
) -> Result<()> {
    let kani_dir = kani_dir()?;
    let os = os_info::get();

    println!("[0/5] Running Varies Kani first-time setup...");

    println!("[1/5] Ensuring the existence of: {}", kani_dir.display());
    std::fs::create_dir_all(&kani_dir)?;

    setup_kani_bundle(&kani_dir, use_local_bundle)?;

    validate_bundle(&kani_dir)?;

    setup_rust_toolchain(&kani_dir, use_local_toolchain)?;

    os_hacks::setup_os_hacks(&kani_dir, &os)?;

    println!("[5/5] Successfully completed Varies Kani first-time setup.");

    Ok(())
}

/// Download and unpack the Kani release bundle
fn setup_kani_bundle(kani_dir: &Path, use_local_bundle: Option<OsString>) -> Result<()> {
    // e.g. `~/.varies-kani/`
    let base_dir = kani_dir.parent().expect("No base directory?");

    if let Some(pathstr) = use_local_bundle {
        println!("[2/5] Installing local Varies Kani bundle: {}", pathstr.to_string_lossy());
        let path = Path::new(&pathstr).canonicalize()?;
        // When given a local bundle, it's often "-latest" but we expect "-1.0" or something.
        // tar supports "stripping" the first directory from the bundle, so do that and
        // extract it directly into the expected (kani_dir) directory (instead of base_dir).
        Command::new("tar")
            .arg("--strip-components=1")
            .arg("-zxf")
            .arg(&path)
            .current_dir(kani_dir)
            .run()
            .context("Failed to extract Varies Kani bundle")?;
    } else {
        let filename = download_filename();
        println!("[2/5] Downloading Varies Kani release bundle: {}", &filename);
        fail_if_unsupported_target()?;
        let bundle = base_dir.join(filename);
        Command::new("curl")
            .args(["-sSLf", "-o"])
            .arg(&bundle)
            .arg(download_url())
            .run()
            .context("Failed to download Kani release bundle")?;

        Command::new("tar")
            .args(["--strip-components=1", "-zxf"])
            .arg(&bundle)
            .current_dir(kani_dir)
            .run()?;

        std::fs::remove_file(bundle)?;
    }
    Ok(())
}

/// The bundle marker prevents an upstream Kani bundle from being used as this fork.
fn validate_bundle(dir: &Path) -> Result<()> {
    let version = std::fs::read_to_string(dir.join("varies-kani-version"))
        .context("Expected a Varies Kani bundle containing varies-kani-version")?;
    if version.trim() != VERSION {
        bail!("Varies Kani bundle version {} does not match launcher {VERSION}", version.trim());
    }
    Ok(())
}

/// Reads the Rust toolchain version that Kani was built against from the file in
/// the Kani release bundle (unpacked in `kani_dir`).
pub(crate) fn get_rust_toolchain_version(kani_dir: &Path) -> Result<String> {
    std::fs::read_to_string(kani_dir.join("rust-toolchain-version"))
        .context("Reading release bundle rust-toolchain-version")
}

pub(crate) fn get_rustc_version_from_build(kani_dir: &Path) -> Result<String> {
    std::fs::read_to_string(kani_dir.join("rustc-version"))
        .context("Reading release bundle rustc-version")
}

/// Install the Rust toolchain version we require
fn setup_rust_toolchain(kani_dir: &Path, use_local_toolchain: Option<OsString>) -> Result<String> {
    // Currently this means we require the bundle to have been unpacked first!
    let toolchain_version = get_rust_toolchain_version(kani_dir)?;
    let rustc_version = get_rustc_version_from_build(kani_dir)?.trim().to_string();

    // Symlink to a local toolchain if the user explicitly requests
    if let Some(local_toolchain_path) = use_local_toolchain {
        let toolchain_path = Path::new(&local_toolchain_path);

        let custom_toolchain_rustc_version =
            get_rustc_version_from_local_toolchain(local_toolchain_path.clone())?;

        if rustc_version == custom_toolchain_rustc_version {
            symlink_rust_toolchain(toolchain_path, kani_dir)?;
            println!(
                "[3/5] Installing rust toolchain from path provided: {}",
                &toolchain_path.to_string_lossy()
            );
            return Ok(toolchain_version);
        } else {
            bail!(
                "The toolchain with rustc {:?} being used to setup is not the same as the one Kani used in its release bundle {:?}. Try to setup with the same version as the bundle.",
                custom_toolchain_rustc_version,
                rustc_version,
            );
        }
    }

    // This is the default behaviour when no explicit path to a toolchain is mentioned
    println!("[3/5] Installing rust toolchain version: {}", &toolchain_version);
    Command::new("rustup").args(["toolchain", "install", &toolchain_version]).run()?;
    let toolchain = home::rustup_home()?.join("toolchains").join(&toolchain_version);
    symlink_rust_toolchain(&toolchain, kani_dir)?;
    Ok(toolchain_version)
}

// This ends the setup steps above.
//
// Just putting a bit of space between that and the helper functions below.

/// The filename of the release bundle
fn download_filename() -> String {
    format!("varies-kani-{VERSION}-{TARGET}.tar.gz")
}

/// Get the version of rustc that is being used to setup kani by the user
fn get_rustc_version_from_local_toolchain(path: OsString) -> Result<String> {
    let path = Path::new(&path);
    let rustc_path = path.join("bin").join("rustc");

    let output = Command::new(rustc_path).arg("--version").output();

    match output {
        Ok(output) => {
            if output.status.success() {
                Ok(String::from_utf8(output.stdout).map(|s| s.trim().to_string())?)
            } else {
                bail!(
                    "Could not parse rustc version string. Toolchain installation likely invalid. "
                );
            }
        }
        Err(_) => bail!("Could not get rustc version. Toolchain installation likely invalid"),
    }
}

/// The download URL for this version of Kani
fn download_url() -> String {
    let tag: &str = &format!("varies-kani-{VERSION}");
    let file: &str = &download_filename();
    format!("https://github.com/huanli-00/varies-kani/releases/download/{tag}/{file}")
}

/// Give users a better error message than "404" if we're on an unsupported platform.
/// This is called just before we try to download the release bundle.
fn fail_if_unsupported_target() -> Result<()> {
    // This is basically going to be reduced to a compile-time constant
    match TARGET {
        "x86_64-unknown-linux-gnu"
        | "x86_64-apple-darwin"
        | "aarch64-unknown-linux-gnu"
        | "aarch64-apple-darwin" => Ok(()),
        _ => bail!("Kani does not support this platform (Rust target {})", TARGET),
    }
}

/// Creates a `kani_dir/toolchain` symlink pointing to `toolchain`.
fn symlink_rust_toolchain(toolchain: &Path, kani_dir: &Path) -> Result<()> {
    let path = kani_dir.join("toolchain");
    // We want setup to be idempotent, so if the symlink already exists, delete instead of failing
    if path.exists() && path.is_symlink() {
        std::fs::remove_file(&path)?;
    }
    std::os::unix::fs::symlink(toolchain, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installation_and_download_are_namespaced() {
        assert_eq!(
            installation_dir(Path::new("/tmp/shared")),
            Path::new("/tmp/shared").join(format!("varies-kani-{VERSION}"))
        );
        assert_eq!(
            download_url(),
            format!(
                "https://github.com/huanli-00/varies-kani/releases/download/varies-kani-{VERSION}/varies-kani-{VERSION}-{TARGET}.tar.gz"
            )
        );
    }

    #[test]
    fn upstream_and_mismatched_bundles_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(validate_bundle(dir.path()).is_err());
        std::fs::write(dir.path().join("varies-kani-version"), "different-version").unwrap();
        assert!(validate_bundle(dir.path()).is_err());
        std::fs::write(dir.path().join("varies-kani-version"), VERSION).unwrap();
        assert!(validate_bundle(dir.path()).is_ok());
    }
}
