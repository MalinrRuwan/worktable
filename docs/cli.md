# Worktable CLI

`worktable-cli` drives the real Worktable app through a local Unix socket. It
does not open the database, keep a second copy of app state, or render a
parallel UI: every command runs inside the running app against the same
`WorktableView` a person uses.

The socket exists only when the app is started with
`WORKTABLE_CONTROL_SOCKET` set. It is a Unix-domain socket with `0600`
permissions and no network listener.

## Build and run

```sh
cargo build -p worktable-app -p worktable-cli

# Start the real app with an isolated temp data directory and a socket.
./target/debug/worktable-cli start

# The command prints the session it started:
# {"socket": "...", "data_dir": "...", "pid": 1234}
```

`start`:

- resolves the app binary from `WORKTABLE_APP_BIN`, then the sibling
  `worktable-app` of the CLI binary;
- sets `WORKTABLE_DB_PATH`, `WORKTABLE_MEDIA_DIR`, and
  `WORKTABLE_CONTROL_SOCKET` to the isolated session;
- writes app diagnostics to `<data_dir>/worktable.log` so `start` can return
  without holding the caller's stdout/stderr open; the app's opt-in
  *Diagnostic logs* setting writes to `<data_dir>/logs/worktable.log`
  instead; `start` never turns that setting on;
- waits for `Ping`;
- skips the first-run tour (pass `--keep-onboarding` to drive it yourself);
- optionally seeds notes from `--seed '<json array>'`, where each item is a
  string or `{"content": "...", "title": "..."}`;
- prints `{"socket", "data_dir", "pid"}`.

The session is also written next to the socket as
`<socket>.session.json`, which `stop` uses to find the process. `stop` sends
`Quit` and waits for the socket and process to finish.

```sh
./target/debug/worktable-cli --socket /tmp/wt/control.sock start \
  --data-dir /tmp/wt/data \
  --seed '["Buy milk", {"content":"Read the GPUI guides","title":"Reading"}]'

./target/debug/worktable-cli --socket /tmp/wt/control.sock state
./target/debug/worktable-cli --socket /tmp/wt/control.sock stop
```

`--socket` defaults to `WORKTABLE_CONTROL_SOCKET`, and every subcommand uses
that same connection. `start` falls back to `<data_dir>/control.sock` when
neither is provided, so a default session is self-contained.

### Point at a manually launched app

```sh
WORKTABLE_CONTROL_SOCKET=/tmp/worktable.sock cargo run -p worktable-app
./target/debug/worktable-cli --socket /tmp/worktable.sock state
```

Without `WORKTABLE_CONTROL_SOCKET` the app opens no socket at all.

## Output and errors

Every command prints JSON. Successful commands print the full protocol
response (`{"ok":true,"message":...,"state":{...}}`) unless noted below:

| Command | Prints |
| --- | --- |
| `state` | the full state snapshot |
| `entry <id>` | the full entry object (uncapped content) |
| `copy` | `{"text": "..."}` |
| `screenshot` | `{"path", "bounds", "region"}` |
| `start` | `{"socket", "data_dir", "pid"}` |
| `stop` | `{"stopped", "pid"}` |
| `batch` | an array of responses |

Failures print a JSON error to stderr and exit `1`. A protocol failure keeps
the app's `Response` shape:

```json
{"ok":false,"error":"No entry with id missing.","state":null}
```

Mutations are queued onto the app's normal service path. `add-note` waits for
the same view to report the new id, so its response already contains the note
and `state.result.id`; other queued writes (edit, image import) can land on the
next `state` command. `batch` also waits after each `add-note` so later steps
can address the new entry.

## Commands

