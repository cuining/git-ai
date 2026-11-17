#[cfg(target_os = "macos")]
use crate::install_hooks_comprehensive::stub_cursor;
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use std::time::{Duration, Instant};
use std::{fs, path::Path, process::Command, process::Output};

const PACKAGE: &str = "dsh-git-ai-attribution";
const USER_PATCH: &str = "# Keep this comment\r\n- id: user-plugin\r\n  disabled: !!js false\r\n";

#[cfg(target_os = "macos")]
fn fake_desktop(repo: &TestRepo, status: u8) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let resources = repo
        .test_home_path()
        .join("Fake Desktop/Contents/Resources");
    let dsh = resources.join("runtime/cli/bin/dsh");
    fs::create_dir_all(dsh.parent().unwrap()).unwrap();
    fs::write(
        &dsh,
        format!(
            "#!/bin/sh\n[ \"$1 $2 $3 $4\" = 'plugin --profile desktop add' ] || exit 97\n[ ! -e package.json.lock ] || exit 98\n: > dsh-add-called\nexit {status}\n"
        ),
    )
    .unwrap();
    fs::set_permissions(dsh, fs::Permissions::from_mode(0o755)).unwrap();
    resources
}

fn run(repo: &TestRepo, home: Option<&Path>, args: &[&str]) -> Output {
    run_with_runtime(repo, home, args, Path::new(""))
}

fn run_with_runtime(
    repo: &TestRepo,
    home: Option<&Path>,
    args: &[&str],
    resources: &Path,
) -> Output {
    let mut command = repo.git_ai_command_without_pre_sync_for_test(args, &[]);
    command.env("GIT_AI_TEST_DSH_RESOURCES", resources);
    if let Some(home) = home {
        command.env("DSH_HOME", home);
    } else {
        command.env_remove("DSH_HOME");
    }
    #[cfg(target_os = "macos")]
    stub_cursor(repo, &mut command);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    output
}

fn assert_report(output: Output, expected: &str) {
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stdout.contains(expected), "{stdout}\n{stderr}");
    assert!(
        !stderr.contains("Hook verification failed for DeepSeek Harness"),
        "{stderr}"
    );
}

#[test]
fn deepseek_harness_uses_the_default_home_and_uninstalls_only_its_registration() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let profile = repo.test_home_path().join(".dsh/profiles/desktop");
    let manifest = profile.join("package.json");
    let patch = profile.join("cordis.patch.yml");
    let manifest_bytes = br#"{"name":"dsh-profile-desktop","dsh":{"profile":{"bundles":[]}}}"#;
    fs::create_dir_all(&profile).unwrap();
    fs::write(&manifest, manifest_bytes).unwrap();
    fs::write(&patch, USER_PATCH).unwrap();

    assert_report(
        run(&repo, None, &["install-hooks"]),
        "DeepSeek Harness: Hooks updated",
    );
    let installed_patch = fs::read_to_string(&patch).unwrap();
    assert!(installed_patch.starts_with(USER_PATCH));
    assert!(installed_patch.contains("id: git-ai-attribution"));
    assert!(installed_patch.contains("gitAiPath:"));
    let user_suffix =
        "# Added after installation\r\n- id: another-user-plugin\r\n  disabled: true\r\n";
    let updated_patch = format!("{installed_patch}{user_suffix}");
    fs::write(&patch, &updated_patch).unwrap();

    assert_report(
        run(&repo, None, &["uninstall-hooks", "--dry-run"]),
        "DeepSeek Harness: Pending removal",
    );
    assert_eq!(fs::read_to_string(&patch).unwrap(), updated_patch);
    assert_report(
        run(&repo, None, &["uninstall-hooks"]),
        "DeepSeek Harness: Hooks removed",
    );
    assert_eq!(
        fs::read_to_string(&patch).unwrap(),
        format!("{USER_PATCH}{user_suffix}")
    );
    assert_eq!(fs::read(&manifest).unwrap(), manifest_bytes);
}

