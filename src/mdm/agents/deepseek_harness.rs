use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{clean_path, generate_diff, home_dir};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

mod desktop;

use desktop::{ARCHIVE_DIR, Desktop, embedded_archive};

const PACKAGE: &str = "dsh-git-ai-attribution";
const PLUGIN_ID: &str = "git-ai-attribution";
const PATCH: &str = "cordis.patch.yml";
const BEGIN: &str = "# BEGIN dsh-git-ai-attribution";
const END: &str = "# END dsh-git-ai-attribution";
const BUNDLE_SELECTION: &str = "/dsh/profile/bundles";
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
        if !dir.join("package.json").try_exists()? {
            return Ok(None);
        }
        Ok(Some(Self {
            home: clean_path(home.canonicalize()?),
            dir: clean_path(dir.canonicalize()?),
        }))
    }

    fn patch_path(&self) -> PathBuf {
        self.dir.join(PATCH)
    }

    fn bundle(&self, require_selection: bool) -> Result<ProfileBundle, GitAiError> {
        let path = self.dir.join("package.json");
        let manifest: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        let bundles = manifest.pointer(BUNDLE_SELECTION).and_then(Value::as_array);
        if require_selection && bundles.is_none() {
            return Err(invalid_bundle_selection(&path));
        }
        let selected =
            bundles.is_some_and(|bundles| bundles.iter().any(|bundle| bundle == PACKAGE));
        let dependency = manifest
            .get("dependencies")
            .and_then(|deps| deps.get(PACKAGE));
        let archive = dependency
            .and_then(Value::as_str)
            .and_then(|spec| spec.strip_prefix("file:"))
            .map(|path| self.dir.join(path))
            .filter(|path| self.owns_archive(path));
        let installed = self.dir.join("node_modules").join(PACKAGE);
        if let Some(archive) = archive {
            let current = BUNDLE.iter().all(|(name, contents)| {
                fs::read_to_string(installed.join(name)).is_ok_and(|actual| actual == *contents)
            }) && archive.is_file();
            return Ok(ProfileBundle::Managed { selected, current });
        }
        if (selected || dependency.is_some())
            && BUNDLE
                .iter()
                .all(|(name, _)| installed.join(name).is_file())
        {
            return Ok(ProfileBundle::External);
        }
        if selected || dependency.is_some() {
            return Err(GitAiError::Generic(format!(
                "An existing package manager registration for {PACKAGE} is incomplete in {}.",
                self.dir.display()
            )));
        }
        Ok(ProfileBundle::Absent)
    }

    fn owns_archive(&self, path: &Path) -> bool {
        path.file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|version| semver::Version::parse(version).is_ok())
            && path.extension().is_some_and(|ext| ext == "tgz")
            && path.parent().is_some_and(|parent| {
                let dir = self.dir.join(ARCHIVE_DIR);
                parent == dir
                    || parent
                        .canonicalize()
                        .is_ok_and(|parent| clean_path(parent) == dir)
            })
    }

    fn select_bundle(&self) -> Result<(), GitAiError> {
        let path = self.dir.join("package.json");
        let original = fs::read_to_string(&path)?;
        let mut manifest: Value = serde_json::from_str(&original)?;
        let bundles = manifest
            .pointer_mut(BUNDLE_SELECTION)
            .and_then(Value::as_array_mut)
            .ok_or_else(|| invalid_bundle_selection(&path))?;
        if bundles.iter().any(|name| name == PACKAGE) {
            return Ok(());
        }
        bundles.push(Value::String(PACKAGE.to_owned()));
        publish_text(
            &path,
            Some(&original),
            &(serde_json::to_string_pretty(&manifest)? + "\n"),
        )
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProfileBundle {
    Absent,
    External,
    Managed { selected: bool, current: bool },
}

impl ProfileBundle {
    fn selected(self) -> bool {
        matches!(self, Self::Managed { selected: true, .. })
    }

    fn current(self) -> bool {
        matches!(self, Self::Managed { current: true, .. })
    }
}

fn invalid_bundle_selection(path: &Path) -> GitAiError {
    GitAiError::Generic(format!(
        "Desktop profile {} has no dsh.profile.bundles array.",
        path.display()
    ))
}

fn embedded_version() -> Result<String, GitAiError> {
    let manifest: Value = serde_json::from_str(BUNDLE[0].1)?;
    Ok(manifest["version"]
        .as_str()
        .expect("embedded bundle has a version")
        .to_owned())
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
    header_end: usize,
    module: Option<Range<usize>>,
    fields: Range<usize>,
    item_indent: usize,
    pending: Option<Range<usize>>,
}

const PENDING_SELECTION: &str = " # pending bundle selection";

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
    let block_start = bom + start;
    let fields_end = bom + end;
    let mut lines = numbered_lines(&text[block_start..fields_end]).skip(1);
    let (offset, line) = lines.next().ok_or_else(invalid)?;
    let (module, item_indent, pending) = match line.strip_prefix(indent) {
        Some("- id: git-ai-attribution") => (None, indent.len() + 2, None),
        Some(row) if row.strip_suffix(PENDING_SELECTION) == Some("- id: git-ai-attribution") => {
            let end = block_start + offset + line.len();
            (
                None,
                indent.len() + 2,
                Some(end - PENDING_SELECTION.len()..end),
            )
        }
        Some("- insert:") => {
            if lines.next().and_then(|(_, line)| line.strip_prefix(indent))
                != Some("    - id: git-ai-attribution")
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
            let module_start = block_start + offset + item_indent + name.start();
            (
                Some(module_start..module_start + name.len()),
                item_indent,
                None,
            )
        }
        _ => return Err(invalid()),
    };
    let fields_start = block_start + lines.next().map_or(end - start, |(offset, _)| offset);
    if numbered_lines(&text[fields_start..fields_end]).any(|(_, line)| {
        is_data(line) && line.len() - line.trim_start_matches(' ').len() < item_indent
    }) {
        return Err(invalid());
    }
    let block_end = text[bom + end..]
        .find('\n')
        .map_or(text.len(), |newline| bom + end + newline + 1);
    Ok(Some(Registration {
        block: block_start..block_end,
        header_end: block_start + offset + line.len(),
        module,
        fields: fields_start..fields_end,
        item_indent,
        pending,
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
    module: Option<&str>,
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
    let header = match module {
        Some(module) => vec![
            BEGIN.to_owned(),
            "- insert:".to_owned(),
            "    - id: git-ai-attribution".to_owned(),
            format!("      name: '{module}'"),
        ],
        None => vec![BEGIN.to_owned(), "- id: git-ai-attribution".to_owned()],
    };
    for line in header {
        prefix.push_str(indent);
        prefix.push_str(&line);
        prefix.push_str(newline);
    }
    let config_indent = if module.is_some() { "      " } else { "  " };
    prefix.push_str(&binary_config(
        binary,
        &format!("{indent}{config_indent}"),
        newline,
    )?);
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

fn publish_text(path: &Path, original: Option<&str>, content: &str) -> Result<(), GitAiError> {
    if original == Some(content) {
        return Ok(());
    }
    with_staging(path.parent().unwrap(), |staging| {
        let staged = staging.join(path.file_name().unwrap());
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

fn update_registration(
    text: &str,
    entry: &Registration,
    module: Option<&str>,
    binary: &Path,
    path: &Path,
) -> Result<String, GitAiError> {
    let indent = " ".repeat(entry.item_indent);
    let mut fields = text[entry.fields.clone()].to_owned();
    let newline = line_ending(text);
    let config_field_offset = numbered_lines(&fields)
        .find_map(|(offset, line)| yaml_field(line, &indent, "config").map(|_| offset));
    if let Some(value) = configured_binary_range(&fields, &indent, path)? {
        fields.replace_range(value, &binary_config_value(binary)?);
    } else if let Some(offset) = config_field_offset {
        let tail = &fields[offset..];
        let value_indent = numbered_lines(tail)
            .skip(1)
            .find(|(_, line)| is_data(line))
            .map(|(_, line)| &line[..line.len() - line.trim_start_matches(' ').len()])
            .filter(|nested| nested.len() > indent.len());
        let default_indent = format!("{indent}  ");
        let value_indent = value_indent.unwrap_or(&default_indent);
        let insert_at = offset + tail.find('\n').expect("config is inside a marked block") + 1;
        let config = format!(
            "{value_indent}gitAiPath: {}{newline}",
            binary_config_value(binary)?
        );
        fields.insert_str(insert_at, &config);
    } else {
        fields.push_str(&binary_config(binary, &indent, newline)?);
    }

    let mut updated = text.to_owned();
    if let (Some(old), Some(new)) = (&entry.module, module) {
        updated.replace_range(entry.fields.clone(), &fields);
        updated.replace_range(old.clone(), new);
    } else if entry.module.is_none() {
        updated.replace_range(entry.fields.clone(), &fields);
        if let Some(pending) = &entry.pending {
            updated.replace_range(pending.clone(), "");
        }
    } else {
        let root = &indent[6..];
        let field_indent = &indent[4..];
        let start = text[entry.block.start..entry.fields.start]
            .find('\n')
            .map(|offset| entry.block.start + offset + 1)
            .expect("registration header contains a newline");
        let mut replacement = format!("{root}- id: {PLUGIN_ID}{newline}");
        for line in fields.split_inclusive('\n') {
            if let Some(field) = line.strip_prefix(&indent) {
                replacement.push_str(field_indent);
                replacement.push_str(field);
            } else {
                replacement.push_str(line);
            }
        }
        updated.replace_range(start..entry.fields.end, &replacement);
    }
    Ok(updated)
}

fn prepare_install(
    profile: &Profile,
    params: &HookInstallerParams,
    desktop: Option<&Desktop>,
    managed_only: bool,
) -> Result<Option<HookUpdate>, GitAiError> {
    let path = profile.patch_path();
    let original = read_patch(&path)?;
    let text = original.as_deref().unwrap_or_default();
    let entry = registration(text, &path)?;
    let bundle = profile.bundle(desktop.is_some())?;
    if bundle == ProfileBundle::External {
        return if entry.is_none() {
            Ok(None)
        } else {
            Err(GitAiError::Generic(format!(
                "Both local and package-managed copies of {PACKAGE} are registered."
            )))
        };
    }
    // Selection belongs to the panel after a managed bundle has been installed.
    // A legacy insertion alongside a package record is an interrupted migration.
    let legacy = entry.as_ref().is_some_and(|entry| entry.module.is_some());
    let pending = entry.as_ref().is_some_and(|entry| entry.pending.is_some());
    if managed_only
        && !(bundle.selected() || (bundle == ProfileBundle::Absent && (legacy || pending)))
    {
        return Ok(None);
    }
    let select = bundle == ProfileBundle::Absent || legacy || entry.is_none() || pending;

    let home_patch = profile.home.join(PATCH);
    check_registration(
        read_patch(&home_patch)?.as_deref().unwrap_or_default(),
        &home_patch,
    )?;
    if let Some(entry) = &entry {
        check_registration(&text[..entry.block.start], &path)?;
        check_registration(&text[entry.block.end..], &path)?;
    } else {
        check_registration(text, &path)?;
    }

    // Older Desktop distributions do not provide a package manager. Their
    // existing local registration still needs to load the compatible bundle.
    let local = bundle == ProfileBundle::Absent && desktop.is_none() && (entry.is_none() || legacy);
    let version = embedded_version()?;
    let directory = format!("local-plugins/{PACKAGE}/{version}");
    let destination = profile.dir.join(&directory);
    let module = local.then(|| format!("./{directory}/package/git-ai.mjs"));
    let updated = match entry.as_ref() {
        Some(entry) => {
            update_registration(text, entry, module.as_deref(), &params.binary_path, &path)?
        }
        None => add_registration(text, module.as_deref(), &params.binary_path, &path)?,
    };
    let files_ready = if local {
        installed_files_match(&destination)?
    } else {
        bundle.current()
    };
    if !local && !files_ready && desktop.is_none() {
        return Err(Desktop::missing());
    }
    let needs_selection = !local && select && !bundle.selected();
    if files_ready && !needs_selection && !pending && text == updated {
        return Ok(None);
    }
    let diff = generate_diff(&path, text, &updated);
    let diff = if !local && (!files_ready || needs_selection) {
        format!("Install {PACKAGE} {version} as a Desktop plugin bundle.\n{diff}")
    } else {
        diff
    };
    Ok(Some(HookUpdate {
        original,
        patch: updated,
        diff,
        bundle: if !local && !files_ready {
            BundleChange::Install
        } else {
            BundleChange::Patch
        },
        local_bundle: (local && !files_ready).then_some(destination),
        select: needs_selection,
    }))
}

enum BundleChange {
    Patch,
    Install,
    Remove,
}

struct HookUpdate {
    original: Option<String>,
    patch: String,
    diff: String,
    bundle: BundleChange,
    local_bundle: Option<PathBuf>,
    select: bool,
}

enum PackageOperation {
    Add {
        archive: PathBuf,
        staged: Option<String>,
    },
    Remove,
}

fn apply_update(
    profile: &Profile,
    desktop: Option<&Desktop>,
    plan: impl Fn() -> Result<Option<HookUpdate>, GitAiError>,
) -> Result<Option<String>, GitAiError> {
    // dsh takes the profile lock. Serialize git-ai's full operation with a
    // separate lock, and use the shared lock for the edits on either side.
    let lock = profile.dir.join(".git-ai-hooks.lock");
    let _lock = crate::utils::LockFile::try_acquire(&lock).ok_or_else(|| {
        GitAiError::Generic(format!(
            "Could not acquire the git-ai hook lock at {}.",
            lock.display()
        ))
    })?;
    let path = profile.patch_path();
    let Some((update, operation)) = profile.with_lock(|| {
        let Some(update) = plan()? else {
            return Ok(None);
        };
        if let Some(destination) = &update.local_bundle {
            publish_bundle(destination)?;
        }
        let operation = match update.bundle {
            BundleChange::Install => {
                let archive = embedded_archive(profile)?;
                let staged = if update.select {
                    // Stage the override before dsh selects the bundle, so a
                    // running Desktop never sees two entries with the same ID.
                    let entry = registration(&update.patch, &path)?.expect("bundle registration");
                    let mut pending = update.patch.clone();
                    pending.insert_str(entry.header_end, PENDING_SELECTION);
                    publish_text(&path, update.original.as_deref(), &pending)?;
                    Some(pending)
                } else {
                    None
                };
                Some(PackageOperation::Add { archive, staged })
            }
            BundleChange::Remove => Some(PackageOperation::Remove),
            BundleChange::Patch => {
                finish_update(profile, &update, update.original.as_deref())?;
                None
            }
        };
        Ok(Some((update, operation)))
    })?
    else {
        return Ok(None);
    };
    match operation {
        Some(PackageOperation::Add { archive, staged }) => {
            let desktop = desktop.ok_or_else(Desktop::missing)?;
            if let Err(error) = desktop.install(profile, &archive) {
                if let Some(staged) = &staged {
                    profile
                        .with_lock(|| restore_patch(&path, staged, update.original.as_deref()))?;
                }
                return Err(error);
            }
            profile.with_lock(|| {
                let bundle = profile.bundle(true)?;
                if !bundle.current() {
                    if let Some(staged) = &staged
                        && !bundle.selected()
                    {
                        restore_patch(&path, staged, update.original.as_deref())?;
                    }
                    return Err(GitAiError::Generic(format!(
                        "DeepSeek Harness did not install {PACKAGE}."
                    )));
                }
                finish_update(
                    profile,
                    &update,
                    staged.as_deref().or(update.original.as_deref()),
                )
            })?;
        }
        Some(PackageOperation::Remove) => {
            desktop.ok_or_else(Desktop::missing)?.remove(profile)?;
            profile.with_lock(|| finish_update(profile, &update, update.original.as_deref()))?;
        }
        None => {}
    }
    Ok(Some(update.diff))
}

fn restore_patch(path: &Path, pending: &str, original: Option<&str>) -> Result<(), GitAiError> {
    if let Some(original) = original {
        return publish_text(path, Some(pending), original);
    }
    if read_patch(path)?.as_deref() != Some(pending) {
        return Err(GitAiError::Generic(format!(
            "{} changed during installation. Try again.",
            path.display()
        )));
    }
    fs::remove_file(path)?;
    Ok(())
}

fn finish_update(
    profile: &Profile,
    update: &HookUpdate,
    original: Option<&str>,
) -> Result<(), GitAiError> {
    let path = profile.patch_path();
    if update.select {
        if read_patch(&path)?.as_deref() != original {
            return Err(GitAiError::Generic(format!(
                "{} changed during installation. Try again.",
                path.display()
            )));
        }
        profile.select_bundle()?;
    }
    if original.is_some() || !update.patch.is_empty() {
        publish_text(&path, original, &update.patch)?;
    }
    Ok(())
}

fn prepare_uninstall(
    profile: &Profile,
    desktop: Option<&Desktop>,
) -> Result<Option<HookUpdate>, GitAiError> {
    let path = profile.patch_path();
    let original = read_patch(&path)?;
    let text = original.as_deref().unwrap_or_default();
    let entry = registration(text, &path)?;
    let bundle = profile.bundle(false).ok();
    let managed = matches!(bundle, Some(ProfileBundle::Managed { .. }));
    if managed && desktop.is_none() {
        return Err(Desktop::missing());
    }
    if entry.is_none() && !managed {
        return Ok(None);
    }
    let mut updated = text.to_owned();
    if let Some(entry) = entry {
        updated.replace_range(entry.block, "");
    }
    if updated != text
        && !updated.lines().any(|line| {
            is_data(line) && !is_document_marker(line, "---") && !is_document_marker(line, "...")
        })
    {
        let end = numbered_lines(&updated)
            .find(|(_, line)| is_document_marker(line, "..."))
            .map_or(updated.len(), |(offset, _)| offset);
        let newline = line_ending(text);
        let prefix = &updated[..end];
        let separator =
            if prefix.trim_start_matches('\u{feff}').is_empty() || prefix.ends_with('\n') {
                ""
            } else {
                newline
            };
        updated.insert_str(end, &format!("{separator}[]{newline}"));
    }
    let diff = generate_diff(&path, text, &updated);
    Ok(Some(HookUpdate {
        original,
        patch: updated,
        diff,
        bundle: if managed {
            BundleChange::Remove
        } else {
            BundleChange::Patch
        },
        local_bundle: None,
        select: false,
    }))
}

impl DeepSeekHarnessInstaller {
    pub(crate) fn refresh_managed(
        params: &HookInstallerParams,
    ) -> Result<Option<String>, GitAiError> {
        Self::install_profile(params, false, true)
    }

    fn install_profile(
        params: &HookInstallerParams,
        dry_run: bool,
        managed_only: bool,
    ) -> Result<Option<String>, GitAiError> {
        let Some(profile) = Profile::find()? else {
            return Ok(None);
        };
        let desktop = Desktop::find();
        let plan = || prepare_install(&profile, params, desktop.as_ref(), managed_only);
        let preview = plan()?;
        if dry_run || preview.is_none() {
            return Ok(preview.map(|update| update.diff));
        }
        apply_update(&profile, desktop.as_ref(), plan)
    }
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
        let desktop = Desktop::find();
        let (hooks_installed, hooks_up_to_date) = match &profile {
            Some(profile)
                if matches!(
                    prepare_install(profile, params, desktop.as_ref(), false),
                    Ok(None)
                ) =>
            {
                (true, true)
            }
            Some(profile) => {
                // An install conflict can still leave an owned registration to uninstall.
                let path = profile.patch_path();
                let installed =
                    registration(read_patch(&path)?.as_deref().unwrap_or_default(), &path)?
                        .is_some()
                        || matches!(profile.bundle(false), Ok(ProfileBundle::Managed { .. }));
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
        Self::install_profile(params, dry_run, false)
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let Some(profile) = Profile::find()? else {
            return Ok(None);
        };
        let desktop = Desktop::find();
        let plan = || prepare_uninstall(&profile, desktop.as_ref());
        let preview = plan()?;
        if dry_run || preview.is_none() {
            return Ok(preview.map(|update| update.diff));
        }
        apply_update(&profile, desktop.as_ref(), plan)
    }
}
