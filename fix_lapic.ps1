$f = 'D:\Development\enlil\enlil-devices\src\interrupt\lapic.rs'
$lines = Get-Content $f
# Line 270 (0-indexed 269): add "let _ = " before "self.accept_interrupt"
$lines[269] = $lines[269] -replace '(\s+)self\.accept_interrupt', '$1let _ = self.accept_interrupt'
# Line 425 (0-indexed 424)
$lines[424] = $lines[424] -replace '(\s+)self\.accept_interrupt', '$1let _ = self.accept_interrupt'
# Line 457 (0-indexed 456)
$lines[456] = $lines[456] -replace '(\s+)self\.accept_interrupt', '$1let _ = self.accept_interrupt'
Set-Content $f $lines
