# AGENTS.md (katt)

## Project Goal & Performance Budget
Ultra-minimal `#![no_std]` + `#![no_main]` native Windows 11 taskbar utility written in **pure Rust** (`raw-dylib` Win32 FFI, **zero external crate dependencies**, linked with `-nostdlib`).

- **Binary target**: **~39 KB** stripped PE (`opt-level = "z"`, LTO, `codegen-units = 1`, `panic = "abort"`, `strip = true`, `-nostdlib`, `--gc-sections`), **0 bytes in `.data`** (static `AppState`, `HidScanner`, `PollOutput`, and UI caches zero-initialized in `.bss` via `MaybeUninit::zeroed()` + `init_in_place`).
- **Memory & zero-allocation budget**: Zero heap allocations on the UI sync/redraw path and routine HID/Bluetooth polling (`SingleThreadCell`, fixed stack buffers, single-DIB in-place premultiplied BGRA rendering, DPI-cached `HFONT`s, thread-safe local `FontCache` for `render_widget_premul_bgra`).
- **Concurrency model**: UI thread (hidden broadcast window + taskbar widget + WinEvent hooks) + 1 lightweight background worker thread (`CreateThread`, `WaitForSingleObject`, spinlock-protected `PollOutput` handoff + `WM_POLL_DONE`) so HID timeouts never block UI sync or fullscreen auto-hide.
- **Minimal OS / DLL footprint**: Direct PE IAT imports ONLY from `kernel32.dll`, `kernelbase.dll` (`RegGetValueW`), `cfgmgr32.dll` (PnP Bluetooth battery properties), `imm32.dll` (`ImmDisableIME`), `user32.dll`, `gdi32.dll`, and UCRT `memset`/`memcpy` (`api-ms-win-crt-string-l1-1-0.dll` / `api-ms-win-crt-private-l1-1-0.dll`). Completely eliminates `windows` crate, `advapi32.dll`, `sechost.dll`, `hid.dll`, `oleaut32.dll`, `setupapi.dll`, `shell32.dll`, `shcore.dll`, `dwmapi.dll`, and `MSCTF.dll`.
- **No `___chkstk_ms` in `-nostdlib`**: Keep every function's local stack frame well below the 4 KB guard page; store large enumeration buffers (`[RAWINPUTDEVICELIST; 128]`, `[RawCandidate; 24]`, `[u16; 8192]`) directly inside `.bss` `HidScanner` (`init_in_place`) and close `io_event` in-place via `scanner.close()`.

---

## Target Hardware & Protocols (`src/drivers/`)

### 1. Generic Bluetooth (`src/drivers/bluetooth.rs` via `cfgmgr32.dll`)
- Enumerates present `BTHLE`, `BTHENUM`, `BTHHFENUM`, and `BTHLEDEVICE` devnodes (`CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT`).
- Reads `DEVPKEY_Bluetooth_Battery` (`{104EA319-6EE2-4701-BD47-8DDBF425BBE5}, 2`) and requires `DEVPKEY_Device_IsConnected == true` (walking `CM_Get_Parent` up to 3 levels for metadata).
- Classifies into `Mouse`, `Keyboard`, `Headset`, or `Gamepad` (e.g. Xbox Wireless Controllers) via `DEVPKEY_DeviceContainer_Category`, `DEVPKEY_Device_FriendlyName`, and `DEVPKEY_Bluetooth_ClassOfDevice` while rejecting phones, PCs, watches, cameras, printers, and displays.
- Deduplicated in `BatteryList::upsert` in favor of `Source::NativeHid` when both transports expose the same `(vid_pid, kind)`.

### 2. Logitech HID++ 2.0 / 1.0 & Vendor (`src/drivers/logitech.rs`, `VID 0x046D`)
- Discovers mice, keyboards, and headsets across Unifying (`0xC52B` 6 slots + HID++ 1.0 `0x0D`/`0x07` fallback + `Col01` fast offline detection), Bolt, LIGHTSPEED receivers, direct USB, `0xFF43` headsets, G535 (`0x0AC4`), and G PRO X 2 LIGHTSPEED (`0x0AF7` on `0xFFA0`).
- Uses `sw_id = 0x0A`, `getDeviceType` (`0x0005` `func 2`), and battery features `0x1004`, `0x1000`, `0x1001`, `0x1F20`.

### 3. HyperX (`src/drivers/hyperx.rs`, `VID 0x03F0` & `0x0951`)
- Cloud III Wireless (`0x03F0:0x05B7`, `0xFF13`, `0x66 0x89`/`0x8A`), Cloud Alpha Wireless (`0x098D`, `0x21 0xBB`), Cloud II Wireless HP (`0x0696`/`0x018B`), Cloud II Wireless Kingston (`0x0951:0x1718`), and Cloud Flight (`0x16C4`/`0x1723`).

