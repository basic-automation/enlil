#!/usr/bin/env python3
"""Build a genuine RPM v4 package for enlil-bridge-agent without rpmbuild.

Writes: 96-byte lead + signature header + main header + gzip-compressed
cpio (newc) payload, per the RPM v4 package format.

Usage:
    build-rpm.py <version> <release> <binary> <outdir>
    build-rpm.py --self-test   # build to memory and parse it back
"""

import gzip
import hashlib
import io
import os
import socket
import struct
import sys
import time

# --- RPM header value types -----------------------------------------------
RPM_INT8 = 2
RPM_INT16 = 3
RPM_INT32 = 4
RPM_INT64 = 5
RPM_STRING = 6
RPM_BIN = 7
RPM_STRING_ARRAY = 8
RPM_I18NSTRING = 9

# --- signature tags --------------------------------------------------------
RPMSIGTAG_SIZE = 1000
RPMSIGTAG_MD5 = 1004
RPMSIGTAG_SHA1 = 269
RPMSIGTAG_SHA256 = 273
RPMSIGTAG_PAYLOADSIZE = 1007

# --- header tags -----------------------------------------------------------
RPMTAG_NAME = 1000
RPMTAG_VERSION = 1001
RPMTAG_RELEASE = 1002
RPMTAG_SUMMARY = 1004
RPMTAG_DESCRIPTION = 1005
RPMTAG_BUILDTIME = 1006
RPMTAG_BUILDHOST = 1007
RPMTAG_SIZE = 1009
RPMTAG_LICENSE = 1014
RPMTAG_ARCH = 1022
RPMTAG_OS = 1023
RPMTAG_FILESIZES = 1028
RPMTAG_FILEMODES = 1030
RPMTAG_FILERDEVS = 1033
RPMTAG_FILEMTIMES = 1034
RPMTAG_FILEDIGESTS = 1035
RPMTAG_FILELINKTOS = 1036
RPMTAG_FILEFLAGS = 1037
RPMTAG_FILEUSERNAME = 1039
RPMTAG_FILEGROUPNAME = 1040
RPMTAG_FILEVERIFYFLAGS = 1045
RPMTAG_PROVIDENAME = 1047
RPMTAG_PROVIDEFLAGS = 1112
RPMTAG_PROVIDEVERSION = 1113
RPMTAG_DIRINDEXES = 1116
RPMTAG_BASENAMES = 1117
RPMTAG_DIRNAMES = 1118
RPMTAG_PAYLOADFORMAT = 1124
RPMTAG_PAYLOADCOMPRESSOR = 1125
RPMTAG_PAYLOADFLAGS = 1126

RPMFILE_CONFIG = 1 << 0

SUMMARY = "In-guest agent for the Enlil inter-guest bridge"
DESCRIPTION = """The enlil-bridge-agent runs inside an Enlil guest and connects to the
host bridge over /dev/enlil-bridge (virtio-serial). It syncs the clipboard,
stages drag-and-drop file transfers, serves the shared filesystem and
forwards notifications between the guest and other guests on the bridge."""

_TYPE_ALIGN = {RPM_INT16: 2, RPM_INT32: 4, RPM_INT64: 8}
_TYPE_FMT = {RPM_INT8: "B", RPM_INT16: ">H", RPM_INT32: ">I", RPM_INT64: ">Q"}


def _encode_values(typ, values):
    if typ in (RPM_STRING, RPM_I18NSTRING):
        return values.encode() + b"\x00", 1
    if typ == RPM_STRING_ARRAY:
        return b"\x00".join(v.encode() for v in values) + b"\x00", len(values)
    if typ == RPM_BIN:
        return bytes(values), len(values)
    fmt = _TYPE_FMT[typ]
    return b"".join(struct.pack(fmt, v & (2 ** (struct.calcsize(fmt) * 8) - 1))
                    for v in values), len(values)


def encode_header(entries):
    """Encode an RPM header (signature or metadata) from (tag, type, values)."""
    data = bytearray()
    index = []
    for tag, typ, values in sorted(entries, key=lambda e: e[0]):
        align = _TYPE_ALIGN.get(typ, 1)
        while len(data) % align:
            data.append(0)
        blob, count = _encode_values(typ, values)
        index.append((tag, typ, len(data), count))
        data += blob
    out = bytearray(b"\x8e\xad\xe8\x01\x00\x00\x00\x00")
    out += struct.pack(">II", len(index), len(data))
    for tag, typ, offset, count in index:
        out += struct.pack(">IIII", tag, typ, offset, count)
    out += data
    while len(out) % 8:
        out.append(0)
    return bytes(out)


