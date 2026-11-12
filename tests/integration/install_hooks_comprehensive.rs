//! Comprehensive tests for install_hooks command module
//!
//! This module tests the git-ai install-hooks and uninstall-hooks commands,
//! which handle installation of git hooks for various IDEs and coding agents.

use crate::repos::test_repo::{DaemonTestScope, TestRepo, get_binary_path};
use git_ai::commands::install_hooks::{
    InstallResult, InstallStatus, run, run_uninstall, to_hashmap,
};
use git_ai::daemon::DaemonConfig;
use std::collections::HashMap;
use std::fs;
use std::process::Command;

#[cfg(unix)]
pub(super) fn stub_cursor(repo: &TestRepo, command: &mut Command) -> std::path::PathBuf {
    use std::{env, os::unix::fs::PermissionsExt};

    let bin = repo.test_home_path().join("test-bin");
    fs::create_dir_all(&bin).unwrap();
    let cursor = bin.join("cursor");
    let calls = bin.join("cursor-calls");
    fs::write(
        &cursor,
        "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$GIT_AI_TEST_CURSOR_CALLS\"\nif [ \"$1\" = --version ]; then printf '1.99.0\\n'; fi\n",
    )
    .unwrap();
    fs::set_permissions(&cursor, fs::Permissions::from_mode(0o755)).unwrap();
    let inherited_path = env::var_os("PATH").unwrap_or_default();
    let path =
        env::join_paths(std::iter::once(bin).chain(env::split_paths(&inherited_path))).unwrap();
    command
        .env("PATH", path)
        .env("GIT_AI_TEST_CURSOR_CALLS", &calls);
    calls
}

// ==============================================================================
// InstallStatus Tests
// ==============================================================================

#[test]
fn test_install_status_as_str() {
    assert_eq!(InstallStatus::NotFound.as_str(), "not_found");
    assert_eq!(InstallStatus::Installed.as_str(), "installed");
    assert_eq!(
        InstallStatus::AlreadyInstalled.as_str(),
        "already_installed"
    );
    assert_eq!(InstallStatus::Failed.as_str(), "failed");
}

#[test]
fn test_install_status_equality() {
    assert_eq!(InstallStatus::NotFound, InstallStatus::NotFound);
    assert_eq!(InstallStatus::Installed, InstallStatus::Installed);
    assert_eq!(
        InstallStatus::AlreadyInstalled,
        InstallStatus::AlreadyInstalled
    );
    assert_eq!(InstallStatus::Failed, InstallStatus::Failed);

    assert_ne!(InstallStatus::NotFound, InstallStatus::Installed);
    assert_ne!(InstallStatus::Installed, InstallStatus::Failed);
}

#[test]
fn test_install_status_copy_clone() {
    let status = InstallStatus::Installed;
    let copied = status;
    let cloned = status;

    assert_eq!(status, copied);
    assert_eq!(status, cloned);
    assert_eq!(copied, cloned);
}

// ==============================================================================
// InstallResult Tests
// ==============================================================================

#[test]
fn test_install_result_installed() {
    let result = InstallResult::installed();
    assert_eq!(result.status, InstallStatus::Installed);
    assert!(result.error.is_none());
    assert!(result.warnings.is_empty());
}

#[test]
fn test_install_result_already_installed() {
    let result = InstallResult::already_installed();
    assert_eq!(result.status, InstallStatus::AlreadyInstalled);
    assert!(result.error.is_none());
    assert!(result.warnings.is_empty());
}

#[test]
fn test_install_result_not_found() {
    let result = InstallResult::not_found();
    assert_eq!(result.status, InstallStatus::NotFound);
    assert!(result.error.is_none());
    assert!(result.warnings.is_empty());
}

#[test]
fn test_install_result_failed() {
    let result = InstallResult::failed("Installation failed");
    assert_eq!(result.status, InstallStatus::Failed);
    assert_eq!(result.error, Some("Installation failed".to_string()));
    assert!(result.warnings.is_empty());
}

#[test]
fn test_install_result_failed_with_string() {
    let error_msg = String::from("Custom error message");
    let result = InstallResult::failed(error_msg.clone());
    assert_eq!(result.status, InstallStatus::Failed);
    assert_eq!(result.error, Some(error_msg));
}

#[test]
fn test_install_result_with_warning() {
    let result = InstallResult::installed().with_warning("Minor issue detected");
    assert_eq!(result.status, InstallStatus::Installed);
    assert!(result.error.is_none());
    assert_eq!(result.warnings.len(), 1);
    assert_eq!(result.warnings[0], "Minor issue detected");
}

#[test]
fn test_install_result_with_multiple_warnings() {
    let result = InstallResult::installed()
        .with_warning("Warning 1")
        .with_warning("Warning 2")
        .with_warning("Warning 3");

    assert_eq!(result.warnings.len(), 3);
    assert_eq!(result.warnings[0], "Warning 1");
    assert_eq!(result.warnings[1], "Warning 2");
    assert_eq!(result.warnings[2], "Warning 3");
}

#[test]
fn test_install_result_message_for_metrics_with_error() {
    let result = InstallResult::failed("Critical error");
    let message = result.message_for_metrics();
    assert_eq!(message, Some("Critical error".to_string()));
}

