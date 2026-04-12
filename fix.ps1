$f = 'D:\Development\enlil\enlil-devices\src\interrupt\msi.rs'
$c = Get-Content $f -Raw
$c = $c.Replace('u32::from(self.masked)', 'self.masked as u32')
Set-Content $f $c -NoNewline