def cpio_newc(entries):
    """Build a cpio 'newc' (070701) archive.

    entries: list of (path, mode, uid, gid, mtime, data|None).
    `data=None` marks a directory entry.
    """
    out = bytearray()
    ino = 1
    for path, mode, uid, gid, mtime, data in entries:
        is_dir = data is None
        fmode = (mode | 0o040000) if is_dir else (mode | 0o100000)
        size = 0 if is_dir else len(data)
        name = path.encode() + b"\x00"
        fields = [ino, fmode, uid, gid, 1, mtime, size, 0, 0, 0, 0,
                  len(name), 0]
        header = b"070701" + b"".join(f"{f:08x}".encode() for f in fields)
        assert len(header) == 110
        out += header + name
        while len(out) % 4:
            out.append(0)
        if not is_dir:
            out += data
            while len(out) % 4:
                out.append(0)
        ino += 1
    # trailer
    name = b"TRAILER!!!\x00"
    fields = [0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, len(name), 0]
    header = b"070701" + b"".join(f"{f:08x}".encode() for f in fields)
    out += header + name
    while len(out) % 4:
        out.append(0)
    return bytes(out)


def build_rpm(version, release, binary_path, service_src, config_src):
    """Assemble the complete .rpm bytes."""
    now = int(time.time())
    buildhost = socket.gethostname()

    with open(binary_path, "rb") as f:
        binary = f.read()
    with open(service_src, "rb") as f:
        service = f.read()
    with open(config_src, "rb") as f:
        config = f.read()

    # (cpio path, mode, uid, gid, mtime, data|None)
    cpio_entries = [
        ("usr/sbin", 0o755, 0, 0, now, None),
        ("etc/enlil", 0o755, 0, 0, now, None),
        ("usr/lib/systemd/system", 0o755, 0, 0, now, None),
        ("usr/sbin/enlil-bridge-agent", 0o755, 0, 0, now, binary),
        ("etc/enlil/bridge-agent.toml", 0o644, 0, 0, now, config),
        ("usr/lib/systemd/system/enlil-bridge-agent.service",
         0o644, 0, 0, now, service),
    ]
    payload_raw = cpio_newc(cpio_entries)
    payload = gzip.compress(payload_raw, compresslevel=9)

    # RPM file list (new-style DIRNAMES/BASENAMES/DIRINDEXES).
    dirnames = ["/usr/", "/etc/", "/usr/lib/systemd/",
                "/usr/sbin/", "/etc/enlil/", "/usr/lib/systemd/system/"]
    diridx = {d: i for i, d in enumerate(dirnames)}
    # (dirname, basename, mode, size, digest, flags, mtime)
    files = [
        ("/usr/", "sbin", 0o040755, 0, "", 0, now),
        ("/etc/", "enlil", 0o040755, 0, "", 0, now),
        ("/usr/lib/systemd/", "system", 0o040755, 0, "", 0, now),
        ("/usr/sbin/", "enlil-bridge-agent", 0o100755, len(binary),
         hashlib.md5(binary).hexdigest(), 0, now),
        ("/etc/enlil/", "bridge-agent.toml", 0o100644, len(config),
         hashlib.md5(config).hexdigest(), RPMFILE_CONFIG, now),
        ("/usr/lib/systemd/system/", "enlil-bridge-agent.service",
         0o100644, len(service),
         hashlib.md5(service).hexdigest(), 0, now),
    ]
    n = len(files)
    installed_size = sum(f[3] for f in files)

    header_entries = [
        (RPMTAG_NAME, RPM_STRING, "enlil-bridge-agent"),
        (RPMTAG_VERSION, RPM_STRING, version),
        (RPMTAG_RELEASE, RPM_STRING, release),
        (RPMTAG_SUMMARY, RPM_I18NSTRING, SUMMARY),
        (RPMTAG_DESCRIPTION, RPM_I18NSTRING, DESCRIPTION),
        (RPMTAG_BUILDTIME, RPM_INT32, [now]),
        (RPMTAG_BUILDHOST, RPM_STRING, buildhost),
        (RPMTAG_SIZE, RPM_INT32, [installed_size]),
        (RPMTAG_LICENSE, RPM_STRING, "MIT"),
        (RPMTAG_ARCH, RPM_STRING, "x86_64"),
        (RPMTAG_OS, RPM_STRING, "linux"),
        (RPMTAG_PAYLOADFORMAT, RPM_STRING, "cpio"),
        (RPMTAG_PAYLOADCOMPRESSOR, RPM_STRING, "gzip"),
        (RPMTAG_PAYLOADFLAGS, RPM_STRING, "9"),
        (RPMTAG_DIRNAMES, RPM_STRING_ARRAY, dirnames),
        (RPMTAG_BASENAMES, RPM_STRING_ARRAY, [f[1] for f in files]),
        (RPMTAG_DIRINDEXES, RPM_INT32, [diridx[f[0]] for f in files]),
        (RPMTAG_FILESIZES, RPM_INT32, [f[3] for f in files]),
        (RPMTAG_FILEMODES, RPM_INT16, [f[2] for f in files]),
        (RPMTAG_FILERDEVS, RPM_INT16, [0] * n),
        (RPMTAG_FILEMTIMES, RPM_INT32, [f[6] for f in files]),
        (RPMTAG_FILEDIGESTS, RPM_STRING_ARRAY, [f[4] for f in files]),
        (RPMTAG_FILELINKTOS, RPM_STRING_ARRAY, [""] * n),
        (RPMTAG_FILEFLAGS, RPM_INT32, [f[5] for f in files]),
        (RPMTAG_FILEUSERNAME, RPM_STRING_ARRAY, ["root"] * n),
        (RPMTAG_FILEGROUPNAME, RPM_STRING_ARRAY, ["root"] * n),
        (RPMTAG_FILEVERIFYFLAGS, RPM_INT32, [0xFFFFFFFF] * n),
        (RPMTAG_PROVIDENAME, RPM_STRING_ARRAY,
         [f"enlil-bridge-agent = {version}-{release}"]),
        (RPMTAG_PROVIDEFLAGS, RPM_INT32, [8]),  # RPMSENSE_EQUAL
        (RPMTAG_PROVIDEVERSION, RPM_STRING_ARRAY, [f"{version}-{release}"]),
    ]
    header = encode_header(header_entries)

    sig_entries = [
        (RPMSIGTAG_SIZE, RPM_INT32, [len(header) + len(payload)]),
        (RPMSIGTAG_MD5, RPM_BIN, hashlib.md5(header + payload).digest()),
        (RPMSIGTAG_SHA1, RPM_STRING,
         hashlib.sha1(header + payload).hexdigest()),
        (RPMSIGTAG_SHA256, RPM_STRING,
         hashlib.sha256(header + payload).hexdigest()),
        (RPMSIGTAG_PAYLOADSIZE, RPM_INT32, [len(payload_raw)]),
    ]
    sig_header = encode_header(sig_entries)

    lead = bytearray(b"\xed\xab\xee\xdb")   # magic
    lead += b"\x03\x00"                    # major, minor version (1 byte each)
    lead += struct.pack(">H", 0)            # type: binary
    lead += struct.pack(">H", 1)            # archnum: x86_64
    lead += b"enlil-bridge-agent" + b"\x00" * (66 - len("enlil-bridge-agent"))
    lead += struct.pack(">H", 1)            # osnum: Linux
    lead += struct.pack(">H", 5)            # RPMSIGTYPE_HEADERSIG
    lead += b"\x00" * 16                    # reserved
    assert len(lead) == 96

    return bytes(lead) + sig_header + header + payload


