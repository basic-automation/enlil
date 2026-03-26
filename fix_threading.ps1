$path = 'D:\Development\enlil\enlil-platform\src\threading\mod.rs'
$lines = Get-Content $path

# Remove empty lines left from import removal (lines 11,12 are blank now)
$cleaned = @()
$prevBlank = $false
foreach ($line in $lines) {
    $isBlank = ($line.Trim() -eq '')
    if ($isBlank -and $prevBlank) { continue }
    $cleaned += $line
    $prevBlank = $isBlank
}

# Now work with cleaned lines
$out = @()
foreach ($line in $cleaned) {
    # Insert Default impl for Priority before Display impl
    if ($line -eq 'impl std::fmt::Display for Priority {') {
        $out += 'impl Default for Priority {'
        $out += '    fn default() -> Self {'
        $out += '        Self::Normal'
        $out += '    }'
        $out += '}'
        $out += ''
    }
    
    # Add PartialEq to CpuAffinity
    if ($line -eq '#[derive(Debug, Clone)]') {
        $out += '#[derive(Debug, Clone, PartialEq)]'
    } else {
        $out += $line
    }
}

$out | Set-Content $path -Encoding UTF8
Write-Host "Fixed $($out.Count) lines"
