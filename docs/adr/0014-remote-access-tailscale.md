# ADR-0014: Remote access over the user's own Tailscale tailnet, or SSH

- **Status:** Accepted (design). The `ssh` mode is implemented (2026-11-14, see the end); the Tailscale modes are still M8.
- **Date:** 2026-11-09

## Context
Because the core is a daemon ([ADR-0001](0001-daemon-from-m0.md)), another Agentloft client can drive it remotely:
- a laptop, now;
- a phone, later.

The product owner wants a secure tunnel and suggested the user's own Tailscale. Our constraints:
- Never expose the daemon publicly.
- Run no relay service of our own.
- Keep the same protocol for local and remote clients.

## Decision
**Transport**
- The same length-delimited frames as the Unix socket, carried as WebSocket binary messages with `permessage-deflate`.
- Off by default.

**Modes**

| Mode | Behaviour |
|---|---|
| `tailscale-serve` (default) | The daemon listens on **127.0.0.1** only. Agentloft configures `tailscale serve --bg --https=<port> http://127.0.0.1:<port>`, so Tailscale provides TLS with the tailnet's certificate and tailnet-only reachability. |
| `tailnet-bind` | The daemon binds to the Tailscale interface address only, relying on WireGuard encryption. |
| `ssh-only` | No listener. The client forwards the Unix socket with `ssh -L`. |

**Funnel guard**
- Agentloft refuses to run, or shuts down the listener, if Tailscale Funnel would make the port public. This is checked when remote access is enabled and periodically afterwards.

**Authentication requires both factors**
1. **Tailnet identity.**
   - In `tailscale-serve` mode, from the `Tailscale-User-Login` header. It is trusted only on the loopback listener that Tailscale fronts.
   - In `tailnet-bind` mode, from LocalAPI `whois` on the peer address.
   - Either way, it is checked against `allowed_tailscale_logins`, which defaults to the host's own login.
2. **Paired device key.**
   - Each client device generates an ed25519 key and is paired once: a 6-digit code is confirmed on the host.
   - Every later connection signs a challenge in `Hello`.

**Authorization**
- Per-device scopes: `full`, or `read-only` (which can still answer approvals).
- A `remote_audit` log records remote commands.
- Pairing attempts are rate-limited, and devices can be revoked instantly.

**Client**
- Clients connect to "Environments": Local plus remote hosts.
- Window groups can mix environments ([ADR-0012](0012-multi-window-and-window-groups.md)).
- Behaviour that differs remotely:
  - host filesystem browsing goes through the daemon;
  - attachments are uploaded over the protocol;
  - local-only actions (Finder, open in editor) are disabled for remote threads.

## Consequences
**Benefits**
- Zero public exposure.
- No infrastructure for us to run.
- Strong default security: tailnet ACLs plus device keys.
- The protocol is identical to local use, so features work remotely for free.
- A mobile client later only needs the protocol and pairing.

**Costs**
- Users need Tailscale, or SSH, configured.
- `tailscale serve` behaviour varies by version and by the macOS variant (App Store vs standalone). It needs probing and fallbacks.
- The listener and pairing code need a security review before release.

**Neutral**
- Latency over the tailnet is dominated by model streaming. Delta coalescing every 30–50 ms keeps bandwidth low.

## Alternatives considered
- **Our own relay or cloud service.** Ongoing infrastructure and a security liability. Rejected.
- **Embedding Tailscale via `tsnet`.** It is Go, not Rust, and would duplicate the user's existing Tailscale.
- **Public HTTPS with OAuth.** Unnecessary exposure.
- **SSH only.** Kept as a mode, but Tailscale gives better ergonomics: stable names, phones, no key distribution.

## References
- PLAN §5.7
- Tailscale `serve` / `funnel` docs
- Tailscale LocalAPI `whois`
- OpenSSH Unix-socket forwarding (`ssh -L path:path`)

## Implementation notes

**`ssh` mode (2026-11-14).** A desktop window opens on another computer's Agentloft (File ▸ Connect to Another Computer…). Changes from the design above:
- The client runs `ssh -T -o BatchMode=yes <host> '<find agentloftd> bridge'` rather than forwarding the socket with `ssh -L`. `agentloftd bridge` starts the daemon there if it isn't running and joins its stdin and stdout to the daemon's socket. This needs no socket paths, no `AllowStreamLocalForwarding`, and nothing listening anywhere.
- The connection is the daemon's ordinary local one (`Peer::Local`): whoever can sign in over SSH can already run a shell there, so no pairing and no second factor.
- SSH runs without a terminal: password prompts and unknown host keys fail with a message saying to run `ssh <host>` in Terminal first. Signing in uses the user's keys, agent and `~/.ssh/config`.
- One window shows one computer. Its store learns that computer's homes from `UiFiles`. Images and project icons come over `ReadPath` / `ProjectIcon`. The side panel's browser is the daemon's streamed page. Finder, editors, the folder browser and in-place video are left out.
- Not yet: copying Agentloft to a computer that lacks it, password prompts, and Settings for the other computer (the Settings window is always this computer's).
