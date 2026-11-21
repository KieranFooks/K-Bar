# Battery Tray

Utilitaire natif Windows 11 ultra-minimaliste écrit en **Rust pur `#![no_std]` + `#![no_main]`** (zéro crate externe, lié avec `-nostdlib`) qui affiche le niveau de batterie des périphériques sans fil (**souris Logitech HID++ 2.0** et **casque HyperX Cloud III Wireless**) dans un bloc monochrome à 2 lignes intégré directement sur la barre des tâches, à côté de l'horloge.

## Fonctionnement

- **Affichage passif unifié** : bloc transparent aux clics (`WS_EX_TRANSPARENT`), monochrome uniforme sur toutes les lignes, détenu par `Shell_TrayWnd` (zéro clignotement de focus), adapté automatiquement au thème clair/sombre Windows 11 (`WM_SETTINGCHANGE`).
- **Masquage plein écran** : piloté par événements (`EVENT_SYSTEM_FOREGROUND`, `MOVESIZEEND`, `MINIMIZE*`, `OBJECT_LOCATIONCHANGE` ciblé sur le thread de la barre des tâches) + garde de 5 s.
- **Actualisation** : toutes les **45 secondes**, plus immédiatement au branchement/débranchement USB (`WM_DEVICECHANGE`) et à la sortie de veille (`WM_POWERBROADCAST`).
- **Empreinte minimale** : zéro allocation sur le chemin rapide de rendu/synchronisation, un seul DIB GDI par redessin, polices `HFONT` cachées par DPI, un seul handle d'événement HID réutilisé, `ImmDisableIME(-1)` pour éviter `MSCTF.dll`, `KERNELBASE!RegGetValueW` pour éviter `advapi32.dll`, et `DeviceIoControl` direct pour éviter `hid.dll`.

## Démarrage automatique avec Windows

### PowerShell (recommandé)

**Activer** le lancement au démarrage de la session :
```powershell
Set-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "BatteryTray" -Value '"C:\Users\kiera\Projects\battery-tray\target\release\battery-tray.exe"'
```

**Désactiver** (supprimer du démarrage automatique) :
```powershell
Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "BatteryTray"
```

**Vérifier** l'entrée actuelle :
```powershell
Get-ItemPropertyValue -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "BatteryTray"
```

### Invite de commandes (`cmd.exe`)

**Activer :**
```cmd
reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v BatteryTray /t REG_SZ /d "\"C:\Users\kiera\Projects\battery-tray\target\release\battery-tray.exe\"" /f
```

**Désactiver :**
```cmd
reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v BatteryTray /f
```

## Lancer / arrêter manuellement

```powershell
# Lancer (ou redémarrer après mise à jour)
Stop-Process -Name battery-tray -ErrorAction SilentlyContinue
Start-Process "C:\Users\kiera\Projects\battery-tray\target\release\battery-tray.exe"
```

## Compilation et diagnostic

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path

# Compilation release ultra-minimale (#![no_std], -nostdlib, LTO + strip)
cargo build --release

# Tests unitaires (9 tests sur les parseurs HID, rendu monochrome, placement et classes Shell)
cargo test

# Diagnostic HID matériel en direct
cargo run --bin poc
```

## Empreinte mesurée

| Métrique | Initial (avant reprise) | Mono-thread `std` (`7d06d79`) | **`#![no_std]` `-nostdlib` (actuel)** |
| --- | --- | --- | --- |
| Taille binaire | 329 728 o (322,0 Ko) | 305 664 o (298,5 Ko) | **24 064 o (23,5 Ko)** (`−92,7 %`) |
| Dépendances Cargo | `windows` (0.62 + WinRT) | `windows` (0.62) | **0 crate externe** |
| Imports PE directs (IAT) | `advapi32`, `setupapi`, `shell32`, `shcore`, `dwmapi`, `hid`, `oleaut32`, `user32`, `gdi32`, `kernel32` | `advapi32`, `hid`, `oleaut32`, `user32`, `gdi32`, `kernel32` | **`kernel32`, `kernelbase`, `imm32`, `user32`, `gdi32`** (+ intrinsics UCRT) |
| Timer de garde UI | 1 000 ms (86 400 réveils/j) | 1 000 ms | **5 000 ms** (`−80 %` + hooks ciblés) |
