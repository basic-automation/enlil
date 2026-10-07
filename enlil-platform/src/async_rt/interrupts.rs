//! Bare-metal interrupt + IPI event source driving the [`Reactor`](super::Reactor).
//!
//! The reactor itself is backend-neutral: it tracks which tasks wait on which
//! sources and fires their wakers when a source is signalled ready via
//! [`Reactor::mark_ready`](super::Reactor::mark_ready). This module is the
//! bare-metal event source that produces those signals (item 1.6, T-1.5) —
//! the layer the reactor's docs say "layers on top", and the counterpart of
//! the Linux [`epoll`](super::epoll::EpollPoller) source.
//!
//! # How it works
//!
//! Two interrupt classes feed the reactor:
//!
//! - **Device interrupts.** A fixed window of IDT vectors
//!   ([`DEVICE_IRQ_VECTOR_BASE`]..`+`[`DEVICE_IRQ_VECTORS`]) each has a
//!   dedicated `extern "x86-interrupt"` stub (generated below) that EOIs the
//!   LAPIC and calls [`Reactor::mark_ready`] for the token claimed for that
//!   vector. A driver claims a vector with
//!   [`InterruptEventSource::add_device_irq`] (backed by a lock-free
//!   vector→token table), registers its reactor token, and the interrupt
//!   handler wakes the parked task — no polling, no timer.
//! - **IPI wakeups.** [`IPI_WAKEUP_VECTOR`] is a dedicated cross-CPU wakeup
//!   vector. [`send_wakeup_ipi`] raises it on another CPU (x2APIC ICR, fixed
//!   delivery); its handler marks the armed IPI token ready, so a task parked
//!   on the IPI source wakes when a peer CPU kicks it.
//!
//! [`InterruptEventSource::poll`] is the blocking wait: with interrupts
//! masked it drains already-ready tokens, then sleeps in `hlt` (re-armed
//! atomically via `sti; hlt` so no wakeup is lost) until an interrupt marks a
//! token or the TSC-derived timeout expires. The executor's
//! [`Executor::run_with_interrupt_source`](super::Executor::run_with_interrupt_source)
//! loop uses it the way the Linux loop uses `epoll_wait`.
//!
//! # Wiring
//!
//! The platform crate cannot touch the IDT (it lives in `enlil-boot`), so
//! this module exports the handler entry points —
//! [`ipi_wakeup_handler_addr`] and [`device_irq_handler_addr`] — for the boot
//! kernel to install with its `install_interrupt_gate`, plus
//! [`InterruptEventSource::install`] to publish the reactor the handlers
//! drive. The pure logic (vector window checks, the x2APIC ICR encoding, the
//! vector→token table) is host-tested; the privileged operations (`wrmsr`,
//! `hlt`, `sti`/`cli`) only ever execute on bare metal.
//!
//! # Test note
//!
//! The module is compiled on the host only under `cfg(test)` so the pure
//! dispatch logic is exercised by `cargo test`; the `extern "x86-interrupt"`
//! stubs and privileged instructions compile there but are never executed by
//! the tests.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use core::time::Duration;

use super::Reactor;

// ---------------------------------------------------------------------------
// Vector layout
// ---------------------------------------------------------------------------

/// IDT vector reserved for cross-CPU executor wakeups.
///
/// Clear of the CPU exceptions (`0x00`–`0x1F`), the boot LAPIC timer
/// (`0x40`), the device-IRQ window below, and the LAPIC spurious vector
/// (`0xFF`).
pub const IPI_WAKEUP_VECTOR: u8 = 0xF0;

/// First IDT vector of the device-IRQ window: each vector in
/// `DEVICE_IRQ_VECTOR_BASE..DEVICE_IRQ_VECTOR_BASE + DEVICE_IRQ_VECTORS` has a
/// dedicated interrupt stub that dispatches to the reactor.
pub const DEVICE_IRQ_VECTOR_BASE: u8 = 0x50;

