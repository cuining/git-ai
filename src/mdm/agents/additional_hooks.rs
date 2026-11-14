use crate::commands::checkpoint_agent::presets::additional_hooks::{
    FileHookAgent, agent_home, hermes,
};
use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{
    binary_exists, ensure_parent_dir, generate_diff, home_dir, settings_path_candidates,
};
use jsonc_parser::{
    ParseOptions,
    cst::{CstObject, CstRootNode},
    json,
};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item};
use yaml_edit::{Mapping, Sequence, YamlFile};

pub(super) struct FileHookInstaller(pub FileHookAgent);

const HERMES_PLUGIN: &str = "git-ai-hooks";
const HERMES_MARKER: &str = "# Managed by git-ai\n";

struct Hooks {
    args: String,
    command: String,
    binary_path: PathBuf,
    events: Vec<(&'static str, Option<String>)>,
}

impl Hooks {
    fn owns(&self, command: &str) -> bool {
        if command == self.command {
            return true;
        }
        let Some(executable) = command.strip_suffix(&self.args) else {
            return false;
        };
        let executable = executable.strip_prefix("& ").unwrap_or(executable);
        if let Some(path) = executable
            .strip_prefix('\'')
            .and_then(|path| path.strip_suffix('\''))
            .or_else(|| {
                executable
                    .strip_prefix('"')
                    .and_then(|path| path.strip_suffix('"'))
            })
        {
            return Path::new(path).is_absolute() && is_git_ai_executable(path);
        }
        executable
            .chars()
            .all(|ch| ch.is_alphanumeric() || "/\\:._-".contains(ch))
            && is_git_ai_executable(executable)
    }
}

fn is_git_ai_executable(path: &str) -> bool {
    matches!(
        path.rsplit(['/', '\\']).next(),
        Some("git-ai" | "git-ai.exe")
    )
}

impl FileHookInstaller {
    fn config_paths(&self) -> Vec<PathBuf> {
        let home = home_dir();
        match self.0 {
            FileHookAgent::Qoder => {
                let dir = agent_home("QODER_CONFIG_DIR", ".qoder");
                (dir.is_dir()
                    || binary_exists("qoder")
                    || binary_exists("qodercli")
                    || editor_installed("Qoder"))
                .then(|| dir.join("settings.json"))
                .into_iter()
                .collect()
            }
            FileHookAgent::Hermes => {
                let dir = hermes::home();
                (dir.is_dir() || binary_exists("hermes") || editor_installed("Hermes"))
                    .then(|| dir.join("config.yaml"))
                    .into_iter()
                    .collect()
            }
            FileHookAgent::Trae => [
                (".trae-cn", "trae-cn", ["Trae CN", "TraeCode CN"]),
                (".trae", "trae", ["Trae", "TraeCode"]),
            ]
            .into_iter()
            .filter_map(|(profile, binary, apps)| {
                let dir = home.join(profile);
                (dir.is_dir() || binary_exists(binary) || apps.into_iter().any(editor_installed))
                    .then(|| dir.join("hooks.json"))
            })
            .collect(),
            FileHookAgent::KimiCode => {
                let dir = agent_home("KIMI_CODE_HOME", ".kimi-code");
                // The older kimi-cli also exposes `kimi`, with a different data directory.
                dir.is_dir()
                    .then(|| dir.join("config.toml"))
                    .into_iter()
                    .collect()
            }
        }
    }