# --- self-test: parse the rpm back ------------------------------------------

def _parse_header(buf, off):
    assert buf[off:off + 4] == b"\x8e\xad\xe8\x01", "bad header magic"
    count, data_len = struct.unpack(">II", buf[off + 8:off + 16])
    entries = {}
    pos = off + 16
    index = []
    for _ in range(count):
        tag, typ, offset, cnt = struct.unpack(">IIII", buf[pos:pos + 16])
        index.append((tag, typ, offset, cnt))
        pos += 16
    data = buf[pos:pos + data_len]
    for tag, typ, offset, cnt in index:
        if typ in (RPM_STRING, RPM_I18NSTRING):
            end = data.index(b"\x00", offset)
            entries[tag] = data[offset:end].decode()
        elif typ == RPM_STRING_ARRAY:
            raw = data[offset:]
            vals, cur = [], bytearray()
            for b in raw:
                if b == 0:
                    vals.append(bytes(cur).decode())
                    cur = bytearray()
                    if len(vals) == cnt:
                        break
                else:
                    cur.append(b)
            entries[tag] = vals
        elif typ == RPM_BIN:
            entries[tag] = data[offset:offset + cnt]
        else:
            fmt = _TYPE_FMT[typ]
            size = struct.calcsize(fmt)
            entries[tag] = [struct.unpack(fmt, data[offset + i * size:
                                                   offset + (i + 1) * size])[0]
                            for i in range(cnt)]
    total = 16 + count * 16 + data_len
    total += (-total) % 8
    return entries, off + total


