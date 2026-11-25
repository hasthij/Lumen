# Lumen

One small Windows 11 app that replaces several tools:

| Lumen module | Replaces | What it does |
|---|---|---|
| **Clock** | Rainmeter | Draws your Rainmeter skin natively (about 10 MB of RAM, near-zero CPU) with size, opacity, colour, 12/24-hour and drag-to-move. |
| **Launcher** (Alt+Space) | PowerToys Run | Apps, files, folders, calculator, units, time, values, window switcher, shell, system commands, settings pages, services, registry and web search. New apps show up without a restart. |
| **Clear taskbar** | TranslucentTB | Transparent, tinted or blurred taskbar per situation (desktop, maximized window, Start, Task View), with a restore button and a fail-safe. |
| **Island** | Dynamic-island style widgets | A floating panel with system, shelf and launch tabs, clipboard tools, a volume/brightness HUD and a Media tab. The Media tab works with any player that has a Windows media session (Spotify, YouTube Music in a browser, VLC and others): title, controls, seek bar, and the album cover with a matching colour tint. When a player publishes no cover, it is looked up online by artist and title (only that text is sent). |
| **MacNotif** | Notification banners | macOS-style notification stack with real backdrop blur, app and avatar pictures, and native sounds kept. |

Everything is configurable from a Windows 11 style settings window (Mica backdrop, theme colours including Ultra black, live sliders).

## Install

Download **LumenSetup.exe** from the [latest release](https://github.com/hasthij/Lumen/releases/latest) and run it. It installs for your account only (no administrator rights).
The files aren't code-signed yet, so SmartScreen may warn: choose *More info*, then *Run anyway*.

If the taskbar ever looks wrong: `lumen.exe --restore --restart-explorer`. To undo everything Lumen changed: uninstall from *Apps and features*, or run `lumen.exe --uninstall`.

## Build

Requires Rust (stable, MSVC) and the Windows 10/11 SDK.

```powershell
cargo build --release -p lumen -p lumen-tap
cargo build --release -p lumen-setup      # embeds the two files above
powershell -File scripts\package.ps1       # builds dist\LumenSetup.exe and the portable zip
```

Crates:

- `crates/lumen`: the app (clock, launcher, island, notifications, settings, taskbar controller)
- `crates/lumen-tap`: a small DLL loaded into Explorer that changes the taskbar's XAML fill (the technique TranslucentTB uses)
- `crates/lumen-setup`: the self-contained installer and uninstaller

Useful switches: `--settings`, `--launcher`, `--restore`, `--uninstall`. The `dev-logs` cargo feature writes logs to a repo folder.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