| Command | Effect |
| --- | --- |
| `start [--data-dir PATH] [--seed JSON] [--keep-onboarding] [--app-bin PATH]` | Launch an isolated real app session |
| `stop` | Quit and wait for the socket/process |
| `state` | Print the full state snapshot |
| `page entries\|assistant\|settings` | Switch the top-level page |
| `settings [general\|appearance\|data\|providers]` | Open Settings, optionally on a category |
| `back` | Close the detail, chats, a tour step, a settings page, or the agent page |
| `add-note <content> [--title TITLE]` | Add a note |
| `add-image <path>` | Import a local image through the media library |
| `search <query>` | Filter entries (switches to Entries) |
| `clear-search` | Clear the filter |
| `select <id> [--extend]` | Select one entry, optionally extending the selection |
| `open [id]` | Open a full entry (default: the selection) |
| `entry <id>` | Print one entry's full detail |
| `close` | Close the entry view, otherwise the chats sheet |
| `edit <content>` | Replace the open entry's markdown |
| `delete [id]` | Delete one entry (default: the selection) |
| `label <red\|amber\|green\|blue\|purple\|none> [--id ID]` | Set or clear a label |
| `copy [id]` | Copy an entry (default: the selection) and print the text |
| `sort <time\|alpha\|topic> [--ascending\|--descending]` | Set the sort mode and optionally its direction |
| `theme <light\|dark\|system>` | Set the theme |
| `toggle-theme` | Flip light/dark |
| `background <on\|off>` | Keep running in the menu bar on window close |
| `onboarding <start\|next\|back\|skip>` | Drive the first-run tour |
| `assistant ask <text>` | Send an assistant prompt |
| `assistant cancel` | Cancel the running answer |
| `knowledge build\|cancel` | Build or stop the knowledge graph |
| `chats open\|new\|close` | Drive the chats sheet |
| `window show\|hide\|focus` | Control the shell window |
| `quit` | Send `Quit` without waiting |
| `screenshot [--path PATH]` | Focus, read window bounds, and capture a PNG |
| `batch <file>` | Send a JSON array of protocol commands in order |
| `cmd '<json>'` | Send one raw protocol command |

`sort` without a direction preserves the mode's default when switching and
toggles the direction when the mode is already active. `label` without `--id`
uses the selection. `delete <id>` deletes only that entry even when several
entries are selected. `copy` with a multi-selection joins them with
`\n\n---\n\n`, matching Copy as list.

### Raw protocol

`cmd` and `batch` speak the same protocol as the app:

```sh
./target/debug/worktable-cli --socket "$SOCKET" \
  cmd '{"action":"add-note","content":"From JSON","title":"Raw"}'

cat > /tmp/commands.json <<'JSON'
[
  {"action":"page","page":"entries"},
  {"action":"search","query":"From JSON"},
  {"action":"label","color":"blue"},
  {"action":"open"},
  {"action":"close"},
  {"action":"clear-search"}
]
JSON
./target/debug/worktable-cli --socket "$SOCKET" batch /tmp/commands.json
```

Every command uses a kebab-case `action` tag. Nested action enums use
`intent`:

```json
{"action":"assistant","intent":"ask","text":"What did I save about GPUI?"}
{"action":"knowledge","intent":"build"}
{"action":"window","intent":"focus"}
```

## State snapshot

`state` is stable and documented. Long entry content (both `entries` and the
open `entry_modal`) is capped at 4000 characters plus an ellipsis;
`entry <id>` returns the full text.

```json
{
  "mode": "entries",
  "settings_tab": null,
  "onboarding": { "step": null, "trusted": true },
  "entries": [
    {
      "id": "8a689512-b4c8-4adb-8c1c-1aec5ab78d86",
      "title": "Reading",
      "content": "Read the GPUI guides",
      "source": "Worktable",
      "created_at": 1793097046974,
      "label": "amber",
      "selected": true
    }
  ],
  "visible_entry_ids": ["8a689512-b4c8-4adb-8c1c-1aec5ab78d86"],
  "query": "",
  "sort": { "mode": "time", "ascending": false },
  "theme": "system",
  "background_on_close": true,
  "entry_modal": {
    "id": "8a689512-b4c8-4adb-8c1c-1aec5ab78d86",
    "title": "Reading",
    "content": "Read the GPUI guides",
    "editing": false
  },
  "assistant": {
    "busy": false,
    "chat_id": null,
    "messages": [
      {
        "role": "user",
        "text": "What did I save?",
        "thinking": "",
        "streaming": false,
        "citations": []
      }
    ],
    "chats": [
      { "id": "chat-id", "title": "What did I save?", "updated_at": 1793097046974 }
    ]
  },
  "providers": {
    "active_provider": null,
    "active_model": null,
    "loading": false,
    "models_error": null,
    "providers": [
      {
        "id": "openai",
        "name": "OpenAI",
        "api_key_set": false,
        "oauth_set": false,
        "models_loading": false,
        "models_error": null,
        "groups": [],
        "models": []
      }
    ]
  },
  "window": {
    "bounds": { "x": 660.0, "y": 155.0, "width": 600.0, "height": 800.0 },
    "visible": true
  }
}
```