/// Number of IDT vectors in the device-IRQ window (so `0x50`–`0x6F`).
pub const DEVICE_IRQ_VECTORS: u8 = 32;

/// Whether `vector` lies in the device-IRQ window.
#[must_use]
pub const fn is_device_irq_vector(vector: u8) -> bool {
    vector >= DEVICE_IRQ_VECTOR_BASE && vector < DEVICE_IRQ_VECTOR_BASE + DEVICE_IRQ_VECTORS
}

/// Compile-time vector-layout check: the device-IRQ window must clear the
/// CPU exception vectors and the boot LAPIC timer vector, and must not reach
/// the IPI wakeup vector or the LAPIC spurious vector.
const _: () = {
    assert!(
        DEVICE_IRQ_VECTOR_BASE >= 0x20,
        "device-IRQ window must clear the CPU exception vectors"
    );
    assert!(
        !is_device_irq_vector(0x40),
        "device-IRQ window must not overlap the boot LAPIC timer vector"
    );
    assert!(
        DEVICE_IRQ_VECTOR_BASE + DEVICE_IRQ_VECTORS <= IPI_WAKEUP_VECTOR,
        "device-IRQ window must not reach the IPI wakeup vector"
    );
    assert!(
        IPI_WAKEUP_VECTOR < 0xFF,
        "IPI vector must not be the LAPIC spurious vector"
    );
};

/// Sentinel in the vector→token table meaning "no token claimed this vector".
/// A claimed token is always a real reactor token, so it can never alias this.
const UNASSIGNED: usize = usize::MAX;

/// Vector → reactor token, indexed by IDT vector.
///
/// Lock-free (`AtomicUsize` per vector) because interrupt stubs read it with
/// interrupts masked and no locking: a stub loads its vector's slot and, if
/// claimed, marks that token ready. Claim/release use compare-exchange so two
/// drivers cannot claim the same vector.
static VECTOR_TOKENS: [AtomicUsize; 256] = [const { AtomicUsize::new(UNASSIGNED) }; 256];

/// The token the IPI wakeup handler marks ready ([`UNASSIGNED`] = none armed).
static IPI_TOKEN: AtomicUsize = AtomicUsize::new(UNASSIGNED);

/// The reactor interrupt stubs dispatch to, published by
/// [`InterruptEventSource::install`]. Null until installed; never cleared, so
/// a non-null load is a valid `&'static Reactor`.
static ACTIVE_REACTOR: AtomicPtr<Reactor> = AtomicPtr::new(core::ptr::null_mut());

/// The installed reactor, if [`InterruptEventSource::install`] has run.
fn active_reactor() -> Option<&'static Reactor> {
    let ptr = ACTIVE_REACTOR.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // SAFETY: `install` stores a valid `&'static Reactor` before
        // interrupts are enabled, and it is never cleared or replaced with a
        // dangling pointer.
        Some(unsafe { &*ptr })
    }
}

/// The token claimed for `vector`, or `None` if the vector is unclaimed (or
/// outside the table — callers pass real IDT vectors, always in range).
fn token_for_vector(vector: u8) -> Option<usize> {
    let token = VECTOR_TOKENS[usize::from(vector)].load(Ordering::Acquire);
    (token != UNASSIGNED).then_some(token)
}

/// Error from [`claim_device_vector`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorClaimError {
    /// The vector is outside the device-IRQ window.
    OutsideWindow(u8),
    /// Another token already claimed the vector.
    AlreadyClaimed(u8),
    /// `usize::MAX` is the table's unclaimed sentinel and cannot be a token.
    ReservedToken,
}

