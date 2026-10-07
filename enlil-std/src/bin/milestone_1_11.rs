//! Phase 1.11 Milestone Verification
//!
//! This binary proves that all std-like APIs work through the enlil platform layer:
//! - `thread::spawn` → [`enlil_std::thread::spawn`]
//! - `sync::Mutex` → [`enlil_std::sync::Mutex`]
//! - `Vec`, `HashMap` → [`enlil_std::collections`]
//! - `async`/`await` → [`enlil_std::future::block_on`]
//! - `println!()` → [`enlil_std::println!`]
//! - `time::Instant` → [`enlil_std::time::Instant`]
//!
//! It builds for both backends from the same source:
//! - `platform-linux`: ordinary hosted binary with `fn main`.
//! - `platform-baremetal` on the `x86_64-unknown-enlil` target (`target_os =
//!   "none"`): freestanding `no_std`/`no_main` ELF with its own heap, panic
//!   handler, and `_start` entry point.

#![cfg_attr(all(feature = "platform-baremetal", target_os = "none"), no_std)]
#![cfg_attr(all(feature = "platform-baremetal", target_os = "none"), no_main)]
#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

// `true` exactly for the freestanding bare-metal target build of this binary.
// (A plain `#[cfg]` alias would need a build script; the explicit conjunction
// keeps the gating visible at each use.)
#[cfg(all(feature = "platform-baremetal", target_os = "none"))]
extern crate alloc;

#[cfg(all(feature = "platform-baremetal", target_os = "none"))]
use alloc::string::String;
#[cfg(all(feature = "platform-baremetal", target_os = "none"))]
use alloc::{format, vec};

use enlil_std::collections::{HashMap, Vec};
use enlil_std::sync::{Arc, Mutex};
use enlil_std::thread;
use enlil_std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Bare-metal runtime glue (freestanding `x86_64-unknown-enlil` build only)
// ---------------------------------------------------------------------------