def _parse_cpio_names(payload_gz):
    raw = gzip.decompress(payload_gz)
    names, pos = [], 0
    while pos + 110 <= len(raw):
        assert raw[pos:pos + 6] == b"070701", "bad cpio magic"
        fields = [int(raw[pos + 6 + i * 8:pos + 14 + i * 8], 16)
                  for i in range(13)]
        namesize, filesize = fields[11], fields[6]
        name = raw[pos + 110:pos + 110 + namesize].rstrip(b"\x00").decode()
        if name == "TRAILER!!!":
            break
        names.append(name)
        # header+name and data are each padded to a 4-byte boundary
        pos += ((110 + namesize + 3) // 4) * 4
        pos += ((filesize + 3) // 4) * 4
    return names


def self_test():
    rpm = build_rpm("0.1.0", "1", "/bin/true", "/bin/true", "/bin/true")
    assert rpm[:4] == b"\xed\xab\xee\xdb", "bad lead magic"
    assert len(rpm) >= 96 + 16
    sig, off = _parse_header(rpm, 96)
    hdr, off = _parse_header(rpm, off)
    payload = rpm[off:]
    # signature digests cover header+payload; recompute the header span
    hdr_off = 96
    _, hdr_off = _parse_header(rpm, hdr_off)
    header_bytes = rpm[hdr_off:off]
    assert sig[RPMSIGTAG_MD5] == hashlib.md5(header_bytes + payload).digest()
    assert sig[RPMSIGTAG_SHA256] == hashlib.sha256(
        header_bytes + payload).hexdigest()
    assert sig[RPMSIGTAG_PAYLOADSIZE] == [len(gzip.decompress(payload))]
    assert hdr[RPMTAG_NAME] == "enlil-bridge-agent"
    assert hdr[RPMTAG_VERSION] == "0.1.0"
    assert hdr[RPMTAG_ARCH] == "x86_64"
    assert hdr[RPMTAG_PAYLOADCOMPRESSOR] == "gzip"
    names = _parse_cpio_names(payload)
    for want in ("usr/sbin/enlil-bridge-agent",
                 "etc/enlil/bridge-agent.toml",
                 "usr/lib/systemd/system/enlil-bridge-agent.service"):
        assert want in names, f"missing {want} in payload"
    # file list consistency
    assert len(hdr[RPMTAG_BASENAMES]) == len(hdr[RPMTAG_DIRINDEXES])
    assert len(hdr[RPMTAG_BASENAMES]) == len(hdr[RPMTAG_FILEDIGESTS])
    print(f"self-test OK: {len(rpm)} bytes, "
          f"{len(hdr[RPMTAG_BASENAMES])} files, payload {names}")


def main(argv):
    if argv[1:2] == ["--self-test"]:
        self_test()
        return 0
    version, release, binary, outdir = argv[1], argv[2], argv[3], argv[4]
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    service = os.path.join(root, "bridge-agent", "packaging",
                           "deb", "enlil-bridge-agent.service")
    config = os.path.join(root, "bridge-agent", "packaging",
                          "deb", "bridge-agent.toml")
    rpm = build_rpm(version, release, binary, service, config)
    name = f"enlil-bridge-agent-{version}-{release}.x86_64.rpm"
    os.makedirs(outdir, exist_ok=True)
    with open(os.path.join(outdir, name), "wb") as f:
        f.write(rpm)
    print(f"wrote {outdir}/{name} ({len(rpm)} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
