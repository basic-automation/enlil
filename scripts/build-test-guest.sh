#!/usr/bin/env bash
# Build the assets for a real Linux guest on the bare-metal enlil kernel:
#   <out>/vmlinuz      a bzImage (default: this machine's running kernel)
#   <out>/initrd.img   a tiny initramfs whose static /init prints markers
#   <out>/cmdline.txt  the guest kernel command line
#
# Nothing here needs root. The kernel image is copied, never built.
#
# Environment:
#   ENLIL_GUEST_KERNEL   path to a bzImage (default /usr/lib/modules/$(uname -r)/vmlinuz)
#
# Usage: scripts/build-test-guest.sh [output-dir]
#
# Exit codes: 0 built; 1 failed; 2 SKIPPED (no kernel image or no C compiler).
set -euo pipefail

OUT="${1:-$(mktemp -d)}"
KERNEL="${ENLIL_GUEST_KERNEL:-/usr/lib/modules/$(uname -r)/vmlinuz}"

if [ ! -r "$KERNEL" ]; then
    echo "SKIP: no readable guest kernel at $KERNEL (set ENLIL_GUEST_KERNEL)"
    exit 2
fi
for t in gcc cpio gzip; do
    command -v "$t" >/dev/null 2>&1 || { echo "SKIP: $t not installed"; exit 2; }
done

# A bzImage has the boot flag at 0x1FE and "HdrS" at 0x202.
if [ "$(dd if="$KERNEL" bs=1 skip=$((0x202)) count=4 status=none)" != "HdrS" ]; then
    echo "FAIL: $KERNEL is not a Linux bzImage (no HdrS magic)"
    exit 1
fi

mkdir -p "$OUT"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/init.c" <<'C'
/* PID 1 of the enlil test guest: prove the kernel booted, the console works,
 * timers fire and the scheduler runs, then power off. */
#include <fcntl.h>
#include <stdio.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/utsname.h>
#include <unistd.h>

int main(void)
{
    struct utsname u;
    int i;

    mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
    int fd = open("/dev/console", O_RDWR);
    if (fd >= 0) {
        dup2(fd, 0);
        dup2(fd, 1);
        dup2(fd, 2);
    }
    uname(&u);
    printf("ENLIL-GUEST-INIT: hello from %s %s (%s)\n", u.sysname, u.release, u.machine);
    fflush(stdout);
    for (i = 1; i <= 3; i++) {
        sleep(1); /* needs a working clock event + scheduler */
        printf("ENLIL-GUEST-TICK %d\n", i);
        fflush(stdout);
    }
    printf("ENLIL-GUEST-INIT-OK\n");
    fflush(stdout);
    sync();
    reboot(RB_POWER_OFF);
    for (;;)
        pause();
}
C

gcc -static -O2 -o "$WORK/init" "$WORK/init.c"

ROOT="$WORK/root"
mkdir -p "$ROOT/dev" "$ROOT/proc" "$ROOT/sys"
cp "$WORK/init" "$ROOT/init"
chmod 755 "$ROOT/init"
( cd "$ROOT" && find . | cpio --quiet -o -H newc | gzip -9 ) > "$OUT/initrd.img"

cp "$KERNEL" "$OUT/vmlinuz"
# Legacy-PIC machine: no ACPI/APIC/PCI. earlycon prints from the first
# instruction; console=ttyS0 carries init's output. The hypervisor appends
# tsc_early_khz so the guest does not calibrate against hardware it cannot see.
printf '%s\n' \
    'console=ttyS0,115200 earlycon=uart8250,io,0x3f8 rdinit=/init acpi=off noapic nolapic nokaslr panic=-1 loglevel=7' \
    > "$OUT/cmdline.txt"

echo "built guest assets in $OUT:"
ls -l "$OUT/vmlinuz" "$OUT/initrd.img" "$OUT/cmdline.txt" | awk '{print "  " $5 "  " $9}'
