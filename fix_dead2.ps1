# Fix dead_code warnings on impl blocks
$root = "D:\Development\enlil\enlil-devices\src"

# 1. backend.rs - LoopbackBackend impl and PipeBackend impl
$f = "$root\net\backend.rs"
$c = Get-Content $f
$out = @()
foreach ($line in $c) {
    if ($line -eq 'impl LoopbackBackend {' -or $line -eq 'impl PipeBackend {') {
        $out += '#[allow(dead_code)]'
    }
    $out += $line
}
$out | Set-Content $f -Encoding UTF8
Write-Host "Fixed backend.rs impl blocks"

# 2. msi.rs - MsiCapability impl
$f = "$root\interrupt\msi.rs"
$c = Get-Content $f
$out = @()
foreach ($line in $c) {
    if ($line -eq 'impl MsiCapability {') {
        $out += '#[allow(dead_code)]'
    }
    $out += $line
}
$out | Set-Content $f -Encoding UTF8
Write-Host "Fixed msi.rs impl block"

# 3. clipboard.rs - ClipboardEntry impl
$f = "$root\bridge\clipboard.rs"
$c = Get-Content $f
$out = @()
foreach ($line in $c) {
    if ($line -eq 'impl ClipboardEntry {') {
        $out += '#[allow(dead_code)]'
    }
    $out += $line
}
$out | Set-Content $f -Encoding UTF8
Write-Host "Fixed clipboard.rs impl block"

Write-Host "Done"
