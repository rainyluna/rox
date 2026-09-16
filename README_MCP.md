# MCP

rox speaks MCP through `rox-mcp`, a small binary that ships beside the rox
executable: MCP over stdio on one side, the [control socket](README_IPC.md) on the
other. Every tool is a straight proxy of one socket method.

## Turning it on

Two switches, both off by default:

1. **Enable AI Features** at the top of Settings > Application. Reveals the MCP and
   ML Models pages.
2. **Enable MCP Server** on Settings > MCP.

The proxy checks both on every tool call, so a flip applies to the next call without
restarting rox or the client. A switched-off toggle, or a rox that isn't running,
comes back as a tool error naming the reason.

## Pointing a client at it

Settings > MCP has a copy-ready snippet in the `mcpServers` shape most clients
read:

```json
{
  "mcpServers": {
    "rox": {
      "command": "/path/to/rox-mcp"
    }
  }
}
```

Claude Code takes the same thing as `claude mcp add rox /path/to/rox-mcp`; in Zed
it's a custom context server with that command. rox has to be running: the proxy
connects to the socket on the first tool call and reconnects by itself when rox
restarts.

The Flatpak names a different command: `flatpak` with the args `run`,
`--command=rox-mcp`, `com.zealsprince.rox`. The binary lives in `/app/bin`, which
the host can't reach, and the runtime dir the socket sits in is only shared among
processes of that app id, so the proxy has to start inside the sandbox.

The AppImage names the `.AppImage` file itself with the single arg `--mcp`, which
its launcher turns into `rox-mcp`. The mount the executable runs from gets a new
random path every launch, so a path into it would be stale by the next start.

The snippet on Settings > MCP already comes out in the right shape for the install
it's running from, so copying it there is enough on every channel.

Two flags cover the non-default socket, and a third opens the drive tools:

| Flag                | Use                                                          |
| ------------------- | ------------------------------------------------------------ |
| `--data-dir <path>` | derive the socket for this data directory (a `--portable` rox) |
| `--socket <path>`   | name the socket outright                                     |
| `--dev`             | add the `ui_` drive tools                                    |

## Tools

| Tool             | Arguments                                                     | Answers                                                              |
| ---------------- | ------------------------------------------------------------- | -------------------------------------------------------------------- |
| `now_playing`    |                                                               | the playing track's tags, where its clock sits, whether audio moves   |
| `transport`      | `action`: `toggle` `play` `pause` `next` `prev` `stop`        | the resulting player state                                           |
| `ab_repeat`      | `action`: `mark` `clear` `set`; `set` takes `a` and `b` in seconds | the player state, with the repeating section in its `ab` field  |
| `search_library` | `query`, optional `limit` (1..500)                            | matching tracks with tags; pins like `artist:name` narrow one field   |
| `get_queue`      |                                                               | the play order with each entry's stable id and the one playing        |
| `rescan_library` |                                                               | starts a background rescan of the library folders                     |
| `get_tasks`      |                                                               | the analysis passes: switch state, tracks to do, progress while running |
| `start_task`     | `pass`: `acoustic` `replaygain` `tempo` `sortnames` `romanize` | starts the pass; answers with count, workers, estimate, and save mode |
| `stop_task`      | `pass`: `acoustic` `replaygain` `tempo` `sortnames` `romanize` | asks the pass to stop at the next file, keeping what's done           |

The socket does everything the tools do and more. Queue edits, seeking, volume, and
artwork stay socket-only, reachable through `roxctl` or any JSON-RPC client. `--dev`
lifts part of the debug scope into MCP as the `ui_` tools, which proxy `debug.windows`,
`debug.panels`, `debug.actions`, `debug.action`, and the synthetic input methods; the
rest of the debug scope stays on the socket.

## Protocol

MCP is JSON-RPC 2.0, one object per line on stdio, the same framing as the socket
itself. The proxy answers `initialize`, `ping`, `tools/list`, and `tools/call`, and
reads and drops notifications. It supports the `2024-11-05`, `2025-03-26`, and
`2025-06-18` revisions, echoing back whichever of those the client asks for and
offering `2025-06-18` when asked for one it doesn't know.
