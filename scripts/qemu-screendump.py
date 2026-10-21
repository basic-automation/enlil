#!/usr/bin/env python3
"""Take a screenshot of a running QEMU through its monitor socket and save it as a PNG.

Usage: qemu-screendump.py <monitor-socket> <out.png>

The QEMU instance must have been started with `-monitor unix:<socket>,server,nowait`.
Exit status 0 on success, 1 if the monitor could not be reached or the dump failed.
"""
import os
import socket
import struct
import sys
import tempfile
import time
import zlib


def monitor(path, command, wait=0.5):
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(5)
    s.connect(path)
    time.sleep(0.2)
    s.recv(65536)  # banner
    s.sendall(command.encode() + b"\n")
    time.sleep(wait)
    try:
        s.recv(65536)
    except OSError:
        pass
    s.close()


def ppm_to_png(ppm, png):
    with open(ppm, "rb") as f:
        data = f.read()
    # P6 <width> <height> <maxval> <binary>
    fields, i = [], 0
    while len(fields) < 4:
        while data[i : i + 1].isspace():
            i += 1
        j = i
        while not data[j : j + 1].isspace():
            j += 1
        fields.append(data[i:j])
        i = j
    i += 1
    width, height = int(fields[1]), int(fields[2])
    raw = b"".join(
        b"\x00" + data[i + y * width * 3 : i + (y + 1) * width * 3] for y in range(height)
    )

    def chunk(tag, body):
        c = struct.pack(">I", len(body)) + tag + body
        return c + struct.pack(">I", zlib.crc32(tag + body) & 0xFFFFFFFF)

    with open(png, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(raw, 6)))
        f.write(chunk(b"IEND", b""))
    return width, height


def main():
    sock, out = sys.argv[1], sys.argv[2]
    ppm = os.path.join(tempfile.mkdtemp(dir=os.environ.get("TMPDIR")), "s.ppm")
    try:
        monitor(sock, f"screendump {ppm}", wait=1.0)
        w, h = ppm_to_png(ppm, out)
    except (OSError, ValueError, IndexError) as e:
        print(f"screendump failed: {e}", file=sys.stderr)
        return 1
    finally:
        try:
            os.unlink(ppm)
        except OSError:
            pass
    print(f"screenshot: {out} ({w}x{h})")
    return 0


sys.exit(main())
