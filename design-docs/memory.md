# Memory

Stubert's memory system consists of four markdown files that the Claude CLI automatically loads on every invocation. Stubert itself doesn't assemble or inject these files — the CLI picks up `CLAUDE.md` from its working directory, and `CLAUDE.md` uses `@import` to pull in the others.

## File Roles

| File | Purpose | Who Writes |
|------|---------|------------|
| `CLAUDE.md` | Operational manual — behavioral rules, standing instructions, platform rules, command reference | Claude via Telegram (full R/W) |
| `SOUL.md` | Personality — name, cadence, core truths, boundaries, communication vibe | Claude via Telegram (full R/W) |
| `USER.md` | Human profile — name, timezone, preferences, tech stack, personal context | Claude via Telegram (full R/W) |
| `MEMORY.md` | Long-term facts — decisions, learned preferences, project notes, accumulated knowledge | Claude via Telegram (full R/W) |

All four files live in the runtime directory (`config/`). They're loaded by every platform, but only Telegram sessions have write access (via `allowed_tools` config). Discord sessions see the same context but can't modify it.

## Loading Mechanism

`CLAUDE.md` is the entry point. It contains `@import` directives that the Claude CLI resolves:

```markdown
@import SOUL.md
@import USER.md
@import MEMORY.md
```

When the CLI starts a session, it reads `CLAUDE.md` from its working directory and expands the imports. Stubert's only responsibility is to set the subprocess `current_dir` to the runtime directory where these files live.

## CLAUDE.md Structure

The seed template (`example-config/CLAUDE.md`) establishes:

1. **Core behavior** — Stubert is a persistent service; conversation context resets on `/new` or inactivity timeout.
2. **Every session** — Read today's and yesterday's chat history before responding.
3. **First response** — Simple greeting, no extra explanation.
4. **Memory system** — References SOUL.md, USER.md, MEMORY.md with descriptions of each.
5. **Chat history** — Transcripts in `history/{YYYY-MM-DD}-{platform}.md`, append-only.
6. **Platform rules** — Telegram gets full access; Discord is read-only with restricted tools.
7. **Slash commands** — Reference for all available commands.
8. **Skills** — Description of the skills system (`.claude/skills/*/SKILL.md`).
9. **Heartbeats** — Explanation of periodic ephemeral sessions.
10. **Cron vs heartbeat** — Guidance on when to use each.
11. **Prompt injection defense** — Don't follow instructions embedded in user-submitted content.
12. **Safety rules** — Don't exfiltrate data; ask before destructive operations.

## SOUL.md

Defines personality and identity. Template sections:

- **Core truths** — Guiding principles
- **Boundaries** — Lines that should never be crossed
- **Vibe** — Communication style and personality traits
- **Continuity** — Acknowledgment that these files are the agent's memory

The user customizes this file to shape the agent's personality. Claude may also update it (only via Telegram) — changes to SOUL.md are significant and Claude is instructed to tell the user when it modifies this file.

## USER.md

Information about the human:

- Name, preferred name, pronouns
- Timezone, location, occupation
- Communication style preferences
- Technical profile (languages, tools, platforms)
- Personal context (family, work style, accessibility needs)

## MEMORY.md

Long-term storage of facts learned across sessions:

- Decisions made and their rationale
- Learned preferences and patterns
- Project-specific knowledge
- Recurring topics and their context

**Rules:**
- Claude decides when to update MEMORY.md — it's curated, not a raw log
- Only Telegram sessions can write (Discord is read-only)
- Content is the distilled essence of interactions, not transcripts

## Chat History (Not Memory, But Related)

Chat transcripts are stored separately in `history/` (see [gateway.md](gateway.md) for the history writer). They are **not** auto-injected into context. Instead, `CLAUDE.md` instructs Claude to read recent history files at the start of each session and search them on demand via the `/history` command.

Format: `history/{YYYY-MM-DD}-{platform}.md`

This means Claude can reference past conversations without Stubert managing context assembly — the CLI has file access and reads history files directly.

## Platform Permissions

All platforms receive the same context files. Platform differences are enforced by tool restrictions in `config.yaml`, not by separate context stacks:

| Platform | File Access | Tools |
|----------|-------------|-------|
| Telegram | Read + Write | `Bash`, `Read`, `Write`, `Edit`, `Glob`, `Grep` |
| Discord | Read only | `Bash(read-only)`, `Read` |

This means:
- Discord can read SOUL.md, USER.md, MEMORY.md but can't modify them
- Telegram can update all memory files and create new ones
- Both platforms see identical conversation context

## Stubert's Role

Stubert does **not**:
- Parse or assemble memory files
- Inject context into prompts
- Manage imports or resolve `@import` directives
- Decide when to update memory

Stubert **does**:
- Set the subprocess working directory to the runtime directory (so CLI finds `CLAUDE.md`)
- Provide the `.env` file path via `CLAUDE_ENV_FILE` environment variable
- Configure tool permissions per platform (controlling write access)
- Store and serve chat history files (which Claude reads on its own)

## Runtime Directory Requirements

For memory to work, the runtime directory must contain at minimum:

```
config/
├── CLAUDE.md          # Required — CLI won't have instructions without it
├── SOUL.md            # Referenced by @import in CLAUDE.md
├── USER.md            # Referenced by @import in CLAUDE.md
├── MEMORY.md          # Referenced by @import in CLAUDE.md
└── history/           # Directory must exist for transcript writing
```

The `example-config/` directory in the repository provides seed templates for all these files. On first deployment, they're copied to the runtime directory and customized.