#[test]
fn deepseek_harness_preserves_package_managed_installations() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path().join("DSH home");
    let profile = home.join("profiles/desktop");
    let installed = profile.join("node_modules").join(PACKAGE);
    let manifest = profile.join("package.json");
    let patch = profile.join("cordis.patch.yml");
    let manifest_bytes = br#"{"name":"dsh-profile-desktop","dependencies":{"dsh-git-ai-attribution":"file:bundle.tgz"},"dsh":{"profile":{"bundles":["dsh-git-ai-attribution"]}}}"#;
    fs::create_dir_all(&installed).unwrap();
    fs::write(&manifest, manifest_bytes).unwrap();
    fs::write(&patch, USER_PATCH).unwrap();
    let package_files = [
        (
            "package.json",
            include_str!("../../agent-support/deepseek-harness/package.json"),
        ),
        (
            "git-ai.mjs",
            include_str!("../../agent-support/deepseek-harness/git-ai.mjs"),
        ),
        (
            "git-ai.cordis.yml",
            include_str!("../../agent-support/deepseek-harness/git-ai.cordis.yml"),
        ),
    ];
    for (name, contents) in package_files {
        fs::write(installed.join(name), contents).unwrap();
    }

    assert_report(
        run(&repo, Some(&home), &["install-hooks"]),
        "DeepSeek Harness: Hooks already up to date",
    );
    run(&repo, Some(&home), &["uninstall-hooks"]);
    assert_eq!(fs::read(&manifest).unwrap(), manifest_bytes);
    assert_eq!(fs::read_to_string(&patch).unwrap(), USER_PATCH);
    assert!(!profile.join("local-plugins").exists());
    for (name, contents) in package_files {
        assert_eq!(fs::read_to_string(installed.join(name)).unwrap(), contents);
    }
}

#[test]
fn deepseek_harness_reports_and_preserves_a_manually_registered_plugin() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path().join("DSH home");
    let profile = home.join("profiles/desktop");
    let patch = profile.join("cordis.patch.yml");
    let user_patch = format!(
        "{USER_PATCH}- insert:\r\n    - id: git-ai-attribution\r\n      name: ./my-copy.mjs\r\n"
    );
    fs::create_dir_all(&profile).unwrap();
    fs::write(profile.join("package.json"), "{}\n").unwrap();
    fs::write(&patch, &user_patch).unwrap();

    let output = run(&repo, Some(&home), &["install-hooks"]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("git-ai-attribution"), "{stderr}");
    assert_eq!(fs::read_to_string(&patch).unwrap(), user_patch);
    assert!(!profile.join("local-plugins").exists());
    run(&repo, Some(&home), &["uninstall-hooks"]);
    assert_eq!(fs::read_to_string(&patch).unwrap(), user_patch);
}

#[test]
fn install_hooks_registers_deepseek_harness_without_rewriting_user_patch() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path().join("DSH home");
    let profile = home.join("profiles/desktop");
    let patch_path = profile.join("cordis.patch.yml");
    let manifest_path = profile.join("package.json");
    let user_patch =
        "\u{feff}# User configuration\r\n- id: user-plugin\r\n  disabled: !!js false\r\n";
    let manifest = r#"{"name":"dsh-profile-desktop","dsh":{"profile":{"bundles":[]}}}"#;
    fs::create_dir_all(&profile).unwrap();
    fs::write(&patch_path, user_patch).unwrap();
    fs::write(&manifest_path, manifest).unwrap();

    assert_report(
        run(&repo, Some(&home), &["install-hooks", "--dry-run"]),
        "DeepSeek Harness: Pending updates",
    );
    assert_eq!(fs::read_to_string(&patch_path).unwrap(), user_patch);
    assert!(!profile.join("local-plugins").exists());

    assert_report(
        run(&repo, Some(&home), &["install-hooks"]),
        "DeepSeek Harness: Hooks updated",
    );
    let installed = fs::read_to_string(&patch_path).unwrap();
    assert!(installed.starts_with(user_patch));
    assert!(installed.contains("    - id: git-ai-attribution\r\n"));
    let module = installed
        .lines()
        .find_map(|line| line.trim().strip_prefix("name: '")?.strip_suffix('\''))
        .unwrap();
    assert!(module.starts_with("./local-plugins/dsh-git-ai-attribution/"));
    assert!(
        fs::read_to_string(profile.join(module))
            .unwrap()
            .contains("export const name = 'git-ai-attribution'")
    );
    assert!(installed.contains("gitAiPath: "));
    assert_eq!(fs::read_to_string(&manifest_path).unwrap(), manifest);

    if let Some(dsh_root) = std::env::var_os("DSH_ROOT") {
        let loader = r#"
import assert from 'node:assert/strict'
import { existsSync } from 'node:fs'
import { isAbsolute } from 'node:path'
import { loadOptionalPatches } from './packages/boot/app-boot/src/index.ts'
const rows = loadOptionalPatches('dsh', process.argv[1]).flatMap(patch => patch.insert ?? [])
const [plugin] = rows.filter(row => row.id === 'git-ai-attribution')
assert.equal((await import(plugin.name)).name, 'git-ai-attribution')
assert.ok(isAbsolute(plugin.config.gitAiPath) && existsSync(plugin.config.gitAiPath))
"#;
        let output = std::process::Command::new("node")
            .args(["--import", "tsx", "--input-type=module", "--eval", loader])
            .arg(&patch_path)
            .current_dir(dsh_root)
            .output()
            .expect("run the Harness profile loader");
        assert!(output.status.success(), "{output:?}");
    }

    assert_report(
        run(&repo, Some(&home), &["install-hooks"]),
        "DeepSeek Harness: Hooks already up to date",
    );
    assert_eq!(fs::read_to_string(&patch_path).unwrap(), installed);

    let standalone = installed
        .split_inclusive('\n')
        .filter(|line| {
            !line.starts_with("      config:") && !line.starts_with("        gitAiPath:")
        })
        .collect::<String>();
    fs::write(&patch_path, standalone).unwrap();
    assert_report(
        run(&repo, Some(&home), &["install-hooks"]),
        "DeepSeek Harness: Hooks updated",
    );
    assert_eq!(fs::read_to_string(&patch_path).unwrap(), installed);
}

