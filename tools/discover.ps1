Write-Output "=== PnP devices (souris/casque/bluetooth/audio/HID) ==="
Get-PnpDevice | Where-Object { $_.FriendlyName -match 'mouse|souris|headset|casque|logitech|razer|steelseries|corsair|hyperx|bluetooth|headphone|audio|micro|webcam|usb' } |
  Select-Object Class, Status, FriendlyName, InstanceId | Format-Table -AutoSize | Out-String -Width 300

Write-Output "=== Tous les devices Bluetooth ==="
Get-PnpDevice -Class Bluetooth | Select-Object Status, FriendlyName, InstanceId | Format-Table -AutoSize | Out-String -Width 300

Write-Output "=== WMI root\wmi BatteryStatus ==="
try {
  Get-CimInstance -Namespace root\wmi -ClassName BatteryStatus |
    Select-Object InstanceName, RemainingCapacity, Charging, Discharging, Critical, PowerOnline |
    Format-Table -AutoSize | Out-String -Width 300
} catch { Write-Output "BatteryStatus: $_" }

Write-Output "=== WMI root\wmi BatteryStaticData ==="
try {
  Get-CimInstance -Namespace root\wmi -ClassName BatteryStaticData |
    Select-Object Tag, UniqueID, Chemistry, DesignCapacity, FullChargedCapacity, CycleCount |
    Format-Table -AutoSize | Out-String -Width 300
} catch { Write-Output "BatteryStaticData: $_" }

Write-Output "=== WinRT Battery devices (DeviceInformation + Battery.GetDeviceSelector) ==="
try {
  Add-Type -AssemblyName System.Runtime.WindowsRuntime
  $battType = [Windows.Devices.Power.Battery,Windows.Devices.Power,ContentType=WindowsRuntime]
  $sel = $battType::GetDeviceSelector()
  $col = [Windows.Devices.Enumeration.DeviceInformation,Windows.Devices.Enumeration,ContentType=WindowsRuntime]::FindAllAsync($sel).AsTask().Result
  foreach ($d in $col) {
    $b = $battType::FromIdAsync($d.Id).AsTask().Result
    $r = $b.GetReport()
    $pct = -1
    if ($r.FullChargeCapacityInMilliwattHours -gt 0) {
      $pct = [math]::Round(100.0 * $r.RemainingCapacityInMilliwattHours / $r.FullChargeCapacityInMilliwattHours)
    }
    [PSCustomObject]@{
      Name = $d.Name; Id = $d.Id; Pct = $pct
      Remaining_mWh = $r.RemainingCapacityInMilliwattHours
      Full_mWh = $r.FullChargeCapacityInMilliwattHours
      Status = $r.Status
    }
  } | Format-Table -AutoSize | Out-String -Width 400
} catch { Write-Output "WinRT battery enum failed: $_" }
