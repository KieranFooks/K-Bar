# Battery Tray

Minimalist `#![no_std]` + `#![no_main]` native Windows 11 utility (zero external crates, linked with `-nostdlib`) that displays wireless peripheral battery levels (**mice, keyboards, headsets, and Bluetooth gamepads**) across **Bluetooth PnP**, **Logitech**, **Razer**, **HyperX**, **SteelSeries**, and **Corsair** in a monochrome 2-row × N-column block integrated directly on the taskbar next to the system tray.

![Windows 11 taskbar preview](docs/preview.png)

## Features

- **Broad peripheral coverage**:
  - **Generic Bluetooth (`cfgmgr32.dll`)**: BLE GATT Battery Service (`0x180F`) and Classic HFP keyboards, mice, headsets/earbuds, and Bluetooth gamepads (including Xbox Wireless Controllers), deduplicated in favor of native HID.
  - **2.4 GHz / USB HID**: Logitech HID++ 2.0/1.0/ADC/G PRO X 2 (mice, keyboards, headsets), HyperX (Cloud II/III/Alpha/Flight), Razer (90-byte feature protocol across wireless mice & keyboards), SteelSeries (Arctis & Nova series), and Corsair (Bragi & Void/HS70).
- **Native taskbar overlay**: click-through (`WS_EX_LAYERED | WS_EX_TRANSPARENT`), owned by `Shell_TrayWnd` (no focus flicker), rendered with `Segoe Fluent Icons` (`Mouse`, `Keyboard`, `Headset`, `Gamepad`) + `Segoe UI`, automatically matching Windows 11 light/dark theme (`WM_SETTINGCHANGE`) and per-monitor DPI (`WM_DPICHANGED`). Expands horizontally (`2 rows × N columns`) for 3–6 peripherals while keeping exact `38 DIP` height.
- **Fullscreen auto-hide**: event-driven via WinEvent hooks (`EVENT_SYSTEM_FOREGROUND`, `MOVESIZEEND`, `MINIMIZE*`, `OBJECT_LOCATIONCHANGE`, `OBJECT_REORDER`) and Z-order occlusion scanning (catches borderless fullscreen games and fullscreen videos even across monitors) plus a 1 s guard timer.
- **Non-blocking live updates**: background worker thread polls every **45 seconds**, plus immediate refresh on USB/Bluetooth plug/unplug (`WM_DEVICECHANGE`) and resume from sleep (`WM_POWERBROADCAST`).
- **Ultra-lean Win32 footprint**: pure `raw-dylib` FFI (`kernel32`, `kernelbase`, `cfgmgr32`, `user32`, `gdi32`, `imm32`), zero heap allocations on the UI/poll fast path.

## Resource Footprint

| Metric | Current (`#![no_std]` `-nostdlib`) |
| --- | --- |
| Binary size | **~39 KB** (`0 B` `.data`) |
| Cargo dependencies | **0 external crates** |
| Threads | **2** (UI + background HID/BT poller) |
| UI guard timer | **1,000 ms** (event-driven hooks) |

## Download & Run (Portable `.exe`)

Download the latest single-file portable executable (~40 KB, zero dependencies, no installer) from [GitHub Releases](https://github.com/KieranFooks/K-Bar/releases/latest):

- **Direct download**: [`battery-tray.exe`](https://github.com/KieranFooks/K-Bar/releases/latest/download/battery-tray.exe) (CalVer releases `vYYYY.MM.DD`)

Double-click `battery-tray.exe` to run it immediately. *(On first launch of an unsigned binary, Windows SmartScreen may ask for confirmation: **More info → Run anyway**.)*

### Optional: One-Line Install & Windows Startup (PowerShell)

```powershell
$dst = "$env:LOCALAPPDATA\Programs\battery-tray\battery-tray.exe"
New-Item -ItemType Directory -Force (Split-Path $dst) | Out-Null
Invoke-WebRequest "https://github.com/KieranFooks/K-Bar/releases/latest/download/battery-tray.exe" -OutFile $dst
Unblock-File $dst
Set-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "BatteryTray" -Value "`"$dst`""
Stop-Process -Name battery-tray -ErrorAction SilentlyContinue
Start-Process $dst
```

Uninstall cleanly:

```powershell
Stop-Process -Name battery-tray -ErrorAction SilentlyContinue
Remove-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "BatteryTray" -ErrorAction SilentlyContinue
Remove-Item "$env:LOCALAPPDATA\Programs\battery-tray" -Recurse -Force -ErrorAction SilentlyContinue
```

## Build from Source

Requires the `x86_64-pc-windows-gnu` Rust toolchain:

```powershell
# Build release binary (target\release\battery-tray.exe)
cargo build --release

# Run unit tests
cargo test

# Live HID + Bluetooth hardware diagnostics
cargo run --bin poc
```

Launch or restart manually:

```powershell
Stop-Process -Name battery-tray -ErrorAction SilentlyContinue
Start-Process ".\target\release\battery-tray.exe"
```

## Run at Windows Startup

From the repository root in PowerShell:

```powershell
# Enable
Set-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "BatteryTray" -Value "`"$PWD\target\release\battery-tray.exe`""

# Disable
Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "BatteryTray"
```