/// Claim a device-IRQ `vector` for reactor `token`.
///
/// The caller must have registered `token` with the reactor first; the
/// IOAPIC/MSI routing that actually delivers device interrupts to the vector
/// is programmed separately.
///
/// # Errors
/// [`VectorClaimError::OutsideWindow`] if the vector is outside the
/// device-IRQ window, [`VectorClaimError::AlreadyClaimed`] if another token
/// claimed it, [`VectorClaimError::ReservedToken`] if `token` is the
/// [`UNASSIGNED`] sentinel.
pub fn claim_device_vector(vector: u8, token: usize) -> Result<(), VectorClaimError> {
    if !is_device_irq_vector(vector) {
        return Err(VectorClaimError::OutsideWindow(vector));
    }
    if token == UNASSIGNED {
        return Err(VectorClaimError::ReservedToken);
    }
    VECTOR_TOKENS[usize::from(vector)]
        .compare_exchange(UNASSIGNED, token, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| VectorClaimError::AlreadyClaimed(vector))
}

/// Release a previously claimed device-IRQ vector. No-op if unclaimed.
pub fn release_device_vector(vector: u8) {
    if is_device_irq_vector(vector) {
        VECTOR_TOKENS[usize::from(vector)].store(UNASSIGNED, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// x2APIC IPI encoding (pure)
// ---------------------------------------------------------------------------

/// `IA32_X2APIC_ICR` MSR: the Interrupt Command Register (single 64-bit MSR
/// in x2APIC mode — no delivery-status polling, unlike xAPIC's MMIO pair).
const IA32_X2APIC_ICR: u32 = 0x830;

/// `IA32_X2APIC_EOI` MSR: write 0 to signal end-of-interrupt.
const IA32_X2APIC_EOI: u32 = 0x80B;

/// The x2APIC ICR value for a **fixed-delivery** IPI to `apic_id` on `vector`.
///
/// Destination APIC id in the high dword, delivery mode `000` (Fixed),
/// destination shorthand `00` (no shorthand — use the id field), vector in
/// the low byte. This is the wakeup kick: the target CPU vectors to `vector`
/// (normally [`IPI_WAKEUP_VECTOR`]).
#[must_use]
pub const fn icr_fixed_ipi(apic_id: u32, vector: u8) -> u64 {
    ((apic_id as u64) << 32) | (vector as u64)
}

// ---------------------------------------------------------------------------
// Privileged operations (execute on bare metal only)
// ---------------------------------------------------------------------------

/// Bare-metal-only primitives. They compile on the host (plain `x86_64`
/// instructions) but must never execute there — the unit tests never call
/// them.
mod hw {
    use super::{IA32_X2APIC_EOI, IA32_X2APIC_ICR};

    /// Write a 64-bit MSR.
    ///
    /// # Safety
    /// `msr` must be writable at the current privilege level with a legal
    /// value; a bad write faults (#GP). Bare metal only.
    unsafe fn wrmsr(msr: u32, value: u64) {
        let low = (value & 0xFFFF_FFFF) as u32;
        let high = (value >> 32) as u32;
        unsafe {
            core::arch::asm!(
                "wrmsr",
                in("ecx") msr,
                in("eax") low,
                in("edx") high,
                options(nomem, nostack, preserves_flags),
            );
        }
    }

    /// Read the TSC.
    pub(super) fn read_tsc() -> u64 {
        // SAFETY: `rdtsc` is unprivileged; the intrinsic just executes it.
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    /// Signal end-of-interrupt to the local APIC. Called from an interrupt
    /// handler before it returns so the LAPIC can deliver further interrupts.
    pub(super) fn signal_eoi() {
        // SAFETY: ring 0 on bare metal; EOI is a standard x2APIC MSR and 0 is
        // its only legal value.
        unsafe { wrmsr(IA32_X2APIC_EOI, 0) };
    }

    /// Send an IPI described by a precomputed x2APIC ICR value (see
    /// [`super::icr_fixed_ipi`]). In x2APIC mode the single MSR write is the
    /// whole send — no delivery-status poll needed.
    pub(super) fn send_ipi(icr: u64) {
        // SAFETY: ring 0 on bare metal; IA32_X2APIC_ICR is the architectural
        // IPI register and the caller built a legal value.
        unsafe { wrmsr(IA32_X2APIC_ICR, icr) };
    }

    /// Enable interrupts (`sti`).
    pub(super) fn enable_interrupts() {
        // SAFETY: bare metal only; the caller upholds the module's masking
        // discipline (documented on `poll`).
        unsafe { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)) };
    }

    /// Mask interrupts (`cli`).
    pub(super) fn disable_interrupts() {
        // SAFETY: bare metal only; see `enable_interrupts`.
        unsafe { core::arch::asm!("cli", options(nomem, nostack, preserves_flags)) };
    }

    /// Atomically unmask interrupts and halt until the next one arrives.
    ///
    /// `sti` takes effect only *after* the next instruction, so the unmask
    /// and the `hlt` are atomic: no interrupt can slip between a readiness
    /// check done with interrupts masked and the sleep. Any interrupt wakes
    /// the halt, its handler runs, and execution resumes after the `hlt`
    /// with interrupts enabled.
    pub(super) fn sleep_until_interrupt() {
        // SAFETY: bare metal only; `hlt` with IF set sleeps until an
        // interrupt, which is exactly the wait this module implements.
        unsafe {
            core::arch::asm!("sti", "hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

// ---------------------------------------------------------------------------
// Interrupt handlers
// ---------------------------------------------------------------------------

/// The stack frame the CPU pushes for an `x86-interrupt` handler.
///
/// No error code: RIP/CS/RFLAGS/RSP/SS. Same layout as the boot IDT's frame
/// type — the platform crate cannot depend on `enlil-boot`, so it carries
/// its own.
#[repr(C)]
pub struct InterruptStackFrame {
    /// Instruction pointer the interrupt preempted.
    pub rip: u64,
    /// Code segment of the preempted context.
    pub cs: u64,
    /// RFLAGS of the preempted context.
    pub rflags: u64,
    /// Stack pointer of the preempted context.
    pub rsp: u64,
    /// Stack segment of the preempted context.
    pub ss: u64,
}

/// Dispatch a device IRQ to the reactor: mark the token claimed for `vector`
/// ready, waking its parked task. Pure (no privileged operations) so the
/// table/dispatch logic is host-testable; the stub EOIs first.
fn dispatch_device_irq(reactor: &Reactor, vector: u8) {
    if let Some(token) = token_for_vector(vector) {
        reactor.mark_ready(token);
    }
}

/// The common tail of every device-IRQ stub: EOI the LAPIC, then feed the
/// reactor. Runs in interrupt context; [`Reactor::mark_ready`] is safe there
/// (spin lock, wakers fired after the lock is released).
fn device_irq_common(vector: u8) {
    // EOI first: the readiness signal is just a mark_ready, so acknowledging
    // the LAPIC up front keeps interrupt latency minimal. (Level-triggered
    // devices must additionally be masked at the IOAPIC until their task
    // services them, or the line re-vectors immediately — that masking is the
    // driver's/IOAPIC policy, layered above this module.)
    hw::signal_eoi();
    if let Some(reactor) = active_reactor() {
        dispatch_device_irq(reactor, vector);
    }
}

/// Dispatch the IPI wakeup: mark the armed IPI token ready. Pure; the handler
/// EOIs first.
fn dispatch_ipi_wakeup(reactor: &Reactor) {
    let token = IPI_TOKEN.load(Ordering::Acquire);
    if token != UNASSIGNED {
        reactor.mark_ready(token);
    }
}

/// The cross-CPU wakeup handler installed at [`IPI_WAKEUP_VECTOR`].
///
/// EOIs, then marks the token armed by
/// [`InterruptEventSource::arm_ipi_wakeup`] ready — waking whatever task the
/// target CPU parked on the IPI source. (A CPU halted in
/// [`InterruptEventSource::poll`] wakes on *any* interrupt regardless; the
/// token gives the wakeup a reactor-visible reason a task can wait on.)
extern "x86-interrupt" fn ipi_wakeup_handler(_frame: InterruptStackFrame) {
    hw::signal_eoi();
    if let Some(reactor) = active_reactor() {
        dispatch_ipi_wakeup(reactor);
    }
}

/// Generate the per-vector device-IRQ stubs plus the address table the boot
/// kernel installs into the IDT.
///
/// Each stub bakes its vector in at compile time, so dispatch needs no
/// runtime vector discovery (no ISR scan): the stub for `0x5A` always means
/// "the device routed to `0x5A` signalled".
macro_rules! device_irq_stubs {
    ($( $name:ident = $vector:expr ),* $(,)?) => {
        $(
            extern "x86-interrupt" fn $name(_frame: InterruptStackFrame) {
                device_irq_common($vector);
            }
        )*

        /// The stubs, indexed by `vector - DEVICE_IRQ_VECTOR_BASE`. Function
        /// pointers (not addresses): pointer→integer casts are runtime-only,
        /// so [`device_irq_handler_addr`] performs the cast when read.
        static DEVICE_IRQ_STUBS: [extern "x86-interrupt" fn(InterruptStackFrame); DEVICE_IRQ_VECTORS as usize] = [
            $( $name ),*
        ];
    };
}

device_irq_stubs!(
    device_irq_0x50 = 0x50,
    device_irq_0x51 = 0x51,
    device_irq_0x52 = 0x52,
    device_irq_0x53 = 0x53,
    device_irq_0x54 = 0x54,
    device_irq_0x55 = 0x55,
    device_irq_0x56 = 0x56,
    device_irq_0x57 = 0x57,
    device_irq_0x58 = 0x58,
    device_irq_0x59 = 0x59,
    device_irq_0x5a = 0x5a,
    device_irq_0x5b = 0x5b,
    device_irq_0x5c = 0x5c,
    device_irq_0x5d = 0x5d,
    device_irq_0x5e = 0x5e,
    device_irq_0x5f = 0x5f,
    device_irq_0x60 = 0x60,
    device_irq_0x61 = 0x61,
    device_irq_0x62 = 0x62,
    device_irq_0x63 = 0x63,
    device_irq_0x64 = 0x64,
    device_irq_0x65 = 0x65,
    device_irq_0x66 = 0x66,
    device_irq_0x67 = 0x67,
    device_irq_0x68 = 0x68,
    device_irq_0x69 = 0x69,
    device_irq_0x6a = 0x6a,
    device_irq_0x6b = 0x6b,
    device_irq_0x6c = 0x6c,
    device_irq_0x6d = 0x6d,
    device_irq_0x6e = 0x6e,
    device_irq_0x6f = 0x6f,
);

/// The address of the IPI wakeup handler, for the boot kernel to install at
/// [`IPI_WAKEUP_VECTOR`] via its IDT gate installer.
#[must_use]
pub fn ipi_wakeup_handler_addr() -> u64 {
    ipi_wakeup_handler as *const () as u64
}

/// The address of the device-IRQ stub for `vector`, for the boot kernel to
/// install via its IDT gate installer. `None` if `vector` is outside the
/// device-IRQ window.
#[must_use]
pub fn device_irq_handler_addr(vector: u8) -> Option<u64> {
    if !is_device_irq_vector(vector) {
        return None;
    }
    Some(DEVICE_IRQ_STUBS[usize::from(vector - DEVICE_IRQ_VECTOR_BASE)] as *const () as u64)
}

// ---------------------------------------------------------------------------
// InterruptEventSource
// ---------------------------------------------------------------------------

/// The bare-metal event source driving a [`Reactor`]: device interrupts and
/// cross-CPU IPI wakeups.
///
/// Created against a `&'static Reactor` (on bare metal the reactor lives in a
/// static), published to the interrupt stubs with [`install`](Self::install),
/// and polled by the executor's run loop with [`poll`](Self::poll) — the
/// counterpart of the Linux [`EpollPoller`](super::epoll::EpollPoller).
///
/// Typical bring-up (in the boot kernel):
/// 1. `let source = InterruptEventSource::new(&REACTOR);`
/// 2. `source.install();`
/// 3. Install [`ipi_wakeup_handler_addr`] at [`IPI_WAKEUP_VECTOR`] and every
///    [`device_irq_handler_addr`] in the window into the IDT (interrupts
///    masked).
/// 4. Enable the LAPIC (x2APIC), route device IRQs to window vectors at the
///    IOAPIC, then enable interrupts.
pub struct InterruptEventSource {
    reactor: &'static Reactor,
}

impl InterruptEventSource {
    /// Create the event source driving `reactor`.
    #[must_use]
    pub const fn new(reactor: &'static Reactor) -> Self {
        Self { reactor }
    }

    /// The reactor this source drives.
    #[must_use]
    pub const fn reactor(&self) -> &'static Reactor {
        self.reactor
    }

    /// Publish this source as the interrupt stubs' dispatch target.
    ///
    /// Stores the reactor pointer the stubs read. Call once, with interrupts
    /// masked, before any gate installed from this module can fire; never
    /// clears, so handlers always see a valid reactor afterwards. Idempotent.
    pub fn install(&self) {
        ACTIVE_REACTOR.store(
            core::ptr::from_ref(self.reactor).cast_mut(),
            Ordering::Release,
        );
    }

    /// Claim a device-IRQ `vector` for reactor `token` (see
    /// [`claim_device_vector`]).
    ///
    /// # Errors
    /// [`VectorClaimError`] if the vector is outside the window, already
    /// claimed, or the token is the reserved sentinel.
    pub fn add_device_irq(&self, vector: u8, token: usize) -> Result<(), VectorClaimError> {
        claim_device_vector(vector, token)
    }

    /// Release a device-IRQ vector previously claimed with
    /// [`add_device_irq`](Self::add_device_irq).
    pub fn remove_device_irq(&self, vector: u8) {
        release_device_vector(vector);
    }

    /// Arm the token the IPI wakeup handler marks ready when
    /// [`IPI_WAKEUP_VECTOR`] fires. A task parks on this token (register +
    /// `set_waker`) to sleep until a peer CPU kicks it with
    /// [`send_wakeup_ipi`].
    pub fn arm_ipi_wakeup(&self, token: usize) {
        IPI_TOKEN.store(token, Ordering::Release);
    }

    /// Block until a device interrupt or IPI marks a reactor token ready, or
    /// `timeout` elapses (`None` = wait indefinitely).
    ///
    /// Drains already-ready tokens first (never sleeps when work is pending),
    /// then halts the CPU until an interrupt arrives: the stub marks its
    /// token and the loop re-drains. The `sti; hlt` sleep is atomic with the
    /// masked readiness check, so a readiness signalled just before the halt
    /// is still observed — no wakeup is lost.
    ///
    /// The timeout is measured on the TSC via
    /// [`tsc_frequency`](crate::time::tsc_frequency); if the TSC is not
    /// calibrated yet the timeout is ignored (waits indefinitely) rather than
    /// expiring immediately.
    ///
    /// Masking discipline: masks interrupts for the check/sleep sequence and
    /// returns with interrupts **enabled** — the executor runs with
    /// interrupts enabled so device interrupts can vector.
    ///
    /// Returns the number of tokens drained as newly ready.
    #[must_use]
    pub fn poll(&self, timeout: Option<Duration>) -> usize {
        let timeout_ticks = timeout.and_then(|d| {
            let freq = crate::time::tsc_frequency();
            if freq == 0 {
                // Uncalibrated TSC: a tick count would be meaningless, so the
                // timeout is ignored (documented above).
                return None;
            }
            let ticks = d.as_nanos() * u128::from(freq) / 1_000_000_000u128;
            Some(u64::try_from(ticks).unwrap_or(u64::MAX))
        });
        let start = hw::read_tsc();

        // Mask for the check/sleep sequence; `sleep_until_interrupt` unmasks
        // atomically with the halt (see its docs).
        hw::disable_interrupts();
        loop {
            let ready: Vec<usize> = self.reactor.take_ready();
            if !ready.is_empty() {
                hw::enable_interrupts();
                return ready.len();
            }
            if timeout_ticks.is_some_and(|t| hw::read_tsc().wrapping_sub(start) >= t) {
                hw::enable_interrupts();
                return 0;
            }
            hw::sleep_until_interrupt();
            hw::disable_interrupts();
        }
    }
}

/// Send a fixed-delivery IPI on [`IPI_WAKEUP_VECTOR`] to the CPU with
/// `apic_id`, waking whatever it parked on the IPI source.
///
/// The target vectors to the IPI handler, which EOIs and marks the armed IPI
/// token ready — the cross-CPU kick that lets one CPU wake another's
/// executor (e.g. after completing device work on its behalf). Requires the
/// LAPIC in x2APIC mode; the ICR write is the whole send (no status poll).
pub fn send_wakeup_ipi(apic_id: u32) {
    hw::send_ipi(icr_fixed_ipi(apic_id, IPI_WAKEUP_VECTOR));
}

// ---------------------------------------------------------------------------
// Tests (pure logic; privileged ops are never executed here)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::AtomicUsize as CoreAtomicUsize;
    use core::task::Waker;

    #[test]
    fn device_irq_window_bounds() {
        assert!(is_device_irq_vector(DEVICE_IRQ_VECTOR_BASE));
        assert!(is_device_irq_vector(0x6F));
        assert!(!is_device_irq_vector(0x4F));
        assert!(!is_device_irq_vector(0x70));
        assert!(!is_device_irq_vector(0x00));
        assert!(!is_device_irq_vector(IPI_WAKEUP_VECTOR));
        assert!(!is_device_irq_vector(0xFF));
        // The window layout itself (clear of exceptions, the boot timer, the
        // IPI and spurious vectors) is enforced by a compile-time assertion
        // above; spot-check the runtime predicate here.
        assert!(!is_device_irq_vector(0x40), "boot LAPIC timer vector");
    }

    #[test]
    fn fixed_ipi_icr_encodes_dest_and_vector() {
        let icr = icr_fixed_ipi(3, IPI_WAKEUP_VECTOR);
        assert_eq!(icr >> 32, 3, "destination APIC id in the high dword");
        assert_eq!(
            icr & 0xFF,
            u64::from(IPI_WAKEUP_VECTOR),
            "vector in bits 7:0"
        );
        assert_eq!((icr >> 8) & 0b111, 0, "delivery mode 000 = Fixed");
        assert_eq!((icr >> 18) & 0b11, 0, "no destination shorthand");
        // Distinct from the INIT/SIPI encodings used for SMP bring-up.
        assert_ne!((icr >> 8) & 0b111, 0b101);
    }

    #[test]
    fn handler_addresses_cover_the_whole_window() {
        for v in DEVICE_IRQ_VECTOR_BASE..DEVICE_IRQ_VECTOR_BASE + DEVICE_IRQ_VECTORS {
            let addr = device_irq_handler_addr(v);
            assert!(addr.is_some_and(|a| a != 0), "vector {v:#x} has a stub");
        }
        assert_eq!(device_irq_handler_addr(0x4F), None);
        assert_eq!(device_irq_handler_addr(0x70), None);
        assert_eq!(device_irq_handler_addr(IPI_WAKEUP_VECTOR), None);
        assert_ne!(ipi_wakeup_handler_addr(), 0);
        // Every stub is a distinct entry point.
        let mut addrs: Vec<u64> = (DEVICE_IRQ_VECTOR_BASE
            ..DEVICE_IRQ_VECTOR_BASE + DEVICE_IRQ_VECTORS)
            .filter_map(device_irq_handler_addr)
            .collect();
        addrs.sort_unstable();
        addrs.dedup();
        assert_eq!(addrs.len(), usize::from(DEVICE_IRQ_VECTORS));
    }

    #[test]
    fn claim_release_round_trip() {
        // A vector the other tests never touch, to avoid clashing with the
        // shared statics (tests run in parallel in one process).
        const V: u8 = 0x6E;
        release_device_vector(V);

        assert_eq!(claim_device_vector(V, 7), Ok(()));
        assert_eq!(token_for_vector(V), Some(7));
        // Double-claim fails; the first claim stands.
        assert_eq!(
            claim_device_vector(V, 8),
            Err(VectorClaimError::AlreadyClaimed(V))
        );
        assert_eq!(token_for_vector(V), Some(7));

        release_device_vector(V);
        assert_eq!(token_for_vector(V), None);
        // Releasing twice is fine.
        release_device_vector(V);
        // Re-claim after release works.
        assert_eq!(claim_device_vector(V, 9), Ok(()));
        assert_eq!(token_for_vector(V), Some(9));
        release_device_vector(V);
    }

    #[test]
    fn claim_rejects_bad_vectors_and_tokens() {
        assert_eq!(
            claim_device_vector(0x40, 1),
            Err(VectorClaimError::OutsideWindow(0x40))
        );
        assert_eq!(
            claim_device_vector(IPI_WAKEUP_VECTOR, 1),
            Err(VectorClaimError::OutsideWindow(IPI_WAKEUP_VECTOR))
        );
        assert_eq!(
            claim_device_vector(0x50, usize::MAX),
            Err(VectorClaimError::ReservedToken)
        );
    }

    /// The dispatch path the stubs run (minus the EOI, which needs real
    /// hardware): a claimed vector's token is marked ready on the reactor.
    #[test]
    fn dispatch_marks_the_claimed_vectors_token_ready() {
        const V: u8 = 0x6D;
        release_device_vector(V);
        let reactor = Reactor::new();
        let token = reactor.register(usize::from(V));
        claim_device_vector(V, token).expect("claim");

        // Nothing ready before the "interrupt".
        assert_eq!(reactor.take_ready(), Vec::new());
        dispatch_device_irq(&reactor, V);
        assert_eq!(reactor.take_ready(), alloc::vec![token]);
        // An unclaimed vector dispatches to nothing.
        release_device_vector(V);
        dispatch_device_irq(&reactor, V);
        assert_eq!(reactor.take_ready(), Vec::new());
    }

    /// The IPI path (minus the EOI): the armed token is marked ready, and an
    /// unarmed IPI marks nothing.
    #[test]
    fn ipi_dispatch_marks_the_armed_token() {
        let reactor = Reactor::new();
        let token = reactor.register(usize::from(IPI_WAKEUP_VECTOR));

        IPI_TOKEN.store(UNASSIGNED, Ordering::Release);
        dispatch_ipi_wakeup(&reactor);
        assert!(reactor.take_ready().is_empty(), "unarmed IPI marks nothing");

        IPI_TOKEN.store(token, Ordering::Release);
        dispatch_ipi_wakeup(&reactor);
        assert_eq!(reactor.take_ready(), alloc::vec![token]);
        IPI_TOKEN.store(UNASSIGNED, Ordering::Release);
    }

    /// A task parked on a device-IRQ token via `set_waker` is woken by the
    /// dispatch — the full interrupt→task-wakeup path the executor relies on.
    #[test]
    fn dispatch_wakes_a_task_parked_on_the_token() {
        use alloc::task::Wake;

        struct CountingWaker(Arc<CoreAtomicUsize>);
        impl Wake for CountingWaker {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        const V: u8 = 0x6C;
        release_device_vector(V);
        let reactor = Reactor::new();
        let token = reactor.register(usize::from(V));
        claim_device_vector(V, token).expect("claim");

        let woken = Arc::new(CoreAtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountingWaker(woken.clone())));
        reactor.set_waker(token, waker);

        dispatch_device_irq(&reactor, V);
        assert_eq!(woken.load(Ordering::SeqCst), 1, "parked task woken");
        assert_eq!(reactor.take_ready(), alloc::vec![token]);
        release_device_vector(V);
    }
}
