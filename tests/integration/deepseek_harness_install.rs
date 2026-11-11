#[cfg(target_os = "macos")]
use crate::install_hooks_comprehensive::stub_cursor;
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use std::{fs, path::Path, process::Output};

const PACKAGE: &str = "dsh-git-ai-attribution";
const USER_PATCH: &str = "# Keep this comment\r\n- id: user-plugin\r\n  disabled: !!js false\r\n";

fn run(repo: &TestRepo, home: Option<&Path>, args: &[&str]) -> Output {
    let mut command = repo.git_ai_command_without_pre_sync_for_test(args, &[]);
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
    assert!(stdout.contains(expected), "{stdout}");
    let stderr = String::from_utf8(output.stderr).unwrap();
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
fn deepseek_harness_upgrades_the_previous_bundle_without_replacing_its_files() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path().join("DSH home");
    let profile = home.join("profiles/desktop");
    let patch = profile.join("cordis.patch.yml");
    let previous_module = format!("./local-plugins/{PACKAGE}/0.1.0/package/git-ai.mjs");
    let previous_file = profile.join(&previous_module);
    let previous_contents = "export const name = 'git-ai-attribution'\n";
    fs::create_dir_all(previous_file.parent().unwrap()).unwrap();
    fs::write(&previous_file, previous_contents).unwrap();
    fs::write(profile.join("package.json"), "{}\n").unwrap();
    fs::write(
        &patch,
        format!(
            "{USER_PATCH}# BEGIN dsh-git-ai-attribution\r\n- insert:\r\n    - id: git-ai-attribution\r\n      name: '{previous_module}'\r\n# END dsh-git-ai-attribution\r\n"
        ),
    )
    .unwrap();

    assert_report(
        run(&repo, Some(&home), &["install-hooks"]),
        "DeepSeek Harness: Hooks updated",
    );
    let updated = fs::read_to_string(&patch).unwrap();
    let version = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../agent-support/deepseek-harness/package.json"
    ))
    .unwrap();
    let version = version["version"].as_str().unwrap();
    let module = format!("./local-plugins/{PACKAGE}/{version}/package/git-ai.mjs");
    assert!(updated.starts_with(USER_PATCH));
    assert!(updated.contains(&format!("name: '{module}'")));
    assert_eq!(
        fs::read_to_string(profile.join(module)).unwrap(),
        include_str!("../../agent-support/deepseek-harness/git-ai.mjs")
    );
    assert_eq!(
        fs::read_to_string(previous_file).unwrap(),
        previous_contents
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