#[test]
fn deepseek_harness_reinstall_refreshes_the_configured_executable() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path().join("DSH home");
    let profile = home.join("profiles/desktop");
    let patch = profile.join("cordis.patch.yml");
    fs::create_dir_all(&profile).unwrap();
    fs::write(profile.join("package.json"), "{}\n").unwrap();
    fs::write(&patch, USER_PATCH).unwrap();
    run(&repo, Some(&home), &["install-hooks"]);

    let installed = fs::read_to_string(&patch).unwrap();
    let configured = installed
        .lines()
        .find(|line| line.trim_start().starts_with("gitAiPath:"))
        .unwrap();
    let extra = " # keep this comment\r\n        captureBash: true";
    let expected = installed.replacen(configured, &format!("{configured}{extra}"), 1);
    for value in [
        r#""/removed/git-ai""#,
        "'/removed/it''s git-ai'",
        "/removed/git-ai",
        r"C:\removed path\git-ai.exe",
    ] {
        let stale = format!("        gitAiPath: {value}{extra}");
        fs::write(&patch, installed.replacen(configured, &stale, 1)).unwrap();

        assert_report(
            run(&repo, Some(&home), &["install-hooks"]),
            "DeepSeek Harness: Hooks updated",
        );
        assert_eq!(fs::read_to_string(&patch).unwrap(), expected);
    }
    assert_report(
        run(&repo, Some(&home), &["install-hooks"]),
        "DeepSeek Harness: Hooks already up to date",
    );

    let unsupported = installed.replacen(
        configured,
        "        gitAiPath: >-\r\n          /removed/git-ai",
        1,
    );
    fs::write(&patch, &unsupported).unwrap();
    let output = run(&repo, Some(&home), &["install-hooks"]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Unsupported gitAiPath"), "{stderr}");
    assert_eq!(fs::read_to_string(&patch).unwrap(), unsupported);
}

