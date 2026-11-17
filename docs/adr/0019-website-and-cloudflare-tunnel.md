# ADR-0019: Open your computers from a website: Cloudflare Tunnel for the connection, Google sign-in for finding them

- **Status:** Accepted. Implemented 2026-11-15. The same day: Tailscale sharing (§1) and Funnel were removed, and public sharing (§4) is Cloudflare Tunnel's; a public computer is shown in place on the website (§5).
- **Date:** 2026-11-15

## Context

Web access ([ADR-0016](0016-web-client.md)) serves Agentloft to browsers, and network access (2026-11-14) lets a phone on the same Wi-Fi or tailnet open it with a pairing link or QR code. Two things were still missing:

1. **A real tailnet connection.** Network access is plain HTTP on every interface. Over a tailnet that is encrypted, but browsers treat it as insecure: no `Secure` cookies, and some features need a secure context. [ADR-0014](0014-remote-access-tailscale.md) planned `tailscale serve`, with HTTPS under the tailnet's own certificate.
2. **A place to find your computers.** The product owner asked for something like T3 Code's hosted app (`app.t3.codes`): a website where you sign in with Google, see your computers, and open one, with no pairing link to carry around.

ADR-0014 rejected "our own relay or cloud service" and "public HTTPS with OAuth". This ADR keeps both rejections for **Agentloft's traffic**. The website only signs you in and points the browser at the computer. It never carries a byte of a session.

## Decision

### 1. Tailscale (removed)

