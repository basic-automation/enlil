$content = [System.IO.File]::ReadAllLines("D:\Development\enlil\enlil-devices\src\usb\routing.rs")
$num = 1
foreach ($line in $content) {
    Write-Output ("{0,4}  {1}" -f $num, $line)
    $num++
}
