use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use git_ai::daemon::{DaemonConfig, read_daemon_pid};
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn claude_bash_commit_reaches_the_global_trace2_engine_and_moss() {
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let global_target =
        DaemonConfig::trace2_event_target_for_path(&repo.daemon_trace_socket_path());
    set_global_target(&repo, &global_target);
    let settings = write_moss_settings(&repo);
    repo.start_dedicated_daemon_for_test();
    configure_relay(&repo, &settings, "enable");

    let mut file = repo.filename("example.txt");
    let path = repo.path().join("example.txt");
    fs::write(&path, "baseline\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "example.txt"])
        .unwrap();
    repo.stage_all_and_commit("baseline").unwrap();
    file.assert_committed_lines(crate::lines!["baseline".human()]);
    fs::write(&path, "baseline\nClaude edit\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.txt"])
        .unwrap();
    repo.git(&["add", "example.txt"]).unwrap();

    ClaudeBashSession::start(&repo, &settings).git(&repo, &["commit", "-m", "Claude Bash commit"]);

    file.assert_committed_lines(crate::lines!["baseline".human(), "Claude edit".ai()]);

    let moss_dir = repo.test_home_path().join(".moss/trace2");
    let commit_trace = trace_for_command(&moss_dir, "Claude Bash commit");
    let frames: Vec<Value> = commit_trace
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let start = frames
        .iter()
        .find(|frame| frame["event"] == "start")
        .unwrap();
    assert!(start["time"].is_string());
    assert!(frames.iter().any(|frame| frame["event"] == "atexit"));
}

#[test]
fn claude_session_start_preserves_moss_settings_and_delivers_git_to_both_targets() {
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let socket_dir = tempfile::tempdir_in("/tmp").unwrap();
    let socket = socket_dir.path().join("engine.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    set_global_target(&repo, &DaemonConfig::trace2_event_target_for_path(&socket));
    let git_config = repo.test_home_path().join(".gitconfig");
    let original_git_config = fs::read(&git_config).unwrap();
    let settings = write_moss_settings(&repo);
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o600)).unwrap();
    let original_settings = read_settings(&settings);
    repo.start_dedicated_daemon_for_test();
    configure_relay(&repo, &settings, "enable");
    configure_relay(&repo, &settings, "enable");
    let enabled_settings = read_settings(&settings);
    assert_eq!(enabled_settings["env"], original_settings["env"]);
    let session_hooks = enabled_settings["hooks"]["SessionStart"]
        .as_array()
        .unwrap();
    assert_eq!(session_hooks.len(), 2);
    assert!(session_hooks.contains(&original_settings["hooks"]["SessionStart"][0]));
    assert_eq!(
        fs::metadata(&settings).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Model MOSS rewriting its managed environment entry after enable.
    let mut reconciled = enabled_settings.clone();
    reconciled["env"]["GIT_TRACE2_EVENT"] = original_settings["env"]["GIT_TRACE2_EVENT"].clone();
    fs::write(&settings, reconciled.to_string()).unwrap();
    repo.restart_dedicated_daemon_for_test();
    ClaudeBashSession::start(&repo, &settings)
        .git(&repo, &["-c", "trace2.probe=with-settings", "version"]);
    let from_moss = trace_for_command(
        &repo.test_home_path().join(".moss/trace2"),
        "trace2.probe=with-settings",
    );
    assert_eq!(read_global_trace(&listener), from_moss);
    assert_eq!(read_settings(&settings), enabled_settings);
    assert_eq!(fs::read(&git_config).unwrap(), original_git_config);

    configure_relay(&repo, &settings, "disable");
    configure_relay(&repo, &settings, "disable");
    assert_eq!(read_settings(&settings), original_settings);
}