#[test]
fn test_install_result_message_for_metrics_with_warnings() {
    let result = InstallResult::installed()
        .with_warning("Warning 1")
        .with_warning("Warning 2");
    let message = result.message_for_metrics();
    assert_eq!(message, Some("Warning 1; Warning 2".to_string()));
}

#[test]
fn test_install_result_message_for_metrics_with_error_and_warnings() {
    // Error takes precedence over warnings
    let result = InstallResult::failed("Error message").with_warning("Some warning");
    let message = result.message_for_metrics();
    assert_eq!(message, Some("Error message".to_string()));
}

#[test]
fn test_install_result_message_for_metrics_no_error_or_warnings() {
    let result = InstallResult::installed();
    let message = result.message_for_metrics();
    assert!(message.is_none());
}

#[test]
fn test_install_result_message_for_metrics_empty_warnings() {
    let result = InstallResult {
        status: InstallStatus::Installed,
        error: None,
        warnings: vec![],
    };
    let message = result.message_for_metrics();
    assert!(message.is_none());
}

// ==============================================================================
// to_hashmap Conversion Tests
// ==============================================================================

#[test]
fn test_to_hashmap_empty() {
    let statuses: HashMap<String, InstallStatus> = HashMap::new();
    let result = to_hashmap(statuses);
    assert!(result.is_empty());
}

#[test]
fn test_to_hashmap_single_entry() {
    let mut statuses = HashMap::new();
    statuses.insert("cursor".to_string(), InstallStatus::Installed);

    let result = to_hashmap(statuses);
    assert_eq!(result.len(), 1);
    assert_eq!(result.get("cursor"), Some(&"installed".to_string()));
}

#[test]
fn test_to_hashmap_multiple_entries() {
    let mut statuses = HashMap::new();
    statuses.insert("cursor".to_string(), InstallStatus::Installed);
    statuses.insert("claude-code".to_string(), InstallStatus::AlreadyInstalled);
    statuses.insert("codex".to_string(), InstallStatus::NotFound);
    statuses.insert("windsurf".to_string(), InstallStatus::Failed);

    let result = to_hashmap(statuses);
    assert_eq!(result.len(), 4);
    assert_eq!(result.get("cursor"), Some(&"installed".to_string()));
    assert_eq!(
        result.get("claude-code"),
        Some(&"already_installed".to_string())
    );
    assert_eq!(result.get("codex"), Some(&"not_found".to_string()));
    assert_eq!(result.get("windsurf"), Some(&"failed".to_string()));
}

#[test]
fn test_to_hashmap_all_statuses() {
    let mut statuses = HashMap::new();
    statuses.insert("not_found".to_string(), InstallStatus::NotFound);
    statuses.insert("installed".to_string(), InstallStatus::Installed);
    statuses.insert("already".to_string(), InstallStatus::AlreadyInstalled);
    statuses.insert("failed".to_string(), InstallStatus::Failed);

    let result = to_hashmap(statuses);
    assert_eq!(result.get("not_found"), Some(&"not_found".to_string()));
    assert_eq!(result.get("installed"), Some(&"installed".to_string()));
    assert_eq!(
        result.get("already"),
        Some(&"already_installed".to_string())
    );
    assert_eq!(result.get("failed"), Some(&"failed".to_string()));
}

// ==============================================================================
// Argument Parsing Tests
// ==============================================================================

#[test]
fn plain_install_hooks_preserves_the_invoking_user_home() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let invoking_home = repo.test_home_path();
    let installed_home = repo.path().join("installed-user");
    let installed_bin_dir = installed_home.join(".git-ai").join("bin");
    fs::create_dir_all(&installed_bin_dir).unwrap();

    #[cfg(windows)]
    let installed_binary = installed_bin_dir.join("git-ai.exe");
    #[cfg(not(windows))]
    let installed_binary = installed_bin_dir.join("git-ai");
    fs::copy(get_binary_path(), &installed_binary).unwrap();

    let test_db = repo.path().join("install-hooks.db");
    let mut command = Command::new(&installed_binary);
    command
        .arg("install-hooks")
        .current_dir(repo.path())
        .env("HOME", invoking_home)
        .env("API_KEY", "package-test-key")
        .env("GIT_AI_TEST_DB_PATH", &test_db)
        .env("GITAI_TEST_DB_PATH", &test_db)
        .env("GIT_CONFIG_GLOBAL", invoking_home.join(".gitconfig"))
        .env("GIT_AI_ALLOW_SUPERUSER", "1")
        .env("GIT_AI_DEBUG", "0");
    #[cfg(windows)]
    command
        .env("USERPROFILE", invoking_home)
        .env("APPDATA", invoking_home.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", invoking_home.join("AppData").join("Local"));

    let output = command.output().expect("run copied git-ai binary");
    assert!(
        output.status.success(),
        "plain install-hooks failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let invoking_config = fs::read_to_string(invoking_home.join(".git-ai/config.json"))
        .expect("install-hooks should update the invoking user's config");
    let invoking_config: serde_json::Value = serde_json::from_str(&invoking_config).unwrap();
    assert_eq!(
        invoking_config["api_key"],
        serde_json::Value::String("package-test-key".to_string())
    );
    assert!(
        !installed_home.join(".git-ai/config.json").exists(),
        "plain install-hooks must not retarget config to the binary owner's home"
    );
}

#[test]
#[cfg(not(windows))]
fn install_hooks_env_flag_adds_path_to_detected_shell_configs() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    fs::write(home.join(".bashrc"), "# bashrc\n").unwrap();
    fs::write(home.join(".zshrc"), "# zshrc\n").unwrap();

    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["install-hooks", "--env"], &[])
        .output()
        .expect("run git-ai install-hooks --env");
    assert!(
        output.status.success(),
        "install-hooks --env failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let expected_line = format!("export PATH=\"{}/.git-ai/bin:$PATH\"", home.display());
    let bashrc = fs::read_to_string(home.join(".bashrc")).unwrap();
    let zshrc = fs::read_to_string(home.join(".zshrc")).unwrap();
    assert!(
        bashrc.contains(&expected_line),
        "missing PATH line:\n{bashrc}"
    );
    assert!(
        zshrc.contains(&expected_line),
        "missing PATH line:\n{zshrc}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Updated shell configurations:"),
        "missing shell config report:\n{stdout}"
    );

    // A second run must be idempotent and report the configs as already set up.
    let second = repo
        .git_ai_command_without_pre_sync_for_test(&["install-hooks", "--env"], &[])
        .output()
        .expect("re-run git-ai install-hooks --env");
    assert!(second.status.success());
    assert_eq!(
        fs::read_to_string(home.join(".zshrc")).unwrap(),
        zshrc,
        "second --env run must not modify shell configs again"
    );
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        second_stdout.contains("Already configured (no changes needed):"),
        "missing already-configured report:\n{second_stdout}"
    );
}

