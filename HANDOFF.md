Island: smoother motion.
- Open/close spring slower (x2.6) + bouncier (zeta .68, liquid .42); content scales 0.88->1 while fading in.
- Tab switch: body slides/fades in (tab_a).
- Clipboard scroll: eased scroll_f, rows drawn at sub-row offset, clipped.
- Paste on click default true; top bar fits width 450-640.
- Not done: perf/lag audit, extra customization settings.

- Copy nudge: on clipboard capture while hidden, island shows Peek pill ~1.3s then retracts (nudge_until in island/mod.rs). Vsync pacer (DwmFlush thread -> WM_FRAME) drives 144Hz animation. Not verified by eye.

- Island settings page now has: copy nudge toggle + length (default 700ms), history days, opacity, outline.
