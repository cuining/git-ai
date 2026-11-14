use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{generate_diff, home_dir, write_atomic};
use crate::utils::LockFile;
use semver::Version;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PACKAGE: &str = "git-ai-deepseek-harness";
const MANIFEST: &str = include_str!("../../../agent-support/deepseek-harness/package.json");
const PATCH: &str = include_str!("../../../agent-support/deepseek-harness/cordis.patch.yml");
const MODULE: &str = include_str!("../../../agent-support/deepseek-harness/index.mjs");
const MANAGED_DIR: &str = ".git-ai-plugin-bundles";
const DESKTOP: &str = "DeepSeek Harness";

#[cfg(target_os = "macos")]
const CLI_PATH: &str = "Contents/Resources/runtime/cli/bin/dsh";
#[cfg(windows)]
const CLI_PATH: &str = "resources/runtime/cli/bin/dsh.cmd";

pub struct DeepSeekHarnessInstaller;

#[derive(Deserialize)]
struct Bundle {
    name: String,
    version: String,
}

struct PluginState {
    spec: Option<String>,
    version: Option<Version>,
    enabled: bool,
}

impl PluginState {
    fn read(profile: &Path) -> Result<Self, GitAiError> {
        let manifest: Value = serde_json::from_slice(&fs::read(profile.join("package.json"))?)?;
        if !manifest.is_object() {
            return Err(GitAiError::Generic(
                "Invalid DeepSeek Harness profile manifest".into(),
            ));
        }
        let spec = manifest["dependencies"]
            .get(PACKAGE)
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    GitAiError::Generic(format!(
                        "Invalid DeepSeek Harness dependency spec for {PACKAGE}"
                    ))
                })
            })
            .transpose()?;
        let enabled = manifest["dsh"]["profile"]["bundles"]
            .as_array()
            .is_some_and(|bundles| bundles.iter().any(|bundle| bundle == PACKAGE));
        let version = if spec.is_some() || enabled {
            bundle_version(&profile.join("node_modules").join(PACKAGE))?
        } else {
            None
        };

        Ok(Self {
            spec,
            version,
            enabled,
        })
    }

    fn is_up_to_date(&self, bundled: &Version) -> bool {
        self.version
            .as_ref()
            .is_some_and(|installed| !installed.cmp_precedence(bundled).is_lt())
    }

    fn description(&self) -> String {
        self.version
            .as_ref()
            .map(|version| describe_bundle(version, self.enabled))
            .unwrap_or_default()
    }
}

fn describe_bundle(version: &Version, enabled: bool) -> String {
    let selection = if enabled { "enabled" } else { "disabled" };
    format!("{PACKAGE} {version} ({selection})\n")
}

fn bundle_version(dir: &Path) -> Result<Option<Version>, GitAiError> {
    let path = dir.join("package.json");
    let contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let bundle: Bundle = serde_json::from_slice(&contents)?;
    if bundle.name != PACKAGE {
        return Err(GitAiError::Generic(format!(
            "Unexpected package {} at {}",
            bundle.name,
            path.display()
        )));
    }
    parse_version(&bundle.version).map(Some)
}

fn parse_version(version: &str) -> Result<Version, GitAiError> {
    Version::parse(version).map_err(|error| {
        GitAiError::Generic(format!("Invalid {PACKAGE} version {version:?}: {error}"))
    })
}

fn bundled_version() -> Result<Version, GitAiError> {
    let bundle: Bundle = serde_json::from_str(MANIFEST)?;
    parse_version(&bundle.version)
}

struct DesktopProfile {
    home: PathBuf,
    dir: PathBuf,
    cli: PathBuf,
}