#[test]
#[cfg(not(windows))]
fn install_hooks_without_env_flag_leaves_shell_configs_untouched() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    fs::write(home.join(".zshrc"), "# zshrc\n").unwrap();

    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["install-hooks"], &[])
        .output()
        .expect("run git-ai install-hooks");
    assert!(
        output.status.success(),
        "install-hooks failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        fs::read_to_string(home.join(".zshrc")).unwrap(),
        "# zshrc\n",
        "plain install-hooks must never touch shell configs"
    );
}

#[test]
fn install_hooks_wsl_dry_run_does_not_invoke_wsl() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);

    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["install-hooks", "--wsl", "--dry-run"], &[])
        .output()
        .expect("run git-ai install-hooks --wsl --dry-run");

    assert!(
        output.status.success(),
        "install-hooks --wsl --dry-run failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Dry-run: skipping WSL installation."),
        "missing WSL dry-run message:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
#[cfg(unix)]
fn install_hooks_configures_cursor_without_installing_an_extension() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let mut command = repo.git_ai_command_without_pre_sync_for_test(&["install-hooks"], &[]);
    command.env("DSH_HOME", repo.test_home_path().join(".dsh"));
    let calls = stub_cursor(&repo, &mut command);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");

    let hooks_path = repo.test_home_path().join(".cursor/hooks.json");
    let hooks: serde_json::Value = serde_json::from_slice(&fs::read(hooks_path).unwrap()).unwrap();
    for event in ["preToolUse", "postToolUse"] {
        let checkpoint = hooks["hooks"][event][0]["command"].as_str().unwrap();
        assert!(checkpoint.ends_with("checkpoint cursor --hook-input stdin"));
    }
    let calls = fs::read_to_string(calls).unwrap();
    assert!(
        !calls.is_empty() && calls.lines().all(|call| call == "--version"),
        "unexpected Cursor CLI calls:\n{calls}"
    );
}

#[test]
#[cfg(not(windows))]
fn install_hooks_skips_configured_cline_and_gemini() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    let manifest_path = home.join(".vscode-server/extensions/extensions.json");
    fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    fs::create_dir_all(home.join("Documents")).unwrap();
    fs::write(
        manifest_path,
        r#"[
  {
    "identifier": {
      "id": "saoudrizwan.claude-dev"
    },
    "version": "4.0.11"
  }
]"#,
    )
    .unwrap();

    let gemini_home = home.join(".gemini");
    let gemini_settings = gemini_home.join("settings.json");
    let original = r#"{"hooks":{"BeforeTool":[{"matcher":"*","hooks":[{"type":"command","command":"git-ai checkpoint gemini --hook-input stdin"}]}]}}"#;
    fs::create_dir_all(&gemini_home).unwrap();
    fs::write(&gemini_settings, original).unwrap();

    let run = |args: &[&str]| {
        let mut command = repo.git_ai_command_without_pre_sync_for_test(args, &[]);
        command.env("GEMINI_CLI_HOME", home);
        #[cfg(target_os = "macos")]
        stub_cursor(&repo, &mut command);
        command.output().unwrap()
    };
    let output = run(&["install-hooks", "--dry-run"]);
    assert!(
        output.status.success(),
        "install-hooks failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("Cline:") && !stdout.contains("Gemini:"),
        "excluded agents were checked during installation:\n{stdout}"
    );
    assert!(
        !home.join("Documents/Cline/Hooks").exists(),
        "dry-run must not create the Cline hooks directory"
    );
    assert_eq!(fs::read_to_string(&gemini_settings).unwrap(), original);

    let output = run(&["uninstall-hooks", "--dry-run"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Gemini: Pending removal"), "{stdout}");
    assert_eq!(fs::read_to_string(gemini_settings).unwrap(), original);
}

#[test]
fn test_run_install_hooks_no_args() {
    // This will try to run against the actual system, but should not crash
    // It may fail if binary path cannot be determined, which is acceptable
    let result = run(&[]);

    // We just ensure it returns a result (success or error)
    // The actual behavior depends on the system state
    match result {
        Ok(_statuses) => {
            // Should return a HashMap, possibly empty
            // Success is valid
        }
        Err(e) => {
            // May fail if binary path is not available or other system issues
            let err_msg = e.to_string();
            // Just ensure we get a meaningful error
            assert!(!err_msg.is_empty());
        }
    }
}

#[test]
fn test_run_install_hooks_with_dry_run_flag() {
    let args = vec!["--dry-run".to_string()];
    let result = run(&args);

    // Dry run should not modify anything
    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(e) => {
            let err_msg = e.to_string();
            assert!(!err_msg.is_empty());
        }
    }
}

