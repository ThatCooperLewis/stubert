# Commands

Stubert exposes nine slash commands through Telegram and Discord. Commands are detected by the adapter layer before normal message flow — they bypass the message queue and session consumer entirely.

## Command Registry

| Command | Description | Spawns CLI Call |
|---------|-------------|:---:|
| `/new` | Start a new chat session | Yes |
| `/context` | Show context usage for current session | Yes |
| `/restart` | Restart the gateway | No |
| `/models [alias]` | List or switch models | No |
| `/skill [name] [args]` | Invoke a skill | Yes |
| `/history <query>` | Search past conversations | No |
| `/status` | Show system status | No |
| `/heartbeat` | Trigger an immediate heartbeat | Yes |
| `/help` | Show all commands | No |

## Command Parsing

```rust
pub fn parse_command(text: &str) -> Option<(&str, &str)> {
    // Returns (command_name, args) or None
}
```

1. Text must start with `/`
2. Split on first whitespace: command token + args
3. Strip `@botname` suffix from command token (Telegram appends `@stubert_bot` in groups)
4. Lowercase the command name
5. Return `None` if the command isn't in the registry

Example: `/models@stubert_bot sonnet` → `("models", "sonnet")`

## Command Handlers

### `/new` — Start New Session

1. Reset the session (new UUID, `initiated = false`)
2. Send immediate confirmation: `"New session started · model: Sonnet 4.6"`
3. Call Claude with greeting prompt: `"A new session has began, please greet the user."`
4. Send Claude's greeting response
5. Write history entries for the greeting exchange
6. Start the inactivity timer

If the greeting CLI call fails, the user still gets the confirmation message plus `"Session started but greeting failed."`

### `/context` — Context Usage

Requires an active session (`initiated = true`). If no active session, returns `"No active session."`

1. Resume the current session
2. Send prompt: `"Report your current context usage: how many tokens used out of the total context window, as a percentage and raw numbers. Be brief."`
3. Return Claude's response
4. Write history, restart inactivity timer

### `/restart` — Restart Gateway

1. Write `restart_origin.json` with `{"platform": "telegram", "chat_id": "12345"}`
2. Return `"Restarting..."`
3. Schedule `SIGTERM` to self (via delayed task, so the response is sent first)

After restart, the gateway reads `restart_origin.json` and posts a greeting to the originating chat (see [gateway.md](gateway.md)).

### `/models [alias]` — List or Switch Models

**No argument (list):**

```
Available models:
* sonnet (active)
  opus
  haiku
```

The current model is marked with `*`.

**With argument (switch):**

1. Validate alias against known models: `sonnet`, `opus`, `haiku`
2. Resolve to full model ID (e.g., `"sonnet"` → `"claude-sonnet-4-6"`)
3. Update session model
4. Save sessions.json
5. Return `"Switched to sonnet."`

If unknown alias: `"Unknown model. Available: sonnet, opus, haiku"`

**Model display names** (used in `/new` confirmation and `/status`):

| Alias | Full Model ID | Display Name |
|-------|---------------|--------------|
| `sonnet` | `claude-sonnet-4-6` | Sonnet 4.6 |
| `opus` | `claude-opus-4-6` | Opus 4.6 |
| `haiku` | `claude-haiku-4-5-20251001` | Haiku 4.5 |

See [claude-cli.md](claude-cli.md) for the `resolve_model()` and `display_model()` functions.

### `/skill [name] [args]` — Invoke Skill

**No argument (list):**

```
Available skills:
  trello — Manage Trello boards and cards
  plex — Search and manage Plex media library
  obsidian-vault — Read and write Obsidian notes
```

**With name (invoke):**

1. Look up skill in the registry
2. Read skill prompt (body after frontmatter)
3. If user provided args, append them: `"{skill_prompt}\n\n{user_args}"`
4. Use skill's `allowed_tools` override if set, otherwise platform defaults
5. Use skill's `add_dirs` override if set, otherwise config defaults
6. Call Claude with the assembled prompt in the current session
7. Write history, restart inactivity timer

If unknown skill: `"Unknown skill. Use /skill to list available skills."`

### `/history <query>` — Search Conversations

1. Require a search query (no args → `"Usage: /history <search term>"`)
2. Case-insensitive substring search across history files for the current platform
3. Return up to 20 matches grouped by date, with one line of context before and after each match

### `/status` — System Status

Returns a text summary:

```
Uptime: 2d 5h 30m
Active sessions: 3
In-flight calls: 1
Model: Sonnet 4.6
```

Computed from gateway state — no CLI call needed.

### `/heartbeat` — Manual Heartbeat

1. Check if the heartbeat system is available
2. Attempt to trigger (checks the overlap mutex)
3. If a heartbeat is already running: `"A heartbeat is already in progress."`
4. Otherwise: execute the heartbeat and return the result

### `/help` — Command List

Returns a formatted list of all commands:

```
/new — Start a new chat session
/context — Show context usage for current session
/restart — Restart the gateway
/models — List or switch models
/skill — Invoke a skill
/history — Search past conversations
/status — Show system status
/heartbeat — Trigger an immediate heartbeat
/help — Show all commands
```

## Skills System

Skills are pre-authored prompt templates stored in `.claude/skills/`:

```
.claude/skills/
├── trello.md
├── plex.md
├── obsidian-vault.md
└── ...
```

Note: The Python implementation used a `{skill-name}/SKILL.md` directory structure. Either convention works — the registry scans for `.md` files.

### Skill File Format

```markdown
---
name: trello
description: Manage Trello boards and cards
allowed_tools: ["Bash", "Read", "Write"]
add_dirs: ["/home/cooper/projects"]
---
You are a Trello integration. Use the Trello API via curl to manage
boards, lists, and cards. The API key and token are available in .env.
```

**Frontmatter fields:**

| Field | Required | Purpose |
|-------|:--------:|---------|
| `name` | Yes | Lookup key for `/skill {name}` |
| `description` | Yes | Shown in `/skill` listing |
| `allowed_tools` | No | Override platform default tools for this skill |
| `add_dirs` | No | Override default directories for this skill |

**Body:** Everything after the closing `---` becomes the prompt sent to Claude.

### Skill Registry

```rust
pub struct SkillRegistry {
    skills: HashMap<String, SkillInfo>,
    skills_dir: PathBuf,
}

impl SkillRegistry {
    pub fn discover(&mut self);                          // Scan directory, parse frontmatter
    pub fn get(&self, name: &str) -> Option<&SkillInfo>; // Lookup by name
    pub fn list_skills(&self) -> Vec<&SkillInfo>;        // All skills
    pub fn read_prompt(&self, name: &str) -> String;     // Body text after frontmatter
}
```

Discovery runs once at gateway startup. Skills are not hot-reloaded — changes require a restart.

## Discord Slash Commands

On Discord, all commands are registered as native slash commands (see [messaging.md](messaging.md)). This gives Discord users autocomplete and parameter hints in the UI.

**Parameterless:** `/new`, `/context`, `/restart`, `/status`, `/help`, `/heartbeat`

**With parameters:**
- `/models model:sonnet` — optional model alias
- `/skill name:trello args:create a new card` — optional name and args
- `/history query:deployment` — optional search query

The adapter builds a text representation (e.g., `"/models sonnet"`) and passes it through the same `parse_command()` → `CommandHandler` flow as Telegram commands. The command handler is platform-agnostic.
