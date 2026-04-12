Set-Location D:\Development\enlil
$content = git show "a03db83:enlil-mgmt/src/tui/mod.rs"
$content | Set-Content -Path "D:\Development\enlil\tui_mod_backup.rs" -Encoding UTF8
Write-Host "Lines: $($content.Count)"
Write-Host "Written to tui_mod_backup.rs"
