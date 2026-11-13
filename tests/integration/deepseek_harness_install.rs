use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const PLUGIN: &str = "git-ai-deepseek-harness";

struct DesktopProfile {
    repo: TestRepo,
    cli: PathBuf,
    home: PathBuf,
    profile: PathBuf,
}

impl DesktopProfile {
    fn new() -> Self {
        let app = std::env::var_os("GIT_AI_TEST_DEEPSEEK_HARNESS_APP")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Applications/DeepSeek Harness.app"));
        let cli = app.join("Contents/Resources/runtime/cli/bin/dsh");
        assert!(cli.is_file(), "official Desktop CLI missing at {cli:?}");

        let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
        let test_home = repo.test_home_path();
        let shim = test_home
            .join("Applications/DeepSeek Harness.app/Contents/Resources/runtime/cli/bin/dsh");
        fs::create_dir_all(shim.parent().unwrap()).unwrap();
        fs::write(
            &shim,
            "#!/bin/sh\nset -eu\nprintf '%s\n' \"$4\" >> \"$DSH_HOME/git-ai-plugin-commands.log\"\nexec \"$GIT_AI_TEST_DESKTOP_DSH\" \"$@\"\n",
        ).unwrap();
        fs::set_permissions(shim, fs::Permissions::from_mode(0o755)).unwrap();
        let home = test_home.join("Harness home");
        fs::create_dir_all(home.join("profiles")).unwrap();
        let profile = home.join("profiles/desktop");
        let fixture = Self {
            repo,
            cli,
            home,
            profile,
        };
        fixture.make_editor_shims();
        fixture.dsh(&["--profile", "web", "--dump-config"]);
        fs::rename(fixture.home.join("profiles/web"), &fixture.profile).unwrap();
        let mut manifest = fixture.manifest();
        manifest["name"] = json!("dsh-profile-desktop");
        fixture.save_manifest(&manifest);
        fixture
    }

