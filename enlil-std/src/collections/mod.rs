//! Collections — re-exports from `std` on the hosted backend, from `alloc`
//! (plus `hashbrown` for the hash-based maps/sets) on the bare-metal backend.

#[cfg(feature = "platform-linux")]
pub use std::collections::{
    BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, LinkedList, VecDeque,
};
#[cfg(feature = "platform-linux")]
pub use std::vec::Vec;

#[cfg(feature = "platform-baremetal")]
pub use alloc::collections::{BTreeMap, BTreeSet, BinaryHeap, LinkedList, VecDeque};
#[cfg(feature = "platform-baremetal")]
pub use alloc::vec::Vec;
#[cfg(feature = "platform-baremetal")]
pub use hashbrown::{HashMap, HashSet};
