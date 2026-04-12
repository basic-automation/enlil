Get-ChildItem -Recurse D:\Development\enlil\enlil-devices\src\*.rs | Where-Object { $_.Length -lt 50 } | ForEach-Object { Write-Host "$($_.FullName) $($_.Length)" }
