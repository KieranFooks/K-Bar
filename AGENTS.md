# AGENTS.md — Battery Tray

## Project Goal & Performance Budget
Ultra-minimal `#![no_std]` + `#![no_main]` single-threaded native Windows 11 taskbar utility written in **pure Rust** (`raw-dylib` Win32 FFI, **zero external crate dependencies**, linked with `-nostdlib`).

- **Binary target**: **~23.5 KB** stripped PE (`opt-level = "z"`, LTO, `codegen-units = 1`, `panic = "abort"`, `strip = true`, `-nostdlib`, `--gc-sections`).
- **Memory & zero-allocation budget**: Zero heap allocations on the UI sync/redraw path and routine HID polling (`SingleThreadCell`, fixed `[u16; 256]`/`[u8; 64]`/`[u16; 8]` stack buffers, single-DIB in-place premultiplied BGRA rendering, DPI-cached `HFONT`s).
- **Minimal OS / DLL footprint**: Direct PE IAT imports ONLY from `kernel32.dll`, `kernelbase.dll` (`RegGetValueW`), `imm32.dll` (`ImmDisableIME`), `user32.dll`, `gdi32.dll`, and 4 UCRT memory intrinsics (`api-ms-win-crt-*`). Completely eliminates `windows` crate, `advapi32.dll`, `sechost.dll`, `hid.dll`, `oleaut32.dll`, `setupapi.dll`, `shell32.dll`, `shcore.dll`, `dwmapi.dll`, and `MSCTF.dll`.
- **No `___chkstk_ms` in `-nostdlib`**: Keep every function's local stack frame strictly `< 2 KB`; store `HidScanner` and `AppState` in `static SingleThreadCell` rather than returning large structs on the stack.

---

## Target Hardware & HID Protocols

### 1. Logitech HID++ 2.0 Mice (G502 LIGHTSPEED, Unifying / Bolt / Direct USB)
- **Discovery**: Stack-only Raw Input enumeration (`GetRawInputDeviceList` + `GetRawInputDeviceInfoW` via `user32.dll`), filtered to `VID == 0x046D`, `UsagePage == 0xFF00`, `Usage == 0x0002` (20-byte long HID++ 2.0 reports `0x11`) BEFORE querying device paths. Candidate interfaces are cached in `HidScanner` so sleeping mice do not trigger repeated Raw Input enumerations every 45 s.
- **Software ID (`sw_id`)**: Always send a non-zero `sw_id` (`0x0A`) in `buf[3] = (func << 4) | SW_ID`. Filter incoming packets strictly by `(report_id == 0x11, dev_idx, feat_idx, func, sw_id)` or HID++ error frames (`0xFF` / `0x8F`) to avoid desync with **Logitech G HUB** or unsolicited `sw_id == 0` notifications.
- **Device indices**:
  - Single-device LIGHTSPEED receivers (`0xC539`, `0xC53A`, `0xC53F`, `0xC541`, `0xC545`, `0xC547`): probed first (`dev_idx = 1`).
  - Multi-device receivers (`0xC52B`, `0xC548`, other `0xC5xx`): probe `1..=6` with a short timeout.
  - Direct wired USB devices (non-`0xC5xx`): probe `0xFF`, fallback `1`.
- **Feature resolution**: Root `0x0000` `func 0` (`getFeature(id)`) directly for `0x0005` DeviceName, `0x1004` UnifiedBattery, `0x1000` BatteryStatus, `0x1001` BatteryVoltage. Fallback product string read uses `DeviceIoControl(IOCTL_HID_GET_PRODUCT_STRING)` directly without `hid.dll`.
- **Battery parsing**:
  - `0x1004`: query **`func 1`** (`get_status`) — `pct = r[4].min(100)`, `charging = matches!(r[6], 1 | 2)`.
  - `0x1000`: query `func 0` — `pct = r[4].min(100)`, `charging = matches!(r[6], 1 | 2 | 4)`.
  - `0x1001`: query `func 0` — `mv = u16::from_be_bytes([r[4], r[5]])` (`2500..=5000`), `charging = (r[6] & 0x80) != 0`, 100-entry Li-ion curve from Linux `hid-logitech-hidpp.c`.