impl DesktopProfile {
    fn detect() -> Result<Option<Self>, GitAiError> {
        let user_home = home_dir();
        let configured = std::env::var("DSH_HOME").ok();
        let home = match configured
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            Some("~") => user_home.clone(),
            Some(path) => match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
                Some(relative) => user_home.join(relative),
                None => PathBuf::from(path),
            },
            None => user_home.join(".dsh"),
        };
        let home = std::path::absolute(home)?;
        let dir = home.join("profiles").join("desktop");
        match fs::metadata(dir.join("package.json")) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        #[cfg(target_os = "macos")]
        let cli = desktop_cli(&user_home);
        #[cfg(windows)]
        let cli = desktop_cli();
        Ok(cli.map(|cli| Self { home, dir, cli }))
    }

    fn lock(&self) -> Result<LockFile, GitAiError> {
        let path = self.dir.join(".git-ai-plugin-install.lock");
        LockFile::try_acquire(&path)
            .ok_or_else(|| GitAiError::Generic(format!("Could not lock {}", path.display())))
    }

    fn manage(&self, operation: &str, spec: &str) -> Result<(), GitAiError> {
        #[cfg(target_os = "macos")]
        let mut command = Command::new(&self.cli);
        #[cfg(windows)]
        let mut command = {
            use std::os::windows::process::CommandExt;
            let app = self.cli.ancestors().nth(5).ok_or_else(|| {
                GitAiError::Generic("Invalid DeepSeek Harness CLI location".into())
            })?;
            let mut command = Command::new(app.join("DeepSeek Harness.exe"));
            // Invoke the same Electron entry as dsh.cmd without a cmd.exe argument round-trip.
            command
                .arg("--expose-internals")
                .arg(app.join(
                    "resources/app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/cli.js",
                ))
                .env("ELECTRON_RUN_AS_NODE", "1")
                .creation_flags(crate::utils::CREATE_NO_WINDOW);
            command
        };
        let output = command
            .args(["plugin", "--profile", "desktop", operation, spec])
            .env("DSH_HOME", &self.home)
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            let diagnostic = if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            };
            return Err(GitAiError::Generic(format!(
                "dsh plugin {operation} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(diagnostic).trim()
            )));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn desktop_cli(user_home: &Path) -> Option<PathBuf> {
    find_desktop_cli(
        "dsh",
        [
            user_home
                .join("Applications/DeepSeek Harness.app")
                .join(CLI_PATH),
            Path::new("/Applications/DeepSeek Harness.app").join(CLI_PATH),
            PathBuf::from("/usr/local/bin/dsh"),
        ],
    )
}

#[cfg(windows)]
fn desktop_cli() -> Option<PathBuf> {
    let default = std::env::var_os("LOCALAPPDATA")
        .map(|path| PathBuf::from(path).join("Programs/DeepSeek Harness"));
    find_desktop_cli(
        "dsh.cmd",
        registered_windows_install()
            .into_iter()
            .chain(default)
            .map(|app| app.join(CLI_PATH)),
    )
}

fn find_desktop_cli(
    command: &str,
    candidates: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    let path_entries = std::env::var_os("PATH").unwrap_or_default();
    candidates
        .into_iter()
        .chain(std::env::split_paths(&path_entries).map(|path| path.join(command)))
        .filter_map(|path| path.canonicalize().ok().map(crate::mdm::utils::clean_path))
        .find(|path| path.is_file() && path.ends_with(CLI_PATH))
}

#[cfg(windows)]
fn registered_windows_install() -> Option<PathBuf> {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};
    let uninstall = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Uninstall")
        .ok()?;
    uninstall.enum_keys().flatten().find_map(|key| {
        let key = uninstall.open_subkey(key).ok()?;
        if key.get_value::<String, _>("DisplayName").ok()?.as_str() != DESKTOP {
            return None;
        }
        let app = PathBuf::from(key.get_value::<String, _>("InstallLocation").ok()?);
        app.join(CLI_PATH).is_file().then_some(app)
    })
}

fn stage_bundle(profile: &Path, version: &Version, binary: &Path) -> Result<String, GitAiError> {
    let binary = binary
        .to_str()
        .ok_or_else(|| GitAiError::Generic("git-ai executable path is not UTF-8".into()))?;
    let definition_prefix = "const gitAi = ";
    let definition = MODULE
        .lines()
        .find(|line| line.starts_with(definition_prefix))
        .ok_or_else(|| {
            GitAiError::Generic("missing executable definition in Harness plugin".into())
        })?;
    let module = MODULE.replacen(
        definition,
        &format!("{definition_prefix}{}", serde_json::to_string(binary)?),
        1,
    );
    let mut digest = Sha256::new();
    for asset in [MANIFEST, PATCH, &module] {
        digest.update(asset.as_bytes());
    }
    let dir = profile
        .join(MANAGED_DIR)
        .join(format!("{version}-{:x}", digest.finalize()));
    for (name, asset) in [
        ("index.mjs", module.as_str()),
        ("cordis.patch.yml", PATCH),
        ("package.json", MANIFEST),
    ] {
        let path = dir.join(name);
        if fs::read(&path).ok().as_deref() != Some(asset.as_bytes()) {
            write_atomic(&path, asset.as_bytes())?;
        }
    }
    let path = dir
        .to_str()
        .ok_or_else(|| GitAiError::Generic("DeepSeek Harness profile path is not UTF-8".into()))?;
    Ok(format!("link:{path}"))
}