#[test]
fn deepseek_harness_npm_upgrade_refreshes_the_previous_bundle() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path().join("DSH home");
    let profile = home.join("profiles/desktop");
    let patch = profile.join("cordis.patch.yml");
    let previous_module = format!("./local-plugins/{PACKAGE}/0.1.1/package/git-ai.mjs");
    let previous_file = profile.join(&previous_module);
    let previous_contents = "export const name = 'git-ai-attribution'\n";
    fs::create_dir_all(previous_file.parent().unwrap()).unwrap();
    fs::write(&previous_file, previous_contents).unwrap();
    let previous_manifest = previous_file.with_file_name("package.json");
    let previous_manifest_contents =
        r#"{"version":"0.1.1","engines":{"dsh":"0.2.1-rc.2 || 0.2.2-rc.1"}}"#;
    fs::write(&previous_manifest, previous_manifest_contents).unwrap();
    fs::write(profile.join("package.json"), "{}\n").unwrap();
    fs::write(
        &patch,
        format!(
            "{USER_PATCH}# BEGIN dsh-git-ai-attribution\r\n- insert:\r\n    - id: git-ai-attribution\r\n      name: '{previous_module}'\r\n      config:\r\n        captureBash: true\r\n# END dsh-git-ai-attribution\r\n"
        ),
    )
    .unwrap();

    let staged = repo.git_ai_command_without_pre_sync_for_test(&[], &[]);
    #[cfg(target_os = "macos")]
    let mut upgrade = {
        let mut command = Command::new("/bin/bash");
        command.arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"));
        command
    };
    #[cfg(windows)]
    let mut upgrade = {
        let mut command = Command::new("powershell");
        command
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.ps1"));
        command
    };
    let output = upgrade
        .envs(
            staged
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .env("GIT_AI_NPM_UPGRADE", "1")
        .env("GIT_AI_LOCAL_BINARY", staged.get_program())
        .env("GIT_AI_TEST_DSH_RESOURCES", "")
        .env("DSH_HOME", &home)
        .env_remove("GIT_AI_RESTART_DAEMON_AFTER_INSTALL")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let version = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../agent-support/deepseek-harness/package.json"
    ))
    .unwrap();
    let version = version["version"].as_str().unwrap();
    let module = format!("./local-plugins/{PACKAGE}/{version}/package/git-ai.mjs");
    let started = Instant::now();
    let updated = loop {
        let actual = fs::read_to_string(&patch).unwrap();
        if actual.contains(&format!("name: '{module}'")) {
            break actual;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "npm upgrade left the old plugin registered: {actual}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(updated.starts_with(USER_PATCH));
    assert!(updated.contains("captureBash: true"));
    let installed_file = profile.join(module);
    assert_eq!(
        fs::read_to_string(&installed_file).unwrap(),
        include_str!("../../agent-support/deepseek-harness/git-ai.mjs")
    );
    let manifest = fs::read_to_string(installed_file.with_file_name("package.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert!(manifest.pointer("/engines/dsh").is_none());
    assert_eq!(
        fs::read_to_string(previous_file).unwrap(),
        previous_contents
    );
    assert_eq!(
        fs::read_to_string(previous_manifest).unwrap(),
        previous_manifest_contents
    );
}

#[rstest::rstest]
#[case::uninstalled("{}", USER_PATCH)]
#[case::manual(
    "{}",
    "- insert:\r\n    - id: git-ai-attribution\r\n      name: ./my-copy.mjs\r\n"
)]
#[case::disabled(
    r#"{"dependencies":{"dsh-git-ai-attribution":"file:local-plugins/dsh-git-ai-attribution/bundles/0.1.1.tgz"},"dsh":{"profile":{"bundles":[]}}}"#,
    USER_PATCH
)]
fn deepseek_harness_upgrade_refresh_respects_plugin_selection(
    #[case] manifest: &str,
    #[case] patch: &str,
) {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path().join("DSH home");
    let profile = home.join("profiles/desktop");
    fs::create_dir_all(&profile).unwrap();
    fs::write(profile.join("package.json"), manifest).unwrap();
    fs::write(profile.join("cordis.patch.yml"), patch).unwrap();

    run(
        &repo,
        Some(&home),
        &["install-hooks", "--refresh-managed-deepseek"],
    );
    assert_eq!(
        fs::read_to_string(profile.join("package.json")).unwrap(),
        manifest
    );
    assert_eq!(
        fs::read_to_string(profile.join("cordis.patch.yml")).unwrap(),
        patch
    );
    assert!(!profile.join("local-plugins").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn deepseek_harness_keeps_the_old_registration_when_bundle_install_fails() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path().join("DSH home");
    let profile = home.join("profiles/desktop");
    let manifest = profile.join("package.json");
    let patch = profile.join("cordis.patch.yml");
    let original_manifest = r#"{"name":"dsh-profile-desktop","dsh":{"profile":{"bundles":[]}}}"#;
    let original_patch = format!(
        "{USER_PATCH}# BEGIN dsh-git-ai-attribution\r\n- insert:\r\n    - id: git-ai-attribution\r\n      name: './local-plugins/{PACKAGE}/0.1.1/package/git-ai.mjs'\r\n# END dsh-git-ai-attribution\r\n"
    );
    fs::create_dir_all(&profile).unwrap();
    fs::write(&manifest, original_manifest).unwrap();
    fs::write(&patch, &original_patch).unwrap();

    for status in [1, 0] {
        let runtime = fake_desktop(&repo, status);
        let output = run_with_runtime(&repo, Some(&home), &["install-hooks"], &runtime);
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("DeepSeek Harness: Failed to update hooks")
        );
        assert_eq!(fs::read_to_string(&manifest).unwrap(), original_manifest);
        assert_eq!(fs::read_to_string(&patch).unwrap(), original_patch);
        assert!(!profile.join("package.json.lock").exists());
        fs::remove_file(profile.join("dsh-add-called"))
            .expect("the bundled dsh command must run without the profile lock held by git-ai");
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires the installed DeepSeek Harness Desktop runtime"]
fn deepseek_harness_migrates_legacy_hook_to_an_installed_desktop_bundle() {
    let runtime = Path::new("/Applications/DeepSeek Harness.app/Contents/Resources");
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path().join("DSH home");
    let profile = home.join("profiles/desktop");
    let manifest = profile.join("package.json");
    let patch = profile.join("cordis.patch.yml");
    let legacy = profile
        .join("local-plugins")
        .join(PACKAGE)
        .join("0.1.1/package");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("git-ai.mjs"), "export const name = 'old'\n").unwrap();
    fs::write(
        &manifest,
        r#"{"name":"dsh-profile-desktop","private":true,"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","@deepseek-ai/dsh-web-app"]}},"testProfileField":"keep"}"#,
    )
    .unwrap();
    let original = format!(
        "{USER_PATCH}# BEGIN dsh-git-ai-attribution\r\n- insert:\r\n    - id: git-ai-attribution\r\n      name: './local-plugins/{PACKAGE}/0.1.1/package/git-ai.mjs'\r\n      config:\r\n        gitAiPath: /removed/git-ai # retain this comment\r\n        captureBash: true\r\n# END dsh-git-ai-attribution\r\n"
    );
    fs::write(&patch, &original).unwrap();
    let original_manifest = fs::read(&manifest).unwrap();
    let run = |args: &[&str]| run_with_runtime(&repo, Some(&home), args, runtime);

    assert_report(
        run(&["install-hooks", "--dry-run"]),
        "DeepSeek Harness: Pending updates",
    );
    assert_eq!(fs::read(&manifest).unwrap(), original_manifest);
    assert_eq!(fs::read_to_string(&patch).unwrap(), original);
    assert!(!profile.join("node_modules").exists());
    assert_report(run(&["install-hooks"]), "DeepSeek Harness: Hooks updated");
    let updated: serde_json::Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert!(
        updated["dependencies"][PACKAGE].is_string(),
        "The Harness plugin panel needs {PACKAGE} in the Desktop profile's dependencies"
    );
    assert_eq!(updated["testProfileField"], "keep");
    assert!(
        updated["dsh"]["profile"]["bundles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == PACKAGE)
    );
    assert_eq!(
        fs::read_to_string(
            profile
                .join("node_modules")
                .join(PACKAGE)
                .join("git-ai.mjs")
        )
        .unwrap(),
        include_str!("../../agent-support/deepseek-harness/git-ai.mjs")
    );
    let patched = fs::read_to_string(&patch).unwrap();
    assert!(patched.starts_with(USER_PATCH));
    assert!(patched.contains("    captureBash: true\r\n"));
    assert!(patched.contains(" # retain this comment\r\n"));
    assert!(!patched.contains("/removed/git-ai"));
    assert!(!patched.contains("0.1.1/package/git-ai.mjs"));
    assert!(legacy.join("git-ai.mjs").is_file());
    assert_desktop_bundle_running(runtime, &home, &profile, "git-ai-attribution");

    assert_report(
        run(&["install-hooks"]),
        "DeepSeek Harness: Hooks already up to date",
    );
    assert_eq!(fs::read_to_string(&patch).unwrap(), patched);

    // Simulate an interruption just before bundle selection was saved.
    let mut deselected = updated.clone();
    deselected["dsh"]["profile"]["bundles"]
        .as_array_mut()
        .unwrap()
        .retain(|name| name != PACKAGE);
    let deselected = serde_json::to_string_pretty(&deselected).unwrap();
    let pending = patched.replace(
        "- id: git-ai-attribution\r\n",
        "- id: git-ai-attribution # pending bundle selection\r\n",
    );
    fs::write(&manifest, &deselected).unwrap();
    fs::write(&patch, pending).unwrap();
    assert_report(run(&["install-hooks"]), "DeepSeek Harness: Hooks updated");
    assert_eq!(fs::read_to_string(&patch).unwrap(), patched);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&manifest).unwrap()).unwrap(),
        updated
    );

    // A later disable from the panel must stay disabled on reinstall.
    fs::write(&manifest, &deselected).unwrap();
    assert_report(
        run(&["install-hooks"]),
        "DeepSeek Harness: Hooks already up to date",
    );
    assert_eq!(fs::read_to_string(&manifest).unwrap(), deselected);
    fs::write(&manifest, serde_json::to_string(&updated).unwrap()).unwrap();

    assert_report(
        run(&["uninstall-hooks", "--dry-run"]),
        "DeepSeek Harness: Pending removal",
    );
    assert_eq!(fs::read_to_string(&patch).unwrap(), patched);
    assert_report(run(&["uninstall-hooks"]), "DeepSeek Harness: Hooks removed");
    let removed =
        serde_json::from_slice::<serde_json::Value>(&fs::read(&manifest).unwrap()).unwrap();
    assert!(removed["dependencies"].get(PACKAGE).is_none());
    assert!(
        !removed["dsh"]["profile"]["bundles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == PACKAGE)
    );
    assert_eq!(removed["testProfileField"], "keep");
    assert_eq!(fs::read_to_string(&patch).unwrap(), USER_PATCH);
    assert!(!profile.join("node_modules").join(PACKAGE).exists());

    // Install again with no legacy row. The retained archive is reusable.
    assert_report(run(&["install-hooks"]), "DeepSeek Harness: Hooks updated");
    let reinstalled =
        serde_json::from_slice::<serde_json::Value>(&fs::read(&manifest).unwrap()).unwrap();
    assert!(
        reinstalled["dsh"]["profile"]["bundles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == PACKAGE)
    );
    assert_report(
        run(&["install-hooks"]),
        "DeepSeek Harness: Hooks already up to date",
    );
}

#[cfg(target_os = "macos")]
fn assert_desktop_bundle_running(runtime: &Path, home: &Path, profile: &Path, id: &str) {
    let script = r#"
import assert from 'node:assert/strict'
import { isAbsolute, join } from 'node:path'
import { pathToFileURL } from 'node:url'
const [resources, dir, id] = process.argv.slice(1)
const packages = join(resources, 'app.asar/dsh/node_modules/@deepseek-ai')
const load = path => import(pathToFileURL(join(packages, path)).href)
const { loadProfileDirectory, loadLayeredEnv } = await load('dsh-app-boot/lib/index.js')
const { runProfile } = await load('dsh/lib/profile-boot.js')
const { readPluginInventory } = await load('dsh-host-plugin-inventory/lib/index.js')
const installAnchor = join(packages, 'dsh/package.json')
const profile = loadProfileDirectory('dsh', dir, installAnchor)
const { ctx, shutdown } = await runProfile({
  environment: loadLayeredEnv('dsh'),
  profile: 'desktop',
  resolvedProfile: { profile, installAnchor },
  patchFiles: [],
  args: ['--no-open', '--host', '127.0.0.1', '--port', '0'],
})
try {
  const bundle = (await ctx.pluginManager.listBundles()).find(bundle => bundle.name === 'dsh-' + id)
  assert.ok(bundle?.installed && bundle.enabled, JSON.stringify(bundle))
  const { entries } = await readPluginInventory(ctx)
  const rows = entries.filter(entry => entry.entryId.endsWith(':' + id))
  assert.equal(rows.length, 1, JSON.stringify(rows))
  assert.equal(rows[0].fiberPhase, 'active', JSON.stringify(rows))
  const plugin = [...ctx.loader.entries()].find(entry => entry.id.endsWith(':' + id))
  assert.ok(isAbsolute(plugin.options.config.gitAiPath))
  assert.equal(plugin.options.config.captureBash, true)
} catch (error) {
  console.error(error)
  process.exitCode = 1
} finally {
  await shutdown.shutdown(process.exitCode || 0)
}
"#;
    let output = std::process::Command::new(runtime.join("../MacOS/DeepSeek Harness"))
        .args([
            "--expose-internals",
            "--input-type=module",
            "--eval",
            script,
            "--",
        ])
        .arg(runtime)
        .arg(profile)
        .arg(id)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .env("DSH_HOME", home)
        .env("DSH_TELEMETRY_DISABLED", "1")
        .env(
            "DSH_PRIMARY_RUNTIME",
            runtime.join("runtime/primary-runtime"),
        )
        .current_dir(home)
        .output()
        .expect("boot the Desktop profile using the installed Harness runtime");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn deepseek_harness_recovers_the_lock_of_an_exited_installer() {
    use sha2::{Digest, Sha256};

    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path().join("DSH home");
    let profile = home.join("profiles/desktop");
    let patch = profile.join("cordis.patch.yml");
    let lock = profile.join("package.json.lock");
    fs::create_dir_all(&profile).unwrap();
    fs::write(profile.join("package.json"), "{}\n").unwrap();
    fs::write(&patch, USER_PATCH).unwrap();

    let mut exited = repo
        .git_ai_command_without_pre_sync_for_test(&["--version"], &[])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(exited.wait().unwrap().success());
    let stale_record = format!("{}\n", exited.id());
    fs::write(&lock, &stale_record).unwrap();

    let digest = format!("{:x}", Sha256::digest(stale_record.as_bytes()));
    let desktop_claim = profile.join(format!("package.json.lock.takeover-{}", &digest[..16]));
    fs::write(&desktop_claim, format!("{}\n", std::process::id())).unwrap();
    let blocked = run(&repo, Some(&home), &["install-hooks"]);
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("profile lock"));
    assert_eq!(fs::read_to_string(&lock).unwrap(), stale_record);
    assert_eq!(fs::read_to_string(&patch).unwrap(), USER_PATCH);
    fs::remove_file(&desktop_claim).unwrap();

    assert_report(
        run(&repo, Some(&home), &["install-hooks"]),
        "DeepSeek Harness: Hooks updated",
    );
    assert!(
        fs::read_to_string(&patch)
            .unwrap()
            .contains("git-ai-attribution")
    );
    assert!(!lock.exists());
    assert!(!desktop_claim.exists());

    fs::write(&lock, stale_record).unwrap();
    assert_report(
        run(&repo, Some(&home), &["uninstall-hooks"]),
        "DeepSeek Harness: Hooks removed",
    );
    assert_eq!(fs::read_to_string(&patch).unwrap(), USER_PATCH);
    assert!(!lock.exists());
}

#[test]
fn deepseek_harness_leaves_the_profile_unchanged_when_locked_or_unsupported() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path().join("DSH home");
    let profile = home.join("profiles/desktop");
    let patch = profile.join("cordis.patch.yml");
    let lock = profile.join("package.json.lock");
    fs::create_dir_all(&profile).unwrap();
    fs::write(profile.join("package.json"), "{}\n").unwrap();

    fs::write(&patch, USER_PATCH).unwrap();
    for lock_record in [format!("{}\n", std::process::id()), String::new()] {
        fs::write(&lock, &lock_record).unwrap();
        let locked = run(&repo, Some(&home), &["install-hooks"]);
        let stderr = String::from_utf8(locked.stderr).unwrap();
        assert!(stderr.contains("profile lock"), "{stderr}");
        assert_eq!(fs::read_to_string(&lock).unwrap(), lock_record);
        assert_eq!(fs::read_to_string(&patch).unwrap(), USER_PATCH);
        assert!(!profile.join("local-plugins").exists());
    }
    fs::remove_file(&lock).unwrap();

    let flow_patch = "# Valid YAML outside the installer's editing scope\r\n[{id: user-plugin, disabled: false}]\r\n";
    fs::write(&patch, flow_patch).unwrap();
    let unsupported = run(&repo, Some(&home), &["install-hooks"]);
    let stderr = String::from_utf8(unsupported.stderr).unwrap();
    assert!(stderr.contains("YAML block sequence"), "{stderr}");
    assert_eq!(fs::read_to_string(&patch).unwrap(), flow_patch);
    assert!(!profile.join("local-plugins").exists());
    assert!(!lock.exists());

    fs::write(&patch, "# Default patch\r\n[]\r\n").unwrap();
    run(&repo, Some(&home), &["install-hooks"]);
    let installed = fs::read_to_string(&patch).unwrap();
    fs::write(&patch, format!("{installed}# User comment without newline")).unwrap();
    assert_report(
        run(&repo, Some(&home), &["uninstall-hooks"]),
        "DeepSeek Harness: Hooks removed",
    );
    let remaining = fs::read_to_string(&patch).unwrap();
    assert!(remaining.ends_with("# User comment without newline\r\n[]\r\n"));
}
