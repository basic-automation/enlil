$f = 'D:\Development\enlil\enlil-devices\src\interrupt\ioapic.rs'
$c = Get-Content $f -Raw
$c = $c -replace 'mmio_write\(IOREGSEL, u64::from\(', 'mmio_write(IOREGSEL, u32::from('
Set-Content $f $c -NoNewline
