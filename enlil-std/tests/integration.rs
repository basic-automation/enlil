//! Integration tests for enlil-std — proves Phase 1.11 milestone.

use enlil_std::thread;
use enlil_std::sync::{Mutex, Arc, channel};
use enlil_std::collections::{Vec, HashMap};
use enlil_std::time::Instant;
use enlil_std::future::block_on;

#[test]
fn thread_spawn_and_join() {
    let handle = thread::spawn(|| 42);
    assert_eq!(handle.join().unwrap(), 42);
}

#[test]
fn mutex_shared_across_threads() {
    let data = Arc::new(Mutex::new(Vec::new()));
    let mut handles = std::vec::Vec::new();

    for i in 0..4 {
        let data = data.clone();
        handles.push(thread::spawn(move || {
            let mut guard = data.lock();
            guard.push(i);
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let guard = data.lock();
    assert_eq!(guard.len(), 4);
}

#[test]
fn vec_and_hashmap() {
    let mut v: Vec<String> = Vec::new();
    v.push("hello".into());
    v.push("enlil".into());
    assert_eq!(v.len(), 2);

    let mut map: HashMap<String, i32> = HashMap::new();
    map.insert("a".into(), 1);
    map.insert("b".into(), 2);
    assert_eq!(map.get("a"), Some(&1));
}

#[test]
fn instant_monotonic() {
    let t1 = Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(10));
    let t2 = Instant::now();
    assert!(t2.elapsed() <= t1.elapsed());
}

#[test]
fn async_await_block_on() {
    let result = block_on(async { 42 });
    assert_eq!(result, 42);
}

#[test]
fn channel_communication() {
    let (tx, rx) = channel(16);
    thread::spawn(move || {
        tx.send(99).unwrap();
    });
    assert_eq!(rx.recv().unwrap(), 99);
}

#[test]
fn println_works() {
    // This exercises enlil_std::io::println! -> platform Console
    enlil_std::println!("Phase 1.11 milestone: println! works through platform layer");
}
