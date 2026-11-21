# AGENTS.md — Battery Tray

## Project Goal & Performance Budget
Lightweight native Windows 11 system tray utility written in **Rust** (`windows-rs` 0.62) that displays live battery levels and charging state for wireless peripherals in the taskbar notification area.

- **Memory target**: < 3 MB private working set (`PrivateMemorySize64`).
- **CPU target**: ~0% idle CPU (100% event-driven Win32 message loop + sleepable `Condvar` HID worker).
- **Binary target**: small stripped release executable (`opt-level = "z"`, LTO, `codegen-units = 1`, `panic = "abort"`, `strip = true`).
- **Zero heavy dependencies**: no webviews, no frameworks, no embedded raster/SVG image assets.

---

## Target Hardware & HID Protocols

### 1. Logitech HID++ 2.0 Mice (G502 LIGHTSPEED, Unifying / Bolt / Direct USB)
- **Interface filter**: `VID == 0x046D`, `UsagePage == 0xFF00`, `OutputReportByteLength == 20` (long HID++ 2.0 reports, `0x11`). Ignore 32-byte or 7-byte sibling interfaces so discovery never stalls.
- **Software ID (`sw_id`)**: Always send a non-zero `sw_id` (e.g. `0x0A`) in `buf[3] = (func << 4) | SW_ID`. Filter incoming packets strictly by `(report_id == 0x11, dev_idx, feat_idx, func, sw_id)` or HID++ error frames (`0xFF` / `0x8F`) to avoid desync with background traffic from **Logitech G HUB** or unsolicited `sw_id == 0` notifications.
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

## UI / UX Guidelines
- **100% Microsoft Windows 11 native look**: use system fonts **`Segoe Fluent Icons`** (fallback `Segoe MDL2 Assets`) and **`Segoe UI`** — no third-party icon packs or flashy colors.
- **Strict monochrome taskbar integration**: white/light glyphs on dark taskbars, dark glyphs on light taskbars, driven by `HKCU\...\Personalize\SystemUsesLightTheme` and live `WM_SETTINGCHANGE` updates.
- **Side-by-side tray icons**: one icon per peripheral with **stable `uID`** (`Mouse = 1`, `Headset = 2`, `Placeholder = 99`) so Windows 11 taskbar pinning/ordering never swaps when a device sleeps.
- **Crisp low-DPI & HiDPI icon rendering**:
  - `size <= 22` (16×16, 20×20): custom 3×5 pixel digits + 4×6 charging bolt + 1px knockout halo around the bottom-right pill.
  - `size > 22` (24×24, 32×32, 48×48): SDF-antialiased rounded pill + `Segoe UI` Bold coverage mask + scaled charging bolt.
- **Native Windows 11 flyout (`popup.rs`)**: DWM rounded corners (`DWMWCP_ROUND`), DWM immersive dark/light mode & border color, Per-Monitor DPI scaling (`GetDpiForWindow`), double-buffered GDI paint, anchored above the clicked tray icon (`Shell_NotifyIconGetRect`), dismisses on outside click (`WA_INACTIVE`) and `Escape`, and includes Refresh, Autostart (`HKCU\...\Run`), and Quit actions.

---

## Critical Win32 & Concurrency Invariants
1. **Never use `HWND_MESSAGE` for the main hidden window**: Windows does **not** deliver broadcast messages (`WM_SETTINGCHANGE`, `TaskbarCreated`, `WM_DEVICECHANGE`, `WM_POWERBROADCAST`) to message-only windows. Always create a hidden top-level window (`parent = None`, `WS_EX_TOOLWINDOW`, never shown).
2. **Never hold `STATE.0` mutex across Win32 window/message calls**: Calling `DestroyWindow` or `SendMessageW` while locking `STATE.0` causes an immediate deadlock when `WM_DESTROY` or another handler locks `STATE.0`. Snapshot state, drop the lock, then call Win32.
3. **Always retire cancelled overlapped HID I/O**: After `CancelIoEx(h, Some(&ov))`, call `GetOverlappedResult(h, &ov, &mut got, true)` before closing the event handle or dropping the buffer so the kernel cannot write into freed stack/heap memory.
4. **Never derive icon alpha from pixel luminance**: Luminance-to-alpha makes dark-on-light icons invisible and punches holes in dark pill text. Always render glyphs white-on-black with `ANTIALIASED_QUALITY` (grayscale coverage, zero ClearType RGB fringes), composite straight RGBA layers, convert to premultiplied BGRA, and pair with a 1bpp AND mask.
5. **Zero handle leaks**: Every `CreateEventW`, `CreateCompatibleDC`, `CreateDIBSection`/`CreateCompatibleBitmap`, `CreateFontW`, `CreateSolidBrush`, `CreatePen`, and `HICON` must restore previous DC selections and be destroyed (`DeleteObject`/`DeleteDC`/`ReleaseDC`/`CloseHandle`/`DestroyIcon`).

---

## Toolchain, Build, Test & Profiling Commands
This machine uses the GNU Rust toolchain (`x86_64-pc-windows-gnu`). Ensure `%USERPROFILE%\.cargo\bin` is on `PATH`:

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path

# Unit tests
cargo test

# Lint / check all binaries
cargo clippy --all-targets

# Release build (stripped, LTO, ~300 KB)
cargo build --release

# Live HID hardware probe (does not launch GUI)
cargo run --bin poc

# Render tray icon contact sheet for visual verification
cargo run --bin glyphtest -- target\glyph_preview.bmp

# Memory / CPU / Handle profiling over a poll interval
Start-Process .\target\release\battery-tray.exe
Start-Sleep -Seconds 5
$p1 = Get-Process battery-tray
Start-Sleep -Seconds 40
$p2 = Get-Process battery-tray
[pscustomobject]@{
    PrivateMB = [math]::Round($p2.PrivateMemorySize64 / 1MB, 2)
    Threads   = $p2.Threads.Count
    Handles0  = $p1.HandleCount
    Handles1  = $p2.HandleCount
    CpuDeltaS = [math]::Round($p2.CPU - $p1.CPU, 4)
}
```
