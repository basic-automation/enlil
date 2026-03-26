# Fix duplicate #[allow(dead_code)] and mangled lines in enlil-devices

function Fix-DuplicateAllow {
    param([string]$Path)
    $lines = [System.IO.File]::ReadAllLines($Path)
    $out = [System.Collections.Generic.List[string]]::new()
    $prev = ""
    foreach ($line in $lines) {
        $trimmed = $line.Trim()
        $prevTrimmed = $prev.Trim()
        # Skip duplicate #[allow(dead_code)]
        if ($trimmed -eq '#[allow(dead_code)]' -and $prevTrimmed -eq '#[allow(dead_code)]') {
            continue
        }
        $out.Add($line)
        $prev = $line
    }
    [System.IO.File]::WriteAllLines($Path, $out.ToArray())
    Write-Host "Fixed $Path ($($lines.Count) -> $($out.Count) lines)"
}

# Fix header.rs mangled line
$headerPath = "D:\Development\enlil\enlil-devices\src\net\header.rs"
$hlines = [System.IO.File]::ReadAllLines($headerPath)
$hout = [System.Collections.Generic.List[string]]::new()
foreach ($line in $hlines) {
    if ($line -match '(.+);    #\[allow\(dead_code\)\]') {
        $hout.Add($matches[1] + ";")
    } else {
        $hout.Add($line)
    }
}
[System.IO.File]::WriteAllLines($headerPath, $hout.ToArray())
Write-Host "Fixed header.rs mangled line"

# Fix duplicates in all affected files
Fix-DuplicateAllow "D:\Development\enlil\enlil-devices\src\bridge\clipboard.rs"
Fix-DuplicateAllow "D:\Development\enlil\enlil-devices\src\interrupt\ioapic.rs"
Fix-DuplicateAllow "D:\Development\enlil\enlil-devices\src\interrupt\msi.rs"
Fix-DuplicateAllow "D:\Development\enlil\enlil-devices\src\net\backend.rs"
Fix-DuplicateAllow "D:\Development\enlil\enlil-devices\src\net\device.rs"
Fix-DuplicateAllow "D:\Development\enlil\enlil-devices\src\net\header.rs"

Write-Host "All done"
