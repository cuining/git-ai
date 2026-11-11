---
name: git-ai-search
description: "Search and restore AI conversation context from git history"
argument-hint: "[search query or workflow]"
allowed-tools: ["Bash(dcc-ai-tracking:*)", "Read", "Glob", "Grep"]
---

# Git AI Search Skill

Search and restore AI conversation context from git history using `dcc-ai-tracking search` and `dcc-ai-tracking continue`.

## Overview

Git AI tracks AI-generated code and the conversations that produced it. This skill helps you:
- **Search** - Find AI prompt sessions by commit, file, pattern, or author
- **Continue** - Restore AI session context to continue work where someone left off
- **Show** - View the full transcript of a specific prompt

### When to Use Each Command

| You want to... | Use... |
|---------------|--------|
| See what AI context exists for a commit | `dcc-ai-tracking search --commit <sha>` |
| See AI context for specific lines | `dcc-ai-tracking search --file <path> --lines <range>` |
| Read the full AI conversation | `dcc-ai-tracking search --commit <sha> --verbose` |
| Get machine-readable output | `dcc-ai-tracking search --json` |
| Find prompts mentioning a topic | `dcc-ai-tracking search --pattern "keyword"` |
| Continue someone's AI session | `dcc-ai-tracking continue --commit <sha>` |
| Continue working on a code region | `dcc-ai-tracking continue --file <path> --lines <range>` |
| Launch Claude with restored context | `dcc-ai-tracking continue --commit <sha> --launch` |
| View a specific prompt transcript | `dcc-ai-tracking show-prompt <id>` |

## Workflow Patterns

### 1. Investigate a Commit

Understand what AI assistance was used in a specific commit:

```bash
# See a summary of AI sessions in the commit
dcc-ai-tracking search --commit abc1234

# View the full conversations
dcc-ai-tracking search --commit abc1234 --verbose

# Get JSON for programmatic processing
dcc-ai-tracking search --commit abc1234 --json | jq '.prompts | keys[]'
```

### 2. Understand a Code Region

Find what AI sessions contributed to specific lines:

```bash
# Search by file
dcc-ai-tracking search --file src/main.rs

# Search specific lines
dcc-ai-tracking search --file src/main.rs --lines 100-150

# Multiple line ranges
dcc-ai-tracking search --file src/main.rs --lines 50-75 --lines 200-250
```

### 3. Continue Someone's Work

Pick up where a teammate left off:

```bash
# See what they were working on
dcc-ai-tracking search --commit abc1234 --verbose

# Restore context and continue in your shell
dcc-ai-tracking continue --commit abc1234

# Or launch directly into Claude Code
dcc-ai-tracking continue --commit abc1234 --launch

# Or copy context to clipboard to paste elsewhere
dcc-ai-tracking continue --commit abc1234 --clipboard
```

### 4. Review a Pull Request

Understand AI involvement in a PR:

```bash
# Get all commits in the PR
COMMITS=$(gh pr view 123 --json commits -q '.commits[].oid')

# Search each commit
for sha in $COMMITS; do
  echo "=== $sha ==="
  dcc-ai-tracking search --commit $sha
done

# Or search the full range
BASE=$(gh pr view 123 --json baseRefOid -q '.baseRefOid')
HEAD=$(gh pr view 123 --json headRefOid -q '.headRefOid')
dcc-ai-tracking search --commit $BASE..$HEAD
```

### 5. Audit AI Involvement in a File

See all AI sessions that touched a file:

```bash
# Full file history
dcc-ai-tracking search --file src/auth/login.rs

# Filter by author
dcc-ai-tracking search --file src/auth/login.rs --author "Alice"

# Filter by tool
dcc-ai-tracking search --file src/auth/login.rs --tool claude
```

### 6. Search by Topic

Find prompts discussing specific topics:

```bash
# Search prompt content
dcc-ai-tracking search --pattern "authentication"
dcc-ai-tracking search --pattern "error handling"

# Combine with filters
dcc-ai-tracking search --pattern "refactor" --tool cursor
```

### 7. Pipe to Other Tools

Integrate with scripts and workflows:

```bash
# Get prompt IDs only
dcc-ai-tracking search --commit abc1234 --porcelain

# Count matches
dcc-ai-tracking search --file src/main.rs --count

# JSON for processing
dcc-ai-tracking search --commit abc1234 --json | jq '.prompts[] | {tool, model, author: .human_author}'

# Find commits with AI-authored code
git log --oneline | while read sha msg; do
  count=$(dcc-ai-tracking search --commit $sha --count 2>/dev/null || echo "0")
  if [ "$count" != "0" ]; then
    echo "$sha ($count sessions): $msg"
  fi
done
```

## Command Reference

### dcc-ai-tracking search

Search for AI prompt sessions by various criteria.

```
dcc-ai-tracking search [OPTIONS]
```

**Context Source (pick one):**
- `--commit <rev>` - Search a specific commit (or range with `sha1..sha2`)
- `--file <path>` - Search by file path
- `--lines <range>` - Filter to line range (requires --file, repeatable)
- `--pattern <text>` - Full-text search in prompt content
- `--prompt-id <id>` - Look up specific prompt by ID

