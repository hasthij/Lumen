# Perf audit (long-run slowdown), uncommitted
- New pacer.rs: island + macnotif vsync threads now park on a condvar when idle (were waking 66x/s each), 8 ms floor if DwmFlush returns instantly.
- winnotif: banner-hider thread 6 ms poll only 4 s after a toast, else 40 ms; listener waits 2 s when WinRT events work; avatar() reads last 16 MB of store, not whole db; LumenAvatars pruned to 200.
- island/media.rs: Manager rebuilt at most every 3 s (was every 500 ms when idle); Spotify window scan every 5 s when not running.
- macnotif frame-time warn threshold 8 -> 40 ms (log spam).
- Tests added: pacer (3), winnotif (3), media (2). 21 pass, release builds.
- Also audited, no change needed: clock, taskbar (60 ms debounce only), app timers, launcher (icon cache capped 300, timers paired), settings, clip (capped 200/20), mixer (polls 1/s only on Sound tab), lumen-tap.
- Known minor, skipped: settings window leaks one HICON (icon::create) per open.
- 21 tests pass, release builds. Not run for hours on screen, so real-world effect unverified.
- Version bumped to 0.6.4 (uncommitted, not released).
