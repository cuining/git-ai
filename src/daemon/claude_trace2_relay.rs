use super::{
    DaemonConfig, local_socket_connects_with_timeout, remove_socket_if_exists,
    set_socket_owner_only,
};
use crate::error::GitAiError;
use crate::mdm::utils::{claude_config_dir, home_dir, shell_quote_path, write_atomic};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::{Builder, NamedTempFile, TempDir};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;

const DELIVERY_TIMEOUT: Duration = Duration::from_millis(250);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);
const TRACE2_EVENT_ENV: &str = "GIT_TRACE2_EVENT";
const SESSION_START: &str = "SessionStart";
const HOOK_SOURCE: &str = "dcc-claude-trace2";

fn moss_trace_dir() -> PathBuf {
    home_dir().join(".moss/trace2")
}

fn relay_socket(config: &DaemonConfig) -> PathBuf {
    config.lock_path.with_file_name("moss.sock")
}

pub(crate) fn configure(args: &[String]) -> Result<(), GitAiError> {
    let enable = match args {
        [arg] if arg == "enable" => true,
        [arg] if arg == "disable" => false,
        _ => {
            return Err(GitAiError::Generic(
                "Usage: claude-trace2 <enable|disable>".into(),
            ));
        }
    };
    let config = DaemonConfig::from_env_or_default_paths()?;
    let socket = relay_socket(&config);
    if enable {
        let moss_dir = moss_trace_dir();
        if global_trace_target()?.is_none_or(|engine| engine == socket) {
            return Err(GitAiError::Generic(
                "Set Git's global trace2.eventTarget to af_unix:stream:<absolute socket path>"
                    .into(),
            ));
        }
        if !moss_dir.is_dir() {
            return Err(GitAiError::Generic(format!(
                "MOSS trace directory does not exist: {}",
                moss_dir.display()
            )));
        }
        if local_socket_connects_with_timeout(&socket, DELIVERY_TIMEOUT).is_err() {
            crate::commands::daemon::restart_daemon(&config).map_err(GitAiError::Generic)?;
            local_socket_connects_with_timeout(&socket, DELIVERY_TIMEOUT)?;
        }
    }

    let settings_path = claude_config_dir().join("settings.json");
    let mut settings: Map<String, Value> = match fs::read(&settings_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Map::new(),
        Err(error) => return Err(error.into()),
    };
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| GitAiError::Generic("Claude settings hooks must be an object".into()))?;
    let blocks = hooks
        .entry(SESSION_START)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| GitAiError::Generic("Claude SessionStart hooks must be an array".into()))?;
    let original = blocks.clone();
    blocks.retain(|block| !is_relay_hook(block));
    if enable {
        let target = DaemonConfig::trace2_event_target_for_path(&socket);
        // Claude sources this check for each Bash tool call. A stale socket
        // after a crash leaves the inherited MOSS target in effect.
        let session_env = format!(
            r#"if [ -S {} ] && IFS= read -r _dcc_trace2_pid 2>/dev/null < {} && kill -0 "$_dcc_trace2_pid" 2>/dev/null; then export {TRACE2_EVENT_ENV}={}; fi; unset _dcc_trace2_pid"#,
            shell_quote_path(&socket.to_string_lossy()),
            shell_quote_path(&socket.with_extension("pid").to_string_lossy()),
            shell_quote_path(&target),
        );
        let command = format!(
            r#"if [ -n "${{CLAUDE_ENV_FILE:-}}" ]; then printf '%s\n' {} >> "$CLAUDE_ENV_FILE"; fi"#,
            shell_quote_path(&session_env),
        );
        blocks.push(json!({
            "matcher": "*",
            "_source": HOOK_SOURCE,
            "hooks": [{"type": "command", "command": command}]
        }));
    }
    if *blocks != original {
        if blocks.is_empty() {
            hooks.remove(SESSION_START);
            if hooks.is_empty() {
                settings.remove("hooks");
            }
        }
        write_atomic(&settings_path, &serde_json::to_vec_pretty(&settings)?)?;
    }
    Ok(())
}

fn is_relay_hook(block: &Value) -> bool {
    block["_source"] == HOOK_SOURCE
}

fn global_trace_target() -> Result<Option<PathBuf>, GitAiError> {
    let config = gix_config::File::from_globals()
        .map_err(|error| GitAiError::GixError(error.to_string()))?;
    Ok(config
        .raw_value_filter("trace2.eventTarget", |metadata| {
            metadata.source.kind() == gix_config::source::Kind::Global
        })
        .ok()
        .and_then(|target| {
            target
                .to_string()
                .strip_prefix("af_unix:stream:")
                .map(PathBuf::from)
        })
        .filter(|path| path.is_absolute()))
}

pub(super) struct RunningRelay {
    socket: PathBuf,
    stop: oneshot::Sender<()>,
    serving: JoinHandle<()>,
}

impl RunningRelay {
    pub(super) async fn stop(self) {
        let Self {
            socket,
            stop,
            mut serving,
        } = self;
        let _ = fs::remove_file(socket.with_extension("pid"));
        let _ = stop.send(());
        if timeout(DRAIN_TIMEOUT, &mut serving).await.is_err() {
            serving.abort();
            let _ = serving.await;
            tracing::warn!("Claude Trace2 relay timed out draining Git connections");
        }
        if let Err(error) = remove_socket_if_exists(&socket) {
            tracing::warn!(%error, "could not remove Claude Trace2 relay socket");
        }
    }
}

