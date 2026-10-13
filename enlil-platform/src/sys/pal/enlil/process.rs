//! Process bindings for the bare-metal kernel.
//!
//! There is no OS to exit to: [`exit`] records the code in a static (for
//! debuggers) and halts the CPU, and the kernel is a single process, so
//! [`id`] is always 0.

use core::sync::atomic::{AtomicI32, Ordering};

/// Exit code recorded before halting; visible to a debugger.
static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

/// Returns the last recorded exit code (for debuggers; the kernel halts).
#[must_use]
pub fn last_exit_code() -> i32 {
    EXIT_CODE.load(Ordering::SeqCst)
}

/// Halts the CPU forever: `cli; hlt` in a loop.
///
/// Interrupts are masked first so a pending interrupt cannot wake the halt
/// into an uninitialized state; there is no OS to return to.
fn halt() -> ! {
    loop {
        // SAFETY: `cli`/`hlt` only affect the calling CPU's interrupt flag
        // and sleep state; no memory is touched.
        unsafe {
            core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
            core::arch::asm!("hlt", options(nomem, nostack));
        }
    }
}

/// Terminates the "process": records `code` and halts the CPU.
///
/// There is no OS to report the code to; it stays in a static for
/// debuggers (see [`last_exit_code`]).
pub fn exit(code: i32) -> ! {
    EXIT_CODE.store(code, Ordering::SeqCst);
    halt();
}

/// Aborts abnormally: halts without recording a code.
pub fn abort() -> ! {
    halt();
}

/// The current "process" id. The kernel is a single process: always 0.
#[must_use]
pub const fn id() -> u32 {
    0
}
