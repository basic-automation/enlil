# bridge-agent

In-guest agent for the Enlil inter-guest bridge (clipboard, drag-and-drop,
shared filesystem, notifications). It runs inside a guest OS and talks to the
host bridge over a byte-stream channel:

- `/dev/enlil-bridge` — guest device node exposed by the hypervisor (default)
- `unix:///run/enlil-bridge.sock` — Unix-domain socket
- `tcp://host:port` — TCP (development / testing)

## Protocol

Frames are byte-compatible with `enlil-devices::bridge::transport`: a fixed
32-byte little-endian header (src u16, dst u16, channel u8, reserved u8,
payload length u32, sequence u64, flags u8, 13 bytes padding) followed by the
payload. Each channel payload starts with a 1-byte message kind; integers
are little-endian, strings are `u16`-length-prefixed UTF-8. See
`src/protocol.rs` for the per-channel message layouts.

Channels: `Clipboard` (0), `DragDrop` (1), `Notify` (2), `SharedFs` (3),
`FastNet` (4), `UrlRoute` (5), `ControlTx` (6), `ControlRx` (7).

## Running

```sh
enlil-bridge-agent --config /etc/enlil/bridge-agent.toml run
enlil-bridge-agent send-text "hello" --to 0   # one-shot clipboard push
enlil-bridge-agent doctor                     # check config + channel
```

## Hypervisor-detection agent (`stealth-check`)

The agent doubles as the in-guest automated detection suite (roadmap item
5.8): `bridge-agent stealth-check` runs the pafish/al-khaser-style checklist
from inside the guest — CPUID hypervisor-present bit, `0x40000000` vendor
signature, leaf-0 CPU vendor genuineness, RDTSC-bracketed CPUID timing,
PCI device-ID enumeration, NIC MAC OUI screening, and (on Windows) registry
artifact inspection — then prints a per-check report. It exits `0` when no
hypervisor tell is found and `1` when one is, so a CI gate can run it in a
booted guest as the automated transparency check:

```sh
bridge-agent stealth-check
# in-guest hypervisor detection report
# [PASS] cpuid/hypervisor-present-bit — CPUID.1:ECX = 0x00000000 ...
# verdict: 6 pass, 0 tell, 1 skipped — no hypervisor tells
```

The check logic lives in `src/stealth.rs` (`bridge_agent::stealth`):
collection is OS/architecture-specific and best-effort, parsing and
evaluation are pure and unit-tested, and the evaluation reuses the shared
`enlil_devices::stealth::detection` primitives. Unsupported checks report
`SKIP`, never a failure.

## Packaging

`scripts/package-bridge-agent.sh` builds all three installers into `dist/`:

| file | built with |
|---|---|
| `enlil-bridge-agent_<ver>_amd64.deb` | `dpkg-deb` (systemd unit + `/etc/enlil/bridge-agent.toml`) |
| `enlil-bridge-agent-<ver>-1.x86_64.rpm` | `scripts/build-rpm.py` — native RPM v4 writer, no `rpmbuild` needed |
| `enlil-bridge-agent-<ver>-x64.msi` | `scripts/build-msi.py` — genuine MSI via `msibuild` IDT imports + embedded CAB |

The `.msi` embeds a `x86_64-pc-windows-gnu` cross-compiled
`enlil-bridge-agent.exe` (needs `zig` or `mingw-w64` as the linker) installed
as the `EnlilBridgeAgent` Windows service, plus the default config under
`%PROGRAMDATA%\Enlil`. WiX authoring for native Windows builds lives in
`bridge-agent/packaging/windows/enlil-bridge-agent.wxs`.