pub(super) fn start(config: &DaemonConfig) -> Result<Option<RunningRelay>, GitAiError> {
    let moss_dir = moss_trace_dir();
    if !moss_dir.is_dir() {
        return Ok(None);
    }
    let socket = relay_socket(config);
    let engine = match global_trace_target() {
        Ok(engine) => engine.filter(|engine| engine != &socket),
        Err(error) => {
            tracing::warn!(%error, "could not read Claude Trace2 relay's global target");
            None
        }
    };
    if engine.is_none() {
        // If the global target is removed, an installed hook must still deliver to MOSS.
        let settings = fs::read(claude_config_dir().join("settings.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let hook_installed = settings
            .and_then(|settings| {
                settings["hooks"][SESSION_START]
                    .as_array()
                    .map(|blocks| blocks.iter().any(is_relay_hook))
            })
            .unwrap_or(false);
        if !hook_installed {
            return Ok(None);
        }
    }

    // MOSS skips subdirectories. The daemon lock protects this directory
    // across restarts so we can reclaim any partial traces after a crash.
    let spool_name = format!(
        ".dcc-relay-{:x}",
        Sha256::digest(config.lock_path.as_os_str().as_encoded_bytes())
    );
    match fs::remove_dir_all(moss_dir.join(&spool_name)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let spool = Builder::new()
        .prefix(&spool_name)
        .rand_bytes(0)
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(&moss_dir)?;
    remove_socket_if_exists(&socket)?;
    let listener = UnixListener::bind(&socket)?;
    set_socket_owner_only(&socket)?;
    fs::write(
        socket.with_extension("pid"),
        format!("{}\n", std::process::id()),
    )?;
    let (stop, shutdown) = oneshot::channel();
    let relay = Arc::new(Relay {
        engine,
        moss_dir,
        spool,
    });
    let serving = tokio::spawn(relay.serve(listener, shutdown));
    Ok(Some(RunningRelay {
        socket,
        stop,
        serving,
    }))
}

struct Relay {
    engine: Option<PathBuf>,
    moss_dir: PathBuf,
    spool: TempDir,
}

impl Relay {
    async fn serve(self: Arc<Self>, listener: UnixListener, mut shutdown: oneshot::Receiver<()>) {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    report_connection(result.map_err(io::Error::other).and_then(|result| result));
                }
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        connections.spawn(Arc::clone(&self).forward(stream));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Claude Trace2 relay could not accept connection");
                        tokio::select! {
                            _ = tokio::time::sleep(ACCEPT_RETRY_DELAY) => {}
                            _ = &mut shutdown => break,
                        }
                    }
                },
                _ = &mut shutdown => break,
            }
        }
        drop(listener);
        while let Some(result) = connections.join_next().await {
            report_connection(result.map_err(io::Error::other).and_then(|result| result));
        }
    }

    async fn forward(self: Arc<Self>, stream: UnixStream) -> io::Result<()> {
        let mut reader = BufReader::new(stream);
        let mut buffer = [0; 8192];
        let mut first = Vec::new();
        if (&mut reader)
            .take(buffer.len() as u64)
            .read_until(b'\n', &mut first)
            .await?
            == 0
        {
            return Ok(());
        }
        let mut engine = None;
        if let Some(path) = &self.engine {
            match timeout(DELIVERY_TIMEOUT, UnixStream::connect(path)).await {
                Ok(Ok(stream)) => engine = Some(stream),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "Claude Trace2 relay could not open global target")
                }
                Err(_) => tracing::warn!("Claude Trace2 relay timed out opening global target"),
            }
        }
        let mut moss = match NamedTempFile::new_in(self.spool.path()) {
            Ok(temp) => {
                let (file, path) = temp.into_parts();
                Some((BufWriter::new(tokio::fs::File::from_std(file)), path))
            }
            Err(error) => {
                tracing::warn!(%error, "Claude Trace2 relay could not stage MOSS trace");
                None
            }
        };
        let mut bytes = first.as_slice();
        loop {
            let (engine_write, moss_write) = tokio::join!(
                timeout(DELIVERY_TIMEOUT, write_bytes(engine.as_mut(), bytes)),
                write_bytes(moss.as_mut().map(|(file, _)| file), bytes),
            );
            if let Err(error) = engine_write
                .map_err(io::Error::from)
                .and_then(|result| result)
            {
                tracing::warn!(%error, "Claude Trace2 relay write to global target failed");
                engine = None;
            }
            if let Err(error) = moss_write {
                tracing::warn!(%error, "Claude Trace2 relay write to MOSS failed");
                moss = None;
            }
            let count = reader.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            bytes = &buffer[..count];
        }
        drop(engine);
        if let Some((mut file, path)) = moss {
            file.flush().await?;
            drop(file);
            path.persist_noclobber(self.moss_dir.join(trace_file_name(&first)))
                .map_err(|error| error.error)?;
        }
        Ok(())
    }
}

async fn write_bytes(
    target: Option<&mut (impl AsyncWrite + Unpin)>,
    bytes: &[u8],
) -> io::Result<()> {
    if let Some(target) = target {
        target.write_all(bytes).await?;
    }
    Ok(())
}

fn trace_file_name(first: &[u8]) -> String {
    if let Ok(frame) = serde_json::from_slice::<Value>(first)
        && let Some(sid) = frame["sid"].as_str()
        && let Some(name) = sid.rsplit('/').next()
        && name.starts_with(|ch: char| ch.is_ascii_digit())
        && name.len() < 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
    {
        return name.to_string();
    }
    crate::uuid::generate_v4()
}

fn report_connection(result: io::Result<()>) {
    if let Err(error) = result {
        tracing::warn!(%error, "Claude Trace2 relay connection failed");
    }
}
