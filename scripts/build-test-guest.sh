#!/usr/bin/env bash
# Build the assets for a real Linux guest on the bare-metal enlil kernel:
#   <out>/vmlinuz      a bzImage (default: this machine's running kernel)
#   <out>/initrd.img   a tiny initramfs whose static /init prints markers
#   <out>/cmdline.txt  the guest kernel command line
#   <out>/vdisk.img    a 64 MiB test disk (the host side of the guest's virtual disk)
#   <out>/disk.txt     the disk assignment: which host disk (by NVMe serial) and
#                      which sector ranges the guest may use
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
 * timers fire and the scheduler runs, the virtual disk works and is confined
 * to its allow-list, then power off. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/fs.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/utsname.h>
#include <unistd.h>

#define SECTOR 512
/* Must match the allow-list in disk.txt: 0-31 ro, 32-63 rw, the rest denied. */
#define RO_SECTOR 8
#define RW_FIRST 32
#define RW_COUNT 16
#define DENIED_SECTOR 64

static int fails;

static void check(int ok, const char *what)
{
    if (ok) {
        printf("ENLIL-VDISK: %s\n", what);
    } else {
        printf("ENLIL-VDISK-FAIL: %s (errno %d)\n", what, errno);
        fails++;
    }
    fflush(stdout);
}

static int zeros(const unsigned char *b, size_t n)
{
    while (n--)
        if (*b++)
            return 0;
    return 1;
}

static void vdisk_test(void)
{
    int fd, i;
    unsigned long long bytes = 0;
    unsigned char *buf, *ref;

    /* The virtio-pci driver probes during boot; allow a moment anyway. */
    for (i = 0; i < 30; i++) {
        fd = open("/dev/vda", O_RDWR | O_DIRECT | O_SYNC);
        if (fd >= 0)
            break;
        usleep(100000);
    }
    if (fd < 0) {
        check(0, "no /dev/vda");
        return;
    }
    if (posix_memalign((void **)&buf, 4096, 64 * 1024) ||
        posix_memalign((void **)&ref, 4096, 64 * 1024)) {
        check(0, "out of memory");
        return;
    }
    check(ioctl(fd, BLKGETSIZE64, &bytes) == 0 && bytes == 64ULL * 1024 * 1024,
          "capacity is 64 MiB");

    /* Sector 0 holds the host's marker: a real read through the device. */
    check(pread(fd, buf, SECTOR, 0) == SECTOR && !memcmp(buf, "ENLIL-VDISK-MAGIC", 17),
          "sector 0 holds the host marker");

    /* Multi-sector write + read-back in the writable range, distinct pattern
     * per sector so any misplaced or merged sector is caught. */
    for (i = 0; i < RW_COUNT * SECTOR; i++)
        ref[i] = (unsigned char)(i * 7 + i / SECTOR + 1);
    check(pwrite(fd, ref, RW_COUNT * SECTOR, (off_t)RW_FIRST * SECTOR) == RW_COUNT * SECTOR,
          "wrote 16 sectors to the writable range");
    memset(buf, 0, RW_COUNT * SECTOR);
    check(pread(fd, buf, RW_COUNT * SECTOR, (off_t)RW_FIRST * SECTOR) == RW_COUNT * SECTOR &&
              !memcmp(buf, ref, RW_COUNT * SECTOR),
          "read the 16 sectors back intact");

    /* The read-only range reads real data but refuses writes. */
    check(pread(fd, buf, SECTOR, (off_t)RO_SECTOR * SECTOR) == SECTOR &&
              !memcmp(buf, "RO-ORIGINAL", 11),
          "read-only range is readable");
    memset(buf, 0xEE, SECTOR);
    errno = 0;
    check(pwrite(fd, buf, SECTOR, (off_t)RO_SECTOR * SECTOR) < 0, "write to the read-only range rejected");

    /* Outside the allow-list: reads as zeros, writes refused. */
    memset(buf, 0xFF, SECTOR);
    check(pread(fd, buf, SECTOR, (off_t)DENIED_SECTOR * SECTOR) == SECTOR && zeros(buf, SECTOR),
          "denied sector reads as zeros");
    memset(buf, 0xEE, SECTOR);
    errno = 0;
    check(pwrite(fd, buf, SECTOR, (off_t)DENIED_SECTOR * SECTOR) < 0, "write to a denied sector rejected");

    check(fsync(fd) == 0, "flush");
    close(fd);
    if (!fails)
        printf("ENLIL-VDISK-OK\n");
    fflush(stdout);
}

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
    vdisk_test();
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
# pci=conf1: use the legacy config mechanism without requiring a chipset
# vendor/class sanity check (the virtual machine has no host bridge yet).
printf '%s\n' \
    'console=ttyS0,115200 earlycon=uart8250,io,0x3f8 rdinit=/init acpi=off noapic nolapic pci=conf1 nokaslr panic=-1 loglevel=7' \
    > "$OUT/cmdline.txt"

# The host side of the guest's virtual disk: 64 MiB with three marked sectors
# the guest will (or must not) touch, and the allow-list that confines it.
truncate -s 64M "$OUT/vdisk.img"
put_sector() { printf '%s' "$2" | dd of="$OUT/vdisk.img" bs=512 seek="$1" conv=notrunc status=none; }
put_sector 0 'ENLIL-VDISK-MAGIC'
put_sector 8 'RO-ORIGINAL'
put_sector 64 'DENIED-ORIGINAL'
cat > "$OUT/disk.txt" <<'D'
# The guest's virtual disk: carved from the NVMe with this serial number.
nvme-serial = enlilvdisk
allow = 0-31 ro     # sectors 0..=31: readable, never writable
allow = 32-63 rw    # sectors 32..=63: read-write
# every other sector is denied: reads return zeros, writes are refused
D

echo "built guest assets in $OUT:"
ls -l "$OUT/vmlinuz" "$OUT/initrd.img" "$OUT/cmdline.txt" "$OUT/vdisk.img" "$OUT/disk.txt" | awk '{print "  " $5 "  " $9}'
