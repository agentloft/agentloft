# Agentloft's website

`agentloft.promptanddone.com`: sign in with Google, see the computers linked to your account, and open one from any browser. Design and security: [ADR-0019](../docs/adr/0019-website-and-cloudflare-tunnel.md).

It is a Cloudflare Worker (`src/`) with a D1 database (`migrations/`) and static pages (`public/`): the landing page at `/` (`index.html`, `landing.*`), and the signed-in app at `/app` (`app.html`, `app.*`), which the Worker also serves for `/link` and `/m/<id>`. Unknown paths get `404.html`. There is no build step and nothing to install; `wrangler` runs through `npx`.

## Test

```sh
npm test            # node --test: tickets, sealing, codes (same HMAC vector as the daemon's tests)
```

## Deploy

The account is picked with `CLOUDFLARE_ACCOUNT_ID` (kept out of the repository):

```sh
export CLOUDFLARE_ACCOUNT_ID=<account id>
npx wrangler@4 d1 migrations apply agentloft-cloud --remote   # after adding a migration
npx wrangler@4 deploy
```

Secrets (set once; `wrangler secret put <NAME>` reads the value from stdin):

| Name | What |
|---|---|
| `GOOGLE_CLIENT_SECRET` | The OAuth client's secret (Google Cloud project "Agentloft", Google Auth Platform ▸ Clients). |
| `SECRETS_KEY` | 32 random bytes, base64url: seals each computer's key in D1. Changing it unlinks every computer. |

Settings in `wrangler.toml`: `GOOGLE_CLIENT_ID` (public), and `ALLOWED_EMAILS` (comma-separated; empty lets any Google account sign in, each seeing only its own computers).

The OAuth client's redirect URIs are `https://agentloft.promptanddone.com/auth/google/callback` and, for `npx wrangler@4 dev`, `http://localhost:8787/auth/google/callback`. The consent screen is in **Testing**: only its listed test users can sign in. Publishing it needs a homepage and a privacy-policy page.

## Run it locally

```sh
npx wrangler@4 d1 migrations apply agentloft-cloud --local
printf 'GOOGLE_CLIENT_SECRET=…\nSECRETS_KEY=…\n' > .dev.vars   # git-ignored
npx wrangler@4 dev                                             # http://localhost:8787
```

Point a scratch daemon at it with `[web] website = "http://localhost:8787"`.