Field notes:

- `mode`: `entries`, `assistant`, or `settings`.
- `settings_tab`: `general`, `appearance`, `data`, `providers`, or `null` for
  the category list.
- `onboarding.step`: `welcome`, `accessibility`, `provider`, or `null` when the
  tour is closed.
- `visible_entry_ids` is the filtered, sorted list actually on screen; use it
  to assert search and sort results.
- `entry_modal` is `null` at rest and while the close morph has unmounted.
- `assistant.messages[].role` is `user` or `assistant`. Citations carry
  `n`, `label`, `snippet`, `host`, and `url`.
- `providers.providers` only contains provider identities; a configured
  provider's `models` comes from authenticated discovery, never a bundled
  list.
- `window.bounds` is in screen points, suitable for `screencapture -R`.
  `window.visible` is false while the display is asleep or the window is
  occluded, even if the window still exists.

## Screenshot

`screenshot` focuses the window, reads `window.bounds` from `state`, and runs:

```sh
screencapture -x -R<x,y,w,h> <path>
```

The default path is `<data_dir>/screenshot-<millis>.png`.

macOS may reject the capture with `could not create image from rect` when the
calling terminal or agent lacks Screen Recording permission, or when the
display is asleep/locked. The CLI reports the exact `screencapture` error and
exits `1`; it never writes a placeholder image. Grant Screen Recording to the
terminal/agent (System Settings → Privacy & Security → Screen Recording) and
unlock/wake the display, then retry. A full-screen `screencapture -x` can still
succeed on a locked screen while the rect/window capture path is denied, so
check `window.visible` in `state` before treating the capture as valid.

## Agent recipes

### Capability loop

```sh
SOCKET=/tmp/worktable.sock
CLI=./target/debug/worktable-cli

session=$($CLI --socket "$SOCKET" start --seed '["A note to search"]')
socket=$(printf '%s' "$session" | python3 -c 'import json,sys; print(json.load(sys.stdin)["socket"])')

# Drive the same controls a person uses.
$CLI --socket "$socket" page entries
$CLI --socket "$socket" add-note "Agent-written note" --title "Agent"
$CLI --socket "$socket" search "Agent"
$CLI --socket "$socket" select "$($CLI --socket "$socket" state | python3 -c 'import json,sys; print(json.load(sys.stdin)["entries"][0]["id"])')"
$CLI --socket "$socket" label green
$CLI --socket "$socket" open
$CLI --socket "$socket" close
$CLI --socket "$socket" page assistant
$CLI --socket "$socket" page settings
$CLI --socket "$socket" settings providers
$CLI --socket "$socket" window show
$CLI --socket "$socket" screenshot
$CLI --socket "$socket" stop
```

### Inspect one entry

```sh
id=$($CLI --socket "$SOCKET" state \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["entries"][0]["id"])')
$CLI --socket "$SOCKET" entry "$id"
```

### Automated regression pass

Use `batch` to make a sequence deterministic: every step is sent in order and
the reply array records exactly which step failed. Keep commands that depend on
an entry id separate so you can fill the id from `state` first.

### Rules for agents

- Do not edit the SQLite database or chat snapshots directly; send commands.
- `start` is the only command that creates a data directory, and it only seeds
  notes through `add-note`.
- Do not run two sessions against the same socket. `start` refuses to unlink a
  socket that still answers.
- A failed screenshot is an environment blocker, not a reason to fake a visual
  check.

The repository smoke test is:

```sh
sh scripts/cli-smoke.sh
```

It builds both binaries, starts a temp session, drives every major surface, and
asserts JSON with Python. On a host where the display is locked or Screen
Recording is not granted, run `ALLOW_SCREENSHOT_BLOCKER=1 sh scripts/cli-smoke.sh`
to complete the remaining steps and print the exact `screencapture` error.