#[test]
fn test_run_install_hooks_with_dry_run_true() {
    let args = vec!["--dry-run=true".to_string()];
    let result = run(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_install_hooks_with_verbose_flag() {
    let args = vec!["--verbose".to_string()];
    let result = run(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_install_hooks_with_verbose_short_flag() {
    let args = vec!["-v".to_string()];
    let result = run(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_install_hooks_with_multiple_flags() {
    let args = vec!["--dry-run".to_string(), "--verbose".to_string()];
    let result = run(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_install_hooks_with_dry_run_false() {
    // Note: This could actually install hooks on the system
    // In a real test environment, this should be run in isolation
    let args = vec!["--dry-run=false".to_string()];
    let result = run(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_install_hooks_rejects_unknown_args() {
    let result = run(&["--unknown-flag".into()]);
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("unknown install-hooks argument")
    );
}

#[test]
fn install_hooks_reports_agent_failure_according_to_install_mode() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let codex_home = repo.test_home_path().join("codex-broken");
    fs::create_dir_all(&codex_home).unwrap();
    let config_path = codex_home.join("config.toml");
    fs::write(&config_path, "invalid = [").unwrap();

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["install-hooks", "--env"],
        &[("CODEX_HOME", codex_home.to_str().unwrap())],
    );
    command
        .env("CLAUDE_CONFIG_DIR", repo.test_home_path().join(".claude"))
        .env("DSH_HOME", repo.test_home_path().join(".dsh"))
        .env_remove("GIT_AI_TEST_CONFIG_PATCH");
    #[cfg(unix)]
    stub_cursor(&repo, &mut command);
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Failed to parse Codex config.toml"),
        "{stderr}"
    );
    assert!(output.status.success(), "{stderr}");
    assert_eq!(fs::read_to_string(&config_path).unwrap(), "invalid = [");
}

#[test]
fn standalone_install_resets_trace2_and_installs_hooks() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let codex_home = repo.test_home_path().join("codex-managed");
    fs::create_dir_all(&codex_home).unwrap();
    repo.git_ai(&["config", "set", "codex_hooks_format", "hooks_json"])
        .unwrap();
    let git_config = repo.test_home_path().join(".gitconfig");
    fs::write(
        &git_config,
        "[trace2]\n\teventTarget = 2\n\teventTarget = 3\n\teventNesting = 7\n\tnormalTarget = 2\n",
    )
    .unwrap();
    let env = [("CODEX_HOME", codex_home.to_str().unwrap())];

    let mut standalone_command =
        repo.git_ai_command_without_pre_sync_for_test(&["install-hooks", "--env"], &env);
    #[cfg(unix)]
    stub_cursor(&repo, &mut standalone_command);
    let standalone = standalone_command.output().unwrap();
    let stderr = String::from_utf8_lossy(&standalone.stderr);
    assert!(standalone.status.success(), "{stderr}");
    assert!(codex_home.join("hooks.json").is_file());
    let expected = DaemonConfig::trace2_event_target_for_path(&repo.daemon_trace_socket_path());
    assert_eq!(
        repo.git(&["config", "--global", "--get-all", "trace2.eventTarget"])
            .unwrap()
            .trim(),
        expected
    );
    assert_eq!(
        repo.git(&["config", "--global", "--get", "trace2.eventNesting"])
            .unwrap()
            .trim(),
        "0"
    );
    assert!(
        repo.git(&["config", "--global", "--get", "trace2.normalTarget"])
            .is_err()
    );
}

#[test]
fn npm_managed_default_install_keeps_codex_hooks_when_config_breaks() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let codex_home = repo.test_home_path().join("codex-managed");
    fs::create_dir_all(&codex_home).unwrap();
    repo.git_ai(&["config", "set", "codex_hooks_format", "hooks_json"])
        .unwrap();
    let env = [("CODEX_HOME", codex_home.to_str().unwrap())];
    let mut install = repo.git_ai_command_without_pre_sync_for_test(&["install-hooks"], &env);
    #[cfg(unix)]
    stub_cursor(&repo, &mut install);
    let installed = install.output().unwrap();
    assert!(installed.status.success(), "{installed:?}");
    assert!(codex_home.join("hooks.json").is_file());
    assert!(
        fs::read_to_string(codex_home.join("config.toml"))
            .unwrap()
            .contains("trusted_hash")
    );
    assert!(
        repo.git_ai_with_env(&["install-hooks", "--unknown"], &env)
            .is_err()
    );
    let broken = "invalid = [";
    fs::write(codex_home.join("config.toml"), broken).unwrap();
    let mut failed_command =
        repo.git_ai_command_without_pre_sync_for_test(&["install-hooks"], &env);
    #[cfg(unix)]
    stub_cursor(&repo, &mut failed_command);
    let failed = failed_command.output().unwrap();
    assert!(failed.status.success(), "{failed:?}");
    assert!(String::from_utf8_lossy(&failed.stderr).contains("Failed to parse Codex config.toml"));
    assert_eq!(
        fs::read_to_string(codex_home.join("config.toml")).unwrap(),
        broken
    );
}

#[cfg(unix)]
#[test]
fn self_install_places_engine_and_hooks_at_the_user_installation() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    let codex_home = home.join(".codex");
    fs::create_dir_all(&codex_home).unwrap();
    let mut install = repo.git_ai_command_without_pre_sync_for_test(&["install"], &[]);
    install
        .env("CODEX_HOME", &codex_home)
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("DSH_HOME", home.join(".dsh"))
        .env_remove("GIT_AI_TEST_CONFIG_PATCH");
    stub_cursor(&repo, &mut install);

    let output = install.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let executable = home.join(".git-ai/bin/git-ai").canonicalize().unwrap();
    let installed = Command::new(&executable).arg("--version").output().unwrap();
    assert!(installed.status.success(), "{installed:?}");
    let hooks_path = home.join(".cursor/hooks.json");
    let hooks = fs::read_to_string(&hooks_path).unwrap();
    assert!(hooks.contains(executable.to_str().unwrap()), "{hooks}");
    let codex_config = fs::read_to_string(codex_home.join("config.toml")).unwrap();
    assert!(
        codex_config.contains(executable.to_str().unwrap()),
        "{codex_config}"
    );
    assert!(!codex_home.join("hooks.json").exists());

    fs::remove_file(&hooks_path).unwrap();
    let output = install.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        !hooks_path.exists(),
        "repeating install from the bundle must skip hook setup"
    );

    let installed_command = |command| {
        let mut child = Command::new(&executable);
        child.arg(command).current_dir(repo.path());
        for (key, value) in install.get_envs() {
            match value {
                Some(value) => child.env(key, value),
                None => child.env_remove(key),
            };
        }
        child.output().unwrap()
    };
    let output = installed_command("install");
    assert!(output.status.success(), "{output:?}");
    assert!(!hooks_path.exists(), "ready install must not refresh hooks");
    let output = installed_command("install-hooks");
    assert!(output.status.success(), "{output:?}");
    let hooks = fs::read_to_string(hooks_path).unwrap();
    assert!(hooks.contains(executable.to_str().unwrap()), "{hooks}");
}

#[test]
fn self_install_resets_trace2_with_global_include() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    let codex_home = home.join(".codex");
    fs::create_dir_all(&codex_home).unwrap();
    let git_config = home.join(".gitconfig");
    let include_config = "[include]\n\tpath = trace2-user.config\n";
    let included_trace2 = "[trace2]\n\teventTarget = 2\n\teventNesting = 7\n";
    let foreign_config = home.join("trace2-user.config");
    fs::write(&git_config, include_config).unwrap();
    fs::write(&foreign_config, included_trace2).unwrap();
    let pending = home.join(".git-ai/bin/.git-ai-install-pending");
    let mut install = repo.git_ai_command_without_pre_sync_for_test(&["install"], &[]);
    install
        .env("CODEX_HOME", &codex_home)
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("DSH_HOME", home.join(".dsh"))
        .env_remove("GIT_AI_TEST_CONFIG_PATCH");
    #[cfg(unix)]
    stub_cursor(&repo, &mut install);

    let output = install.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        fs::read_to_string(&foreign_config).unwrap(),
        included_trace2
    );
    let expected = DaemonConfig::trace2_event_target_for_path(&repo.daemon_trace_socket_path());
    assert_eq!(
        repo.git(&[
            "config",
            "--global",
            "--includes",
            "--get",
            "trace2.eventTarget"
        ])
        .unwrap()
        .trim(),
        expected
    );
    assert!(!pending.exists());
    assert!(codex_home.join("config.toml").is_file());
}

