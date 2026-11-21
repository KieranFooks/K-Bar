# Battery Tray

Minimalist `#![no_std]` + `#![no_main]` native Windows 11 utility (zero external crates, linked with `-nostdlib`) that displays wireless peripheral battery levels (**Logitech HID++ 2.0 mice** and **HyperX Cloud III Wireless**) in a monochrome two-line block integrated directly on the taskbar next to the system tray.

![Windows 11 taskbar preview](docs/preview.png)

## Features

- **Native taskbar overlay**: click-through (`WS_EX_TRANSPARENT`), owned by `Shell_TrayWnd` (no focus flicker), rendered with `Segoe Fluent Icons` + `Segoe UI`, automatically matching Windows 11 light/dark theme (`WM_SETTINGCHANGE`) and per-monitor DPI (`WM_DPICHANGED`).
- **Fullscreen auto-hide**: event-driven via WinEvent hooks (`EVENT_SYSTEM_FOREGROUND`, `MOVESIZEEND`, `MINIMIZE*`, `OBJECT_LOCATIONCHANGE`) plus a 5 s guard timer.
- **Live updates**: polls every **45 seconds**, plus immediate refresh on USB plug/unplug (`WM_DEVICECHANGE`) and resume from sleep (`WM_POWERBROADCAST`).
- **Ultra-lean Win32 footprint**: pure `raw-dylib` FFI (`kernel32`, `kernelbase`, `user32`, `gdi32`, `imm32`), single-threaded, zero heap allocations on the UI/poll fast path.

## Resource Footprint

| Metric | Current (`#![no_std]` `-nostdlib`) |
| --- | --- |
| Binary size | **18,944 B (18.5 KB)** (`0 B` `.data`) |
| Cargo dependencies | **0 external crates** |
| Private RAM (`PrivateMemorySize64`) | **1.21 MB** |
| Threads | **1** |
| Kernel handles | **84** |
| Loaded modules / DLLs | **16** |
| UI guard timer | **5,000 ms** (event-driven hooks) |

## Build & Run

Requires the `x86_64-pc-windows-gnu` Rust toolchain:

```powershell
# Build release binary (target\release\battery-tray.exe)
cargo build --release

# Run unit tests
cargo test

# Live HID hardware diagnostics
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
