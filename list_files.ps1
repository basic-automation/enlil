$lines = Get-Content "D:\Development\enlil\clippy_output.txt" | Where-Object { $_ -match '^\s+-->' }
$files = @{}
foreach($l in $lines) {
    $f = ($l.Trim() -replace '--> ','') -replace ':.*$',''
    if($files.ContainsKey($f)){$files[$f]++}else{$files[$f]=1}
}
$files.GetEnumerator() | Sort-Object Value -Descending | Format-Table -AutoSize