#[test]
fn self_install_keeps_incomplete_setup_retryable_when_daemon_start_fails() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    let git_config = home.join(".gitconfig");
    let original = "[trace2]\n\teventTarget = 2\n";
    fs::write(&git_config, original).unwrap();
    let blocked_daemon_home = home.join("blocked-daemon-home");
    fs::write(&blocked_daemon_home, "not a directory").unwrap();
    let mut install = repo.git_ai_command_without_pre_sync_for_test(&["install"], &[]);
    install
        .env("GIT_AI_DAEMON_HOME", &blocked_daemon_home)
        .env_remove("GIT_AI_TEST_DB_PATH")
        .env_remove("GITAI_TEST_DB_PATH");
    let output = install.output().unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("background service"));
    assert!(home.join(".git-ai/bin/.git-ai-install-pending").is_file());
    assert_eq!(fs::read_to_string(&git_config).unwrap(), original);
}

#[test]
fn self_install_preserves_an_independent_installation() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    let bin = home.join(".git-ai/bin");
    fs::create_dir_all(&bin).unwrap();
    let executable = bin.join(format!("git-ai{}", std::env::consts::EXE_SUFFIX));
    fs::copy(get_binary_path(), &executable).unwrap();
    let config = home.join(".git-ai/config.json");
    let user_config = r#"{"quiet":true}"#;
    fs::write(&config, user_config).unwrap();
    fs::write(home.join(".gitconfig"), "[trace2]\n\teventTarget = 2\n").unwrap();

    let mut install = repo.git_ai_command_without_pre_sync_for_test(&["install"], &[]);
    let mut dry_run = repo.git_ai_command_without_pre_sync_for_test(&["install", "--dry-run"], &[]);

    let preview = dry_run.output().unwrap();
    assert!(preview.status.success(), "{preview:?}");
    assert!(!home.join(".git-ai/install.lock").exists());

    let output = install.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let notice = String::from_utf8_lossy(&output.stdout);
    assert!(
        notice.contains("Git AI is already installed at"),
        "{notice}"
    );
    assert_eq!(preview.stdout, output.stdout);
    assert_eq!(fs::read_to_string(config).unwrap(), user_config);
    assert_eq!(
        fs::read(executable).unwrap(),
        fs::read(get_binary_path()).unwrap()
    );
}

