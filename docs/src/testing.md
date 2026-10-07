# Testing

## The test suite

`cargo test --workspace` runs the workspace unit and integration tests. The
suite is the primary gate: CI runs it on every push and pull request, and no
branch is published with red tests.

## KVM availability

The KVM-backed path requires a Linux host with `/dev/kvm`; live guest-boot
tests additionally need nested virtualization. Many dev machines and CI
runners lack one or both.

The project's rule, enforced in `enlil-core`'s `kvm_backend::is_kvm_available()`:

- **KVM-dependent tests self-skip when the hardware is absent.** A skip is
  reported as a skip — it is never a failure, and it is never presented as
  verification.
- **Nothing is faked.** A test that cannot run does not pretend to run. In
  particular, guest-boot verification cannot be claimed from a host without
  `/dev/kvm`.

CI installs `acpica-tools` and `dmidecode` so the ACPI/SMBIOS integration
tests that self-skip when those binaries are absent become hard gates instead
of silent skips. The same principle applies anywhere a test's precondition can
be provided: provide it, don't excuse it.

## What to run locally

```sh
cargo test --workspace          # everything; KVM tests self-skip without /dev/kvm
cargo test -p enlil-config      # a single crate
cargo test -- --skip kvm        # if you want the non-KVM subset explicitly
```

When writing a new KVM-dependent test, gate it on
`kvm_backend::is_kvm_available()` with a clear skip message naming the missing
precondition — do not `#[ignore]` it (ignored tests rot silently).
