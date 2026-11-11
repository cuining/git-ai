use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{generate_diff, home_dir};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const PACKAGE: &str = "dsh-git-ai-attribution";
const PLUGIN_ID: &str = "git-ai-attribution";
const PATCH: &str = "cordis.patch.yml";
const BEGIN: &str = "# BEGIN dsh-git-ai-attribution";
const END: &str = "# END dsh-git-ai-attribution";
const BUNDLE: [(&str, &str); 3] = [
    (
        "package.json",
        include_str!("../../../agent-support/deepseek-harness/package.json"),
    ),
    (
        "git-ai.mjs",
        include_str!("../../../agent-support/deepseek-harness/git-ai.mjs"),
    ),
    (
        "git-ai.cordis.yml",
        include_str!("../../../agent-support/deepseek-harness/git-ai.cordis.yml"),
    ),
];
static MODULE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^name: '(\./local-plugins/dsh-git-ai-attribution/[0-9][\w.+-]*/package/git-ai\.mjs)'$",
    )
    .unwrap()
});
static EMPTY_SEQUENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^( *)(\[[ \t]*\])[ \t]*(?:#.*)?$").unwrap());
static CONFIGURED_BINARY_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^("(?:[^"\\]|\\.)*"|'(?:[^']|'')*'|[/\\.~_\p{L}\p{N}].*?)(?:[ \t]+#.*|[ \t]*)$"#)
        .unwrap()
});

pub struct DeepSeekHarnessInstaller;

struct Profile {
    home: PathBuf,
    dir: PathBuf,
}

impl Profile {
    fn find() -> Result<Option<Self>, GitAiError> {
        let home = std::env::var_os("DSH_HOME")
            .filter(|value| !value.to_string_lossy().trim().is_empty())
            .map(|value| match value.to_str() {
                Some("~") => home_dir(),
                Some(value) if value.starts_with("~/") || value.starts_with("~\\") => {
                    home_dir().join(&value[2..])
                }
                _ => PathBuf::from(value),
            })
            .unwrap_or_else(|| home_dir().join(".dsh"));
        let dir = home.join("profiles").join("desktop");
        Ok(dir
            .join("package.json")
            .try_exists()?
            .then_some(Self { home, dir }))
    }

    fn patch_path(&self) -> PathBuf {
        self.dir.join(PATCH)
    }

    fn package_managed(&self) -> Result<bool, GitAiError> {
        let manifest: Value =
            serde_json::from_str(&fs::read_to_string(self.dir.join("package.json"))?)?;
        let selected = manifest
            .pointer("/dsh/profile/bundles")
            .and_then(Value::as_array)
            .is_some_and(|bundles| {
                bundles
                    .iter()
                    .any(|bundle| bundle.as_str() == Some(PACKAGE))
            });
        let dependency = manifest
            .get("dependencies")
            .and_then(|deps| deps.get(PACKAGE))
            .is_some();
        if selected
            && BUNDLE.iter().all(|(name, _)| {
                self.dir
                    .join("node_modules")
                    .join(PACKAGE)
                    .join(name)
                    .is_file()
            })
        {
            return Ok(true);
        }
        if selected || dependency {
            return Err(GitAiError::Generic(format!(
                "An existing package manager registration for {PACKAGE} is incomplete in {}.",
                self.dir.display()
            )));
        }
        Ok(false)
    }

    fn with_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, GitAiError>,
    ) -> Result<T, GitAiError> {
        let lock = self.dir.join("package.json.lock");
        let mut file = create_profile_lock_file(&lock)
            .or_else(|error| {
                if matches!(
                    error.kind(),
                    ErrorKind::AlreadyExists | ErrorKind::PermissionDenied
                ) && reclaim_profile_lock(&lock)?
                {
                    create_profile_lock_file(&lock)
                } else {
                    Err(error)
                }
            })
            .map_err(|error| {
                GitAiError::Generic(format!(
                    "Could not acquire Desktop profile lock at {}: {error}",
                    lock.display()
                ))
            })?;
        let result = writeln!(file, "{}", std::process::id())
            .map_err(GitAiError::from)
            .and_then(|()| operation());
        drop(file);
        let release = fs::remove_file(lock);
        let value = result?;
        release?;
        Ok(value)
    }
}

fn create_profile_lock_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn exited_profile_lock_record(path: &Path) -> Option<String> {
    if !fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let record = fs::read_to_string(path).ok()?;
    let pid = record.strip_suffix('\n')?;
    if !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let pid = pid.parse::<i32>().ok()?;
    (pid > 0 && !crate::daemon::process_alive(pid as u32)).then_some(record)
}