    fn hooks(&self, params: &HookInstallerParams) -> Hooks {
        let args = format!(" checkpoint {} --hook-input stdin", self.id());
        let path = params.binary_path.to_string_lossy();
        let executable = if cfg!(windows) {
            match self.0 {
                FileHookAgent::Qoder | FileHookAgent::Trae => {
                    format!("& '{}'", path.replace('\'', "''"))
                }
                _ => format!("\"{path}\""),
            }
        } else {
            format!("'{}'", path.replace('\'', "'\\''"))
        };
        let tools = self.0.tool_names().join("|");
        let matcher = if self.0 == FileHookAgent::Qoder {
            tools
        } else {
            format!("^({tools})$")
        };
        let mut events: Vec<_> = self
            .0
            .edit_events()
            .map(|event| (event, Some(matcher.clone())))
            .into_iter()
            .collect();
        match self.0 {
            FileHookAgent::Trae => events.extend([("UserPromptSubmit", None), ("Stop", None)]),
            FileHookAgent::KimiCode => events.push(("Stop", None)),
            _ => {}
        }
        Hooks {
            command: format!("{executable}{args}"),
            args,
            binary_path: params.binary_path.clone(),
            events,
        }
    }

    fn config_with_hooks(
        &self,
        original: &str,
        hooks: &Hooks,
        install: bool,
    ) -> Result<String, GitAiError> {
        match self.0 {
            FileHookAgent::Qoder | FileHookAgent::Trae => edit_json(original, hooks, install),
            FileHookAgent::Hermes => edit_hermes_config(original, hooks, install),
            FileHookAgent::KimiCode => edit_toml(original, hooks, install),
        }
    }

    fn edit_config(
        &self,
        path: &Path,
        hooks: &Hooks,
        install: bool,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let original = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let updated = self
            .config_with_hooks(&original, hooks, install)
            .map_err(|error| GitAiError::Generic(format!("{}: {error}", path.display())))?;
        let hermes_files = if self.0 == FileHookAgent::Hermes {
            hermes_plugin_updates(path, &hooks.binary_path, install)?
        } else {
            Vec::new()
        };
        if updated == original && hermes_files.is_empty() {
            return Ok(None);
        }
        if !dry_run {
            if install {
                for (path, content) in &hermes_files {
                    write_config(path, content)?;
                }
            }
            if updated != original {
                write_config(path, &updated)?;
            }
            if !install {
                for (path, _) in &hermes_files {
                    fs::remove_file(path)?;
                }
            }
        }
        // Agent configuration files may contain credentials. Display only the hook change.
        let definitions = if self.0 == FileHookAgent::Hermes {
            format!("plugins:\n  enabled:\n    - {HERMES_PLUGIN}\n")
        } else {
            hooks
                .events
                .iter()
                .map(|(event, _)| format!("{event}: {}\n", hooks.command))
                .collect::<String>()
        };
        let (before, after) = if install {
            ("", definitions.as_str())
        } else {
            (definitions.as_str(), "")
        };
        Ok(Some(generate_diff(path, before, after)))
    }

    fn update(
        &self,
        params: &HookInstallerParams,
        install: bool,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let hooks = self.hooks(params);
        let mut diffs = Vec::new();
        for path in self.config_paths() {
            if let Some(diff) = self.edit_config(&path, &hooks, install, dry_run)? {
                diffs.push(diff);
            }
        }
        Ok((!diffs.is_empty()).then(|| diffs.join("\n")))
    }
}

impl HookInstaller for FileHookInstaller {
    fn name(&self) -> &str {
        match self.0 {
            FileHookAgent::Qoder => "Qoder",
            FileHookAgent::Hermes => "Hermes Agent",
            FileHookAgent::Trae => "TRAE",
            FileHookAgent::KimiCode => "Kimi Code",
        }
    }

