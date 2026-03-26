$path = 'D:\Development\enlil\enlil-platform\src\sync\mod.rs'
$lines = [System.IO.File]::ReadAllLines($path)
$out = New-Object System.Collections.Generic.List[string]
for ($i = 0; $i -lt $lines.Count; $i++) {
    # Skip the duplicate closing brace (line index 843, the extra "    }")
    if ($i -eq 843 -and $lines[$i] -eq '    }') {
        Write-Host "Removed duplicate } at line $($i+1)"
        continue
    }
    $out.Add($lines[$i])
}
[System.IO.File]::WriteAllLines($path, $out.ToArray())
Write-Host "Done"