The first version shared the listener on the tailnet with `tailscale serve` (HTTPS under the tailnet's certificate, as [ADR-0014](0014-remote-access-tailscale.md) planned), with a guard against Funnel, and then made it public with `tailscale funnel`. Funnel's relay carried about 0.8 MB/s (§4), so public sharing moved to Cloudflare Tunnel. With that, a phone needs nothing installed, and the product owner asked to remove Tailscale altogether: nothing of theirs used the tailnet. The Tailscale code, its settings (`[web] tailscale`, `tailscale_cli`, `funnel`; ignored in an older config) and its tests are gone. Network access no longer counts a carrier-grade NAT address (100.64.0.0/10, the range tailnets use) as private: only this computer and RFC 1918 / IPv6 private ranges are.

### 2. The website: `agentloft.promptanddone.com`

A Cloudflare Worker with D1 storage (`cloud/`). Its static page is plain HTML and JavaScript under a strict CSP.

| What | How |
|---|---|
| Sign in | Google OAuth 2.0 authorization code with PKCE and `state` (scopes `openid email profile`). The ID token comes straight from Google's token endpoint; `aud`, `iss`, `exp` and `email_verified` are checked. Optional `ALLOWED_EMAILS`. |
| Session | `__Host-al_session`: 256 random bits, `HttpOnly; Secure; SameSite=Lax`; D1 keeps only its SHA-256; 30 days. Every POST must come from the site's own origin (`Origin`, `Sec-Fetch-Site`). |
| Linking a computer | Like `gh auth login`. The daemon asks for a link (`/api/link/start`) and gets a device code to poll with, plus a page to open: `/link#k=<256-bit key>`, the key in the fragment. Settings opens that page; the person signs in and approves. Both sides show the same `ABCD-EFGH` code. The daemon polls (`/api/link/poll`) and collects, once, its machine id and its own 256-bit key. The key is kept in `<home>/web/website.json` (0600). |
| Keys at rest | Each computer's key is sealed in D1 with AES-256-GCM under the `SECRETS_KEY` Worker secret, bound to the machine id. A copy of the database alone opens nothing. |
| Reporting | While linked, the daemon reports its name, version and address (`/api/machine/report`, `Authorization: Bearer <id>.<key>`) on every change and every five minutes. The address is the public one (§4), or the local network's when only network access is on; the report says whether it is public. A 401 means the computer was removed on the website, so the daemon forgets its key. |
| Opening a computer | `POST /api/machines/:id/open` returns `<address>/#ticket=<ticket>`. A public computer is shown in a frame (§5). Otherwise the page navigates there. |

**The ticket**

```
base64url(JSON payload) "." base64url(HMAC-SHA256(computer's key, first part))
payload: { v: 1, m: machine id, u: account id, e: email, aud: origin, iat, exp: iat + 120 s, n: nonce }
```

The daemon (`crates/loftd/src/web/website.rs`, `POST /api/ticket`) accepts a ticket only if all of these hold:

- it is signed with this computer's key (constant-time check);
- it names this computer and the account it was linked to;
- `aud` is the very origin the page was loaded from;
- it is current, allowing 60 s of clock skew and at most 10 minutes of life;
- its nonce hasn't been used.

It then opens an ordinary session, exactly as a pairing token does. The session is labelled with the Google account, and the ticket is removed from the address bar. Wrong tickets count against the same rate limiter as wrong pairing tokens.

### 3. What it takes to open a computer from a phone

Either of:

- **Public (§4), nothing on the phone or to set up:** Settings ▸ Web access ▸ **Link this computer**. Linking turns on **Public through Cloudflare** with a quick tunnel (§4a), and `cloudflared` is fetched if the computer has none. No Cloudflare account is needed; someone with a domain in Cloudflare can give a name under it instead.
- **Local network only:** Settings ▸ Web access ▸ **Network access**, then **Link this computer**; the website opens it in the whole tab, from that network.

Then on the phone: open `agentloft.promptanddone.com`, sign in with Google, tap the computer.

### 4. Public with Cloudflare Tunnel: nothing to install on the phone

The product owner then asked for a phone that needs nothing installed: open the address in any browser. The first answer was Tailscale Funnel (`tailscale funnel` instead of `serve`). In use it was too slow. Funnel's relay carried about 0.8 MB/s (6.5 Mbit/s, measured), and tailscaled on this computer was at 6% CPU, so the limit was the relay. The app's first load took 9–33 s, and the owner's videos (20–42 Mbit/s) buffered constantly. Funnel was removed. `[web] tunnel = "<name>"` (Settings ▸ Web access ▸ Public with Cloudflare Tunnel) puts the computer at a name of the owner's own instead (`crates/loftd/src/web/cloudflare.rs`).

- **Set up once,** with the Cloudflare login `[web] cloudflare_cert` names (a `cloudflared tunnel login` origin certificate; unset means cloudflared's default):
  - `cloudflared tunnel create` makes `agentloft-<the name, dots as dashes>`, with its credentials in `<home>/web/cloudflare-tunnel.json` (0600). A tunnel of that name already there is reused (`tunnel token --cred-file`).
  - `tunnel route dns` points the name at it. A DNS record for the name that points elsewhere is **never overwritten**: the status says the name is taken.
  - What was set up is kept in `<home>/web/cloudflare.json`.
- **Run:** the daemon runs `cloudflared tunnel --no-autoupdate run --credentials-file … --url http://127.0.0.1:<port> <id>` and reads its log. The status turns "on" at "Registered tunnel connection", and that moment wakes the website report. A `cloudflared` that exits is started again at the next sync: every minute, and on every config change. Turning the tunnel off, or the daemon stopping, stops it. The tunnel and DNS record stay for next time. A `cloudflared` a crashed daemon left behind (pid recorded, command line naming our credentials file) is stopped at startup.
- **The name:** one label under the zone (`agentloft-desk.example.com`). Cloudflare's free certificate covers `*.example.com`, not deeper names. Free plan: no bandwidth charge, uploads of at most 100 MB a request. Cloudflare's terms let it limit accounts that mainly serve video through the free CDN.
- **TLS ends at Cloudflare's edge.** Cloudflare sees the traffic, as it already serves the website. That is the price of its speed. Funnel passed TLS through to this computer.

### 4a. Quick tunnels: no Cloudflare account

Product owner, on what other users need: "each user don't need a cloudflare account do they?", then "do A [quick tunnels] but for security user need to access via the agentloft.promptanddone.com". A named tunnel needs a Cloudflare account and a domain in it, which most users don't have; putting everyone's tunnel under the owner's domain would make the owner's account carry and answer for everyone's traffic. So `[web] tunnel = "quick"` (Settings ▸ Web access ▸ **Public through Cloudflare**, with the own-domain field left empty) runs a Cloudflare quick tunnel:

- `cloudflared tunnel --no-autoupdate --config <home>/web/cloudflared-quick/config.yml --url http://127.0.0.1:<port>`, with `HOME` (and `USERPROFILE`) set to that folder and `TUNNEL_ORIGIN_CERT` / `TUNNEL_TOKEN` removed: no login or config of the user's is ever used, and nothing is made in any account. The config file exists (so a system-wide `/etc/cloudflared` one doesn't apply) and isn't empty (cloudflared logs an error for an empty one).
- Cloudflare gives it a random `<words>.trycloudflare.com` name, a new one each start. The daemon reads it from the log line that shows it (`quick_host`, strict: one label under `trycloudflare.com`). That name, and only the current run's, is the public address: the lock above applies to it unchanged (checked live: Cloudflare passes the `Host` through, and a request arrives from `cloudflared` on loopback). Any other `trycloudflare.com` name is refused like any unknown host.
- **It runs only while the computer is linked to the website.** On that address nothing but a website ticket signs a browser in, so unlinked it would only be an open door with no key: the status says to link first. Linking starts it at once; unlinking stops it.
- The address changes, so nobody types it: the daemon reports each new one to the website at once (the same report as a named tunnel's), and the website frames whatever was reported last. A bookmark of the bare address goes stale; the website's doesn't.
- **Nothing to do for the user** (owner: "so end user doesn't have to do anything"): **linking turns it on.** On the move from unlinked to linked, the daemon writes `[web] enabled = true` and `tunnel = "quick"`, unless a tunnel is already set; turning it off later sticks until the next link. A computer with no `cloudflared` gets **the pinned release** (`web/cloudflared_install.rs`, `VERSION`): downloaded from Cloudflare's GitHub releases over HTTPS, checked against SHA-256 sums compiled in (for the macOS `.tgz`, both the archive's and the binary's, the one Cloudflare's notes list), unpacked and renamed into `<home>/web/cloudflared-bin/<version>/`. One on PATH or set as `cloudflared_cli` is always used instead; one set in the config is never replaced. A failed download is retried after ten minutes. T3 Code, asked about by the owner, does the same in spirit: it installs a pinned `cloudflared` itself; but it runs tunnels it creates in its own Cloudflare account under its own domain (a connector token per computer), which §4a avoids.
- **Costs:** nothing on anyone's Cloudflare account. The owner's account runs only the website (sign-in, linking, a report every five minutes per computer, tickets), within the Workers and D1 free tier for a few hundred computers.
- **Limits:** Cloudflare calls quick tunnels a testing service: no uptime promise, at most 200 requests in flight, no Server-Sent Events (Agentloft uses a WebSocket). Enough for one person reaching their own computer; a domain of one's own (named tunnel) is the sturdier choice.

**The lock on the public address** (product owner's decision: "Google sign-in is the way in"):

| Path | On the public address |
|---|---|
| Page shell (`index.html`, `boot.js`, `app.css`, icons, manifest) | Served. It asks the browser to sign in on the website (`/api/session` answers `pairing: false` and the website's address). |
| `POST /api/pair` (pairing link or code) | Refused, `403 pairing_off_here`, even with the right token. |
| A session made by pairing anywhere else (cookie or bearer) | Doesn't count. |
| `POST /api/ticket` | The only way in. The ticket rules are §2's: signed with this computer's key, for the linked account, for this origin, two minutes, once. The session it opens is marked as a ticket's. |
| `/ws`, `/api/file`, the app's files | Only with a ticket's session. |

- "Public" goes by the address (the `Host` the tunnel's name), not by who asks. Pairing still works on `127.0.0.1`, and with network access on the local network's addresses. Sessions are recorded with how they were made (`ticket` in `sessions.json`).
- **Brute force:** wrong tickets count against the same limiter as wrong tokens: 5 a minute per client, then a minute's lockout; 50 a minute from everywhere and everyone waits a minute. Tickets and sessions are 256-bit, and HMAC keys never leave the computer and the website.
- **Who the client is:** every request through the tunnel arrives from `cloudflared` on loopback. For requests addressed to the tunnel's name (strictly that name: an IP literal or `localhost` never counts), the client is `CF-Connecting-IP`, which Cloudflare's edge sets and a browser can't. The client address is used for rate limits and the sessions list, never to grant anything.
- **Limits:** our caps are global: 128 connections, 32 WebSockets, a 10 s deadline for request heads, 16 MiB messages. Someone on the internet can use those up (denial of service) or trip the global brake on ticket sign-in for a minute at a time. None of it gets them in.
- **Trust moves to the website:** the website and the Google account are the whole lock, together with the Cloudflare account that runs both the website and the tunnel. Whoever controls the Worker's secrets and database, the Google account or the Cloudflare account can sign in from anywhere. Two-factor sign-in on those accounts matters.
- **Replays across a restart:** used ticket nonces are kept in memory. A ticket used minutes before a restart could be used again if someone had it and it hadn't expired. The fragment is removed from the address bar and history at once, and tickets live two minutes.
- **Terminals and the daemon's browser** follow `[web] allow_terminal` / `allow_browser`. Unset, they are off once anything but this computer can reach the listener. A user who set them on explicitly keeps them on the public address. A signed-in session could already run agents, which is as much.
- **Speed over a slow link:** besides the tunnel, the web client is served brotli-compressed (8.7 MB instead of 11.4 MB with gzip), with a progress count. Large WebSocket messages are deflated for browsers that offer `deflate` in their hello: a 4.2 MB thread arrives as 1.1 MB.

### 5. A public computer stays at the website's address

The product owner asked for the address bar to stay at `agentloft.promptanddone.com`, as on `app.t3.codes`. **Open** on a public computer goes to `/m/<id>`. That page fills the window with a frame showing `<address>/#ticket=…`. The page in the frame is the computer's own page shell and app, and it talks only to the computer. The website still carries no traffic, and the UI always matches the daemon's version.

- **Who may frame it:** on the public address, and only while linked, the daemon sends `frame-ancestors <the linked website's origin>` and no `X-Frame-Options`. That origin is read from `website.json` and accepted only as a bare origin. Every other address keeps `frame-ancestors 'none'` and `X-Frame-Options: DENY`. The website can already sign in with a ticket of its own, so being allowed to frame the page gives it no new power.
- **The cookie in a frame:** a frame from another site never gets `SameSite=Strict` cookies (a name under the website's own domain is the same site, and would). A ticket redeemed with `framed: true` on the public address therefore gets `__Host-…; Secure; HttpOnly; SameSite=None; Partitioned` instead (CHIPS). The browser keeps that cookie for frames under the website's site alone and sends it nowhere else. A browser that ignores `Partitioned` and still allows third-party cookies keeps it as an ordinary `SameSite=None` cookie. Even then, it opens nothing from another site: state-changing requests and the WebSocket must come from the page's own origin (`Origin`, `Sec-Fetch-Site`), no response has CORS headers, and `Cross-Origin-Resource-Policy: same-origin` stays. Signing out clears both kinds.
- **A browser that keeps no cookie even partitioned:** after a framed ticket sign-in, the page checks `/api/session`. If it is not signed in, the page asks the website (`postMessage`, to the website's origin only) to open the computer in the whole tab, with a new ticket, as before §5. The website accepts that message only from the frame's window and the computer's origin.
- **Reopening:** a frame that is already signed in doesn't spend its ticket, so reloading `/m/<id>` doesn't add sessions.
- **The website's page:** the Worker serves `/m/<id>` itself, with a CSP whose `frame-src` is exactly that computer's origin: only a public one of the signed-in account's. Every other page frames nothing. The frame is allowed `clipboard-read`, `clipboard-write` and `fullscreen`, which the app uses. `interactive-widget=resizes-content` makes the on-screen keyboard shrink the frame, not cover it, because a page in a frame doesn't see the keyboard. The frame sits inside the safe area, because a page in a frame isn't told the notch's insets.
- **Computers on a local network only** (network access, plain HTTP) still open in the whole tab: an HTTPS page can't frame them, and Chrome would ask for Local Network Access.

## Consequences

**Benefits**

- HTTPS at a name of the owner's own, with Cloudflare's certificate and nothing to manage. Phones get a secure context and `Secure`, `__Host-` cookies, and need nothing installed.
- No pairing links to copy for daily use. A new phone needs only the Google account.
- The website holds no Agentloft data and carries no traffic. Taking it down stops sign-in from it; it stops nothing else.

**Costs and risks**

- **The website can mint tickets for every linked computer.** Whoever controls the Worker, its `SECRETS_KEY` and its database can sign in to any of them: from anywhere when the computer is public (§4), else from its local network. Unlink, or remove the computer on the website, to withdraw that trust.
- **The website is a service to run.** It is small (one Worker, one D1 database, two secrets), but it is one.
- **Google OAuth app in "Testing"** (as set up on 2026-11-15): only the listed test users can sign in. Publishing it needs a privacy-policy page and a homepage.
- **Device-code phishing:** someone can send a link of their own computer and ask you to approve it. Their computer would then show in your list. Opening it gives them nothing of yours: the ticket is for their computer. The page names the computer and shows the code to compare.
- **Cloudflare is in the path** (§4): it sees the traffic, and its account is part of the lock.

## Alternatives considered

- **A relay through the website** (like T3 Connect): it works without Tailscale, but every byte crosses our service. ADR-0014's objection stands. Cloudflare Tunnel (§4) is the owner's own tunnel to their own name; the website still carries nothing.
- **Tailscale Funnel for the public address** (§4's first version, removed the same day): TLS ends on this computer, but the relay carried about 0.8 MB/s, too little for the app's first load and for video.
- **Serving the web client from the website**, talking to the computer cross-origin: two copies of the UI that must match each daemon's protocol, CORS on the daemon, and Chrome's Local Network Access prompts. Sending the browser to the computer, or framing the computer's own page (§5), serves its own matching UI and avoids all three.
- **Keeping the website's address by relaying through it** (`agentloft.promptanddone.com/m/<id>/…` proxied to the computer): every byte would cross Cloudflare in the clear (TLS would end there), and every browser would reach the computer from Cloudflare's addresses, defeating per-client rate limits. Framing keeps the address with neither cost.
- **Probing reachability from the website's page** (fetching the computer's address): blocked by `Cross-Origin-Resource-Policy`, and it would trigger Local Network Access prompts in Chrome. "Online" comes from the last report instead.
- **Typing the code instead of carrying a key in the opened link:** an 8-character code can be guessed during its 10 minutes. A guessed code would link the victim's computer to the guesser's account. The 256-bit key in the opened page can't be guessed; the short code is only for comparing.

## References

- [ADR-0014](0014-remote-access-tailscale.md), [ADR-0016](0016-web-client.md)
- `crates/loftd/src/web/cloudflare.rs`, `crates/loftd/src/web/website.rs`, `crates/loftd/src/web/http.rs` (`/api/ticket`), `crates/loft-ui/src/settings/web.rs`
- `cloud/` (the Worker), `cloud/README.md` (deploying it)
- Tests: `crates/loftd/tests/web.rs` (`the_tunnel_takes_only_tickets_from_the_website`, `a_tunnel_made_before_is_reused`, `*ticket*`, `linking_*`, `unlinking_*`, `large_messages_come_compressed_when_the_browser_offers`), `crates/loftd/src/web/cloudflare.rs`, `crates/loftd/src/web/http.rs` (`listed_names_match_only_themselves`, cookie tests), `crates/loftd/src/web/website.rs` (`only_the_linked_website_may_frame_this_computer`), `cloud/test/`
