$path = 'D:\Development\enlil\enlil-platform\src\sync\mod.rs'
$lines = [System.IO.File]::ReadAllLines($path)
# Line 839 (0-indexed 838): replace "let guard = lock.lock();"
# Line 840 (0-indexed 839): replace "let guard = cvar.wait_while(guard, ...)"
# Merge into one line and shift remaining up
$lines[838] = '        let guard = cvar.wait_while(lock.lock(), |started| !*started);'
$lines[839] = '        assert!(*guard);'
$lines[840] = '        drop(guard);'
$lines[841] = '        handle.join().unwrap();'
$lines[842] = '    }'
[System.IO.File]::WriteAllLines($path, $lines)
Write-Host "Fixed sync test"