**Filters (combinable):**
- `--author <name>` - Filter by human author (substring match)
- `--tool <name>` - Filter by tool (claude, cursor, etc.)
- `--since <date>` - Only prompts after date
- `--until <date>` - Only prompts before date

**Output Format (pick one):**
- (default) - Human-readable summary
- `--json` - Full JSON output
- `--verbose` - Detailed output with full transcripts
- `--porcelain` - Machine-readable prompt IDs only
- `--count` - Just the count of matching prompts

### dcc-ai-tracking continue

Restore AI session context for continuation.

```
dcc-ai-tracking continue [OPTIONS]
```

**Context Source (pick one, or none for TUI):**
- `--commit <rev>` - Continue from a commit
- `--file <path>` - Continue from a file
- `--lines <range>` - Limit to line range (with --file)
- `--prompt-id <id>` - Continue specific prompt

**Agent Selection:**
- `--agent <name>` - Target agent (claude, cursor; default: claude)
- `--tool <name>` - Alias for --agent

**Output Mode (pick one):**
- (default) - Print formatted context to stdout
- `--launch` - Spawn agent CLI with context
- `--clipboard` - Copy context to clipboard
- `--json` - Output as structured JSON

**Options:**
- `--max-messages <n>` - Limit messages per prompt (default: 50)

### dcc-ai-tracking show-prompt

Display the full transcript of a specific prompt.

```
dcc-ai-tracking show-prompt <prompt_id> [OPTIONS]
```

- `--json` - Output as JSON
- `--raw` - Show raw internal format

## Output Formats

### Default Output

Human-readable summary:

```
Found 2 AI prompt session(s) for commit abc1234

[1] Prompt a1b2c3d4 (claude / claude-sonnet-4)
    Author: Alice
    Files: src/main.rs:10-50

[2] Prompt e5f6g7h8 (cursor / gpt-4)
    Author: Bob
    Files: src/lib.rs:100-150, src/utils.rs:20-30
```

### --json Output

Full structured data for programmatic use:

```json
{
  "source": {
    "sha": "abc1234",
    "author": "Alice",
    "date": "2025-01-15",
    "message": "Add authentication"
  },
  "prompts": [
    {
      "id": "a1b2c3d4",
      "tool": "claude",
      "model": "claude-sonnet-4",
      "author": "Alice",
      "messages": [...]
    }
  ]
}
```

### --verbose Output

Full conversations inline:

```
=== Session 1: Prompt a1b2c3d4 ===
Tool: claude (claude-sonnet-4)
Author: Alice

**User**:
Help me implement user authentication...

**Assistant**:
I'll help you implement authentication. Here's my approach...
```

### --porcelain Output

One prompt ID per line (for scripting):

```
a1b2c3d4
e5f6g7h8
```

### --count Output

Just the number:

```
2
```

## Tips

### Combining Filters

Filters use AND semantics - prompts must match ALL specified filters:

```bash
# Claude prompts by Alice in the last 7 days
dcc-ai-tracking search --commit HEAD~10..HEAD --tool claude --author "Alice" --since 7d
```

### Using with Git Commands

Combine with git log to explore history:

```bash
# Find commits with high AI involvement
git log --oneline --since="1 week ago" | while read sha msg; do
  dcc-ai-tracking search --commit $sha --count 2>/dev/null
done

# Search AI context for files changed in a commit
git diff-tree --no-commit-id --name-only -r abc1234 | while read file; do
  dcc-ai-tracking search --file "$file"
done
```

### Restoring Context for Different Agents

The `--agent` flag controls context formatting:

```bash
# For Claude Code
dcc-ai-tracking continue --commit abc1234 --agent claude --launch

# For other tools, copy to clipboard
dcc-ai-tracking continue --commit abc1234 --clipboard
```

### Inspecting Specific Prompts

If search returns many results, drill down:

```bash
# Get the IDs
dcc-ai-tracking search --commit abc1234 --porcelain

# View a specific one
dcc-ai-tracking show-prompt a1b2c3d4
```

### Performance Tips

- Use `--commit` for fastest results (direct note lookup)
- `--file` requires blame computation (slower for large files)
- `--pattern` searches the database (fast substring match)
- Add `--count` first to check result size before verbose output

## Examples

### Quick checks

```bash
# Any AI in this commit?
dcc-ai-tracking search --commit HEAD --count

# What tools were used?
dcc-ai-tracking search --commit abc1234 --json | jq '.prompts[].tool' | sort -u

# Who used AI in this file?
dcc-ai-tracking search --file src/main.rs --json | jq '.prompts[].author' | sort -u
```

### Detailed exploration

```bash
# Full context of the most recent AI session
dcc-ai-tracking search --commit HEAD --verbose | head -100

# Continue the exact session that wrote these lines
dcc-ai-tracking continue --file src/main.rs --lines 50-100 --launch
```

### Integration with CI/CD

```bash
# In a PR check, report AI involvement
PROMPT_COUNT=$(dcc-ai-tracking search --commit $PR_HEAD --count 2>/dev/null || echo "0")
if [ "$PROMPT_COUNT" -gt 0 ]; then
  echo "This PR includes $PROMPT_COUNT AI-assisted session(s)"
  dcc-ai-tracking search --commit $PR_HEAD
fi
```
