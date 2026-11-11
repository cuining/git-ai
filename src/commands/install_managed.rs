use clap::{Parser, ValueEnum};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

const STATE_FILE: &str = "dcc-install.json";
const LEASE_FILE: &str = "install.lock";
const STATE_TEMP_PREFIX: &str = ".git-ai-managed-state-";
const BINARY_TEMP_PREFIX: &str = ".git-ai-managed-bin-";
const LEASE_OWNER_PREFIX: &str = "dcca";
const RETRY_MS: u64 = 5 * 60_000;
const BINARY_NAME: &str = if cfg!(windows) {
    "dcc-ai-tracking.exe"
} else {
    "dcc-ai-tracking"
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
enum Agent {
    Claude,
    Codex,
    Opencode,
}

impl Agent {
    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Parser)]
#[command(name = "dcc-ai-tracking install-managed")]
struct Options {
    #[arg(long, value_enum)]
    agent: Agent,
    #[arg(long, value_parser = parse_sha256)]
    sha256: String,
    #[arg(long)]
    force: bool,
}

fn parse_sha256(value: &str) -> Result<String, &'static str> {
    if value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(value.to_owned())
    } else {
        Err("expected a lowercase SHA-256 digest")
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum Outcome {
    Ready,
    Skipped { reason: &'static str },
    Conflict { reason: &'static str },
    Failed { reason: &'static str },
}

impl Outcome {
    fn failed(reason: &'static str) -> Self {
        Self::Failed { reason }
    }

    fn external() -> Self {
        Self::Conflict {
            reason: "external-installation",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallState {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    binary_sha256: Option<String>,
    initialized_tools: Vec<Agent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_failure_at: Option<u64>,
}

impl InstallState {
    fn pending(version: &Version) -> Self {
        Self {
            version: version.to_string(),
            binary_sha256: None,
            initialized_tools: Vec::new(),
            last_failure_at: None,
        }
    }

    fn save(&self, marker: &Path) -> Result<(), Outcome> {
        regular_file(marker)?;
        let write = || -> io::Result<()> {
            let mut file = tempfile::Builder::new()
                .prefix(STATE_TEMP_PREFIX)
                .tempfile_in(marker.parent().expect("marker has parent"))?;
            serde_json::to_writer(&mut file, self)?;
            file.write_all(b"\n")?;
            file.as_file().sync_all()?;
            file.persist(marker).map_err(|error| error.error)?;
            Ok(())
        };
        write().map_err(|_| Outcome::failed("state-write-failed"))
    }
}

pub(super) fn run(args: &[String]) {
    let options = Options::parse_from(
        std::iter::once("install-managed").chain(args.iter().map(String::as_str)),
    );
    let outcome = install(&options).err().unwrap_or(Outcome::Ready);
    println!(
        "{}",
        serde_json::to_string(&outcome).expect("serialize install result")
    );
}

fn install(options: &Options) -> Result<(), Outcome> {
    let source =
        crate::utils::current_git_ai_exe().map_err(|_| Outcome::failed("source-unavailable"))?;
    if digest(&source).map_err(|_| Outcome::failed("source-unavailable"))? != options.sha256 {
        return Err(Outcome::failed("source-checksum-mismatch"));
    }
    let version = Version::parse(env!("CARGO_PKG_VERSION")).expect("valid package version");
    let root = crate::config::git_ai_dir_path()
        .ok_or_else(|| Outcome::failed("install-root-unavailable"))?;
    let marker = root.join(STATE_FILE);
    let binary = root.join("bin").join(BINARY_NAME);
    // Inspect an unowned or newer installation without creating lease files in it.
    read_state(&root, &version)?;
    create_directory(&root)?;
    let lease = Lease::acquire(root.join(LEASE_FILE))?;
    let prior_state = read_state(&root, &version)?;
    lease.check()?;
    let mut state = prior_state
        .clone()
        .unwrap_or_else(|| InstallState::pending(&version));
    let exists = regular_file(&binary)?;
    let stable_matches = exists && digest(&binary).is_ok_and(|hash| hash == options.sha256);
    let current = stable_matches
        && state.version == version.to_string()
        && state.binary_sha256.as_deref() == Some(&options.sha256);
    if !options.force
        && current
        && state.initialized_tools.contains(&options.agent)
        && daemon_healthy(&binary)
    {
        return Ok(());
    }
    if !options.force
        && state
            .last_failure_at
            .is_some_and(|ts| now_ms().saturating_sub(ts) < RETRY_MS)
    {
        return Err(Outcome::Skipped {
            reason: "retry-pending",
        });
    }

    if prior_state.is_none() {
        // Claim an empty root before creating anything that could look like an external install on retry.
        state.save(&marker)?;
    }
    remove_abandoned_temps(&root, STATE_TEMP_PREFIX);
    remove_abandoned_temps(&root.join("bin"), BINARY_TEMP_PREFIX);
    let restart = !current || state.initialized_tools.is_empty();
    let result = (|| {
        if !stable_matches {
            let staged = stage_binary(&source, &binary, &options.sha256)?;
            let had_binary = exists || state.binary_sha256.is_some();
            let replace = || {
                lease.check()?;
                regular_file(&binary)?;
                let pending = InstallState::pending(&version);
                // Prevent older installers from downgrading after an interrupted replacement.
                pending.save(&marker)?;
                if staged.persist(&binary).is_err() {
                    state.save(&marker)?;
                    return Err(Outcome::failed("binary-install-failed"));
                }
                state = pending;
                Ok(())
            };
            if had_binary {
                super::daemon::with_stopped_daemon(replace)
                    .map_err(|_| Outcome::failed("daemon-shutdown-failed"))??;
            } else {
                replace()?;
            }
        } else if !current {
            state = InstallState::pending(&version);
        }
        if !regular_file(&binary)? || digest(&binary).ok().as_deref() != Some(&options.sha256) {
            return Err(Outcome::failed("installed-checksum-mismatch"));
        }
        lease.check()?;
        state.binary_sha256 = Some(options.sha256.clone());
        state
            .initialized_tools
            .retain(|agent| *agent != options.agent);
        state.save(&marker)?;
        super::install_hooks::initialize_managed_local(&binary, options.agent.name(), restart)
            .map_err(|_| Outcome::failed("initialization-failed"))?;
        lease.check()?;
        let mut ready = state.clone();
        ready.initialized_tools.push(options.agent);
        ready.last_failure_at = None;
        ready.save(&marker)
    })();
    if matches!(result, Err(Outcome::Failed { .. })) && lease.check().is_ok() {
        state.last_failure_at = Some(now_ms());
        let _ = state.save(&marker);
    }
    result
}

fn read_state(root: &Path, version: &Version) -> Result<Option<InstallState>, Outcome> {
    let Some(root_metadata) = metadata(root)? else {
        return Ok(None);
    };
    if !root_metadata.is_dir() || is_link(&root_metadata) {
        return Err(Outcome::external());
    }
    let marker = root.join(STATE_FILE);
    if !regular_file(&marker)? {
        for entry in fs::read_dir(root).map_err(|_| Outcome::failed("install-root-unavailable"))? {
            let entry = entry.map_err(|_| Outcome::failed("install-root-unavailable"))?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(Outcome::external)?;
            let legacy_candidate = name
                .strip_prefix('.')
                .and_then(|name| name.strip_suffix(".lock-candidate"))
                .filter(|id| {
                    id.len() == 36
                        && id
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
                });
            if name.starts_with(STATE_TEMP_PREFIX)
                || legacy_candidate.is_some_and(|id| id.starts_with(LEASE_OWNER_PREFIX))
            {
                // Recover if killed while writing our first atomic state or lease candidate.
                regular_file(&entry.path())?;
            } else if name == LEASE_FILE || legacy_candidate.is_some() {
                if read_lease(&entry.path())?.is_none() && entry.path().exists() {
                    return Err(Outcome::external());
                }
            } else {
                return Err(Outcome::external());
            }
        }
        return Ok(None);
    }
    let bytes = fs::read(marker).map_err(|_| Outcome::failed("state-read-failed"))?;
    let state: InstallState = serde_json::from_slice(&bytes).map_err(|_| Outcome::Conflict {
        reason: "invalid-install-state",
    })?;
    let installed = Version::parse(&state.version).map_err(|_| Outcome::Conflict {
        reason: "invalid-install-state",
    })?;
    if installed > *version {
        return Err(Outcome::Conflict {
            reason: "version-conflict",
        });
    }
    if let Some(bin) = metadata(&root.join("bin"))?
        && (!bin.is_dir() || is_link(&bin))
    {
        return Err(Outcome::external());
    }
    Ok(Some(state))
}

fn metadata(path: &Path) -> Result<Option<Metadata>, Outcome> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(_) => Err(Outcome::failed("install-path-unavailable")),
    }
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

fn regular_file(path: &Path) -> Result<bool, Outcome> {
    match metadata(path)? {
        None => Ok(false),
        Some(metadata) if metadata.is_file() && !is_link(&metadata) => Ok(true),
        Some(_) => Err(Outcome::external()),
    }
}

fn create_directory(path: &Path) -> Result<(), Outcome> {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => match metadata(path)? {
            Some(metadata) if metadata.is_dir() && !is_link(&metadata) => Ok(()),
            _ => Err(Outcome::external()),
        },
        Err(_) => Err(Outcome::failed("install-path-unavailable")),
    }
}

fn digest(path: &Path) -> io::Result<String> {
    let mut hash = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

fn remove_abandoned_temps(dir: &Path, prefix: &str) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(prefix))
                && regular_file(&entry.path()).is_ok_and(|regular| regular)
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn stage_binary(source: &Path, binary: &Path, expected: &str) -> Result<NamedTempFile, Outcome> {
    let dir = binary.parent().expect("binary has parent");
    create_directory(dir)?;
    let copy = || -> io::Result<NamedTempFile> {
        let mut file = tempfile::Builder::new()
            .prefix(BINARY_TEMP_PREFIX)
            .tempfile_in(dir)?;
        io::copy(&mut File::open(source)?, &mut file)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        file.as_file().sync_all()?;
        Ok(file)
    };
    let staged = copy().map_err(|_| Outcome::failed("binary-stage-failed"))?;
    if digest(staged.path()).ok().as_deref() != Some(expected) {
        return Err(Outcome::failed("source-checksum-mismatch"));
    }
    Ok(staged)
}

fn daemon_healthy(binary: &Path) -> bool {
    Command::new(binary)
        .args(["daemon", "health"])
        .env_remove("GIT_AI")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaseRecord {
    pid: u32,
    owner_id: String,
}

struct Lease {
    path: PathBuf,
    record: LeaseRecord,
}

impl Lease {
    fn acquire(path: PathBuf) -> Result<Self, Outcome> {
        let id = format!("{:032x}", rand::random::<u128>());
        let owner_id = format!(
            "{LEASE_OWNER_PREFIX}{}-{}-{}-{}-{}",
            &id[4..8],
            &id[8..12],
            &id[12..16],
            &id[16..20],
            &id[20..]
        );
        let record = LeaseRecord {
            pid: std::process::id(),
            owner_id,
        };
        let candidate = path.with_file_name(format!(".{}.lock-candidate", record.owner_id));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&candidate)
            .map_err(|_| Outcome::failed("lock-unavailable"))?;
        let result = (|| {
            serde_json::to_writer(&mut file, &record)
                .map_err(|_| Outcome::failed("lock-unavailable"))?;
            file.sync_all()
                .map_err(|_| Outcome::failed("lock-unavailable"))?;
            drop(file);
            let started = Instant::now();
            loop {
                match fs::hard_link(&candidate, &path) {
                    Ok(()) => return Ok(()),
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                        reap_stale_lease(&path)?;
                        if started.elapsed() >= Duration::from_secs(5) {
                            return Err(Outcome::failed("lock-timeout"));
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => return Err(Outcome::failed("lock-unavailable")),
                }
            }
        })();
        let _ = fs::remove_file(candidate);
        result?;
        Ok(Self { path, record })
    }

    fn check(&self) -> Result<(), Outcome> {
        if read_lease(&self.path)?.as_ref() == Some(&self.record) {
            Ok(())
        } else {
            Err(Outcome::failed("lock-lost"))
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.check().is_ok() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn read_lease(path: &Path) -> Result<Option<LeaseRecord>, Outcome> {
    if !regular_file(path)? {
        return Ok(None);
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Outcome::failed("lock-unavailable")),
    };
    let record = serde_json::from_slice::<LeaseRecord>(&bytes)
        .ok()
        .filter(|record| record.pid > 0 && record.pid <= i32::MAX as u32);
    Ok(record)
}

fn reap_stale_lease(path: &Path) -> Result<(), Outcome> {
    if let Some(record) = read_lease(path)?
        && !crate::daemon::process_alive(record.pid)
        // Lock the existing inode without creating an empty legacy lease on a race.
        && let Ok(stale) = File::options().read(true).write(true).open(path)
        && stale.try_lock().is_ok()
        && read_lease(path)?.as_ref() == Some(&record)
    {
        let _ = fs::remove_file(path);
    }
    Ok(())
}
