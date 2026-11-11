use crate::repos::test_repo::{DaemonTestScope, TestRepo, get_binary_path};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[test]
fn managed_install_rejects_source_without_expected_digest() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let output = repo
        .git_ai_command_without_pre_sync_for_test(
            &[
                "install-managed",
                "--agent",
                "codex",
                "--sha256",
                &"0".repeat(64),
            ],
            &[],
        )
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        json!({"status": "failed", "reason": "source-checksum-mismatch"})
    );
    assert!(output.stderr.is_empty());
    assert!(
        !repo
            .test_home_path()
            .join(".dcc-ai-tracking/dcc-install.json")
            .exists()
    );
}

fn current_digest() -> &'static str {
    static DIGEST: OnceLock<String> = OnceLock::new();
    DIGEST.get_or_init(|| format!("{:x}", Sha256::digest(fs::read(get_binary_path()).unwrap())))
}

fn tracking_root(repo: &TestRepo) -> PathBuf {
    repo.test_home_path().join(".dcc-ai-tracking")
}

fn tracking_binary(root: &Path) -> PathBuf {
    root.join("bin").join(if cfg!(windows) {
        "dcc-ai-tracking.exe"
    } else {
        "dcc-ai-tracking"
    })
}

fn managed_install(repo: &TestRepo, hash: &str) -> Value {
    let output = repo
        .git_ai_command_without_pre_sync_for_test(
            &["install-managed", "--agent", "codex", "--sha256", hash],
            &[],
        )
        .env_remove("GIT_AI_TEST_CONFIG_PATCH")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    serde_json::from_slice(&output.stdout).expect("one machine-readable response")
}

#[test]
fn managed_install_refuses_to_claim_an_external_root() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let root = tracking_root(&repo);
    let config = fs::read(root.join("config.json")).unwrap();

    assert_eq!(
        managed_install(&repo, current_digest()),
        json!({"status": "conflict", "reason": "external-installation"})
    );
    assert_eq!(fs::read(root.join("config.json")).unwrap(), config);
    assert!(!root.join("dcc-install.json").exists());
    assert!(!root.join("install.lock").exists());
}

#[cfg(unix)]
#[test]
fn managed_install_refuses_a_symlinked_root() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let root = tracking_root(&repo);
    let other_root = repo.test_home_path().join("outside");
    fs::rename(&root, &other_root).unwrap();
    std::os::unix::fs::symlink(&other_root, &root).unwrap();

    assert_eq!(
        managed_install(&repo, current_digest()),
        json!({"status": "conflict", "reason": "external-installation"})
    );
    assert!(!other_root.join("dcc-install.json").exists());
    assert!(!other_root.join("install.lock").exists());
}

#[test]
fn managed_install_preserves_newer_ledger_and_binary() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let root = tracking_root(&repo);
    let marker = root.join("dcc-install.json");
    let newer = br#"{"version":"99.0.0","initializedTools":["codex"]}"#;
    fs::write(&marker, newer).unwrap();
    let binary = tracking_binary(&root);
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::write(&binary, b"newer engine").unwrap();

    assert_eq!(
        managed_install(&repo, current_digest()),
        json!({"status": "conflict", "reason": "version-conflict"})
    );
    assert_eq!(fs::read(marker).unwrap(), newer);
    assert_eq!(fs::read(binary).unwrap(), b"newer engine");
}

#[test]
fn managed_install_respects_live_legacy_lease() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let root = tracking_root(&repo);
    let marker = root.join("dcc-install.json");
    let legacy = br#"{"version":"1.7.7-dcc.3","initializedTools":[]}"#;
    fs::write(&marker, legacy).unwrap();
    let lock = root.join("install.lock");
    let record = json!({"pid": std::process::id(), "ownerId": "legacy"}).to_string();
    fs::write(&lock, &record).unwrap();

    assert_eq!(
        managed_install(&repo, current_digest()),
        json!({"status": "failed", "reason": "lock-timeout"})
    );
    assert_eq!(fs::read_to_string(lock).unwrap(), record);
    assert_eq!(fs::read(marker).unwrap(), legacy);
}

