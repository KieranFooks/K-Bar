# AGENTS.md — Battery Tray

## Project Goal & Performance Budget
Lightweight native Windows 11 taskbar utility written in **Rust** (`windows-rs` 0.62) that displays live battery levels and charging state for wireless peripherals in a **single unified 2-line taskbar block** (Option 1A).

- **Memory target**: < 3 MB private working set (`PrivateMemorySize64`).
- **CPU target**: ~0% idle CPU (100% event-driven Win32 message loop + sleepable `Condvar` HID worker + zero-allocation cached redraws).
- **Binary target**: small stripped release executable (`opt-level = "z"`, LTO, `codegen-units = 1`, `panic = "abort"`, `strip = true`).
- **Zero heavy dependencies**: no webviews, no frameworks, no embedded raster/SVG image assets.

---

## Target Hardware & HID Protocols

### 1. Logitech HID++ 2.0 Mice (G502 LIGHTSPEED, Unifying / Bolt / Direct USB)
- **Interface filter**: `VID == 0x046D`, `UsagePage == 0xFF00`, `OutputReportByteLength == 20` (long HID++ 2.0 reports, `0x11`). Ignore 32-byte or 7-byte sibling interfaces so discovery never stalls.
- **Software ID (`sw_id`)**: Always send a non-zero `sw_id` (`0x0A`) in `buf[3] = (func << 4) | SW_ID`. Filter incoming packets strictly by `(report_id == 0x11, dev_idx, feat_idx, func, sw_id)` or HID++ error frames (`0xFF` / `0x8F`) to avoid desync with background traffic from **Logitech G HUB** or unsolicited `sw_id == 0` notifications.
- **Device indices**:
  - Single-device LIGHTSPEED receivers (`0xC539`, `0xC53A`, `0xC53F`, `0xC541`, `0xC545`, `0xC547`): probe `dev_idx = 1` first.
  - Multi-device receivers (`0xC52B`, `0xC548`, other `0xC5xx`): probe `1..=6` with a short timeout.
  - Direct wired USB devices (non-`0xC5xx`): probe `0xFF`, fallback `1`.
- **Feature resolution**: Use Root `0x0000` `func 0` (`getFeature(id)`) directly for target feature IDs (`0x0005` DeviceName, `0x1004` UnifiedBattery, `0x1000` BatteryStatus, `0x1001` BatteryVoltage) — do not enumerate the entire feature table on every poll.
- **Battery parsing**:
  - `0x1004`: query **`func 1`** (`get_status`) — `pct = r[4].min(100)`, `charging = matches!(r[6], 1 | 2)`.
  - `0x1000`: query `func 0` — `pct = r[4].min(100)`, `charging = matches!(r[6], 1 | 2 | 4)`.
  - `0x1001`: query `func 0` — `mv = u16::from_be_bytes([r[4], r[5]])` (validate `2500..=5000`), `charging = (r[6] & 0x80) != 0`, map `mv` using the 100-entry Li-ion curve from Linux `hid-logitech-hidpp.c`.

### 2. HyperX Cloud III Wireless (`VID 0x03F0`, `PID 0x05B7`)
- **Interface filter**: `UsagePage == 0xFF13`, `in = 62`, `out = 62`.
- **Packets**: Report ID `0x66`, command byte at `buf[1]`. Fallback to `HidD_SetFeature` if `WriteFile` fails. Validate incoming packets with `r.len() >= 5 && r[0] == 0x66 && r[1] == cmd`.
- **Battery (`0x89`)**: If `r[2] == 0 && r[3] == 0`, the headset is **powered off / offline** (`pct: None`, `online: false`). Otherwise `pct = Some(r[4].min(100))`, `online = true`.
- **Charging (`0x8A`)**: Query only when online; `charging = r[2] != 0`.

---