### 4. Razer (`src/drivers/razer.rs`, `VID 0x1532`)
- Table-driven ~60 wireless mouse & keyboard PIDs (Viper, DeathAdder, Basilisk, Cobra, Naga, Orochi, Pro Click, BlackWidow, DeathStalker, Joro) via 90-byte `razer_report` (`IOCTL_HID_SET_FEATURE` / `GET_FEATURE` with XOR CRC, `open_hid_feature` fallback access `0`).

### 5. SteelSeries (`src/drivers/steelseries.rs`, `VID 0x1038`) & Corsair (`src/drivers/corsair.rs`, `VID 0x1B1C`)
- SteelSeries: Arctis 7+, Arctis Nova 7/7X/Gen 2 (discrete & %), Nova 5, Nova Pro Wireless, Arctis 7/Pro, Arctis 9.
- Corsair: Bragi (`0xFF42`, properties `0x0F`/`0x10` for Virtuoso, HS80, Slipstream mice/keyboards) and Void/HS70 (`0xFFC5`, `0xC9 0x64`).

---

## UI / UX Architecture (Passive 2-Row × N-Column Taskbar Block)
- **100% Microsoft Windows 11 native look**: `Segoe Fluent Icons` (`Mouse` `0xE962`, `Keyboard` `0xE765`, `Headset` `0xE7F6`, `Gamepad` `0xE7FC`, `Bolt` `0xE945`, `Battery` `0xEBA0`) + `Segoe UI`, grayscale `ANTIALIASED_QUALITY` coverage converted in-place to premultiplied 32bpp BGRA in a single GDI DIB (`draw_widget_to_dib`), zero third-party assets.
- **2-row × N-column grid (`src/widget.rs`)**: Fixed `38 DIP` height aligned with the Windows 11 clock; 1 column for 0–2 devices, expanding horizontally to 2–3 columns for 3–6 devices.
- **Strict uniform monochrome integration**: Driven by `HKCU\...\Personalize\SystemUsesLightTheme` (via `kernelbase!RegGetValueW` + `RRF_RT_REG_DWORD`) and live `WM_SETTINGCHANGE`. Uniform opacity across all cells.
- **Passive click-through taskbar block (`src/widget.rs`)**:
  - `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` `WS_POPUP` **owned by `Shell_TrayWnd`**, fullscreen occlusion auto-hide, `1 000 ms` guard timer + WinEvent hooks coalesced via `SYNC_PENDING`.

---

## Critical Win32 & Concurrency Invariants
1. **Never use `HWND_MESSAGE` for the main hidden window**: Windows does not deliver broadcast messages (`WM_SETTINGCHANGE`, `TaskbarCreated`, `WM_DEVICECHANGE`, `WM_POWERBROADCAST`) to message-only windows. Always create an unshown top-level window, and handle both `WM_CLOSE` and `WM_DESTROY` idempotently.
2. **Never hold mutable references or re-enter across Win32 calls**: Snapshot/take `WidgetWindow` from `APP`, drop borrows, guard with `syncing`/`polling` flags, then call Win32 (`DestroyWindow` / `sync_position`). Read `LAST_OWNER_ATTEMPT` by value before `DestroyWindow`.
3. **Always pass valid mutable `*mut OVERLAPPED` on `FILE_FLAG_OVERLAPPED` handles & retire cancelled I/O**: Use `hid_ioctl` / `send_report` / `recv_report` with `ResetEvent(io_event)` before every `WriteFile`/`ReadFile`/`DeviceIoControl`, pass `*mut OVERLAPPED` (never `&OVERLAPPED`, so LLVM cannot assume kernel-mutated memory is `readonly`), and after `CancelIoEx(h, ov)`, ALWAYS call `GetOverlappedResult(h, ov, &mut got, 1)` before returning.
4. **Never use ClearType or luminance-to-alpha**: Render white-on-black with `ANTIALIASED_QUALITY`, average grayscale coverage into alpha, and premultiply monochrome BGRA in-place before `UpdateLayeredWindow`.
5. **Zero handle/GDI leaks**: Restore old DC selections (`old_bmp`, `old_font`) and destroy DIBs, DCs, `FontCache` `HFONT`s, `io_event` (`HidScanner::close`), `HWINEVENTHOOK`s, and `instance_mutex` cleanly on exit.
6. **Keep `HidPath` 4-byte aligned (`#[repr(align(4))]`)**: `win32k!NtUserGetRawInputDeviceInfo` probes the output buffer for 4-byte alignment even on `RIDI_DEVICENAME` (`[u16; 256]`) and fails with `ERROR_NOACCESS` (`998`) if `opt-level = "z"` packs a 2-byte-aligned `[u16; 256]` stack local at `addr % 4 == 2`.

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