fn check_missing_dependency_version(
    profile: &Path,
    state: &PluginState,
    bundled: &Version,
) -> Result<(), GitAiError> {
    if let Some(spec) = &state.spec
        && state.version.is_none()
    {
        let declared = if let Some(path) = spec
            .strip_prefix("link:")
            .or_else(|| spec.strip_prefix("file:"))
        {
            bundle_version(&profile.join(path))?
        } else {
            Version::parse(spec).ok()
        };
        if declared.is_none_or(|version| version.cmp_precedence(bundled).is_gt()) {
            return Err(GitAiError::Generic(format!(
                "{PACKAGE} is declared as {spec:?} but missing from node_modules. Restore it through the DeepSeek Harness plugin manager and retry."
            )));
        }
    }
    Ok(())
}

fn owns_bundle(profile: &Path, state: &PluginState) -> bool {
    let Some(target) = state
        .spec
        .as_deref()
        .and_then(|spec| spec.strip_prefix("link:"))
    else {
        return false;
    };
    profile
        .join(target)
        .canonicalize()
        .ok()
        .zip(profile.join(MANAGED_DIR).canonicalize().ok())
        .is_some_and(|(target, managed)| target.parent() == Some(managed.as_path()))
}

impl HookInstaller for DeepSeekHarnessInstaller {
    fn name(&self) -> &str {
        DESKTOP
    }

    fn id(&self) -> &str {
        "deepseek-harness"
    }

    fn process_names(&self) -> Vec<&str> {
        vec![DESKTOP]
    }

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let Some(profile) = DesktopProfile::detect()? else {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        };
        let state = PluginState::read(&profile.dir)?;
        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: state.spec.is_some(),
            hooks_up_to_date: state.is_up_to_date(&bundled_version()?),
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let Some(profile) = DesktopProfile::detect()? else {
            return Ok(None);
        };
        let bundled = bundled_version()?;
        let mut before = PluginState::read(&profile.dir)?;
        if before.is_up_to_date(&bundled) {
            return Ok(None);
        }

        let _lock = if dry_run {
            None
        } else {
            let lock = profile.lock()?;
            before = PluginState::read(&profile.dir)?;
            if before.is_up_to_date(&bundled) {
                return Ok(None);
            }
            Some(lock)
        };
        check_missing_dependency_version(&profile.dir, &before, &bundled)?;
        let expected_enabled = before.spec.is_none() || before.enabled;
        let expected = describe_bundle(&bundled, expected_enabled);

        if !dry_run {
            let spec = stage_bundle(&profile.dir, &bundled, &params.binary_path)?;
            profile.manage("add", &spec)?;
            let actual = PluginState::read(&profile.dir)?;
            if actual.spec.is_none()
                || actual.version.as_ref() != Some(&bundled)
                || actual.enabled != expected_enabled
            {
                return Err(GitAiError::Generic(format!(
                    "dsh plugin add did not produce the expected state {}",
                    expected.trim_end()
                )));
            }
        }
        Ok(Some(generate_diff(
            &profile.dir.join("package.json"),
            &before.description(),
            &expected,
        )))
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let Some(profile) = DesktopProfile::detect()? else {
            return Ok(None);
        };
        let before = PluginState::read(&profile.dir)?;
        if !owns_bundle(&profile.dir, &before) {
            return Ok(None);
        }
        if !dry_run {
            let _lock = profile.lock()?;
            if !owns_bundle(&profile.dir, &PluginState::read(&profile.dir)?) {
                return Ok(None);
            }
            profile.manage("remove", PACKAGE)?;
            let after = PluginState::read(&profile.dir)?;
            if after.spec.is_some() || after.enabled {
                return Err(GitAiError::Generic(format!(
                    "dsh plugin remove did not remove {PACKAGE}"
                )));
            }
        }
        Ok(Some(generate_diff(
            &profile.dir.join("package.json"),
            &before.description(),
            "",
        )))
    }
}