fn reclaim_profile_lock(lock: &Path) -> std::io::Result<bool> {
    let Some(record) = exited_profile_lock_record(lock) else {
        return Ok(false);
    };
    // Desktop's dsh-atomic-write uses the same claim name. Hold the claim
    // while checking the record again so recovery cannot unlink a new lock.
    let digest = format!("{:x}", Sha256::digest(record.as_bytes()));
    let claim_path = lock.with_extension(format!("lock.takeover-{}", &digest[..16]));
    let mut claim = match create_profile_lock_file(&claim_path) {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::AlreadyExists | ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let result = writeln!(claim, "{}", std::process::id()).and_then(|()| {
        if exited_profile_lock_record(lock).as_deref() == Some(record.as_str()) {
            fs::remove_file(lock).map(|()| true)
        } else {
            Ok(false)
        }
    });
    drop(claim);
    let _ = fs::remove_file(claim_path);
    result
}

fn read_patch(path: &Path) -> Result<Option<String>, GitAiError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some(fs::read_to_string(path)?)),
        Ok(_) => Err(GitAiError::Generic(format!(
            "Expected a regular file at {}.",
            path.display()
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn numbered_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.split_inclusive('\n').scan(0, |offset, line| {
        let start = *offset;
        *offset += line.len();
        Some((start, line.trim_end_matches(['\r', '\n'])))
    })
}

fn is_data(line: &str) -> bool {
    let line = line.trim_start_matches('\u{feff}').trim();
    !line.is_empty() && !line.starts_with('#')
}

fn is_document_marker(line: &str, marker: &str) -> bool {
    line.trim_start_matches('\u{feff}')
        .strip_prefix(marker)
        .is_some_and(|rest| {
            rest.trim().is_empty()
                || (rest.starts_with([' ', '\t']) && rest.trim_start().starts_with('#'))
        })
}

fn sequence_indent(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches(' ');
    trimmed
        .strip_prefix('-')
        .filter(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
        .map(|_| &line[..line.len() - trimmed.len()])
}

struct Registration {
    block: Range<usize>,
    module: Range<usize>,
    item_indent: usize,
}

fn registration(text: &str, path: &Path) -> Result<Option<Registration>, GitAiError> {
    let bom = text.len() - text.strip_prefix('\u{feff}').unwrap_or(text).len();
    let markers: Vec<_> = numbered_lines(&text[bom..])
        .filter(|(_, line)| matches!(line.trim_start_matches(' '), BEGIN | END))
        .collect();
    if markers.is_empty() {
        return Ok(None);
    }
    let invalid = || {
        GitAiError::Generic(format!(
            "Installer registration was edited in {}.",
            path.display()
        ))
    };
    let [(start, begin), (end, finish)] = markers.as_slice() else {
        return Err(invalid());
    };
    let indent = begin.strip_suffix(BEGIN).ok_or_else(invalid)?;
    let root_indent = numbered_lines(&text[bom..])
        .find(|(_, line)| is_data(line) && !is_document_marker(line, "---"))
        .and_then(|(_, line)| sequence_indent(line));
    if finish.strip_suffix(END) != Some(indent) || root_indent != Some(indent) {
        return Err(invalid());
    }
    let mut lines = numbered_lines(&text[bom + start..bom + end]).skip(1);
    if lines.next().map(|(_, line)| line.strip_prefix(indent)) != Some(Some("- insert:"))
        || lines.next().map(|(_, line)| line.strip_prefix(indent))
            != Some(Some("    - id: git-ai-attribution"))
    {
        return Err(invalid());
    }
    let (offset, line) = lines.next().ok_or_else(invalid)?;
    let item_indent = indent.len() + 6;
    let name = line
        .strip_prefix(indent)
        .and_then(|line| line.strip_prefix("      "))
        .and_then(|name| MODULE_PATH.captures(name))
        .and_then(|captures| captures.get(1))
        .ok_or_else(invalid)?;
    let module_start = bom + start + offset + item_indent + name.start();
    let block_end = text[bom + end..]
        .find('\n')
        .map_or(text.len(), |newline| bom + end + newline + 1);
    Ok(Some(Registration {
        block: (bom + start)..block_end,
        module: module_start..module_start + name.len(),
        item_indent,
    }))
}

fn check_registration(text: &str, path: &Path) -> Result<(), GitAiError> {
    if text
        .lines()
        .any(|line| is_data(line) && line.contains(PLUGIN_ID))
    {
        return Err(GitAiError::Generic(format!(
            "A user-managed {PLUGIN_ID} registration already exists in {}.",
            path.display()
        )));
    }
    Ok(())
}

fn line_ending(text: &str) -> &str {
    match text.find('\n') {
        Some(index) if text[..index].ends_with('\r') => "\r\n",
        Some(_) => "\n",
        None if cfg!(windows) => "\r\n",
        None => "\n",
    }
}

fn binary_config_value(binary: &Path) -> Result<String, GitAiError> {
    let binary = binary.to_str().ok_or_else(|| {
        GitAiError::Generic("git-ai path must be UTF-8 for the Desktop profile.".to_owned())
    })?;
    Ok(serde_json::to_string(binary)?)
}

fn binary_config(binary: &Path, indent: &str, newline: &str) -> Result<String, GitAiError> {
    let binary = binary_config_value(binary)?;
    Ok(format!(
        "{indent}config:{newline}{indent}  gitAiPath: {binary}{newline}"
    ))
}

fn yaml_field<'a>(line: &'a str, indent: &str, name: &str) -> Option<&'a str> {
    let (key, value) = line.strip_prefix(indent)?.split_once(':')?;
    let key = key.trim_end_matches([' ', '\t']);
    let key = ['\'', '"']
        .into_iter()
        .find_map(|quote| key.strip_prefix(quote)?.strip_suffix(quote))
        .unwrap_or(key);
    (key == name).then_some(value)
}

fn configured_binary_range(
    tail: &str,
    indent: &str,
    path: &Path,
) -> Result<Option<Range<usize>>, GitAiError> {
    let unsupported = || {
        GitAiError::Generic(format!(
            "Unsupported gitAiPath configuration in {}. Use a single-line scalar in a block mapping.",
            path.display()
        ))
    };
    let mut fields = numbered_lines(tail)
        .skip(1)
        .filter(|(_, line)| is_data(line))
        .take_while(|(_, line)| line.starts_with(indent));
    let Some(config) = fields.find_map(|(_, line)| yaml_field(line, indent, "config")) else {
        return Ok(None);
    };
    if !config.trim().is_empty() && !config.trim_start().starts_with('#') {
        return Err(unsupported());
    }
    let mut config_fields = fields
        .take_while(|(_, line)| line[indent.len()..].starts_with(' '))
        .peekable();
    let Some(&(_, first_field)) = config_fields.peek() else {
        return Ok(None);
    };
    let field_indent =
        &first_field[..first_field.len() - first_field.trim_start_matches(' ').len()];
    let Some((offset, line, field)) = config_fields.find_map(|(offset, line)| {
        yaml_field(line, field_indent, "gitAiPath").map(|field| (offset, line, field))
    }) else {
        return Ok(None);
    };
    let scalar = field.trim_start_matches([' ', '\t']);
    if scalar.len() == field.len()
        || config_fields.next().is_some_and(|(_, next)| {
            next.strip_prefix(field_indent)
                .is_some_and(|rest| rest.starts_with(' '))
        })
    {
        return Err(unsupported());
    }
    let value = CONFIGURED_BINARY_VALUE
        .captures(scalar)
        .and_then(|captures| captures.get(1))
        .ok_or_else(unsupported)?;
    let token = value.as_str();
    if !token.starts_with(['\'', '"'])
        && (token.ends_with(':') || token.contains(": ") || token.contains(":\t"))
    {
        return Err(unsupported());
    }
    let start = offset + line.len() - scalar.len();
    Ok(Some(start..start + value.len()))
}

fn add_registration(
    original: &str,
    module: &str,
    binary: &Path,
    path: &Path,
) -> Result<String, GitAiError> {
    let bom = original.len() - original.strip_prefix('\u{feff}').unwrap_or(original).len();
    let mut lines: Vec<_> = numbered_lines(&original[bom..])
        .filter(|(_, line)| is_data(line))
        .collect();
    if lines
        .first()
        .is_some_and(|(_, line)| is_document_marker(line, "---"))
    {
        lines.remove(0);
    }
    let end = if lines
        .last()
        .is_some_and(|(_, line)| is_document_marker(line, "..."))
    {
        bom + lines.pop().unwrap().0
    } else {
        original.len()
    };
    let unsupported = || {
        GitAiError::Generic(format!(
            "Expected a single YAML block sequence in {}.",
            path.display()
        ))
    };
    if lines
        .iter()
        .any(|(_, line)| is_document_marker(line, "---") || is_document_marker(line, "..."))
    {
        return Err(unsupported());
    }

    let mut prefix = original[..end].to_owned();
    let mut indent = "";
    if let Some((offset, first)) = lines.first() {
        if let Some(empty) = EMPTY_SEQUENCE.captures(first).filter(|_| lines.len() == 1) {
            indent = empty.get(1).unwrap().as_str();
            let token = empty.get(2).unwrap();
            prefix.replace_range(bom + offset + token.start()..bom + offset + token.end(), "");
        } else {
            indent = sequence_indent(first).ok_or_else(unsupported)?;
        }
    }

    let newline = line_ending(original);
    if prefix.len() > bom && !prefix.ends_with('\n') {
        prefix.push_str(newline);
    }
    for line in [
        BEGIN,
        "- insert:",
        "    - id: git-ai-attribution",
        &format!("      name: '{module}'"),
    ] {
        prefix.push_str(indent);
        prefix.push_str(line);
        prefix.push_str(newline);
    }
    prefix.push_str(&binary_config(binary, &format!("{indent}      "), newline)?);
    prefix.push_str(indent);
    prefix.push_str(END);
    prefix.push_str(newline);
    prefix.push_str(&original[end..]);
    Ok(prefix)
}

fn with_staging<T>(
    parent: &Path,
    operation: impl FnOnce(&Path) -> Result<T, GitAiError>,
) -> Result<T, GitAiError> {
    let staging = parent.join(format!(".git-ai-install-{:016x}", rand::random::<u64>()));
    fs::create_dir(&staging)?;
    let result = operation(&staging);
    let _ = fs::remove_dir_all(&staging);
    result
}

fn publish_patch(path: &Path, original: Option<&str>, content: &str) -> Result<(), GitAiError> {
    if original == Some(content) {
        return Ok(());
    }
    with_staging(path.parent().unwrap(), |staging| {
        let staged = staging.join(PATCH);
        {
            let mut file = fs::File::create(&staged)?;
            file.write_all(content.as_bytes())?;
            if original.is_some() {
                file.set_permissions(fs::metadata(path)?.permissions())?;
            }
            file.sync_all()?;
        }
        if read_patch(path)?.as_deref() != original {
            return Err(GitAiError::Generic(format!(
                "{} changed during installation. Try again.",
                path.display()
            )));
        }
        if original.is_some() {
            fs::rename(staged, path)?;
        } else {
            fs::hard_link(staged, path)?;
        }
        Ok(())
    })
}

fn installed_files_match(destination: &Path) -> Result<bool, GitAiError> {
    match fs::symlink_metadata(destination) {
        Ok(meta) if meta.is_dir() => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
        Ok(_) => return Err(bundle_conflict(destination)),
    }
    for (name, expected) in BUNDLE {
        let file = destination.join("package").join(name);
        if read_patch(&file)?.as_deref() != Some(expected) {
            return Err(bundle_conflict(destination));
        }
    }
    Ok(true)
}

fn bundle_conflict(destination: &Path) -> GitAiError {
    GitAiError::Generic(format!(
        "Existing plugin files differ from the bundle at {}.",
        destination.display()
    ))
}

fn publish_bundle(destination: &Path) -> Result<(), GitAiError> {
    let parent = destination.parent().unwrap();
    fs::create_dir_all(parent)?;
    with_staging(parent, |staging| {
        let package = staging.join("package");
        fs::create_dir(&package)?;
        for (name, source) in BUNDLE {
            fs::write(package.join(name), source)?;
        }
        fs::rename(staging, destination)?;
        Ok(())
    })
}

fn install(
    profile: &Profile,
    params: &HookInstallerParams,
    write: bool,
) -> Result<Option<String>, GitAiError> {
    let path = profile.patch_path();
    let original = read_patch(&path)?;
    let text = original.as_deref().unwrap_or_default();
    let registration = registration(text, &path)?;
    if profile.package_managed()? {
        return if registration.is_none() {
            Ok(None)
        } else {
            Err(GitAiError::Generic(format!(
                "Both local and package-managed copies of {PACKAGE} are registered."
            )))
        };
    }

    let home_patch = profile.home.join(PATCH);
    check_registration(
        read_patch(&home_patch)?.as_deref().unwrap_or_default(),
        &home_patch,
    )?;
    let manifest: Value = serde_json::from_str(BUNDLE[0].1)?;
    let version = manifest["version"]
        .as_str()
        .expect("embedded bundle has a version");
    let directory = format!("local-plugins/{PACKAGE}/{version}");
    let destination = profile.dir.join(&directory);
    let module = format!("./{directory}/package/git-ai.mjs");
    let updated = if let Some(entry) = registration {
        check_registration(&text[..entry.block.start], &path)?;
        check_registration(&text[entry.module.end..], &path)?;
        let mut updated = text.to_owned();
        let tail_start = entry.module.end;
        let tail = &text[tail_start..entry.block.end];
        let indent = " ".repeat(entry.item_indent);
        if let Some(value) = configured_binary_range(tail, &indent, &path)? {
            updated.replace_range(
                (tail_start + value.start)..(tail_start + value.end),
                &binary_config_value(&params.binary_path)?,
            );
        } else if !tail.lines().skip(1).any(is_data) {
            // Older script installations have a module path but no configuration.
            let after_name = tail_start + tail.find('\n').unwrap() + 1;
            updated.insert_str(
                after_name,
                &binary_config(&params.binary_path, &indent, line_ending(text))?,
            );
        }
        updated.replace_range(entry.module, &module);
        updated
    } else {
        check_registration(text, &path)?;
        add_registration(text, &module, &params.binary_path, &path)?
    };
    let files_ready = installed_files_match(&destination)?;
    if files_ready && text == updated {
        return Ok(None);
    }
    if write {
        if !files_ready {
            publish_bundle(&destination)?;
        }
        publish_patch(&path, original.as_deref(), &updated)?;
    }
    Ok(Some(generate_diff(&path, text, &updated)))
}

fn uninstall(profile: &Profile, write: bool) -> Result<Option<String>, GitAiError> {
    let path = profile.patch_path();
    let Some(text) = read_patch(&path)? else {
        return Ok(None);
    };
    let Some(entry) = registration(&text, &path)? else {
        return Ok(None);
    };
    // Other rows added between the markers are not part of our registration.
    if numbered_lines(&text[entry.module.end..entry.block.end])
        .skip(1)
        .any(|(_, line)| {
            is_data(line) && line.len() - line.trim_start_matches(' ').len() < entry.item_indent
        })
    {
        return Err(GitAiError::Generic(format!(
            "Installer registration contains additional rows in {}.",
            path.display()
        )));
    }
    let mut updated = text.clone();
    updated.replace_range(entry.block, "");
    if !updated.lines().any(|line| {
        is_data(line) && !is_document_marker(line, "---") && !is_document_marker(line, "...")
    }) {
        let end = numbered_lines(&updated)
            .find(|(_, line)| is_document_marker(line, "..."))
            .map_or(updated.len(), |(offset, _)| offset);
        let newline = line_ending(&text);
        let prefix = &updated[..end];
        let separator =
            if prefix.trim_start_matches('\u{feff}').is_empty() || prefix.ends_with('\n') {
                ""
            } else {
                newline
            };
        updated.insert_str(end, &format!("{separator}[]{newline}"));
    }
    if write {
        publish_patch(&path, Some(&text), &updated)?;
    }
    Ok(Some(generate_diff(&path, &text, &updated)))
}

impl HookInstaller for DeepSeekHarnessInstaller {
    fn name(&self) -> &str {
        "DeepSeek Harness"
    }

    fn id(&self) -> &str {
        "deepseek-harness"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["DeepSeek Harness"]
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let profile = Profile::find()?;
        let (hooks_installed, hooks_up_to_date) = match &profile {
            Some(profile) if matches!(install(profile, params, false), Ok(None)) => (true, true),
            Some(profile) => {
                // An install conflict can still leave an owned registration to uninstall.
                let path = profile.patch_path();
                let installed =
                    registration(read_patch(&path)?.as_deref().unwrap_or_default(), &path)?
                        .is_some();
                (installed, false)
            }
            None => (false, false),
        };
        Ok(HookCheckResult {
            tool_installed: profile.is_some(),
            hooks_installed,
            hooks_up_to_date,
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let Some(profile) = Profile::find()? else {
            return Ok(None);
        };
        let preview = install(&profile, params, false)?;
        if dry_run || preview.is_none() {
            return Ok(preview);
        }
        profile.with_lock(|| install(&profile, params, true))
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let Some(profile) = Profile::find()? else {
            return Ok(None);
        };
        let preview = uninstall(&profile, false)?;
        if dry_run || preview.is_none() {
            return Ok(preview);
        }
        profile.with_lock(|| uninstall(&profile, true))
    }
}