    fn id(&self) -> &str {
        self.0.id()
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let paths = self.config_paths();
        let hooks = self.hooks(params);
        let mut hooks_installed = false;
        let mut hooks_up_to_date = !paths.is_empty();
        for path in &paths {
            hooks_installed |= self.edit_config(path, &hooks, false, true)?.is_some();
            hooks_up_to_date &= self.edit_config(path, &hooks, true, true)?.is_none();
        }
        Ok(HookCheckResult {
            tool_installed: !paths.is_empty(),
            hooks_installed,
            hooks_up_to_date,
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        self.update(params, true, dry_run)
    }

    fn uninstall_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        self.update(params, false, dry_run)
    }

    fn process_names(&self) -> Vec<&str> {
        match self.0 {
            FileHookAgent::Qoder => vec!["Qoder", "qodercli"],
            FileHookAgent::Hermes => vec!["hermes"],
            FileHookAgent::Trae => vec!["Trae", "Trae CN", "TraeCode", "TraeCode CN", "trae-cn"],
            FileHookAgent::KimiCode => vec!["kimi"],
        }
    }
}

fn editor_installed(name: &str) -> bool {
    settings_path_candidates(name)
        .iter()
        .any(|path| path.is_file())
        || (cfg!(target_os = "macos")
            && [
                PathBuf::from("/Applications"),
                home_dir().join("Applications"),
            ]
            .iter()
            .any(|dir| dir.join(format!("{name}.app")).is_dir()))
}

fn write_config(path: &Path, content: &str) -> Result<(), GitAiError> {
    let target = if path.is_symlink() {
        path.canonicalize()?
    } else {
        path.to_path_buf()
    };
    ensure_parent_dir(&target)?;
    let parent = target.parent().unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    if let Ok(metadata) = fs::metadata(&target) {
        file.as_file().set_permissions(metadata.permissions())?;
    }
    file.write_all(content.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(target).map_err(|error| error.error)?;
    Ok(())
}

fn invalid_shape(key: &str) -> GitAiError {
    GitAiError::Generic(format!("Unsupported hook configuration at {key}"))
}

fn edit_json(original: &str, desired: &Hooks, install: bool) -> Result<String, GitAiError> {
    let source = original.trim_start_matches('\u{feff}');
    let root = CstRootNode::parse(source, &ParseOptions::default())
        .map_err(|error| GitAiError::Generic(error.to_string()))?;
    if root.value().is_none() && !install {
        return Ok(original.to_owned());
    }
    let config = root
        .object_value_or_create()
        .ok_or_else(|| invalid_shape("root"))?;
    if config.get("hooks").is_none() && !install {
        return Ok(original.to_owned());
    }
    let hooks = config
        .object_value_or_create("hooks")
        .ok_or_else(|| invalid_shape("hooks"))?;
    for (event, matcher) in &desired.events {
        if hooks.get(event).is_none() && !install {
            continue;
        }
        let groups = hooks
            .array_value_or_create(event)
            .ok_or_else(|| invalid_shape(event))?;
        let mut owned = Vec::new();
        for group in groups
            .elements()
            .into_iter()
            .filter_map(|node| node.as_object())
        {
            let Some(commands) = group.array_value("hooks") else {
                continue;
            };
            let group_matcher = json_string(&group, "matcher");
            for node in commands.elements() {
                let Some(entry) = node.as_object() else {
                    continue;
                };
                let command = json_string(&entry, "command");
                if let Some(command) = command.filter(|command| desired.owns(command)) {
                    let current = command == desired.command
                        && &group_matcher == matcher
                        && !json_bool(&group, "async")
                        && !json_bool(&entry, "async")
                        && !json_bool(&entry, "asyncRewake")
                        && json_string(&entry, "type").as_deref() == Some("command")
                        && entry.get("if").is_none()
                        && entry.get("args").is_none();
                    owned.push((node, commands.clone(), group.clone(), current));
                }
            }
        }
        if install && owned.len() == 1 && owned[0].3 {
            continue;
        }
        let removed = !owned.is_empty();
        for (node, commands, group, _) in owned.into_iter().rev() {
            node.remove();
            if commands.elements().is_empty() {
                group.remove();
            }
        }
        if install {
            let command = desired.command.as_str();
            let group = groups
                .append(json!({
                    "hooks": [{"type": "command", "command": command}]
                }))
                .as_object()
                .expect("new hook group is an object");
            if let Some(matcher) = matcher {
                group.append("matcher", matcher.as_str().into());
            }
        } else if removed
            && groups.elements().is_empty()
            && let Some(property) = hooks.get(event)
        {
            property.remove();
        }
    }
    let updated = root.to_string();
    if source.len() != original.len() {
        Ok(format!("\u{feff}{updated}"))
    } else {
        Ok(updated)
    }
}

fn json_string(object: &CstObject, key: &str) -> Option<String> {
    object
        .get(key)?
        .value()?
        .as_string_lit()?
        .decoded_value()
        .ok()
}

fn json_bool(object: &CstObject, key: &str) -> bool {
    object
        .get(key)
        .and_then(|prop| prop.value())
        .and_then(|node| node.as_boolean_lit())
        .is_some_and(|value| value.value())
}

fn hermes_plugin_updates(
    config_path: &Path,
    binary: &Path,
    install: bool,
) -> Result<Vec<(PathBuf, String)>, GitAiError> {
    let dir = config_path.with_file_name("plugins").join(HERMES_PLUGIN);
    let conflict = || GitAiError::Generic(format!("Unmanaged Hermes plugin at {}", dir.display()));
    if dir.is_symlink() {
        return Err(conflict());
    }
    let manifest = include_str!("../../../agent-support/hermes/plugin.yaml");
    let python = include_str!("../../../agent-support/hermes/__init__.py")
        .strip_prefix(HERMES_MARKER)
        .expect("managed Hermes plugin marker");
    // A JSON string literal also makes a valid Python string for paths.
    let binary = serde_json::to_string(&binary.to_string_lossy())?;
    let python = format!("{HERMES_MARKER}GIT_AI = {binary}\n{python}");
    let mut updates = Vec::new();
    for (name, content) in [("plugin.yaml", manifest), ("__init__.py", python.as_str())] {
        let path = dir.join(name);
        if path.is_symlink() {
            return Err(conflict());
        }
        match fs::read_to_string(&path) {
            Ok(current) if !current.starts_with(HERMES_MARKER) => return Err(conflict()),
            Ok(current) if !install || current != content => {
                updates.push((path, content.to_owned()))
            }
            Err(error) if error.kind() != ErrorKind::NotFound => return Err(error.into()),
            Err(_) if install => updates.push((path, content.to_owned())),
            _ => {}
        }
    }
    Ok(updates)
}

fn edit_hermes_config(
    original: &str,
    desired: &Hooks,
    install: bool,
) -> Result<String, GitAiError> {
    let yaml: YamlFile = original
        .parse()
        .map_err(|error: yaml_edit::YamlError| GitAiError::Generic(error.to_string()))?;
    let mut docs = yaml.documents();
    let Some(document) = docs.next() else {
        return if install {
            edit_hermes_config(&format!("{original}\nplugins: {{}}\n"), desired, true)
        } else {
            Ok(original.to_owned())
        };
    };
    if docs.next().is_some() {
        return Err(invalid_shape("root"));
    }
    let root = document.as_mapping().ok_or_else(|| invalid_shape("root"))?;
    if original.contains(&desired.args)
        && let Some(hooks) = root.get_mapping("hooks")
    {
        for (event, _) in &desired.events {
            let Some(entries) = hooks.get_sequence(*event) else {
                continue;
            };
            for (index, node) in entries
                .values()
                .collect::<Vec<_>>()
                .into_iter()
                .enumerate()
                .rev()
            {
                let command = node
                    .as_mapping()
                    .and_then(|entry| entry.get("command"))
                    .and_then(|command| command.as_scalar().map(|scalar| scalar.as_string()));
                if command.is_some_and(|command| desired.owns(&command)) {
                    entries.remove(index);
                }
            }
        }
    }

    if root
        .get("plugins")
        .is_none_or(|node| node.as_scalar().is_some_and(|scalar| scalar.is_null()))
    {
        if !install {
            return Ok(yaml.to_string());
        }
        if root.get("<<").is_some() {
            return Err(invalid_shape("plugins (YAML merge)"));
        }
        root.set("plugins", Mapping::new());
    }
    let plugins = root
        .get_mapping("plugins")
        .ok_or_else(|| invalid_shape("plugins"))?;
    if plugins
        .get("enabled")
        .is_none_or(|node| node.as_scalar().is_some_and(|scalar| scalar.is_null()))
    {
        if !install {
            return Ok(yaml.to_string());
        }
        if plugins.get("<<").is_some() {
            return Err(invalid_shape("plugins.enabled (YAML merge)"));
        }
        plugins.set("enabled", Sequence::new());
    }
    let enabled = plugins
        .get_sequence("enabled")
        .ok_or_else(|| invalid_shape("plugins.enabled"))?;
    let owned: Vec<_> = enabled
        .values()
        .enumerate()
        .filter_map(|(index, node)| {
            (node.as_scalar()?.as_string() == HERMES_PLUGIN).then_some(index)
        })
        .collect();
    if install && owned.is_empty() {
        enabled.push(HERMES_PLUGIN);
    } else if !install {
        for index in owned.into_iter().rev() {
            enabled.remove(index);
        }
    }
    Ok(yaml.to_string())
}

fn edit_toml(original: &str, desired: &Hooks, install: bool) -> Result<String, GitAiError> {
    let mut config = original
        .parse::<DocumentMut>()
        .map_err(|error| GitAiError::Generic(error.message().to_owned()))?;
    let Some(hooks) = config.get_mut("hooks") else {
        if !install {
            return Ok(original.to_owned());
        }
        config.insert("hooks", Item::ArrayOfTables(toml_hooks(desired)));
        return Ok(config.to_string());
    };
    if let Some(array) = hooks.as_array_mut() {
        if toml_hooks_current(
            array.iter().filter_map(|item| item.as_inline_table()),
            desired,
            install,
        ) {
            return Ok(original.to_owned());
        }
        array.retain(|item| {
            item.as_inline_table()
                .is_none_or(|table| !owns_toml_hook(table, desired))
        });
        if install {
            for table in toml_hooks(desired) {
                array.push(table.into_inline_table());
            }
        }
    } else {
        let tables = hooks
            .as_array_of_tables_mut()
            .ok_or_else(|| invalid_shape("hooks"))?;
        if toml_hooks_current(tables.iter(), desired, install) {
            return Ok(original.to_owned());
        }
        tables.retain(|table| !owns_toml_hook(table, desired));
        if install {
            tables.extend(toml_hooks(desired));
        }
        if tables.is_empty() {
            config.remove("hooks");
        }
    }
    Ok(config.to_string())
}

fn owns_toml_hook(table: &impl toml_edit::TableLike, desired: &Hooks) -> bool {
    table
        .get("command")
        .and_then(Item::as_str)
        .is_some_and(|command| desired.owns(command))
}

fn toml_hooks_current<'a, T: toml_edit::TableLike + 'a>(
    tables: impl Iterator<Item = &'a T>,
    desired: &Hooks,
    install: bool,
) -> bool {
    let owned: Vec<_> = tables
        .filter(|table| owns_toml_hook(*table, desired))
        .collect();
    if !install {
        return owned.is_empty();
    }
    owned.len() == desired.events.len()
        && desired.events.iter().all(|(event, matcher)| {
            owned.iter().any(|table| {
                table.get("command").and_then(Item::as_str) == Some(desired.command.as_str())
                    && table.get("event").and_then(Item::as_str) == Some(*event)
                    && table.get("matcher").and_then(Item::as_str) == matcher.as_deref()
            })
        })
}

fn toml_hooks(desired: &Hooks) -> ArrayOfTables {
    desired
        .events
        .iter()
        .map(|(event, matcher)| {
            let mut entry = InlineTable::new();
            entry.insert("event", (*event).into());
            entry.insert("command", desired.command.as_str().into());
            if let Some(matcher) = matcher {
                entry.insert("matcher", matcher.as_str().into());
            }
            entry.into_table()
        })
        .collect()
}
