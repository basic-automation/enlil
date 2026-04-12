$base = "D:\Development\enlil\enlil-devices\src"

# --- display/mod.rs: add #[allow] on FrameRef::new (lines 46) ---
$f = "$base\display\mod.rs"
$lines = Get-Content $f
$out = @()
for ($i = 0; $i -lt $lines.Count; $i++) {
    # Add allow above FrameRef::new
    if ($lines[$i] -match '^\s+pub fn new\(width: u32, height: u32, pixel_format: PixelFormat\)' -and $i -gt 0 -and $lines[$i-1] -notmatch 'allow') {
        $out += "    #[allow(clippy::cast_possible_truncation)]"
    }
    # Fix drop(frames) - move outside if block
    if ($lines[$i] -match '^\s+drop\(frames\);$' -and $i -gt 0 -and $lines[$i-1] -match '^\s+frames\.pop_front') {
        # Skip this misplaced drop, we'll add it after the if block
        continue
    }
    # After the if block closing brace in submit_frame, add drop
    if ($lines[$i] -match '^\s+\}$' -and $i -gt 0 -and $lines[$i-1] -match 'drop\(frames\)') {
        # Already handled above
        $out += $lines[$i]
        continue
    }
    $out += $lines[$i]
}
# Re-read and fix submit_frame drop placement properly
$content = $out -join "`n"
# In submit_frame, after the if block that does pop_front, add drop(frames)
$content = $content -replace '(frames\.pop_front\(\);\s*\})\s*\n(\s*\*self\.current_frame)', "`$1`n        drop(frames);`n`$2"
# Fix composite_frame - add allow above it
$content = $content -replace '(\s*/// # Panics\s*\n\s*/// Panics if an internal lock is poisoned\.\s*\n\s*#\[must_use\]\s*\n\s*pub fn composite_frame)', "    #[allow(clippy::significant_drop_tightening)]`n`$1"
Set-Content $f $content -NoNewline
Write-Host "Fixed display/mod.rs"

# --- net/backend.rs: fix option_if_let_else ---
$f = "$base\net\backend.rs"
$c = Get-Content $f -Raw
$c = $c -replace 'if let Some\(frame\) = value \{\s*\n\s*let len = frame\.len\(\)\.min\(buf\.len\(\)\);\s*\n\s*buf\[\.\.len\]\.copy_from_slice\(&frame\[\.\.len\]\);\s*\n\s*Ok\(len\)\s*\n\s*\} else \{\s*\n\s*Ok\(0\)\s*\n\s*\}', 'value.map_or(Ok(0), |frame| {
            let len = frame.len().min(buf.len());
            buf[..len].copy_from_slice(&frame[..len]);
            Ok(len)
        })'
Set-Content $f $c -NoNewline
Write-Host "Fixed net/backend.rs"

# --- net/device.rs: const fn, let-else, match_same_arms ---
$f = "$base\net\device.rs"
$c = Get-Content $f -Raw
$c = $c -replace 'pub fn activate\(', 'pub const fn activate('
$c = $c -replace 'let desc = match self\.tx_queue\.pop_available\(\) \{\s*\n\s*Ok\(d\) => d,\s*\n\s*Err\(_\) => break,\s*\n\s*\};', 'let Ok(desc) = self.tx_queue.pop_available() else { break };'
$c = $c -replace 'let mut desc = match self\.rx_queue\.pop_available\(\) \{\s*\n\s*Ok\(d\) => d,\s*\n\s*Err\(_\) => break,\s*\n\s*\};', 'let Ok(mut desc) = self.rx_queue.pop_available() else { break };'
$c = $c -replace 'Ok\(0\) => break,\s*\n\s*Ok\(n\) =>', 'Ok(0) | Err(_) => break,' + "`n                Ok(n) =>"
# Remove the now-duplicate Err(_) arm
$c = $c -replace '\n\s*Err\(_\) => break,\s*\n\s*\}', "`n                }"
Set-Content $f $c -NoNewline
Write-Host "Fixed net/device.rs"

# --- net/header.rs: const fn gso ---
$f = "$base\net\header.rs"
$c = Get-Content $f -Raw
$c = $c -replace 'pub fn gso\(', 'pub const fn gso('
Set-Content $f $c -NoNewline
Write-Host "Fixed net/header.rs"

# --- net/switch.rs: rename fields, allow cast, fix lookup ---
$f = "$base\net\switch.rs"
$c = Get-Content $f -Raw
$c = $c -replace 'pub frames_in:', 'pub received:'
$c = $c -replace 'pub frames_forwarded:', 'pub forwarded:'
$c = $c -replace 'pub frames_flooded:', 'pub flooded:'
$c = $c -replace 'pub frames_dropped:', 'pub dropped:'
$c = $c -replace 'self\.stats\.frames_in', 'self.stats.received'
$c = $c -replace 'self\.stats\.frames_forwarded', 'self.stats.forwarded'
$c = $c -replace 'self\.stats\.frames_flooded', 'self.stats.flooded'
$c = $c -replace 'self\.stats\.frames_dropped', 'self.stats.dropped'
# Add allow for cast_possible_truncation on add_port
$c = $c -replace '(pub fn add_port\(&mut self)', '#[allow(clippy::cast_possible_truncation)]' + "`n    `$1"
# Fix lookup to take MacAddress by value
$c = $c -replace 'fn lookup\(&self, mac: &MacAddress\)', 'fn lookup(&self, mac: MacAddress)'
$c = $c -replace 'self\.fdb\.get\(mac\)', 'self.fdb.get(&mac)'
Set-Content $f $c -NoNewline
Write-Host "Fixed net/switch.rs"

# --- net/virtqueue.rs: finish_non_exhaustive ---
$f = "$base\net\virtqueue.rs"
$c = Get-Content $f -Raw
$c = $c -replace '\.finish\(\)', '.finish_non_exhaustive()'
Set-Content $f $c -NoNewline
Write-Host "Fixed net/virtqueue.rs"

# --- timer/paravirt.rs: const fn compute_scale ---
$f = "$base\timer\paravirt.rs"
$c = Get-Content $f -Raw
$c = $c -replace 'pub fn compute_scale\(', 'pub const fn compute_scale('
Set-Content $f $c -NoNewline
Write-Host "Fixed timer/paravirt.rs"

Write-Host "All fixes applied"
