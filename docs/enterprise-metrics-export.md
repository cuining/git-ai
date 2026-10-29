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

Prospective enterprise collection requires `feature_flags.enterprise_metrics`.
The flag defaults to `false` in debug and release builds. The supported config
command enables collection for the running daemon after a restart.

```bash
git-ai config --add feature_flags.enterprise_metrics true
git-ai bg restart
```

`GIT_AI_ENTERPRISE_METRICS=true` in the daemon's environment also enables the
flag. New OpenCode usage requires the existing `transcript_streaming` and
`token_usage_metrics` flags, which default to `true`. A tracked OpenCode session
is acquired through the existing OpenCode checkpoint hook. The usage worker
rescans tracked sessions at startup, after transcript processing, and during its
periodic sweep. `git-ai await` drains notification-driven work. A startup backfill
can continue afterward. Export itself does not trigger acquisition. Existing
snapshots remain readable when enterprise collection is disabled.

Collection covers traced ordinary commits, amend, mapped rebase, cherry-pick,
deferred cherry-pick, revert, squash provenance, and supported clean two-parent
merges. Operation capture hydrates immutable Git inputs in batches outside Trace2
ingestion. Proven autosquash mappings can retain multiple sources. Unproven
fixup mappings and unrecorded historical operations remain unavailable.

Local commit facts and source-only checkpoint observations are separate from
pending upload events. Collection works when `rewrite_metrics_events` is false.
Synthetic merge and additional revert observations remain local even when legacy
rewrite uploads are enabled. Collection adds no provenance to uploaded schemas
or public authorship Notes.

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
count only the regular-file side's text lines. Rename deletions use the previous
path's production/test and language rules. Additions, source, and acceptance use
the destination path. The changed-file count remains one.

The local file-count policy counts every eligible changed path once, including
binary, rename-only, mode-only, and deletion-only changes. Moss's file-count rule
for these cases remains unconfirmed. Legacy events cover only their recorded text
hunks. `diff_coverage` distinguishes `complete_first_parent` from
`recorded_text_hunks`. Neither establishes that every historical operation was
recorded.

## Output contract

The top-level `schema_version` is `git-ai.enterprise-metrics/1`. This is a local
snapshot format. `moss_compatible` remains `false`.

- `commits`: production/test additions, deletions, final AI contributions, file
  counts, language counts, Agent breakdown, recorded repository/branch/commit
  identities, and rewrite sources when available. New local facts have an immutable
  repository-storage and commit key. That internal key is not an enterprise
  repository identity. A fact replaces only its exactly bound legacy event.
  Only validated local amend and rebase replacement evidence suppresses original
  observations in the same repository storage. Partial targets, conflicting
  successors, and cycles retain originals. Replacements resolve before the time
  window. Legacy source links, cherry-pick, revert, squash, update-ref, and unknown
  non-fast-forward links retain their sources as provenance. Multiple sources
  remain in `source_commits`; `source_commit` is null unless there is exactly one.
- Full AI generation counts remain `null`. Optional
  `prod_ai_surviving_source_line_num` and `test_ai_surviving_source_line_num` count
  committed added lines containing verified surviving AI bytes. They are partial
  lower bounds, independent of final acceptance. Two AI lines followed by a
  known-human token edit can yield two surviving-source lines and one accepted
  AI line. Deleted and unstaged lines do not enter committed counts.
  `source_evidence` is `partial_verified_surviving_bytes` when such capture is
  available, or `unavailable` for missing evidence. Missing historical capture
  never becomes a complete zero. Whole replacements, ambiguous moves, split or
  joined lines, and incomplete cross-commit ancestry do not establish full
  generation. Retained companions carry source across verified unchanged spans,
  supported renames, and INITIAL. Chronological checkpoint replay preserves
  recorded erasures before retyping. A later proven descendant snapshot replaces
  earlier source evidence, including an empty snapshot. Missing or unrelated
  source histories remain unavailable.
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
  fail the export. Nullable parent enrichment is independent of counter revision
  equality.
- Token totals equal ordinary input + output + cache reads + cache creation.
  `reasoning_tokens` is a subset of output and must not be added again. Missing
  reasoning counts remain `null`.
- `token_root_totals`: totals of the selected latest leaf buckets, grouped by
  observed root and model. A descendant with several models contributes to each
  model's observed descendant count. Root totals are an alternative aggregate;
  do not add them to the leaf records. `aggregation_complete` remains `false`
  because a local snapshot does not prove every descendant has finished.
  With enterprise collection enabled, nonempty parent observations persist
  independently of mutable counters. Disabled collection preserves the parent
  attributes already present on ordinary events.
  Export also collects valid parent metadata from every retained legacy revision,
  including records with invalid counters. Graph validation precedes counter
  selection and date filtering. Different nonempty parents and cycles fail the
  export even outside the requested window. An absent parent is unknown and
  never proves a root, so `root_relation_complete` remains `false`.
  Internal session IDs stay distinct from native `external_session_id` values.
  Replace previous root snapshots as ancestry or counters change.