### 2. HyperX Cloud III Wireless (`VID 0x03F0`, `PID 0x05B7`)
- **Interface filter**: `UsagePage == 0xFF13`, 62-byte reports.
- **Packets**: Report ID `0x66`, cmd at `buf[1]`. Fallback to `DeviceIoControl(IOCTL_HID_SET_FEATURE)` if `WriteFile` fails. Validate incoming packets with `r.len() >= 5 && r[0] == 0x66 && r[1] == cmd`.
- **Battery (`0x89`)**: If `r[2] == 0 && r[3] == 0`, headset is powered off (`pct: None`, `online: false`). Otherwise `pct = Some(r[4].min(100))`, `online = true`.
- **Charging (`0x8A`)**: Query only when online; `charging = r[2] != 0`.

---

## UI / UX Architecture (Passive Unified Taskbar Block)
- **100% Microsoft Windows 11 native look**: `Segoe Fluent Icons` (fallback `Segoe MDL2 Assets`) + `Segoe UI`, grayscale `ANTIALIASED_QUALITY` coverage converted in-place to premultiplied 32bpp BGRA in a single GDI DIB (`draw_widget_to_dib`), zero third-party assets.
- **Strict uniform monochrome integration**: Driven by `HKCU\...\Personalize\SystemUsesLightTheme` (via `kernelbase!RegGetValueW`) and live `WM_SETTINGCHANGE`. Uniform opacity across both rows.
- **Passive click-through taskbar block (`src/widget.rs`)**:
  - `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` `WS_POPUP` **owned by `Shell_TrayWnd`** (`win32k` guarantees atomic Z-order directly above the taskbar with zero focus flicker; reattaches on `TaskbarCreated` if Explorer restarts).
  - **Fullscreen auto-hide**: Event-driven via `EVENT_SYSTEM_FOREGROUND`, `EVENT_SYSTEM_MOVESIZEEND`, `EVENT_SYSTEM_MINIMIZESTART..=END`, plus `EVENT_OBJECT_LOCATIONCHANGE` scoped strictly to `Shell_TrayWnd`'s PID/TID and a 5 000 ms guard timer (ignores `IsZoomed` and Windows 11 Shell XAML overlays).

---

## Critical Win32 & Concurrency Invariants
1. **Never use `HWND_MESSAGE` for the main hidden window**: Windows does not deliver broadcast messages (`WM_SETTINGCHANGE`, `TaskbarCreated`, `WM_DEVICECHANGE`, `WM_POWERBROADCAST`) to message-only windows. Always create an unshown top-level window.
2. **Never hold mutable borrows/pointers across Win32 calls that re-enter `wndproc`**: Copy state/take `WidgetWindow` from `APP` (`SingleThreadCell`), drop pointer use, call Win32, then store back.
3. **Always retire cancelled overlapped HID I/O**: After `CancelIoEx(h, &ov)`, call `GetOverlappedResult(h, &ov, &mut got, 1)` before returning/resetting the reusable `io_event`. Call `ResetEvent(io_event)` before every `WriteFile`/`ReadFile`.
4. **Never use ClearType or luminance-to-alpha**: Render white-on-black with `ANTIALIASED_QUALITY`, average grayscale coverage into alpha, and premultiply monochrome BGRA in-place before `UpdateLayeredWindow`.
5. **Zero handle/GDI leaks**: Restore old DC selections (`old_bmp`, `old_font`) and destroy DIBs, DCs, `FontCache` `HFONT`s, `io_event`, `HWINEVENTHOOK`s, and `instance_mutex` cleanly on exit.

---

## Toolchain & Build Commands
GNU Rust toolchain (`x86_64-pc-windows-gnu`), `%USERPROFILE%\.cargo\bin` on `PATH`:

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path

cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
cargo run --bin poc
cargo run --bin glyphtest -- target\widget_preview.bmp
```
