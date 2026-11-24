# Lumen MacNotif (0.6.0, uncommitted, Cargo stays 0.5.4)
- Built + 12 tests pass. Lumen cards verified by screenshot (contrast raised, tint floor 72%).
- Sound: Lumen chime + its toggles REMOVED. Native Windows sound plays; toasts are removed from the center 1.8s after capture (so sound is not clipped); banner hidden by CoreWindow hider.
- Native banner hidden by a CoreWindow hider thread; toast removed 1.8 s later so Windows' own sound plays. NotificationChanged fails for unpackaged apps (0x80070490), so the listener polls every 120 ms (500 ms if blocking is off); the first pass clears the Action Center backlog because 75 stale toasts made each poll cost 230 ms (idle CPU 50% -> 2%).
- Cards use a real backdrop blur (screen grab + box blur) with Blur, Colour boost, Tint sliders; blur 0 = flat tint. Test toasts for Discord/Telegram are silent like the real apps. Unverified: audio by ear, real Discord, multi-monitor/high-DPI capture.
- Unverified by ear: native sound no longer clipped, Discord/Telegram own sound with real apps.

## MacNotif update (0.6.0, uncommitted)
- Native Win11 banner = CoreWindow "New notification" (ShellExperienceHost). winnotif.rs hider thread SW_HIDEs it every 6 ms while blocking. Verified: not visible, Lumen cards show.
- Settings > MacNotif: "Test as Discord / Telegram / other app" send real toasts via hidden PowerShell (Data_Xml_Dom feature crashes rustc, removed).
- Sound fix: ToastEnabled untouched; scheme sounds are restored (never muted) at start.
