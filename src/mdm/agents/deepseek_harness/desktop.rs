use super::{BUNDLE, PACKAGE, Profile, bundle_conflict, embedded_version, with_staging};
use crate::error::GitAiError;
#[cfg(target_os = "macos")]
use crate::mdm::utils::home_dir;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(super) const ARCHIVE_DIR: &str = "local-plugins/dsh-git-ai-attribution/bundles";
#[cfg(target_os = "macos")]
const CLI: &str = "runtime/cli/bin/dsh";
#[cfg(windows)]
const CLI: &str = "runtime/cli/bin/dsh.cmd";

pub(super) struct Desktop(PathBuf);

impl Desktop {
    pub(super) fn find() -> Option<Self> {
        desktop_cli().map(Self)
    }

    pub(super) fn missing() -> GitAiError {
        GitAiError::Generic(
            "Could not find the bundled dsh command in the DeepSeek Harness Desktop installation."
                .to_owned(),
        )
    }

    pub(super) fn install(&self, profile: &Profile, archive: &Path) -> Result<(), GitAiError> {
        self.run(profile, "add", archive)
    }

    pub(super) fn remove(&self, profile: &Profile) -> Result<(), GitAiError> {
        self.run(profile, "remove", Path::new(PACKAGE))
    }

    fn run(&self, profile: &Profile, operation: &str, package: &Path) -> Result<(), GitAiError> {
        // Desktop's dsh acquires package.json.lock itself.
        let mut command = Command::new(&self.0);
        command
            .args(["plugin", "--profile", "desktop", operation])
            .arg(package)
            .args(["--config.offline=true", "--config.ignore-scripts=true"])
            .env("DSH_HOME", &profile.home)
            .env_remove("NODE_OPTIONS")
            .env_remove("NODE_PATH")
            .current_dir(&profile.dir)
            .stdin(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(crate::utils::CREATE_NO_WINDOW);
        }
        let output = command.output().map_err(|error| {
            GitAiError::Generic(format!(
                "Could not start the DeepSeek Harness package manager: {error}"
            ))
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            return Err(GitAiError::Generic(format!(
                "DeepSeek Harness bundle {operation} failed in {}: {detail}",
                profile.dir.display(),
            )));
        }
        Ok(())
    }
}

fn desktop_cli() -> Option<PathBuf> {
    #[cfg(feature = "test-support")]
    if let Some(path) = std::env::var_os("GIT_AI_TEST_DSH_RESOURCES") {
        let cli = PathBuf::from(path).join(CLI);
        return cli.is_file().then_some(cli);
    }

    #[cfg(target_os = "macos")]
    let candidates = [
        PathBuf::from("/Applications"),
        home_dir().join("Applications"),
    ]
    .map(|dir| dir.join("DeepSeek Harness.app/Contents/Resources"));
    #[cfg(windows)]
    let candidates = windows_desktop_resources();

    candidates
        .into_iter()
        .map(|resources| resources.join(CLI))
        .find(|cli| cli.is_file())
}

#[cfg(windows)]
fn windows_desktop_resources() -> Vec<PathBuf> {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let mut candidates = Vec::new();
    if let Ok(uninstall) = hkcu.open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Uninstall")
    {
        candidates.extend(uninstall.enum_keys().filter_map(|key| {
            let entry = uninstall.open_subkey(key.ok()?).ok()?;
            let name: String = entry.get_value("DisplayName").ok()?;
            if name != "DeepSeek Harness" {
                return None;
            }
            let location: String = entry.get_value("InstallLocation").ok()?;
            let location = PathBuf::from(location);
            location.is_absolute().then(|| location.join("resources"))
        }));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("Programs/DeepSeek Harness/resources"));
    }
    candidates
}

pub(super) fn embedded_archive(profile: &Profile) -> Result<PathBuf, GitAiError> {
    let version = embedded_version()?;
    let path = profile.dir.join(ARCHIVE_DIR).join(format!("{version}.tgz"));
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    with_staging(parent, |staging| {
        let staged = staging.join("bundle.tgz");
        let encoder = flate2::write::GzEncoder::new(
            fs::File::create(&staged)?,
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(encoder);
        for (name, contents) in BUNDLE {
            let mut header = tar::Header::new_ustar();
            header.set_mode(0o644);
            header.set_size(contents.len() as u64);
            tar.append_data(&mut header, format!("package/{name}"), contents.as_bytes())?;
        }
        tar.into_inner()?.finish()?.sync_all()?;
        match fs::hard_link(&staged, &path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(&path)?.is_file() || fs::read(&path)? != fs::read(&staged)?
                {
                    return Err(bundle_conflict(&path));
                }
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    })?;
    Ok(path)
}