- `coverage`: explicit limitations and skipped-record diagnostics. The current
  export always sets `moss_compatible: false`. Independent local Token extraction
  covers existing Claude Code and Codex JSONL, plus the native OpenCode SQLite
  message format described below. Completed-turn output and full generation
  ancestry remain unsupported.

The final AI committed share is accepted AI added lines divided by eligible
added lines for the selected classification. A zero denominator is unavailable.
The share measures recorded final acceptance. Surviving-source lower bounds,
full generation, and employee productivity are separate quantities.

## Native OpenCode Token usage

The supported source is `message.data` assistant usage with real `session`
metadata, as verified against OpenCode v1.18.18 commit
`31406ccc51b4bd2a4e1e086b2bcaa5f7f804f26d`. Stored input already excludes
caches. Normalized output adds stored visible output and reasoning. Total adds
input, normalized output, cache read, and cache write once. For stored input
100, output 40, reasoning 10, cache read 30, and cache write 20, export reports
output 50 and total 200. Reasoning remains an output subset. Step-finish parts
and session cumulative totals are alternate representations and are not added.

Each acquisition reads one consistent read-only SQLite transaction. It selects
usage, model, and session metadata without reading text or tool parts. Complete
session snapshots replace that source's previous observations atomically with
bucket revisions and availability. Main-file timestamps and size do not skip
SQLite reads. Shared databases, WAL-only changes, tied update times, decreases,
message deletion, and model or timestamp moves can revise old and new buckets.
Initially empty usage creates no bucket. A later zero revision corrects usage
that was previously present. Unchanged rescans and daemon restarts preserve the
revision. `emitted_seq` is an ingestion revision and has no turn identity.

Native message identity includes tool, native session, and native message ID.
A message created before its new session proves copied fork history and is
excluded. Equal or missing creation boundaries are ambiguous and unavailable.
Only `session.parent_id` supplies ancestry. Assistant `parentID` links a user
message and supplies no session parent evidence.

A failed or invalid snapshot retains the previous parsing state and persists a
closed `native_usage_unavailable_*` diagnostic. Its current local buckets are
withheld, including previously valid usage. A validated session parent from the
same source transaction remains available even when the first usage read fails. The exporter does not turn an
unreadable observation into zero or resurrect an older valid counter. A valid
complete rescan recovers the session. Conflicting copies of one native message
and overflowing aggregate counters also withhold the session.

New OpenCode usage stays in private local tables in `metrics-db`. Neither the
ordinary pending uploader nor `git-ai reingest --all` reads those tables.
Pre-existing SessionEvents and Claude or Codex uploads retain their existing
policy. Export does not change daemon upload configuration, pricing, or network
behavior. The local snapshot adds no enterprise identity service, authenticated
employee binding, or enterprise delivery API.

Historical JSON without usage, the newer `session_message` schema, cumulative-only
`session.tokens_*` history, auxiliary or compaction completeness, Moss v1 or v2
naming, completed user turns, and unknown native roots remain unsupported.
There is no synthetic completion state or cumulative-history adapter.

## Retained history and merge eligibility

The snapshot reports retained local observations after verified replacements.
It does not query current branches to reconstruct an employee's complete history.
Rebase replacement requires retained source companions and uniquely preserved
added lines. Dropped or changed mappings that cannot prove replacement retain
original observations. Many-source identities remain intact.

An exported merge has exactly two parents. Its reconstructed automatic merge
must match the immutable target tree. Imported evidence maps unchanged second-parent
spans into first-parent additions. `merge_eligibility.related_additions` includes
all eligible related additions, including human lines, unknown lines, and files
excluded from attribution recognition. `verified_ai` counts their independent
surviving source lines once. Eligibility requires positive verified AI and
`2 * verified_ai > related_additions`. A 49/100 or 50/100 merge is withheld;
51/100 is eligible. Final acceptance still follows the captured final Notes and
can differ from source. Commit totals retain the complete first-parent diff.

Byte-identical imported snapshots support repeated literal lines because their
coordinates are unchanged. Changed mappings with repeated nonempty lines remain
unsupported. Conflict or manual resolution, octopus, ambiguous import mapping,
and missing source produce closed diagnostic reasons and withheld merge rows.
These observations remain unavailable. Unsupported mappings do not establish
complete source ancestry. Final acceptance follows existing authorship Notes,
including their first-file and later-attestation precedence. A source-only
unstaged line that moves into a staged range can lack final AI ownership in
Notes. Independent surviving-source evidence can still exist. The export does
not reconstruct missing final ownership or change the public Notes format.

Only explicitly listed metadata and counters are serialized. Prompt text,
transcript events, tool arguments/results, source content, commit messages,
authorship Notes, API credentials, and arbitrary custom attributes are not
copied into the snapshot.

All exported dates are explicitly UTC. Timestamps have the source dataset's
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
