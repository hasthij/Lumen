# HANDOFF — 0.5.0 themes (uncommitted, 0.4.0 released at 1646b99)
- Done: theme knobs + presets (smoke default, ink, frost, liquid, aura, custom); glass render (blur/sat/tint/sheen/grain/edge/radius); Beam L orb + icon; versions 0.5.0.
- Done: Settings > Island > Look: Theme/Tint/Font/Tabs pickers, 7 sliders (tint, blur, saturation, edge, radius, text size, hue), Grain/Sheen toggles. Custom keeps current knobs.
- Done: smooth wheel (vertical + horizontal) on Sound faders/brightness, eased 1-exp(-dt*18), device pushed ~every 50 ms; mute/drag/WM_MIX cancel or skip ease. Settings scroll eased.
- Debug build runs; reviewer fixes 1-4 applied.
- Skipped: migrated old configs get smoke blur but keep old opacity 0.92 (pays capture under near-opaque tint).
- Skipped: Media-tab wheel volume, clip/picker list scroll easing (row-step lists).
- Left: eyeball test, release build, tests, then commit/release 0.5.0.