#[test]
fn claude_bash_git_reaches_moss_when_the_global_socket_is_unavailable() {
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let unavailable = repo.test_home_path().join("unavailable.sock");
    set_global_target(
        &repo,
        &DaemonConfig::trace2_event_target_for_path(&unavailable),
    );
    let settings = write_moss_settings(&repo);
    repo.start_dedicated_daemon_for_test();
    configure_relay(&repo, &settings, "enable");
    ClaudeBashSession::start(&repo, &settings)
        .git(&repo, &["-c", "trace2.probe=engine-offline", "version"]);
    let trace = trace_for_command(
        &repo.test_home_path().join(".moss/trace2"),
        "trace2.probe=engine-offline",
    );
    assert!(trace.contains(r#""event":"atexit""#));
}

#[test]
fn claude_bash_keeps_moss_traces_across_a_relay_crash() {
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let socket_dir = tempfile::tempdir_in("/tmp").unwrap();
    let engine = socket_dir.path().join("engine.sock");
    let listener = UnixListener::bind(&engine).unwrap();
    listener.set_nonblocking(true).unwrap();
    set_global_target(&repo, &DaemonConfig::trace2_event_target_for_path(&engine));
    let settings = write_moss_settings(&repo);
    let moss_dir = repo.test_home_path().join(".moss/trace2");
    repo.start_dedicated_daemon_for_test();
    configure_relay(&repo, &settings, "enable");
    let session = ClaudeBashSession::start(&repo, &settings);
    session.git(&repo, &["-c", "trace2.probe=relay-running", "version"]);
    assert_eq!(
        trace_for_command(&moss_dir, "trace2.probe=relay-running"),
        read_global_trace(&listener),
    );

    let config = DaemonConfig::from_home(&repo.daemon_home_path());
    let pid = read_daemon_pid(&config).unwrap();
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
    repo.shutdown_dedicated_daemon_for_test();
    assert!(config.lock_path.with_file_name("moss.sock").exists());
    session.git(&repo, &["-c", "trace2.probe=relay-crashed", "version"]);
    let trace = trace_for_command(&moss_dir, "trace2.probe=relay-crashed");
    assert!(trace.contains(r#""event":"atexit""#));

    repo.start_dedicated_daemon_for_test();
    session.git(&repo, &["-c", "trace2.probe=relay-restarted", "version"]);
    assert_eq!(
        trace_for_command(&moss_dir, "trace2.probe=relay-restarted"),
        read_global_trace(&listener),
    );
    let staging_dirs = fs::read_dir(&moss_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .count();
    assert_eq!(
        staging_dirs, 1,
        "the previous relay left a staging directory"
    );
}

fn read_global_trace(listener: &UnixListener) -> String {
    let (mut stream, _) = listener
        .accept()
        .expect("Git should connect to the global engine");
    // The relay closes this stream before publishing the MOSS trace.
    // macOS rejects setting a read timeout on the disconnected socket.
    stream.set_nonblocking(true).unwrap();
    let mut trace = String::new();
    stream.read_to_string(&mut trace).unwrap();
    trace
}

fn set_global_target(repo: &TestRepo, target: &str) {
    let home = repo.test_home_path();
    fs::write(
        home.join("trace2.gitconfig"),
        format!("[trace2]\n\teventTarget = {target}\n"),
    )
    .unwrap();
    repo.git_og(&["config", "--global", "include.path", "trace2.gitconfig"])
        .unwrap();
}

fn write_moss_settings(repo: &TestRepo) -> PathBuf {
    let home = repo.test_home_path();
    let moss_dir = home.join(".moss/trace2");
    fs::create_dir_all(&moss_dir).unwrap();
    let settings = home.join(".claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(
        &settings,
        json!({
            "env": {"GIT_TRACE2_EVENT": moss_dir, "EXISTING_SETTING": "preserved"},
            "hooks": {"SessionStart": [{
                "matcher": "startup|resume",
                "hooks": [{"type": "command", "command": "true # MOSS SessionStart"}]
            }]}
        })
        .to_string(),
    )
    .unwrap();
    settings
}

fn read_settings(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn configure_relay(repo: &TestRepo, settings: &Path, operation: &str) {
    let config_dir = settings.parent().unwrap().to_str().unwrap();
    repo.git_ai_with_env(
        &["claude-trace2", operation],
        &[("CLAUDE_CONFIG_DIR", config_dir)],
    )
    .unwrap();
}

struct ClaudeBashSession {
    env_file: PathBuf,
    target: String,
}

impl ClaudeBashSession {
    fn start(repo: &TestRepo, settings_path: &Path) -> Self {
        let settings = read_settings(settings_path);
        let target = settings["env"]["GIT_TRACE2_EVENT"]
            .as_str()
            .unwrap()
            .to_owned();
        let env_file = settings_path.with_file_name("session's env.sh");
        fs::write(&env_file, "export EXISTING_SESSION_VAR=preserved\n").unwrap();
        for block in settings["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
        {
            for hook in block["hooks"].as_array().unwrap() {
                let output = Command::new("sh")
                    .args(["-c", hook["command"].as_str().unwrap()])
                    .current_dir(repo.path())
                    .env("CLAUDE_ENV_FILE", &env_file)
                    .env("GIT_TRACE2_EVENT", &target)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{output:?}");
            }
        }
        Self { env_file, target }
    }

    fn git(&self, repo: &TestRepo, args: &[&str]) {
        // Claude sources the hook's exports before running Bash tool commands.
        let bash = Command::new("bash")
            .args([
                "-ec",
                r#". "$CLAUDE_ENV_FILE"; test "$EXISTING_SESSION_VAR" = preserved; printf '%s' "$GIT_TRACE2_EVENT""#,
            ])
            .env("CLAUDE_ENV_FILE", &self.env_file)
            .env("GIT_TRACE2_EVENT", &self.target)
            .output()
            .unwrap();
        assert!(bash.status.success(), "{bash:?}");
        let bash_target = String::from_utf8(bash.stdout).unwrap();
        repo.git_og_with_env(args, &[("GIT_TRACE2_EVENT", &bash_target)])
            .unwrap();
    }
}

fn trace_for_command(dir: &Path, argv_marker: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(trace) = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_file())
            .map(|path| fs::read_to_string(path).unwrap())
            .find(|trace| trace.contains(argv_marker))
        {
            return trace;
        }
        assert!(
            Instant::now() < deadline,
            "MOSS did not receive {argv_marker}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
