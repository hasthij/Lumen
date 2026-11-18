# HANDOFF — island features (uncommitted, v0.3.1 base)
- Done: clipboard pin/delete/filter, thumbnails, vol/bri HUD, System tab, Launch grid + searchable app picker (type to filter).
- Sound tab: master + one column per playing app; Spotify/Discord pinned by default (items.mix_pins); "+" opens searchable session picker to pin more; right-click a column unpins. Box widens to fit every column.
- Brightness: one iOS pill per monitor (mixer::brightness_all, DDC/CI + WMI fallback for laptop panel); HUD wheel uses same path. Fixed: WmiSetBrightness Timeout must be VT_I4 (0i32), VT_UI4 silently failed.
- Shelf tab replaced by Sticky notes (items.notes in island_items.toml): "+" new note, click to edit (typing via WM_CHAR, Enter newline, Esc done), right-click delete.
- Perf: mixer sampling on worker thread, repaint only on change, hidden edge poll 100 Hz -> 40 Hz.
- Removed unused IslandConfig launch/shelf Vecs. Build release OK, 12 tests pass, relaunched. Not yet eyeballed by user.
- Limits: notes have no caret/selection (append/backspace only); box width capped 1600px.
- Deferred: Google Calendar (OAuth). Notifications = separate top-right module.
- Next: user tests, then commit + release 0.3.2 when asked.
