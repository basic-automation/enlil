$files = @(
    'src\kvm_backend.rs',
    'src\exit_handler.rs',
    'src\acpi.rs',
    'src\smbios.rs',
    'src\timing_stealth.rs',
    'src\lib.rs',
    'src\vcpu.rs',
    'src\vm.rs',
    'src\error.rs',
    'src\ept.rs',
    'src\memory.rs',
    'src\serial.rs',
    'src\cpuid.rs',
    'src\vtpm.rs',
    'Cargo.toml'
)

Set-Location 'D:\Development\enlil\enlil-core'

foreach ($f in $files) {
    Write-Output '==============================================================================='
    Write-Output "FILE: $f"
    Write-Output '==============================================================================='
    Get-Content $f -Raw
    Write-Output ''
    Write-Output ''
}