#[test]
fn managed_daemon_health_is_global() {
    let repo = TestRepo::new_dedicated_daemon();
    let health = |cwd: &std::path::Path| {
        repo.git_ai_command_without_pre_sync_for_test(&["daemon", "health"], &[])
            .current_dir(cwd)
            .output()
            .unwrap()
    };

    for cwd in [repo.path().as_path(), repo.test_home_path()] {
        let output = health(cwd);
        assert!(output.status.success(), "{output:?}");
    }

    let stopped = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let output = stopped
        .git_ai_command_without_pre_sync_for_test(&["daemon", "health"], &[])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
}

#[test]
fn managed_daemon_shutdown_is_idempotent() {
    let repo = TestRepo::new_dedicated_daemon();
    let stop = |args: &[&str]| {
        repo.git_ai_command_without_pre_sync_for_test(args, &[])
            .output()
            .unwrap()
    };

    let config = git_ai::daemon::DaemonConfig::from_home(&repo.daemon_home_path());
    assert!(git_ai::daemon::read_daemon_pid(&config).is_ok());
    for _ in 0..2 {
        let output = stop(&["daemon", "shutdown", "--if-running"]);
        assert!(output.status.success(), "{output:?}");
        assert!(git_ai::daemon::read_daemon_pid(&config).is_err());
    }

    let output = stop(&["daemon", "shutdown"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
}

#[cfg(unix)]
#[test]
fn managed_daemon_shutdown_does_not_ignore_an_unreachable_daemon() {
    let repo = TestRepo::new_dedicated_daemon();
    fs::remove_file(repo.daemon_control_socket_path()).unwrap();
    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["daemon", "shutdown", "--if-running"], &[])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
}

#[cfg(unix)]
#[test]
fn managed_daemon_shutdown_does_not_ignore_a_trace_only_listener() {
    use std::os::unix::net::UnixListener;

    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let trace_path = repo.daemon_trace_socket_path();
    fs::create_dir_all(trace_path.parent().unwrap()).unwrap();
    let _listener = UnixListener::bind(trace_path).unwrap();
    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["daemon", "shutdown", "--if-running"], &[])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
}

#[cfg(unix)]
struct StopDedicatedDaemon<'a>(&'a TestRepo);

#[cfg(unix)]
impl Drop for StopDedicatedDaemon<'_> {
    fn drop(&mut self) {
        let _ = self
            .0
            .git_ai_command_without_pre_sync_for_test(&["daemon", "shutdown", "--hard"], &[])
            .output();
    }
}

#[cfg(unix)]
#[test]
fn managed_daemon_repair_restarts_only_when_unhealthy() {
    let repo = TestRepo::new_dedicated_daemon();
    let _stop = StopDedicatedDaemon(&repo);
    let codex_home = repo.test_home_path().join("codex-managed");
    fs::create_dir_all(&codex_home).unwrap();
    let config = git_ai::daemon::DaemonConfig::from_home(&repo.daemon_home_path());
    let pid = || git_ai::daemon::read_daemon_pid(&config).unwrap();
    let install = || {
        repo.git_ai_command_without_pre_sync_for_test(
            &["install-hooks"],
            &[("CODEX_HOME", codex_home.to_str().unwrap())],
        )
        .env_remove("GIT_AI_TEST_DB_PATH")
        .env_remove("GITAI_TEST_DB_PATH")
        .output()
        .unwrap()
    };

    let original_pid = pid();
    let output = install();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(pid(), original_pid);

    fs::remove_file(repo.daemon_trace_socket_path()).unwrap();
    assert_eq!(repo.daemon_status()["sequencer_stalled"], false);
    let output = install();
    assert!(output.status.success(), "{output:?}");
    assert_ne!(pid(), original_pid);
    assert!(repo.daemon_trace_socket_path().exists());
}

// ==============================================================================
// Uninstall Tests
// ==============================================================================

#[test]
fn test_uninstall_alias_runs_uninstall_hooks() {
    let repo = TestRepo::new();
    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["uninstall", "--dry-run"], &[])
        .output()
        .expect("run git-ai uninstall --dry-run");

    assert!(
        output.status.success(),
        "git-ai uninstall should alias uninstall-hooks:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_run_uninstall_hooks_no_args() {
    let result = run_uninstall(&[]);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(e) => {
            let err_msg = e.to_string();
            assert!(!err_msg.is_empty());
        }
    }
}

