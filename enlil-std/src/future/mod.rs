//! Async runtime — backed by `enlil_platform::async_rt`.

pub use enlil_platform::async_rt::{block_on, Executor, PriorityExecutor, Reactor, TaskPriority};
pub use std::future::Future;
