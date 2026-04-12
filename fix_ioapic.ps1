$f = 'D:\Development\enlil\enlil-devices\src\interrupt\ioapic.rs'
$c = Get-Content $f -Raw
$c = $c -replace 'u32::from\(self\.destination\) << 24', '(self.destination as u32) << 24'
$c = $c -replace 'self\.entries\[usize::from\(irq\)\]', 'self.entries[irq as usize]'
Set-Content $f $c -NoNewline