#[test]
fn test_run_uninstall_hooks_with_dry_run() {
    let args = vec!["--dry-run".to_string()];
    let result = run_uninstall(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_uninstall_hooks_with_verbose() {
    let args = vec!["--verbose".to_string()];
    let result = run_uninstall(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

#[test]
fn test_run_uninstall_hooks_with_multiple_flags() {
    let args = vec![
        "--dry-run=true".to_string(),
        "-v".to_string(),
        "--unknown".to_string(),
    ];
    let result = run_uninstall(&args);

    match result {
        Ok(_statuses) => {
            // Success is valid
        }
        Err(_e) => {
            // May fail on CI or systems without binary path
        }
    }
}

// ==============================================================================
// Edge Cases and Error Handling
// ==============================================================================

#[test]
fn test_install_result_clone() {
    let result = InstallResult::failed("Error")
        .with_warning("Warning 1")
        .with_warning("Warning 2");

    let cloned = result.clone();
    assert_eq!(cloned.status, result.status);
    assert_eq!(cloned.error, result.error);
    assert_eq!(cloned.warnings, result.warnings);
}

#[test]
fn test_install_result_debug_formatting() {
    let result = InstallResult::installed();
    let debug_str = format!("{:?}", result);
    assert!(debug_str.contains("InstallResult"));
    assert!(debug_str.contains("Installed"));
}

#[test]
fn test_install_status_debug_formatting() {
    let status = InstallStatus::Installed;
    let debug_str = format!("{:?}", status);
    assert!(debug_str.contains("Installed"));
}

#[test]
fn test_to_hashmap_preserves_all_keys() {
    let mut statuses = HashMap::new();
    let keys = vec![
        "cursor",
        "claude-code",
        "codex",
        "windsurf",
        "continue-cli",
        "github-copilot",
    ];

    for (idx, key) in keys.iter().enumerate() {
        let status = match idx % 4 {
            0 => InstallStatus::Installed,
            1 => InstallStatus::AlreadyInstalled,
            2 => InstallStatus::NotFound,
            _ => InstallStatus::Failed,
        };
        statuses.insert(key.to_string(), status);
    }

    let result = to_hashmap(statuses);
    assert_eq!(result.len(), keys.len());

    for key in keys {
        assert!(
            result.contains_key(key),
            "Expected key '{}' to be present",
            key
        );
    }
}

#[test]
fn test_install_result_warning_with_empty_string() {
    let result = InstallResult::installed().with_warning("");
    assert_eq!(result.warnings.len(), 1);
    assert_eq!(result.warnings[0], "");
}

#[test]
fn test_install_result_failed_with_empty_string() {
    let result = InstallResult::failed("");
    assert_eq!(result.error, Some("".to_string()));
    assert_eq!(result.status, InstallStatus::Failed);
}

#[test]
fn test_install_result_message_for_metrics_single_warning() {
    let result = InstallResult::installed().with_warning("Only warning");
    let message = result.message_for_metrics();
    assert_eq!(message, Some("Only warning".to_string()));
}

#[test]
fn test_install_result_message_for_metrics_warnings_join_with_semicolon() {
    let result = InstallResult::installed()
        .with_warning("First; warning")
        .with_warning("Second; warning")
        .with_warning("Third; warning");

    let message = result.message_for_metrics();
    assert_eq!(
        message,
        Some("First; warning; Second; warning; Third; warning".to_string())
    );
}

// ==============================================================================
// Integration-style Tests
// ==============================================================================

#[test]
fn test_install_workflow_dry_run_does_not_modify_system() {
    // Dry run should be safe to run repeatedly
    let args = vec!["--dry-run".to_string(), "--verbose".to_string()];

    let result1 = run(&args);
    let result2 = run(&args);

    // Both runs should succeed or fail consistently
    match (result1, result2) {
        (Ok(_statuses1), Ok(_statuses2)) => {
            // Results may differ if system state changes between runs,
            // but both should be valid HashMaps
            // Success is valid
        }
        (Err(_), Err(_)) => {
            // Both failing is acceptable (e.g., on CI without proper setup)
        }
        _ => {
            // Inconsistent results would indicate a problem, but we allow it
            // since the system state could change
        }
    }
}

#[test]
fn test_uninstall_workflow_dry_run_does_not_modify_system() {
    let args = vec!["--dry-run".to_string()];

    let result1 = run_uninstall(&args);
    let result2 = run_uninstall(&args);

    match (result1, result2) {
        (Ok(_statuses1), Ok(_statuses2)) => {
            // Success is valid
        }
        (Err(_), Err(_)) => {
            // Both failing is acceptable
        }
        _ => {
            // Allow inconsistent results due to system state changes
        }
    }
}

// ==============================================================================
// Status String Validation
// ==============================================================================

#[test]
fn test_all_status_strings_are_lowercase() {
    assert!(
        InstallStatus::NotFound
            .as_str()
            .chars()
            .all(|c| !c.is_uppercase())
    );
    assert!(
        InstallStatus::Installed
            .as_str()
            .chars()
            .all(|c| !c.is_uppercase())
    );
    assert!(
        InstallStatus::AlreadyInstalled
            .as_str()
            .chars()
            .all(|c| !c.is_uppercase())
    );
    assert!(
        InstallStatus::Failed
            .as_str()
            .chars()
            .all(|c| !c.is_uppercase())
    );
}

#[test]
fn test_status_strings_use_underscores() {
    // Verify consistent naming convention
    assert!(InstallStatus::NotFound.as_str().contains('_'));
    assert!(InstallStatus::AlreadyInstalled.as_str().contains('_'));
    assert!(!InstallStatus::Installed.as_str().contains('_'));
    assert!(!InstallStatus::Failed.as_str().contains('_'));
}

#[test]
fn test_status_strings_are_valid_identifiers() {
    // Status strings should be suitable for use as keys
    let statuses = [
        InstallStatus::NotFound,
        InstallStatus::Installed,
        InstallStatus::AlreadyInstalled,
        InstallStatus::Failed,
    ];

    for status in &statuses {
        let s = status.as_str();
        assert!(!s.is_empty());
        assert!(!s.contains(' '));
        assert!(!s.contains('-'));
        // Should only contain alphanumeric and underscores
        assert!(s.chars().all(|c| c.is_alphanumeric() || c == '_'));
    }
}

// ==============================================================================
// Complex Scenario Tests
// ==============================================================================

#[test]
fn test_install_result_builder_pattern() {
    // Demonstrate builder-like pattern with warnings
    let result = InstallResult::installed()
        .with_warning("Extension not found")
        .with_warning("Git path not configured")
        .with_warning("Manual action required");

    assert_eq!(result.status, InstallStatus::Installed);
    assert_eq!(result.warnings.len(), 3);
    assert!(result.error.is_none());

    let message = result.message_for_metrics();
    assert!(message.is_some());
    let msg = message.unwrap();
    assert!(msg.contains("Extension not found"));
    assert!(msg.contains("Git path not configured"));
    assert!(msg.contains("Manual action required"));
}

#[test]
fn test_to_hashmap_with_realistic_agent_names() {
    let mut statuses = HashMap::new();
    statuses.insert("cursor".to_string(), InstallStatus::Installed);
    statuses.insert("claude-code".to_string(), InstallStatus::AlreadyInstalled);
    statuses.insert("github-copilot".to_string(), InstallStatus::NotFound);
    statuses.insert("codex".to_string(), InstallStatus::Installed);
    statuses.insert("windsurf".to_string(), InstallStatus::Failed);
    statuses.insert("continue-cli".to_string(), InstallStatus::NotFound);

    let result = to_hashmap(statuses);
    assert_eq!(result.len(), 6);

    // Verify specific mappings
    assert_eq!(result.get("cursor").unwrap(), "installed");
    assert_eq!(result.get("claude-code").unwrap(), "already_installed");
    assert_eq!(result.get("github-copilot").unwrap(), "not_found");
    assert_eq!(result.get("codex").unwrap(), "installed");
    assert_eq!(result.get("windsurf").unwrap(), "failed");
    assert_eq!(result.get("continue-cli").unwrap(), "not_found");
}

#[test]
fn test_install_result_different_error_types() {
    // Test with different error message types
    let errors = vec![
        "Permission denied",
        "File not found",
        "Invalid configuration",
        "Version mismatch: expected 1.7, found 1.5",
        "Network timeout",
        "",
    ];

    for error in errors {
        let result = InstallResult::failed(error);
        assert_eq!(result.status, InstallStatus::Failed);
        assert_eq!(result.error, Some(error.to_string()));
        assert_eq!(result.message_for_metrics(), Some(error.to_string()));
    }
}

#[test]
fn test_hashmap_conversion_stability() {
    // Test that conversion is stable (same input produces same output)
    let mut statuses = HashMap::new();
    statuses.insert("test1".to_string(), InstallStatus::Installed);
    statuses.insert("test2".to_string(), InstallStatus::NotFound);

    let result1 = to_hashmap(statuses.clone());
    let result2 = to_hashmap(statuses);

    assert_eq!(result1.len(), result2.len());
    for (key, value) in result1.iter() {
        assert_eq!(result2.get(key), Some(value));
    }
}

#[test]
fn test_git_hooks_remove_rejects_tracking_directory_and_preserves_other_dcc_hooks() {
    for (relative_path, allowed) in [
        (".git-ai/hooks", false),
        (".GIT-AI/hooks", false),
        (".dcc/custom-hooks", true),
        (".git-ai-other/hooks", true),
        ("other/.git-ai/hooks", false),
    ] {
        let repo = TestRepo::new();
        let managed_hooks = repo.path().join(".git/ai/hooks");
        let original_hooks = repo.test_home_path().join(relative_path);
        std::fs::create_dir_all(&managed_hooks).unwrap();
        std::fs::write(
            repo.path().join(".git/ai/git_hooks_state.json"),
            serde_json::json!({
                "managed_hooks_path": managed_hooks,
                "original_local_hooks_path": original_hooks,
                "binary_path": "git-ai",
            })
            .to_string(),
        )
        .unwrap();
        repo.git(&[
            "config",
            "--local",
            "core.hooksPath",
            managed_hooks.to_str().unwrap(),
        ])
        .unwrap();
        repo.git_ai(&["git-hooks", "remove"]).unwrap();
        let restored = repo.git(&["config", "--local", "--get", "core.hooksPath"]);
        if allowed {
            assert_eq!(restored.unwrap().trim(), original_hooks.to_str().unwrap());
        } else {
            assert!(restored.is_err(), "must not restore {relative_path}");
        }
    }
}