    fn make_editor_shims(&self) {
        let bin = self.repo.test_home_path().join("fixture-bin");
        fs::create_dir_all(&bin).unwrap();
        for editor in ["code", "cursor", "windsurf"] {
            let path = bin.join(editor);
            fs::write(
                &path,
                "#!/bin/sh\ncase \"$1\" in\n  --version) echo 99.0.0 ;;\n  --list-extensions) echo git-ai.git-ai-vscode ;;\n  *) exit 0 ;;\nesac\n",
            ).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn dsh(&self, args: &[&str]) {
        let command = self.repo.git_ai_command_without_pre_sync_for_test(&[], &[]);
        let mut cli = std::process::Command::new(&self.cli);
        cli.args(args)
            .current_dir(self.repo.path())
            .envs(
                command
                    .get_envs()
                    .filter_map(|(key, value)| value.map(|value| (key, value))),
            )
            .env("DSH_HOME", &self.home)
            .env("npm_config_store_dir", self.home.join("store"))
            .env("npm_config_cache", self.home.join("npm-cache"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("DEEPSEEK_API_KEY", "fixture")
            .env("DEEPSEEK_BASE_URL", "http://127.0.0.1:9")
            .stdin(std::process::Stdio::null());
        let result = cli.output().unwrap();
        assert!(
            result.status.success(),
            "dsh {args:?}\n{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    fn install(&self, args: &[&str]) -> String {
        let mut command = self
            .repo
            .git_ai_command_without_pre_sync_for_test(args, &[]);
        let path = command
            .get_envs()
            .find(|(name, _)| name == &"PATH")
            .and_then(|(_, value)| value)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| std::env::var_os("PATH").unwrap());
        let path = std::env::join_paths(
            [self.repo.test_home_path().join("fixture-bin")]
                .into_iter()
                .chain(std::env::split_paths(&path)),
        )
        .unwrap();
        let output = command
            .env("PATH", path)
            .env("DSH_HOME", &self.home)
            .env("GIT_AI_TEST_DESKTOP_DSH", &self.cli)
            .env("npm_config_offline", "true")
            .env("npm_config_ignore_scripts", "true")
            .env("npm_config_store_dir", self.home.join("store"))
            .env("npm_config_cache", self.home.join("npm-cache"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let output_text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{output_text}");
        output_text
    }

    fn plugin_commands(&self) -> String {
        fs::read_to_string(self.home.join("git-ai-plugin-commands.log")).unwrap_or_default()
    }

    fn manifest(&self) -> Value {
        read_json(&self.profile.join("package.json"))
    }

    fn save_manifest(&self, manifest: &Value) {
        fs::write(
            self.profile.join("package.json"),
            serde_json::to_vec_pretty(manifest).unwrap(),
        )
        .unwrap();
    }

    fn seed_plugin(&self, version: &str, enabled: bool, protocol: &str) {
        let package = self.home.join("fixture package").join(version);
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("package.json"),
            serde_json::to_vec_pretty(&json!({
                "name": PLUGIN,
                "version": version,
                "type": "module",
                "exports": {"./index.mjs": "./index.mjs"},
                "dsh": {"bundle": {"patch": "./cordis.patch.yml"}},
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(package.join("index.mjs"), "export function apply() {}\n").unwrap();
        fs::write(
            package.join("cordis.patch.yml"),
            format!("- insert:\n  - id: {PLUGIN}\n    name: {PLUGIN}/index.mjs\n"),
        )
        .unwrap();
        self.dsh(&[
            "plugin",
            "--profile",
            "desktop",
            "add",
            "--offline",
            "--ignore-scripts",
            &format!("{protocol}:{}", package.display()),
        ]);
        if !enabled {
            let mut manifest = self.manifest();
            manifest["dsh"]["profile"]["bundles"]
                .as_array_mut()
                .unwrap()
                .retain(|name| name != PLUGIN);
            self.save_manifest(&manifest);
        }
        self.assert_state(version, enabled);
    }

    fn assert_state(&self, version: &str, enabled: bool) {
        let package = self
            .profile
            .join("node_modules")
            .join(PLUGIN)
            .join("package.json");
        assert_eq!(read_json(&package)["version"], version);
        let manifest = self.manifest();
        assert!(manifest["dependencies"][PLUGIN].as_str().is_some());
        assert_eq!(
            manifest["dsh"]["profile"]["bundles"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == PLUGIN),
            enabled
        );
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap_or_else(|error| panic!("read {path:?}: {error}")))
        .unwrap()
}

#[test]
#[ignore = "requires the official macOS Desktop package"]
fn deepseek_harness_install_official_cli_version_and_enablement() {
    for (before, enabled, expected_version, expected_enabled, add) in [
        (None, false, "0.2.4", true, true),
        (Some("0.2.2"), true, "0.2.4", true, true),
        (Some("0.2.2"), false, "0.2.4", false, true),
        (Some("0.2.3"), true, "0.2.4", true, true),
        (Some("0.2.3"), false, "0.2.4", false, true),
        (Some("0.2.4-rc.1"), true, "0.2.4", true, true),
        (Some("0.2.4"), false, "0.2.4", false, false),
        (Some("0.2.4"), true, "0.2.4", true, false),
        (Some("0.3.0"), true, "0.3.0", true, false),
        (Some("0.3.0"), false, "0.3.0", false, false),
    ] {
        let fixture = DesktopProfile::new();
        if let Some(version) = before {
            let protocol = if version == "0.2.2" && !enabled {
                "file"
            } else {
                "link"
            };
            fixture.seed_plugin(version, enabled, protocol);
        }
        let patch = fixture.profile.join("cordis.patch.yml");
        let user_patch = if before == Some("0.2.2") && enabled {
            "# User configuration\n- id: git-ai-deepseek-harness\n  name: git-ai-deepseek-harness/index.mjs\n  disabled: true\n"
        } else {
            "# User configuration\n[]\n"
        };
        fs::write(&patch, user_patch).unwrap();
        let path = fixture.profile.join("package.json");
        let manifest = fs::read(&path).unwrap();
        let lockfile = fixture.profile.join("pnpm-lock.yaml");
        let lock = fs::read(&lockfile).ok();
        let preview = fixture.install(&["install", "--dry-run", "--verbose"]);
        assert!(
            preview.contains(if add {
                "DeepSeek Harness: Pending updates"
            } else {
                "DeepSeek Harness: Hooks already up to date"
            }),
            "{before:?}, enabled {enabled}\n{preview}"
        );
        assert_eq!(fs::read(&path).unwrap(), manifest);
        assert_eq!(fs::read(&lockfile).ok(), lock);
        assert!(fixture.plugin_commands().is_empty());

        let result = fixture.install(&["install"]);
        assert!(
            result.contains(if add {
                "DeepSeek Harness: Hooks updated"
            } else {
                "DeepSeek Harness: Hooks already up to date"
            }),
            "{before:?}, enabled {enabled}\n{result}"
        );
        fixture.assert_state(expected_version, expected_enabled);
        if !add {
            assert_eq!(fs::read(&path).unwrap(), manifest);
            assert_eq!(fs::read(&lockfile).ok(), lock);
        }
        let updated = fs::read(&path).unwrap();
        assert!(
            fixture
                .install(&["install"])
                .contains("DeepSeek Harness: Hooks already up to date")
        );
        assert_eq!(fs::read(&path).unwrap(), updated);
        assert_eq!(fixture.plugin_commands(), if add { "add\n" } else { "" });
        assert_eq!(fs::read_to_string(&patch).unwrap(), user_patch);

        if before.is_none() {
            assert!(
                fixture
                    .install(&["uninstall", "--dry-run"])
                    .contains("DeepSeek Harness: Pending removal")
            );
            assert_eq!(fixture.plugin_commands(), "add\n");
            fixture.assert_state("0.2.4", true);
            assert!(
                fixture
                    .install(&["uninstall"])
                    .contains("DeepSeek Harness: Hooks removed")
            );
            assert_eq!(fixture.plugin_commands(), "add\nremove\n");
            assert!(fixture.manifest()["dependencies"][PLUGIN].is_null());
        }
    }
}

#[test]
fn deepseek_harness_install_leaves_profile_to_dsh_and_retries() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path().join("Harness home");
    let profile = home.join("profiles/desktop");
    let cli = repo
        .test_home_path()
        .join("Applications/DeepSeek Harness.app/Contents/Resources/runtime/cli/bin/dsh");
    fs::create_dir_all(&profile).unwrap();
    fs::create_dir_all(cli.parent().unwrap()).unwrap();
    let fixture = DesktopProfile {
        repo,
        home,
        profile,
        cli,
    };
    fixture.make_editor_shims();

    let manifest_path = fixture.profile.join("package.json");
    let initial = b"{\n  \"name\": \"dsh-profile-desktop\",\n  \"dependencies\": {},\n  \"dsh\": { \"profile\": { \"bundles\": [\"fixture-other\"] } }\n}\n";
    fs::write(&manifest_path, initial).unwrap();
    let patch = fixture.profile.join("cordis.patch.yml");
    fs::write(&patch, "# User configuration\n[]\n").unwrap();
    fs::write(
        &fixture.cli,
        "#!/bin/sh\nset -eu\ntest \"$#\" -eq 5\ntest \"$1 $2 $3 $4\" = 'plugin --profile desktop add'\ncase \"$5\" in link:*) ;; *) exit 9 ;; esac\ncp \"$DSH_HOME/profiles/desktop/package.json\" \"$DSH_HOME/manifest-at-add.json\"\necho 'fixture package manager interrupted before installation' >&2\nexit 1\n",
    ).unwrap();
    fs::set_permissions(&fixture.cli, fs::Permissions::from_mode(0o755)).unwrap();
    for _ in 0..2 {
        let output = fixture.install(&["install"]);
        assert!(
            fixture
                .repo
                .test_home_path()
                .join(".cursor/hooks.json")
                .is_file()
        );
        assert!(!output.contains("Cursor: Extension"), "{output}");
        assert!(!output.contains("VS Code:"), "{output}");
        assert!(
            output.contains("DeepSeek Harness: Failed to update hooks"),
            "{output}"
        );
        assert!(
            output.contains("fixture package manager interrupted before installation"),
            "{output}"
        );
        assert_eq!(
            fs::read(fixture.home.join("manifest-at-add.json")).unwrap(),
            initial
        );
        assert_eq!(fs::read(&manifest_path).unwrap(), initial);
        assert_eq!(
            fs::read_to_string(&patch).unwrap(),
            "# User configuration\n[]\n"
        );
        assert!(!fixture.profile.join("node_modules").exists());
        fs::remove_file(fixture.home.join("manifest-at-add.json")).unwrap();
    }
}
