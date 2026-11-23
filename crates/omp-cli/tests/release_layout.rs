//! The release tarball's layout, end to end: build a tarball with
//! `scripts/package-release.sh`, unpack it outside the repository, and run
//! `omt --extension` from it. The binary has to find `ext-host/host.ts` through
//! its own location alone (no `OMP_RS_EXT_HOST`, no checkout above it), which is
//! what a release asset and an `install.sh` prefix both rely on.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn bun_on_path() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join("bun");
            candidate.is_file()
        })
    })
}

/// `scripts/package-release.sh` expects a built binary; the test binary's own
/// path is not `omt`, so an executable copy stands in for it.
fn fake_binary(dir: &Path) -> PathBuf {
    let bin = dir.join("omt");
    std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

fn temp_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let agent = home.path().join(".omp/agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("models.yml"),
        "providers:\n  mock:\n    baseUrl: http://127.0.0.1:9/v1\n    apiKey: unused\n    api: openai-completions\n    models:\n      - id: m\n        name: M\n        contextWindow: 8000\n        maxTokens: 100\n",
    )
    .unwrap();
    std::fs::write(
        agent.join("config.yml"),
        "startup:\n  setupWizard: false\n  checkUpdate: false\n  changelogMode: hidden\n",
    )
    .unwrap();
    home
}

#[test]
fn release_tarball_runs_extensions_outside_the_checkout() {
    if !bun_on_path() {
        eprintln!("skipping release tarball test: bun not found on PATH");
        return;
    }
    let root = repo_root();
    let work = tempfile::tempdir().unwrap();
    let tarball = work.path().join("omt-test.tar.gz");
    let bin = fake_binary(work.path());
    let package = Command::new("sh")
        .arg(root.join("scripts/package-release.sh"))
        .arg(&bin)
        .arg(&tarball)
        .arg("omt")
        .arg(&root)
        .output()
        .expect("run package-release.sh");
    assert!(
        package.status.success(),
        "package-release.sh failed: {}",
        String::from_utf8_lossy(&package.stderr)
    );

    let listing = Command::new("tar")
        .arg("-tzf")
        .arg(&tarball)
        .output()
        .expect("list the tarball");
    let entries = String::from_utf8_lossy(&listing.stdout);
    for required in ["bin/omt", "ext-host/host.ts", "LICENSE", "NOTICE.md"] {
        assert!(
            entries
                .lines()
                .any(|line| line.trim_end_matches('/') == required),
            "tarball is missing {required}: {entries}"
        );
    }

    let prefix = work.path().join("prefix");
    std::fs::create_dir_all(&prefix).unwrap();
    let untar = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&prefix)
        .output()
        .expect("unpack the tarball");
    assert!(
        untar.status.success(),
        "tar failed: {}",
        String::from_utf8_lossy(&untar.stderr)
    );

    let unpacked_bin = prefix.join("bin/omt");
    std::fs::copy(env!("CARGO_BIN_EXE_omt"), &unpacked_bin).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&unpacked_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let home = temp_home();
    let ext = prefix.join("probe.ts");
    std::fs::write(
        &ext,
        "import fs from \"node:fs\";\nimport os from \"node:os\";\nimport path from \"node:path\";\nexport default function (pi) {\n  pi.on(\"session_start\", () => fs.appendFileSync(path.join(os.homedir(), \"ext-started.log\"), \"session_start\\n\"));\n}\n",
    )
    .unwrap();

    let out = Command::new(&unpacked_bin)
        .env("HOME", home.path())
        .env_remove("OMP_RS_EXT_HOST")
        .env_remove("OMP_AGENT_DIR")
        .env_remove("OMP_AUTH_BROKER_URL")
        .env_remove("OMP_AUTH_BROKER_TOKEN")
        .env("OMP_RS_EXT_SHARED", "0")
        .env("NO_COLOR", "1")
        .current_dir(work.path())
        .args(["-p", "--no-session", "--no-tools", "--model", "mock/m"])
        .args(["--extension", ext.to_str().unwrap()])
        .arg("hello")
        .output()
        .expect("run the unpacked binary");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("ext-host/host.ts not found"),
        "the unpacked binary could not find its extension host: {stderr}"
    );
    assert!(
        !stderr.contains("extensions disabled"),
        "the extension host failed to start: {stderr}"
    );
    let log = std::fs::read_to_string(home.path().join("ext-started.log"))
        .unwrap_or_else(|_| panic!("extension never ran; stderr: {stderr}"));
    assert!(log.contains("session_start"), "extension log: {log}");

    // install.sh links `~/.local/bin/omt` at a versioned prefix instead of running
    // the unpacked tree in place, so the launch goes through a symlink.
    #[cfg(unix)]
    {
        std::fs::remove_file(home.path().join("ext-started.log")).unwrap();
        let bin_dir = work.path().join("link/bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let link = bin_dir.join("omt");
        std::os::unix::fs::symlink(&unpacked_bin, &link).unwrap();
        let out = Command::new(&link)
            .env("HOME", home.path())
            .env_remove("OMP_RS_EXT_HOST")
            .env_remove("OMP_AGENT_DIR")
            .env_remove("OMP_AUTH_BROKER_URL")
            .env_remove("OMP_AUTH_BROKER_TOKEN")
            .env("OMP_RS_EXT_SHARED", "0")
            .env("NO_COLOR", "1")
            .current_dir(work.path())
            .args(["-p", "--no-session", "--no-tools", "--model", "mock/m"])
            .args(["--extension", ext.to_str().unwrap()])
            .arg("hello")
            .output()
            .expect("run the linked binary");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("ext-host/host.ts not found"),
            "the linked binary could not find its extension host: {stderr}"
        );
        let log = std::fs::read_to_string(home.path().join("ext-started.log"))
            .unwrap_or_else(|_| panic!("extension never ran through the link; stderr: {stderr}"));
        assert!(log.contains("session_start"), "extension log: {log}");
    }
}
