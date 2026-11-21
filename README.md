# Battery Tray

Utilitaire natif Windows 11 minimaliste en Rust qui affiche le niveau de batterie des périphériques sans fil (**souris Logitech HID++ 2.0** et **casque HyperX Cloud III Wireless**) dans un bloc monochrome à 2 lignes intégré directement sur la barre des tâches, à côté de l'horloge.

## Fonctionnement

- **Affichage passif unifié** : bloc transparent aux clics (`WS_EX_TRANSPARENT`), monochrome (s'adapte automatiquement au thème clair/sombre Windows 11 via `WM_SETTINGCHANGE`) et à teinte uniforme sur toutes les lignes.
- **Masquage plein écran** : disparaît automatiquement dès qu'une application ou un jeu passe en plein écran sur le même écran, ou si la barre des tâches est masquée automatiquement.
- **Actualisation** : toutes les **45 secondes**, plus immédiatement au branchement/débranchement USB (`WM_DEVICECHANGE`) et à la sortie de veille (`WM_POWERBROADCAST`).
- **Zéro dépendance lourde** : n'utilise ni WebView, ni `setupapi.dll`, ni `shell32.dll`, ni `shcore.dll`, ni `dwmapi.dll`. Découverte HID via Raw Input (`user32.dll`) et boucle événementielle mono-thread.

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
# Lancer
Start-Process "C:\Users\kiera\Projects\battery-tray\target\release\battery-tray.exe"

# Arrêter
Stop-Process -Name battery-tray
```

## Compilation et diagnostic

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path

# Compilation release optimisée (LTO + strip)
cargo build --release

# Tests unitaires
cargo test

# Diagnostic HID matériel en direct
cargo run --bin poc
```

## Empreinte mesurée

Mesuré sur `target\release\battery-tray.exe` (Windows 11, après 40 s au repos) :

| Métrique | Avant | Après | Gain |
| --- | --- | --- | --- |
| Taille binaire | 329 728 o (322,0 Ko) | **304 640 o (297,5 Ko)** | −24,5 Ko |
| RAM privée (`PrivateMemorySize64`) | 1,64 Mo | **1,14 Mo** | −30 % |
| Working Set | 9,14 Mo | **7,14 Mo** | −22 % |
| CPU au repos (`CpuDeltaS`) | 0,0000 s | **0,0000 s** | — |
| Threads (boot → repos) | 5 → 3 | 4 → **1** | **−2 threads** |
| Handles noyau | 128 | **91** (`+0` fuite) | **−37 (−29 %)** |
| Modules / DLL chargés | 29 | **20** | **−9 DLL** |

DLL supprimées : `setupapi.dll`, `DEVOBJ.dll`, `cfgmgr32.dll`, `WINTRUST.dll`, `CRYPT32.dll`, `MSASN1.dll`, `shell32.dll`, `shcore.dll`, `dwmapi.dll`.