/// Halt the CPU until the next interrupt.
#[cfg(all(feature = "platform-baremetal", target_os = "none"))]
fn halt() -> ! {
    loop {
        // SAFETY: `hlt` is the freestanding idle instruction; interrupts stay
        // enabled so the CPU wakes on the next IRQ.
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

/// Bootstrap heap size for the freestanding milestone binary (8 MiB).
#[cfg(all(feature = "platform-baremetal", target_os = "none"))]
const HEAP_SIZE: usize = 8 * 1024 * 1024;

/// Backing store for the bare-metal global heap, installed in `_start`.
#[cfg(all(feature = "platform-baremetal", target_os = "none"))]
static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

#[cfg(all(feature = "platform-baremetal", target_os = "none"))]
#[global_allocator]
static ALLOCATOR: enlil_platform::memory::PlatformAllocator =
    enlil_platform::memory::PlatformAllocator;

#[cfg(all(feature = "platform-baremetal", target_os = "none"))]
#[panic_handler]
fn panic_handler(_info: &core::panic::PanicInfo<'_>) -> ! {
    use enlil_platform::io::{PlatformIo, SerialPort};
    let serial = SerialPort::new(SerialPort::COM1);
    serial.init();
    let _ = PlatformIo::write(&serial, b"\r\n*** MILESTONE PANIC ***\r\n");
    halt();
}

/// Freestanding entry point for the `x86_64-unknown-enlil` target.
#[cfg(all(feature = "platform-baremetal", target_os = "none"))]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // SAFETY: `HEAP` is exclusively owned by the entry point here; the heap is
    // installed exactly once before the first allocation.
    unsafe {
        enlil_platform::memory::init_baremetal_heap(
            core::ptr::addr_of_mut!(HEAP).cast::<u8>(),
            HEAP_SIZE,
        );
    }
    enlil_platform::init();
    milestone_main();
    halt();
}

#[cfg(not(all(feature = "platform-baremetal", target_os = "none")))]
fn main() {
    milestone_main();
}

#[allow(clippy::too_many_lines)] // Linear milestone script: one block per API under test.
fn milestone_main() {
    enlil_std::println!("=== Enlil Phase 1.11 Milestone Test ===");
    enlil_std::println!(
        "Platform: enlil-platform ({})",
        enlil_platform::backend_name()
    );
    enlil_std::println!();

    // --- 1. Vec and HashMap (collections through platform allocator) ---
    enlil_std::println!("[1/6] Collections (Vec, HashMap)...");
    let v: Vec<String> = vec!["hello".into(), "from".into(), "enlil".into()];
    assert_eq!(v.len(), 3);
    assert_eq!(v.join(" "), "hello from enlil");

    let mut map: HashMap<&str, i32> = HashMap::new();
    map.insert("cores", 16);
    map.insert("guests", 4);
    map.insert("memory_gb", 64);
    assert_eq!(map.get("cores"), Some(&16));
    assert_eq!(map.len(), 3);
    enlil_std::println!("  Vec: {:?}", v);
    enlil_std::println!("  HashMap: {:?}", map);
    enlil_std::println!("  ✓ Collections working");
    enlil_std::println!();

    // --- 2. Mutex (sync through platform layer) ---
    enlil_std::println!("[2/6] Sync (Mutex)...");
    let counter = Arc::new(Mutex::new(0u64));
    {
        let mut guard = counter.lock();
        *guard += 42;
    }
    assert_eq!(*counter.lock(), 42);
    enlil_std::println!("  Mutex value: {}", *counter.lock());
    enlil_std::println!("  ✓ Mutex working");
    enlil_std::println!();

    // --- 3. thread::spawn (threading through platform layer) ---
    enlil_std::println!("[3/6] Threading (thread::spawn)...");
    let shared = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut handles = Vec::new();

    for i in 0..4 {
        let shared = Arc::clone(&shared);
        let handle = thread::spawn(move || {
            let msg = format!("thread-{i} reporting");
            shared.lock().push(msg);
        });
        handles.push(handle);
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    let results = shared.lock();
    assert_eq!(results.len(), 4);
    enlil_std::println!("  Spawned 4 threads, collected {} messages", results.len());
    for msg in results.iter() {
        enlil_std::println!("    - {}", msg);
    }
    drop(results);
    enlil_std::println!("  ✓ Threading working");
    enlil_std::println!();

    // --- 4. Instant and Duration (time through platform layer) ---
    enlil_std::println!("[4/6] Time (Instant, Duration)...");
    let start = Instant::now();
    enlil_std::time::sleep(Duration::from_millis(10));
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(5)); // allow some slack
    enlil_std::println!("  Slept 10ms, measured: {:?}", elapsed);
    enlil_std::println!("  ✓ Time working");
    enlil_std::println!();

    // --- 5. println! (I/O through platform console) ---
    enlil_std::println!("[5/6] I/O (println!)...");
    enlil_std::print!("  print! works... ");
    enlil_std::println!("and println! works too");
    enlil_std::println!("  Formatted: pi ≈ {:.4}", core::f64::consts::PI);
    enlil_std::println!("  ✓ I/O working");
    enlil_std::println!();

    // --- 6. async/await (async runtime through platform layer) ---
    enlil_std::println!("[6/6] Async (async/await, block_on)...");
    let result = enlil_std::future::block_on(async {
        let a = async { 21 }.await;
        let b = async { 21 }.await;
        a + b
    });
    assert_eq!(result, 42);
    enlil_std::println!("  block_on(async {{ 21 + 21 }}) = {}", result);
    enlil_std::println!("  ✓ Async working");
    enlil_std::println!();

    // --- Combined: threaded Mutex<Vec<String>> with async ---
    enlil_std::println!("[COMBINED] Threaded Mutex<Vec<String>> with timing...");
    let data = Arc::new(Mutex::new(Vec::<String>::new()));
    let start = Instant::now();

    let mut handles = Vec::new();
    for i in 0..8 {
        let data = Arc::clone(&data);
        handles.push(thread::spawn(move || {
            let value = enlil_std::future::block_on(async move { format!("async-thread-{i}") });
            data.lock().push(value);
        }));
    }
    for h in handles {
        h.join().expect("thread panicked");
    }
    let elapsed = start.elapsed();
    let final_data = data.lock();
    assert_eq!(final_data.len(), 8);
    enlil_std::println!(
        "  8 threads × async produced {} results in {:?}",
        final_data.len(),
        elapsed
    );
    drop(final_data);
    enlil_std::println!("  ✓ Combined test passed");
    enlil_std::println!();

    enlil_std::println!("=== ALL MILESTONE 1.11 TESTS PASSED ===");
    enlil_std::println!("  ✓ std::thread::spawn — via enlil-platform threading");
    enlil_std::println!("  ✓ std::sync::Mutex — via enlil-platform sync");
    enlil_std::println!("  ✓ Vec, HashMap — via enlil-platform allocator");
    enlil_std::println!("  ✓ async/await — via enlil-platform async runtime");
    enlil_std::println!("  ✓ println!() — via enlil-platform console I/O");
    enlil_std::println!("  ✓ std::time::Instant — via enlil-platform time");
}
