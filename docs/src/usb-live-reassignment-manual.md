# Manual verification: two mice + two keyboards, live TUI reassignment

Phase 4.7 milestone. This is the hardware-gated half of the acceptance
criteria: the software path is covered by
`enlil-mgmt/tests/usb_live_reassign_e2e.rs` (keystroke → wire → daemon →
registry, no hardware), but the "independent PCs" promise is only real when
physical pointers and keyboards visibly move between guests.

## Prerequisites

- Linux host with `/dev/kvm`, IOMMU not required (devices are forwarded
  through the virtual xHCI, not VFIO).
- Two physical USB mice and two physical USB keyboards plugged into the host.
  Note each device's VID:PID and port path (`lsusb`, or the console's USB tab
  itself).
- Two guests configured with a virtual xHCI controller each (the Renesas
  uPD720202 at 00:04.0 from Phase 4.4) and booted to a desktop or shell where
  mouse/keyboard input is observable.
- A `[usb.routing]` block routing the four devices two-and-two, e.g.:

```toml
[usb.routing]
default_guest = "guest-a"

[[usb.routing.rules]]
match = "vidpid:046d:c077"   # mouse A
target = "guest-a"

[[usb.routing.rules]]
match = "vidpid:046d:c31c"   # keyboard A
target = "guest-a"

[[usb.routing.rules]]
match = "vidpid:046d:c52b"   # mouse B
target = "guest-b"

[[usb.routing.rules]]
match = "vidpid:046d:c534"   # keyboard B
target = "guest-b"
```

(Use the real VID:PIDs from your hardware; `port:1-2` or `serial:...`
matchers work too.)

## Procedure

1. Boot the daemon with the config above. The USB monitor enumerates the
   four devices and the hot-plug dispatcher routes them per the rules.
2. In another terminal, launch the console:
   `enlil-mgmt console --addr 127.0.0.1:PORT` (the daemon's management
   socket). Press `F2` for the USB tab.
3. **Inventory check.** All four devices are listed with bus / VID:PID /
   product / speed / assigned guest / port. Mouse A + keyboard A show
   `guest-a`, mouse B + keyboard B show `guest-b`.
4. **Sanity.** Wiggle mouse A: the pointer moves on guest A's screen.
   Type on keyboard B: characters appear in guest B.
5. **Live reassign.** Select mouse A (`↑`/`↓`), press `2`. The status line
   shows `ok: device … moved to guest-b (port …)` and the table's
   assignment column flips to `guest-b`.
6. **Observe.** Wiggle mouse A again: the pointer now moves on guest B.
   Guest A sees a USB disconnect (its driver gets the port status-change);
   guest B sees a connect and enumerates the mouse.
7. **Move it back.** With mouse A still selected, press `1`. Pointer
   control returns to guest A.
8. **Cycle + detach.** Select keyboard A, press `c` (cycle to the next
   guest), then `x` (detach back to the hypervisor). The row shows
   `hypervisor`; typing on keyboard A reaches neither guest.
9. **Hot-plug.** Physically unplug mouse B and replug it: a notice lands in
   the "Hot-plug notices" pane and the device reattaches per the routing
   rules.

## Expected results

- Every reassignment completes in well under a second of operator time
  (virtual unplug + replug, no guest reboot).
- A failed move (e.g. target guest not booted) leaves the device on its
  original guest — it is never stranded — and the status line shows
  `FAILED: …` with the reason.
- `q` quits the console; the daemon keeps running.

## Daemon integration note

The console speaks to `enlil-core` over the management socket; the daemon
applies each `ClientMessage::UsbCommand` through
`enlil_core::usb_service::UsbService::handle_client_message` against the
live `XhciRegistry` and returns the `CommandResponse` (+ fresh
`UsbDeviceList`) the TUI renders. Wiring the socket accept loop into the
daemon's run loop is the remaining integration step for this procedure.
