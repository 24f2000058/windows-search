# Writing a Flint plugin (API 1)

A plugin is a separate program, written in any language, that answers
what the user types after its **prefix** and can add **result actions** to
the context menu. Flint starts it only after the user approves it, talks to
it over stdin/stdout, and stops it when the launcher hides. Design and
limits: ADR-008 and ADR-019 in [decisions.md](decisions.md).

The reference implementation is
[`crates/flint-plugin-epoch`](../crates/flint-plugin-epoch/src/main.rs): a
Unix-time converter with a "Copy SHA-256" action.

## Installing

1. Put the plugin in its own folder: `%LOCALAPPDATA%\Flint\plugins\<name>\`.
   In portable mode use `<exe>\data\plugins\<name>\`.
2. Set `plugins.enabled = true` in `config.toml` and restart Flint.
3. Type the plugin's prefix (or `/plugins`) and press Enter to review it.
   The prompt shows the program, a fingerprint of the folder and every
   permission requested.

Any later change to a file in the folder, or to the requested permissions,
needs approval again. To revoke a plugin, open `/plugins`, press Enter on
it, and choose Yes.

## Manifest: `plugin.toml`

```toml
api = 1                          # required; this document describes API 1
id = "epoch"                     # a-z, 0-9, '-', at most 32 characters
name = "Epoch converter"
version = "0.1.0"
description = "Converts Unix timestamps."   # optional, at most 300 chars
exe = "flint-plugin-epoch.exe"   # relative path inside the plugin folder
prefix = "epoch"                 # optional; the user types "epoch …"
permissions = ["selection_paths"]

[[actions]]                      # optional, at most 8
id = "sha256"
title = "Copy SHA-256"

[limits]                         # optional
timeout_ms = 1500                # per request, 50–10000
memory_mb = 64                   # 8–512 (enforced on Windows)
```

Manifests are validated strictly: unknown keys are rejected, `exe` cannot
leave the folder, and Flint's own prefixes are reserved (for example
`content`, `ext`, `size`, `diag`). A plugin needs a `prefix`, `actions`,
or both. Actions require `selection_paths`.

### Permissions

| Permission | What Flint does |
|---|---|
| `selection_paths` | Result actions receive the full paths of the selected results. Without it they receive none. |
| `open_urls` | Results may carry `open_url` actions; Flint opens them in the browser on Enter. Without it the action is removed. |
| `launch_processes` | The plugin may start child processes. Without it the Job Object allows only one process (Windows). |
| `network` | Declared and shown to the user. **Not enforced:** Flint cannot block a plugin's network access. |

Permissions limit what Flint gives a plugin and does for it. They do not
limit what the plugin program can do on its own with the rights it runs
with. See "Isolation" below.

## Protocol

Every message is a **frame**: a 4-byte little-endian length, then that
many bytes of UTF-8 JSON (one object). A frame larger than 1 MiB is a
protocol error. Flint sends one request and waits for exactly one reply
before it sends the next. Write nothing else to stdout; stderr is
discarded.

Requests: `{"id": <n>, "method": "<name>", "params": {...}}`.

Replies: `{"id": <same n>, "result": {...}}`, or
`{"id": <same n>, "error": "<message for the user>"}`.

### `hello` (always first)

```json
{"id":1,"method":"hello","params":{"api":1,"permissions":["selection_paths"]}}
```

`permissions` lists what the user granted. Reply `{"id":1,"result":{"api":1}}`.
If the reply names any other `api`, the plugin is disabled for the session.

### `query`

Sent while the user types `<prefix> <text>`. `text` is what follows the
prefix, at most 512 characters. Queries are latest-wins, so a query that
was overtaken by newer typing is never sent.

```json
{"id":2,"method":"query","params":{"text":"1700000000"}}
```

```json
{"id":2,"result":{"items":[
  {"title":"2023-11-14 22:13:20 UTC","subtitle":"Enter copies",
   "action":{"type":"copy","text":"2023-11-14 22:13:20 UTC"}}
]}}
```

Item actions, performed by Flint when the user presses Enter:

| `type` | Fields | Effect |
|---|---|---|
| `copy` | `text` (at most 8 KiB) | Copies text to the clipboard. |
| `open_url` | `url` | Opens an http(s) URL. Needs `open_urls`; the URL is re-checked first. |
| `search` | `query` | Replaces the search text. |
| `none` (default) | | Nothing happens. |

Flint shows at most 20 items. Titles and subtitles are cut to 300
characters, and control characters and bidirectional-override characters
are removed.

### `action`

Sent when the user chooses one of the plugin's actions in the context
menu.

```json
{"id":3,"method":"action","params":{"action":"sha256","paths":["C:\\Users\\me\\a.txt"]}}
```

Paths are sent only with `selection_paths`, and only for entries that
still match the index. Reply `{"message": "...", "copy": "..."}`: the
message is shown in the status line, and `copy`, if present, goes to the
clipboard.

## Failures and limits

| Event | What happens |
|---|---|
| No reply within `timeout_ms` | The process is killed; the next request restarts it. |
| Crash or exit | The request fails; restart after a backoff (250 ms, doubling up to 30 s). |
| Malformed frame, invalid JSON, wrong `id`, oversized frame | Protocol error; the process is killed. |
| 3 crashes, timeouts or protocol errors in a session | The plugin is disabled until Flint restarts. `/plugins` and `/diag` show why. |
| `error` reply | Shown to the user. This is not a failure and does not count. |

The launcher, file search and every other plugin keep working throughout.

## Isolation (Windows)

Each plugin process:

* Runs with a **low-integrity** token, so it cannot write to the user's
  files, registry or most of the profile. If Windows refuses the
  low-integrity launch, the plugin runs at normal integrity, and `/diag`
  says so.
* Runs in a **Job Object** with:
  * a memory limit
  * a hard 25% CPU cap
  * one process only, unless `launch_processes` is granted
  * no clipboard, global atoms, desktop switching or system-setting
    changes
  * kill-on-close, so it dies with Flint
* Inherits only its pipe handles.

It can still **read** what the user can read, and it can use the network.
This is risk reduction, not a sandbox. Only install plugins you trust.
Because the token is low integrity, a plugin that needs to store state can
write only to locations such as `%USERPROFILE%\AppData\LocalLow`.
