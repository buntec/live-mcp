# Security

live-mcp gives whatever drives it full control of Live:
- Anything that holds the token in `~/.live-mcp/token` (mode 0600) can use the
  whole Live Object Model through the Remote Script: create, change and delete
  tracks, clips and devices, and call any public Live API method. Every MCP
  client you connect `live-mcp serve` to has that access. Live's undo history
  is the safety net, so save your set before handing it to an agent.
- `analyze_file` reads any audio file your user account can read.
- The Remote Script (127.0.0.1:19418) and the Ear listener (127.0.0.1:19419)
  accept connections from this machine only. On a machine shared with other
  users, a process that holds one of those ports while Live or the server isn't
  running would receive the token from a client that connects to it.

## Reporting a vulnerability

Use **Report a vulnerability** on the repository's Security tab rather than a
public issue.
