$f = 'D:\Development\enlil\enlil-devices\src\virtio\transport.rs'
$c = Get-Content $f -Raw
$c = $c.Replace('pub const fn queue(&self, index: usize)', 'pub fn queue(&self, index: usize)')
Set-Content $f $c -NoNewline
