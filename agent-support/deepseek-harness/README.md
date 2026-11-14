# DeepSeek Harness

Git AI captures file edits made through the native DeepSeek Harness `write`
and `edit` tools. It sends the before and after snapshots through the
`agent-v1` checkpoint protocol with the session ID and model. Each AI
checkpoint can also include the user's text, visible assistant replies, and
tool calls from the session up to that edit. Shell commands and other file
mutation tools are outside the line attribution scope.

## Desktop

Open DeepSeek Harness Desktop once to initialize its profile. Fully quit it, run
`git-ai install`, and reopen it. Git AI uses Desktop's bundled CLI on macOS and
Windows. The embedded plugin requires Harness `0.2.2-rc.2` or later.

| Installed plugin | Result |
| --- | --- |
| Absent | Install the bundled version and enable it |
| Older and enabled | Update and keep it enabled |
| Older and disabled | Update and keep it disabled |
| Same version or newer | Skip the package operation and keep the user's setting |

Git AI stages a local copy of its plugin and invokes
`dsh plugin --profile desktop add link:<path>` only when installation or an
update is needed. DeepSeek Harness manages the profile and lockfile.
Its package manager may also resolve missing dependencies already declared
in the profile. `git-ai install --dry-run --verbose` shows the proposed
version and selection without running the package manager.

A retry after interruption checks the profile and installed files again.
If a first install stopped after the dependency was recorded but before
Harness enabled it, turn it on in Desktop's Plugin Manager. This preserves
the same behavior as an existing disabled plugin.

## Headless

With `git-ai` on PATH, place this overlay beside `index.mjs` and load it with
`dsh --profile headless --patch /path/to/overlay.yml "Your task"`.

```yaml
- insert:
  - id: git-ai-deepseek-harness
    name: ./index.mjs
```

## Read a commit's transcript

Use `git-ai show <commit>` to find the edited line's
`s_<session>::t_<checkpoint>` key, then read its snapshot.

```sh
git-ai show-prompt "$attestation_id" --commit "$commit"
```

The JSON output includes `transcript.messages` when that checkpoint's
snapshot is available. The snapshot is saved locally in the repository's
Git metadata directory. Each edit has its own snapshot. A later edit can
include replies generated after an earlier edit. Git Notes carry the
attestation key and session metadata, while the messages stay on the
machine that recorded them. The plugin omits tool results, injected
context, reasoning, and attachment contents. Sessions exceeding the
512 KiB transcript limit still get line checkpoints.

## Verification

Run the event tests with `node --test agent-support/deepseek-harness/index.test.mjs`.
The Rust integration tests use temporary repositories and Harness homes.

```sh
task test CARGO_TEST_ARGS='--test integration' TEST_FILTER=deepseek_harness
task test CARGO_TEST_ARGS='--test integration' TEST_FILTER=deepseek_harness_install_official_cli EXTRA_TEST_BINARY_ARGS=--ignored
GIT_AI_TEST_DEEPSEEK_HARNESS_CLI=/path/to/dsh task test CARGO_TEST_ARGS='--test integration' TEST_FILTER=deepseek_harness EXTRA_TEST_BINARY_ARGS=--ignored
```
