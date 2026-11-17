-- Whether a computer's address is public (Tailscale Funnel). The site shows
-- a public computer in a frame, so the browser's address stays the site's;
-- a tailnet-only one is opened in the whole tab, as before.
ALTER TABLE machines ADD COLUMN public INTEGER NOT NULL DEFAULT 0;
