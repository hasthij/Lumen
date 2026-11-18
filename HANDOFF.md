# Lumen handoff (2026-11-18)
- Pushed up to 513ede5; everything below is uncommitted. Release needs GitHub asset LumenSetup.exe (tag vX.Y.Z) for the update check.
- Done: drag-out of text/files (user-verified), file size on rows, Spotify detection via audio peak + iTunes timeline, vector media icons.
- Tuned UI applied: PAD 26, body font 13, tabs (gap 4, r16), row 42/gap 6/r16, icon 30/r12, media buttons rounded squares (BTN 40, r7), row hover lift 1.5px + press squash 0.95 eased over animation_ms. Skipped: buttons/tiles have no hover animation, tab widths fixed (96/76).
- New: Settings > Island > "Paste on click" (default off) sends Ctrl+V after a row click (SendInput; island never takes focus).
- Build/test pass (12 tests). NOT checked on screen: row/button look, paste-on-click, Spotify, file sizes. rust-reviewer skipped.
- Tomorrow: optimization check (idle CPU/RAM, repaint, timers, caches); feature check across modules.
