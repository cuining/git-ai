# git-ai plugin for DeepSeek Harness

This standalone Cordis plugin captures local `write` and `edit` checkpoints as `deepseek-harness`. It uses the existing git-ai daemon and Trace2 configuration for attribution at commit time. The plugin runs on the Harness host and needs git-ai and the target worktree on that same filesystem.

## Install in Desktop

On macOS and Windows, open Desktop once to initialize its profile. Run `git-ai install-hooks` with a DCC tracking build that includes this plugin.

```text
git-ai install-hooks --dry-run
git-ai install-hooks
```

On Desktop versions that include the bundled `dsh` command, the plugin appears as `dsh-git-ai-attribution` under **Plugins → Installed plugins**. Refresh the panel if it is already open. If Harness requests a restart, reopen Desktop to load the plugin. The installer passes its embedded archive to Desktop's `dsh plugin --profile desktop add` command. It migrates older Git AI registrations, keeping their configuration and refreshing the executable path.

Older Desktop versions use a local module in `cordis.patch.yml`. Fully quit and reopen those versions after installation to load the module. The sidebar lists package-managed bundles. Use the runtime plugin inventory in Settings to inspect a local module.

The installer accepts the default empty patch and single-document YAML block sequences. Use `--dry-run` to preview the changes. `git-ai uninstall-hooks` removes Git AI's managed registration and any bundle it installed. Installations added from the Harness plugin panel are managed there.

Set `DSH_HOME` if Desktop uses a custom profile home. Use a preset that exposes the `write` and `edit` tools, such as Standard.

The bundle enables file capture. The foreground Bash overlay below targets the headless profile.

For a Linux CLI installation, create the bundle archive with `npm pack --ignore-scripts` in this directory and run `dsh plugin --profile headless add /absolute/path/to/dsh-git-ai-attribution-0.1.2.tgz`.

## Load in the headless profile

Use the DeepSeek Harness headless profile and a working git-ai installation. Keep [git-ai.cordis.yml](git-ai.cordis.yml) beside `git-ai.mjs`, then run these commands from a Git worktree.

```sh
dsh --profile headless --patch /absolute/path/to/git-ai.cordis.yml --dump-config
dsh --profile headless --patch /absolute/path/to/git-ai.cordis.yml 'Edit an existing file with the edit tool'
```

For an overlay saved elsewhere, use the absolute path to `git-ai.mjs` in `name`. Set `gitAiPath` to an absolute path if Harness uses a different `PATH` from your interactive shell.

| Setting | Default | Purpose |
| --- | --- | --- |
| `gitAiPath` | `git-ai` | An absolute executable path or a command found on the host `PATH`. An explicit value selects that executable. |
| `timeoutMs` | `3000` | Timeout for each checkpoint process in milliseconds. |
| `captureBash` | `false` | Enable checkpoint pairs for foreground `bash`. |

## Enable foreground Bash

Pass [foreground-bash.cordis.yml](foreground-bash.cordis.yml) as the overlay in place of `git-ai.cordis.yml`. Keep it beside `git-ai.mjs`. It enables capture and disables both explicit background jobs and timeout promotion on the shipped headless Bash tool. The Bash row's `config` is replaced as a unit, so carry over any other Bash settings when adapting the overlay. Use `workdir` to select the child worktree before execution.

```sh
dsh --profile headless --patch /absolute/path/to/foreground-bash.cordis.yml 'Run a foreground command in the target worktree'
```

Bash capture uses git-ai's worktree snapshots. In workspace-write mode, set `workdir` to the worktree root inside the session workspace. Read-only sessions skip checkpoint capture.

Scripts that change a different repository or commit before the command returns can require git-ai recovery. Recovery can infer attribution from adjacent lines. For example, a nearby pre-existing untracked line may be assigned to an AI session by the commit-time edge heuristic. Use separate edit and commit calls when verifying direct attribution. Persistent terminals and remote workspaces are outside this plugin's supported scope.

## Verify

In a disposable worktree, have Harness edit a file, then commit the result. Inspect `git-ai stats HEAD` to check the recorded attribution. With the Git notes backend, inspect the note using `git notes --ref=ai show HEAD`. Checkpoint failures are logged through Cordis and tool execution continues.

Run the plugin tests or the opt-in Git and daemon integration from the git-ai repository root. `DSH_ROOT` points to a DeepSeek Harness checkout with dependencies installed.

```sh
DSH_ROOT=/path/to/deepseek-harness node agent-support/deepseek-harness/test/run.mjs
DSH_ROOT=/path/to/deepseek-harness task test TEST_FILTER=deepseek_harness_install CARGO_TEST_ARGS='--test integration'
DSH_ROOT=/path/to/deepseek-harness task test TEST_FILTER=deepseek_harness_attributes_tool_edits_in_two_repositories CARGO_TEST_ARGS='--test integration' EXTRA_TEST_BINARY_ARGS='--ignored' TEST_THREADS=1
```
