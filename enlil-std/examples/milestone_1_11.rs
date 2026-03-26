//! Phase 1.11 Milestone Verification
//!
//! This binary proves that all std-like APIs work through the enlil platform layer:
//! - std::thread::spawn → enlil_std::thread::spawn
//! - std::sync::Mutex → enlil_std::sync::Mutex
//! - Vec, HashMap → enlil_std::collections
//! - async/await → enlil_std::future::block_on
//! - println!() → enlil_std::println!
//! - std::time::Instant → enlil_std::time::Instant

use enlil_std::collections::{HashMap, Vec};
use enlil_std::sync::{Arc, Mutex};
use enlil_std::thread;
use enlil_std::time::{Duration, Instant};

fn main() {
    enlil_std::println!("=== Enlil Phase 1.11 Milestone Test ===");
    enlil_std::println!("Platform: enlil-platform ({})", enlil_platform::backend_name());
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
    let mut handles = std::vec::Vec::new();

    for i in 0..4 {
        let shared = Arc::clone(&shared);
        let handle = thread::spawn(move || {
            let msg = format!("thread-{} reporting", i);
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
    enlil_std::println!("  Formatted: pi ≈ {:.4}", std::f64::consts::PI);
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

    let mut handles = std::vec::Vec::new();
    for i in 0..8 {
        let data = Arc::clone(&data);
        handles.push(thread::spawn(move || {
            let value = enlil_std::future::block_on(async move {
                format!("async-thread-{}", i)
            });
            data.lock().push(value);
        }));
    }
    for h in handles {
        h.join().expect("thread panicked");
    }
    let elapsed = start.elapsed();
    let final_data = data.lock();
    assert_eq!(final_data.len(), 8);
    enlil_std::println!("  8 threads × async produced {} results in {:?}", final_data.len(), elapsed);
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
