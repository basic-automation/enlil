//! Async runtime — backed by `enlil_platform::async_rt`.

pub use core::future::Future;
pub use enlil_platform::async_rt::{Executor, PriorityExecutor, Reactor, TaskPriority, block_on};
