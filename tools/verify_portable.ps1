param(
    [Parameter(Mandatory = $true)]
    [string]$Path,
    [int]$MaxBytes = 65536,
    [string]$ExpectedVersion = ""
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $Path)) {
    throw "Executable not found: $Path"
}

$resolved = (Resolve-Path -LiteralPath $Path).Path
$bytes = [System.IO.File]::ReadAllBytes($resolved)
$size = $bytes.Length
Write-Host ("Binary: {0} ({1} bytes)" -f $resolved, $size)

if ($size -gt $MaxBytes) {
    throw ("Binary size {0} bytes exceeds budget {1} bytes" -f $size, $MaxBytes)
}

function Read-U16([byte[]]$b, [int]$off) { [BitConverter]::ToUInt16($b, $off) }
function Read-U32([byte[]]$b, [int]$off) { [BitConverter]::ToUInt32($b, $off) }

if ((Read-U16 $bytes 0) -ne 0x5A4D) { throw "Invalid MZ signature" }
$peOff = [int](Read-U32 $bytes 0x3C)
if ((Read-U32 $bytes $peOff) -ne 0x00004550) { throw "Invalid PE signature" }

$coffOff = $peOff + 4
$machine = Read-U16 $bytes $coffOff
if ($machine -ne 0x8664) { throw ("Expected x86_64 (0x8664), got 0x{0:X4}" -f $machine) }

$numSections = [int](Read-U16 $bytes ($coffOff + 2))
$optSize = [int](Read-U16 $bytes ($coffOff + 16))
$optOff = $coffOff + 20

$magic = Read-U16 $bytes $optOff
if ($magic -ne 0x020B) { throw ("Expected PE32+ (0x020B), got 0x{0:X4}" -f $magic) }

$subsystem = Read-U16 $bytes ($optOff + 68)
if ($subsystem -ne 2) { throw ("Expected Windows GUI subsystem (2), got {0}" -f $subsystem) }

$numDirs = [int](Read-U32 $bytes ($optOff + 108))
$importRva = if ($numDirs -gt 1) { Read-U32 $bytes ($optOff + 112 + 8) } else { 0 }

$secOff = $optOff + $optSize
$sections = @()
for ($i = 0; $i -lt $numSections; $i++) {
    $s = $secOff + ($i * 40)
    $nameBytes = $bytes[$s..($s + 7)]
    $name = [System.Text.Encoding]::ASCII.GetString($nameBytes).TrimEnd([char]0)
    $vSize = Read-U32 $bytes ($s + 8)
    $vAddr = Read-U32 $bytes ($s + 12)
    $rawSize = Read-U32 $bytes ($s + 16)
    $rawPtr = Read-U32 $bytes ($s + 20)
    $sections += [pscustomobject]@{
        Name = $name
        VirtualSize = $vSize
        VirtualAddress = $vAddr
        RawSize = $rawSize
        RawPtr = $rawPtr
    }
}

Write-Host ("Sections: {0}" -f (($sections | ForEach-Object { "{0}({1}B)" -f $_.Name, $_.VirtualSize }) -join ", "))
$dataSec = $sections | Where-Object { $_.Name -eq ".data" }
if ($dataSec -and $dataSec.RawSize -gt 0) {
    throw ("Unexpected non-empty .data section ({0} bytes on disk)" -f $dataSec.RawSize)
}

function Rva-To-Offset([uint32]$rva) {
    foreach ($sec in $sections) {
        $span = [Math]::Max($sec.VirtualSize, $sec.RawSize)
        if ($rva -ge $sec.VirtualAddress -and $rva -lt ($sec.VirtualAddress + $span)) {
            return [int](($rva - $sec.VirtualAddress) + $sec.RawPtr)
        }
    }
    return -1
}

function Read-AsciiZ([byte[]]$b, [int]$off) {
    $end = $off
    while ($end -lt $b.Length -and $b[$end] -ne 0) { $end++ }
    [System.Text.Encoding]::ASCII.GetString($b, $off, $end - $off)
}

$allowedDlls = @(
    "kernel32.dll",
    "kernelbase.dll",
    "cfgmgr32.dll",
    "user32.dll",
    "gdi32.dll",
    "imm32.dll",
    "api-ms-win-crt-private-l1-1-0.dll",
    "api-ms-win-crt-string-l1-1-0.dll"
)

$imports = @()
if ($importRva -ne 0) {
    $impOff = Rva-To-Offset $importRva
    if ($impOff -lt 0) { throw "Could not map import directory RVA" }
    while ($true) {
        $nameRva = Read-U32 $bytes ($impOff + 12)
        $firstThunk = Read-U32 $bytes ($impOff + 16)
        if ($nameRva -eq 0 -and $firstThunk -eq 0) { break }
        $nameOff = Rva-To-Offset $nameRva
        if ($nameOff -lt 0) { throw "Invalid import DLL name RVA" }
        $dll = (Read-AsciiZ $bytes $nameOff).ToLowerInvariant()
        $imports += $dll
        if ($allowedDlls -notcontains $dll) {
            throw ("Unexpected DLL import: {0}" -f $dll)
        }
        $impOff += 20
    }
}

Write-Host ("Imports ({0}): {1}" -f $imports.Count, ($imports -join ", "))

if ($ExpectedVersion -ne "") {
    $verInfo = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($resolved)
    Write-Host ("FileVersion: {0} | ProductName: {1}" -f $verInfo.FileVersion, $verInfo.ProductName)
    if ($verInfo.FileVersion -ne $ExpectedVersion) {
        throw ("Expected FileVersion '{0}', got '{1}'" -f $ExpectedVersion, $verInfo.FileVersion)
    }
}

Write-Host "Portable PE verification PASSED."