#[test]
fn managed_install_checks_file_bytes_independently_of_legacy_ledger() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let root = tracking_root(&repo);
    let hash = current_digest();
    let marker = root.join("dcc-install.json");
    let legacy =
        json!({"version": "1.7.7-dcc.3", "binarySha256": hash, "initializedTools": ["codex"]});
    fs::write(&marker, legacy.to_string()).unwrap();
    fs::write(
        root.join("install.lock"),
        json!({"pid": 2_000_000_000_u32, "ownerId": "interrupted-legacy"}).to_string(),
    )
    .unwrap();
    let binary = tracking_binary(&root);
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::write(&binary, b"untrusted bytes").unwrap();
    let codex_home = repo.test_home_path().join(".codex");
    fs::create_dir_all(&codex_home).unwrap();
    // Deliberately fail hook initialization after the verified engine is copied.
    fs::write(codex_home.join("config.toml"), "invalid = [").unwrap();

    assert_eq!(
        managed_install(&repo, hash),
        json!({"status": "failed", "reason": "initialization-failed"})
    );
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(binary).unwrap())),
        hash
    );
    let state: Value = serde_json::from_slice(&fs::read(marker).unwrap()).unwrap();
    assert_eq!(state["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(state["binarySha256"], hash);
    assert_eq!(state["initializedTools"], json!([]));
    assert!(state["lastFailureAt"].is_u64());
    assert!(!root.join("install.lock").exists());
    assert_eq!(
        managed_install(&repo, hash),
        json!({"status": "skipped", "reason": "retry-pending"})
    );
}

#[test]
fn managed_install_reuses_a_verified_ready_binary() {
    let repo = TestRepo::new_dedicated_daemon();
    let root = tracking_root(&repo);
    let marker = root.join("dcc-install.json");
    let binary = tracking_binary(&root);
    let hash = current_digest();
    let state = json!({"version": env!("CARGO_PKG_VERSION"), "binarySha256": hash, "initializedTools": ["codex"]})
        .to_string();
    fs::write(&marker, &state).unwrap();
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::copy(get_binary_path(), binary).unwrap();
    let config = git_ai::daemon::DaemonConfig::from_home(&repo.daemon_home_path());
    let pid = git_ai::daemon::read_daemon_pid(&config).unwrap();

    assert_eq!(managed_install(&repo, hash), json!({"status": "ready"}));
    assert_eq!(fs::read_to_string(marker).unwrap(), state);
    assert_eq!(git_ai::daemon::read_daemon_pid(&config).unwrap(), pid);
}

#[cfg(unix)]
#[test]
fn managed_install_does_not_replace_while_daemon_stop_fails() {
    let repo = TestRepo::new_dedicated_daemon();
    let root = tracking_root(&repo);
    let marker = root.join("dcc-install.json");
    let binary = tracking_binary(&root);
    fs::write(
        &marker,
        br#"{"version":"1.7.7-dcc.3","initializedTools":["codex"]}"#,
    )
    .unwrap();
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::write(&binary, b"prior binary").unwrap();
    fs::remove_file(repo.daemon_control_socket_path()).unwrap();

    assert_eq!(
        managed_install(&repo, current_digest()),
        json!({"status": "failed", "reason": "daemon-shutdown-failed"})
    );
    let state: Value = serde_json::from_slice(&fs::read(marker).unwrap()).unwrap();
    assert_eq!(state["version"], "1.7.7-dcc.3");
    assert_eq!(state["initializedTools"], json!(["codex"]));
    assert_eq!(fs::read(binary).unwrap(), b"prior binary");
}

#[cfg(unix)]
#[test]
fn managed_install_initializes_from_the_verified_stable_path() {
    struct StopDaemon<'a>(&'a TestRepo);
    impl Drop for StopDaemon<'_> {
        fn drop(&mut self) {
            let _ = self
                .0
                .git_ai_command_without_pre_sync_for_test(
                    &["daemon", "shutdown", "--if-running"],
                    &[],
                )
                .output();
        }
    }

    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _stop = StopDaemon(&repo);
    let root = tracking_root(&repo);
    fs::remove_dir_all(&root).unwrap();
    let codex_home = repo.test_home_path().join(".codex");
    fs::create_dir_all(&codex_home).unwrap();
    let args = &[
        "install-managed",
        "--agent",
        "codex",
        "--sha256",
        current_digest(),
    ];
    let output = repo
        .git_ai_command_without_pre_sync_for_test(
            args,
            &[("CODEX_HOME", codex_home.to_str().unwrap())],
        )
        .env_remove("GIT_AI_TEST_DB_PATH")
        .env_remove("GITAI_TEST_DB_PATH")
        .env_remove("GIT_AI_TEST_CONFIG_PATCH")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"status": "ready"})
    );
    assert!(output.stderr.is_empty(), "{output:?}");

    let state: Value =
        serde_json::from_slice(&fs::read(root.join("dcc-install.json")).unwrap()).unwrap();
    assert_eq!(state["initializedTools"], json!(["codex"]));
    assert_eq!(state["binarySha256"], current_digest());
    let hooks = fs::read_to_string(codex_home.join("hooks.json")).unwrap();
    assert!(
        hooks.contains(".dcc-ai-tracking/bin/dcc-ai-tracking"),
        "{hooks}"
    );
    assert!(hooks.contains("checkpoint codex"), "{hooks}");
}

