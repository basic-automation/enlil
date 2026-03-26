$base = "D:\Development\enlil\enlil-devices\src"

function Insert-Before($file, $lineNum, $text) {
    $c = Get-Content $file
    $out = @()
    for ($i = 0; $i -lt $c.Count; $i++) {
        if ($i -eq ($lineNum - 1)) {
            $out += $text
        }
        $out += $c[$i]
    }
    $out | Set-Content $file -Encoding UTF8
    Write-Host "Inserted at line $lineNum in $file"
}

# 1. net\header.rs line 39: RSC_INFO
Insert-Before "$base\net\header.rs" 39 "    #[allow(dead_code)]"

# 2. interrupt\msi.rs line 66: MsiCapability struct
Insert-Before "$base\interrupt\msi.rs" 65 "#[allow(dead_code)]"

# 3. net\backend.rs line 97: LoopbackBackend, line 146+1=147: PipeBackend
# Do PipeBackend first (higher line) then LoopbackBackend
$c = Get-Content "$base\net\backend.rs"
$out = @()
for ($i = 0; $i -lt $c.Count; $i++) {
    if ($c[$i] -match '^pub struct LoopbackBackend' -or $c[$i] -match '^pub struct PipeBackend') {
        $out += "#[allow(dead_code)]"
    }
    $out += $c[$i]
}
$out | Set-Content "$base\net\backend.rs" -Encoding UTF8
Write-Host "Fixed backend.rs"

# 4. interrupt\ioapic.rs line 9: IOAPIC_BASE
Insert-Before "$base\interrupt\ioapic.rs" 9 "#[allow(dead_code)]"

# 5. net\device.rs line 16: DeviceStatus enum
Insert-Before "$base\net\device.rs" 15 "#[allow(dead_code)]"

# 6. bridge\clipboard.rs line 72: ClipboardEntry struct
Insert-Before "$base\bridge\clipboard.rs" 71 "#[allow(dead_code)]"

Write-Host "Done"
