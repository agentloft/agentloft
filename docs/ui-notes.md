# UI notes

How some things in `loft-ui` are meant to be done, and why.

- Text people may want to copy (settings rows and headings, the thread title in the top bar) is `crate::settings::selectable(text)` (gpui-base's `SelectableText`): text in a plain `div` takes no part in the window's selection. Don't put `.claims_press()` on it: that is exactly what turns selection off for a press. The top bar's double-click zooms the window only off text (`TextSelection::is_selectable_at`). A dev-hook drag that starts or ends in the gap beside text selects nothing; aim inside the letters.
- A scrolling box inside a scrolling page (Settings ▸ Voice's model lists, `model_list`) must stop the wheel itself, or GPUI scrolls both: a wrapper's `on_scroll` (the box's own listener has already run) stops propagation unless the box is at its end in that direction. `AGENTLOFT_DEV_CLICK='scroll x,y,after_ms,dy'` sends a wheel to the Settings window.