#[cfg(unix)]
#[test]
fn managed_install_parent_kill_cannot_leave_hook_initializer_running() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let root = tracking_root(&repo);
    fs::write(
        root.join("dcc-install.json"),
        br#"{"version":"1.7.7-dcc.3","initializedTools":[]}"#,
    )
    .unwrap();
    let config_path = root.join("config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    let real_git = config["git_path"].as_str().unwrap().to_owned();
    let home = repo.test_home_path();
    let git = home.join("delayed-git");
    let entered = home.join("git-entered");
    let release = home.join("git-release");
    let finished = home.join("git-finished");
    fs::write(
        &git,
        r#"#!/bin/sh
if [ "$1" = '--version' ] && [ ! -e "$INSTALL_TEST_ENTERED" ]; then
    touch "$INSTALL_TEST_ENTERED"
    while [ ! -e "$INSTALL_TEST_RELEASE" ]; do sleep 0.02; done
    touch "$INSTALL_TEST_FINISHED"
fi
exec "$INSTALL_TEST_GIT" "$@"
"#,
    )
    .unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
    config["git_path"] = json!(git);
    fs::write(&config_path, config.to_string()).unwrap();
    let codex_home = home.join(".codex");
    fs::create_dir_all(&codex_home).unwrap();
    let mut child = repo
        .git_ai_command_without_pre_sync_for_test(
            &[
                "install-managed",
                "--agent",
                "codex",
                "--sha256",
                current_digest(),
            ],
            &[("CODEX_HOME", codex_home.to_str().unwrap())],
        )
        .env_remove("GIT_AI_TEST_CONFIG_PATCH")
        .env("INSTALL_TEST_GIT", real_git)
        .env("INSTALL_TEST_ENTERED", &entered)
        .env("INSTALL_TEST_RELEASE", &release)
        .env("INSTALL_TEST_FINISHED", &finished)
        .spawn()
        .unwrap();
    let wait_for = |file: &Path| {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        file.exists()
    };
    let reached_initializer = wait_for(&entered);
    let killed = child.kill(); // Intentionally leave descendants untouched.
    fs::write(release, []).unwrap();
    let _ = child.wait();
    assert!(
        reached_initializer,
        "the initializer must have reached git --version"
    );
    killed.unwrap();
    assert!(wait_for(&finished), "release the read-only git probe");

    // A surviving install-hooks child would continue as soon as the probe returns.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let actual: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        assert_eq!(
            actual, config,
            "a child changed config after the lease owner died"
        );
        assert!(!codex_home.join("hooks.json").exists());
        std::thread::sleep(Duration::from_millis(20));
    }
}
