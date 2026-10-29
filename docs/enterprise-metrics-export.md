# Local enterprise metrics export

`git-ai export-metrics` exports recorded code contributions and Token usage to a
versioned JSON snapshot. It reads the local metrics database without starting the
daemon, refreshing prices, contacting a service, or changing delivery state.

```bash
git-ai export-metrics --username employee-123 --output metrics.json
```

`--username` explicitly labels the owner of this local database. It does not
filter commits by Git author or discover an employee's identity. Use the stable
employee ID supplied by your deployment, and do not combine multiple people's
databases under one label. Git author, reporting employee, and committer are
different identities.

The default source is `~/.git-ai/internal/metrics-db`. Use `--database PATH` to
read a specific existing database. A missing or unreadable source is an error;
the command does not create a replacement database. Existing output files are
never overwritten. Omit `--output` to write JSON to stdout.

Prospective enterprise collection requires the `enterprise_metrics` feature flag.
It defaults to `false` in debug and release builds. Set
`GIT_AI_ENTERPRISE_METRICS=true` for the daemon, or set
`"feature_flags": {"enterprise_metrics": true}` in its configuration. Existing
snapshots remain readable when collection is disabled.

Collection currently covers traced ordinary `git commit` operations with one
created commit. Merge, rewrite, fixup, and multi-target capture are unavailable.
Local commit facts are separate from pending upload events. Enabling collection
does not add provenance to uploaded events or public authorship Notes.

## Classification

Pass `--config classification.json` to customize the rules:

```json
{
  "test_patterns": [
    "**/tests/**",
    "**/test/**",
    "**/__tests__/**",
    "*.test.*",
    "*_test.*",
    "*Test.java"
  ],
  "language_extensions": {
    "rs": "Rust",
    "java": "Java",
    "md": "Markdown"
  }
}
```

`test_patterns` replaces the default list; `[]` classifies all eligible files as
production. Patterns match either the repository-relative POSIX path or its
basename. Defaults recognize `test`, `tests`, `__tests__`, `spec`, and `specs`
directories, `test_*`, `*_test.*`, `*.test.*`, `*.spec.*`, and Java
`*Test.java`, `*Tests.java`, and `*Spec.java` filenames. These are configurable
defaults, not a claim to match an existing corporate classification policy.

`language_extensions` extends the built-in extension map. Keys use lowercase
extensions without a leading dot. Unknown extensions are reported as `Unknown`;
ambiguous `.h` files are not guessed to be C or C++. The effective rules are
included in the output so downstream reports can retain the classification used.

Root `.gitmodules`, every file named `page.lock`, and pure gitlink changes are
excluded. New local facts count additions and deletions against the first parent,
or the empty tree for an initial commit. Attribution recognition ignores do not
remove vendor, generated, Markdown, text, or other lockfiles from this denominator.
Binary files contribute zero text lines. Regular-file and gitlink type changes
count only the regular-file side's text lines.

The local file-count policy counts every eligible changed path once, including
binary, rename-only, mode-only, and deletion-only changes. Moss's file-count rule
for these cases remains unconfirmed. Legacy events cover only their recorded text
hunks. `diff_coverage` distinguishes `complete_first_parent` from
`recorded_text_hunks`. Neither establishes that every historical operation was
recorded.

## Output contract

The top-level `schema_version` is `git-ai.enterprise-metrics/1`. This is a local
snapshot format, **not** a drop-in implementation of the Moss protocol.

- `commits`: production/test additions, deletions, final AI contributions, file
  counts, language counts, Agent breakdown, recorded repository/branch/commit
  identities, and rewrite sources when available. New local facts have an immutable
  repository-storage and commit key. That internal key is not an enterprise
  repository identity. A fact replaces only its exactly bound legacy event.
  A valid legacy rewrite record supersedes its
  explicitly recorded sources within the same repository. A parent commit is
  never used as a rewrite source. Multiple sources remain in `source_commits`.
- Full AI generation counts remain `null`. Optional
  `prod_ai_surviving_source_line_num` and `test_ai_surviving_source_line_num` count
  committed added lines containing verified surviving AI bytes. They are partial
  lower bounds, independent of final acceptance. Two AI lines followed by a
  known-human token edit can yield two surviving-source lines and one accepted
  AI line. Deleted and unstaged lines do not enter committed counts.
  `source_evidence` is `partial_verified_surviving_bytes` when such capture is
  available, or `unavailable` for missing evidence. Missing historical capture
  never becomes a complete zero. Whole replacements, ambiguous moves, split or
  joined lines, and cross-commit ancestry do not establish full generation.
  Language and Agent breakdowns retain the same distinction. A line with several
  source Agents counts once globally and once for each contributing Agent.
- `known_human_line_num` and `unknown_line_num` distinguish known human
  contributions from missing attribution. They are not combined into a claim of
  human authorship.
- `language_stats` is a JSON string containing production and test additions,
  deletions, generation, acceptance, and optional surviving-source counts per
  language.
- `token_usage`: five-minute UTC buckets, marked `record_type: token_bucket`.
  Keep only the highest `emitted_seq` for each
  `(session_id, model, speed, timestamp)` key. These values are cumulative bucket
  revisions, not additive observations or completed user turns. Downward and
  zero corrections are retained. Conflicting payloads with the same revision
  fail the export instead of choosing an arbitrary result.
- Token totals equal ordinary input + output + cache reads + cache creation.
  `reasoning_tokens` is a subset of output and must not be added again. Missing
  reasoning counts remain `null`.
- `token_root_totals`: totals of the selected latest leaf buckets, grouped by
  observed root and model. A descendant with several models contributes to each
  model's observed descendant count. Root totals are an alternative aggregate;
  do not add them to the leaf records. `aggregation_complete` remains `false`
  because a local snapshot does not prove every descendant has finished.
  A later recorded parent ID can enrich older buckets with missing ancestry;
  different nonempty parent IDs are treated as a conflict. Root aggregates can
  therefore change as more history becomes available. Replace snapshots rather
  than adding the same root totals again on every export.
- `coverage`: explicit limitations and skipped-record diagnostics. The current
  export always sets `moss_compatible: false`. Independent local Token extraction
  currently covers Claude Code and Codex; OpenCode extraction, completed-turn
  output, independent generation provenance, and Moss's merge eligibility rule
  remain separate work.

Only explicitly listed metadata and counters are serialized. Prompt text,
transcript events, tool arguments/results, source content, commit messages,
authorship Notes, API credentials, and arbitrary custom attributes are not
copied into the snapshot.

All exported dates are explicitly UTC; timestamps have the source dataset's
second precision, represented as milliseconds where named `timestamp`.
`timestamp_source` identifies actual commit time or legacy recording time.
`recorded_at` retains capture time separately. New facts expose their immutable
`parent_commits` and leave an uncaptured branch unavailable.
`--since RFC3339` and `--until RFC3339` require whole seconds and select an inclusive/exclusive window by
commit time (recording time when unavailable) and Token **bucket start**.
Filtering happens after reconciling revisions. It does not prorate partially
overlapping buckets.

```bash
git-ai export-metrics --username employee-123 \
  --since 2026-10-01T00:00:00+08:00 \
  --until 2026-11-01T00:00:00+08:00 \
  --config classification.json --output october-metrics.json
```

The snapshot only covers locally retained events. It cannot establish that every
employee tool, historical commit, or descendant session was recorded. It also
does not alter the background daemon's existing upload configuration; exporting
locally and configuring collection/upload policy are separate operations.
