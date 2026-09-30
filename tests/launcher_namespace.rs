// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn executable(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn local_install_and_launch_coexist_with_official_commands() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    let official = temp.path().join("official-home");
    let fork_home = temp.path().join("fork-home");
    let bundle = temp.path().join("bundle");
    let toolchain = temp.path().join("toolchain");
    for dir in [&bin, &official, &bundle.join("bin"), &toolchain.join("bin")] {
        fs::create_dir_all(dir).unwrap();
    }
    let official_script = "#!/bin/sh\nprintf 'official-kani\\n'\n";
    for name in ["kani", "cargo-kani"] {
        executable(&bin.join(name), official_script);
    }
    fs::write(official.join("installation"), "official installation").unwrap();
    fs::copy(env!("CARGO_BIN_EXE_cargo-varies-kani"), bin.join("cargo-varies-kani")).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_varies-kani"), bin.join("varies-kani")).unwrap();

    fs::write(bundle.join("varies-kani-version"), env!("CARGO_PKG_VERSION")).unwrap();
    fs::write(bundle.join("rust-toolchain-version"), "namespace-test").unwrap();
    fs::write(bundle.join("rustc-version"), "rustc namespace-test").unwrap();
    executable(&toolchain.join("bin/rustc"), "#!/bin/sh\nprintf 'rustc namespace-test\\n'\n");
    executable(
        &bundle.join("bin/kani-driver"),
        "#!/bin/sh\nprintf 'fork-driver\\n'; printf '%s\\n' \"$@\"\n",
    );
    let archive = temp.path().join("bundle.tar.gz");
    success(
        Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(temp.path())
            .arg("bundle")
            .output()
            .unwrap(),
    );
    let path = std::env::join_paths(
        std::iter::once(bin.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let command = || {
        let mut cmd = Command::new("cargo");
        cmd.env("PATH", &path)
            .env("KANI_HOME", &official)
            .env("VARIES_KANI_HOME", &fork_home)
            .current_dir(temp.path());
        cmd
    };
    success(
        command()
            .args(["varies-kani", "setup", "--use-local-bundle"])
            .arg(&archive)
            .arg("--use-local-toolchain")
            .arg(&toolchain)
            .output()
            .unwrap(),
    );
    assert!(
        fork_home.join(format!("varies-kani-{}/toolchain", env!("CARGO_PKG_VERSION"))).exists()
    );
    let output = success(command().args(["varies-kani", "--version"]).output().unwrap());
    assert_eq!(output, "fork-driver\nvaries-kani\n--version\n");
    let output = success(
        Command::new(bin.join("varies-kani"))
            .arg("input.rs")
            .env("VARIES_KANI_HOME", &fork_home)
            .output()
            .unwrap(),
    );
    assert_eq!(output, "fork-driver\ninput.rs\n");
    assert_eq!(success(command().args(["kani", "--version"]).output().unwrap()), "official-kani\n");
    for name in ["kani", "cargo-kani"] {
        assert_eq!(fs::read_to_string(bin.join(name)).unwrap(), official_script);
    }
    assert_eq!(fs::read_to_string(official.join("installation")).unwrap(), "official installation");
}