## UI / UX Architecture (Option 1A — Unified Taskbar Block)
- **100% Microsoft Windows 11 native look**: uses system fonts **`Segoe Fluent Icons`** (fallback `Segoe MDL2 Assets`) and **`Segoe UI`** — no third-party icon packs or flashy colors.
- **Strict monochrome integration**: light glyphs/text on dark taskbars, dark glyphs/text on light taskbars, driven by `HKCU\...\Personalize\SystemUsesLightTheme` and live `WM_SETTINGCHANGE` updates.
- **Unified 2-line taskbar block (`src/widget.rs`)**:
  - Single `WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST` `WS_POPUP` window rendered with `UpdateLayeredWindow` (premultiplied 32bpp BGRA).
  - Positioned cleanly to the left of `TrayNotifyWnd` on `Shell_TrayWnd`, matching the Windows 11 Clock block height (`38` DIPs centered in the `48`-DIP taskbar) with subtle rounded hover/active pill highlights.
  - **Fullscreen auto-hide**: hides automatically when `SHQueryUserNotificationState` reports `QUNS_RUNNING_D3D_FULL_SCREEN`, when the taskbar auto-hides, or when an opaque foreground window covers the full monitor (`EVENT_SYSTEM_FOREGROUND` hook + lightweight 1 s check).
  - **Horizontal drag-to-adjust**: users can drag the block horizontally along the taskbar; the offset (`OffsetX`) persists in `HKCU\Software\BatteryTray` and can be reset from the flyout (`Réinitialiser la position`).
- **Native Windows 11 flyout (`src/popup.rs`)**: DWM rounded corners (`DWMWCP_ROUND`), DWM immersive dark/light mode & border color, Per-Monitor DPI scaling (`GetDpiForWindow`), double-buffered GDI paint, anchored directly above the taskbar block, dismisses on outside click (`WA_INACTIVE`) and `Escape`, and includes **Actualiser**, **Lancer au démarrage** (`HKCU\...\Run`), optional **Réinitialiser la position**, and **Quitter**.

---

## Critical Win32 & Concurrency Invariants
1. **Never use `HWND_MESSAGE` for the main hidden window**: Windows does **not** deliver broadcast messages (`WM_SETTINGCHANGE`, `TaskbarCreated`, `WM_DEVICECHANGE`, `WM_POWERBROADCAST`) to message-only windows. Always create a hidden top-level window (`parent = None`, `WS_EX_TOOLWINDOW`, never shown).
2. **Never hold `STATE.0` mutex across Win32 window/message calls**: Calling `DestroyWindow` or `SendMessageW` while locking `STATE.0` causes an immediate deadlock when `WM_DESTROY` or another handler locks `STATE.0`. Snapshot state, drop the lock, then call Win32.
3. **Always retire cancelled overlapped HID I/O**: After `CancelIoEx(h, Some(&ov))`, call `GetOverlappedResult(h, &ov, &mut got, true)` before closing the event handle or dropping the buffer so the kernel cannot write into freed stack/heap memory.
4. **Never use ClearType or luminance-to-alpha**: Always render text/glyphs white-on-black with `ANTIALIASED_QUALITY` (grayscale coverage, zero ClearType RGB fringes), composite straight RGBA layers, and convert via `rgba_to_premul_bgra` before `UpdateLayeredWindow`.
5. **Zero handle leaks**: Every `CreateEventW`, `CreateCompatibleDC`, `CreateDIBSection`/`CreateCompatibleBitmap`, `CreateFontW`, `CreateSolidBrush`, `CreatePen`, and `HWINEVENTHOOK` must restore previous DC selections and be destroyed (`DeleteObject`/`DeleteDC`/`ReleaseDC`/`CloseHandle`/`UnhookWinEvent`).

---

## Toolchain, Build, Test & Profiling Commands
This machine uses the GNU Rust toolchain (`x86_64-pc-windows-gnu`). Ensure `%USERPROFILE%\.cargo\bin` is on `PATH`:

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path

# Unit tests
cargo test

# Lint / check all binaries
cargo clippy --all-targets -- -D warnings

# Release build (stripped, LTO)
cargo build --release

# Live HID hardware probe (does not launch GUI)
cargo run --bin poc

# Render Option 1A taskbar block contact sheet for visual verification
cargo run --bin glyphtest -- target\widget_preview.bmp
```
