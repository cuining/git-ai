use crate::repos::test_repo::{DaemonTestScope, TestRepo};
#[cfg(unix)]
use base64::Engine;
#[cfg(unix)]
use flate2::{Compression, write::GzEncoder};
use serde_json::json;
#[cfg(unix)]
use sha2::{Digest, Sha512};

fn platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("linux", "x86_64") => "linux-x64",
        ("windows", "aarch64") => "win32-arm64",
        ("windows", "x86_64") => "win32-x64",
        _ => panic!("unsupported npm build target"),
    }
}

const PREFIX: &str = "/artifactory/api/npm/npm-local/@test/git-ai-fork";

fn mock_latest(registry: &mut mockito::Server, version: &str, integrity: &str) -> mockito::Mock {
    let package = format!("@test/git-ai-fork-{}", platform());
    let (os, cpu) = platform().split_once('-').unwrap();
    registry
        .mock("GET", format!("{PREFIX}-{}/latest", platform()).as_str())
        .with_body(json!({
            "name": package, "version": version, "os": [os], "cpu": [cpu],
            "dist": {"tarball": format!("{}/artifact.tgz", registry.url()), "integrity": integrity}
        }).to_string())
        .create()
}

fn upgrade(repo: &TestRepo, registry: &mockito::Server) -> Result<String, String> {
    let registry_url = registry.url();
    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["upgrade"],
        &[
            (
                "GIT_AI_NPM_UPDATE_PACKAGE_URL",
                &format!("{registry_url}{PREFIX}"),
            ),
            ("GIT_AI_API_BASE_URL", &registry_url),
            ("GIT_AI_SKIP_PATH_UPDATE", "1"),
        ],
    );
    #[cfg(windows)]
    command.env("USERPROFILE", repo.test_home_path());
    #[cfg(unix)]
    command.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    let output = command.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

#[test]
fn npm_upgrade_checks_latest_without_downloading_the_current_version() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut registry = mockito::Server::new();
    let artifact = registry.mock("GET", "/artifact.tgz").expect(0).create();
    let latest = mock_latest(&mut registry, env!("CARGO_PKG_VERSION"), "sha512-unused");

    let output =
        upgrade(&repo, &registry).expect("the current version should need no installation");
    assert!(output.contains("already on the latest version"));
    latest.assert();
    artifact.assert();
}

#[test]
fn npm_upgrade_rejects_a_bad_checksum_before_installing() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut registry = mockito::Server::new();
    let executable = repo.test_home_path().join(if cfg!(windows) {
        ".git-ai/bin/git-ai.exe"
    } else {
        ".git-ai/bin/git-ai"
    });
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    std::fs::write(&executable, "original git-ai").unwrap();
    let latest = mock_latest(&mut registry, "999.0.0", "sha512-wrong");
    let artifact = registry
        .mock("GET", "/artifact.tgz")
        .with_body("invalid artifact")
        .create();

    let error = upgrade(&repo, &registry).unwrap_err();
    assert!(error.contains("SHA-512 integrity mismatch"), "{error}");
    assert_eq!(
        std::fs::read_to_string(executable).unwrap(),
        "original git-ai"
    );
    latest.assert();
    artifact.assert();
}

#[cfg(unix)]
#[test]
fn npm_upgrade_installs_the_verified_binary_and_runs_hooks_without_npm() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut registry = mockito::Server::new();
    let binary = b"#!/bin/sh\ncase \"$1\" in\n--version) echo 999.0.0 ;;\ninstall-hooks) printf '%s' \"$2\" > \"$HOME/hook-installed\" ;;\nesac\n";
    let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    let mut header = tar::Header::new_ustar();
    header.set_mode(0o755);
    header.set_size(binary.len() as u64);
    header.set_cksum();
    tar.append_data(&mut header, "package/bin/git-ai", binary.as_slice())
        .unwrap();
    let bytes = tar.into_inner().unwrap().finish().unwrap();
    let integrity = format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&bytes))
    );
    let latest = mock_latest(&mut registry, "999.0.0", &integrity);
    let artifact = registry
        .mock("GET", "/artifact.tgz")
        .with_body(bytes)
        .create();

    let output = upgrade(&repo, &registry).unwrap();
    assert!(output.contains("Successfully installed 999.0.0"));
    assert_eq!(
        std::fs::read(repo.test_home_path().join(".git-ai/bin/git-ai")).unwrap(),
        binary
    );
    assert_eq!(
        std::fs::read_to_string(repo.test_home_path().join("hook-installed")).unwrap(),
        "--env"
    );
    latest.assert();
    artifact.assert();
}
