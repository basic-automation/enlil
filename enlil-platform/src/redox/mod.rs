//! Redox OS-derived platform components (roadmap 1.10).
//!
//! Ports of five Redox OS subsystems into the platform layer, each keeping
//! its upstream design and naming so the lineage stays obvious:
//!
//! - [`ralloc`]: the `ralloc` slab allocator — power-of-two size classes over
//!   4 KiB slabs with intrusive free lists, direct-page fallback for
//!   oversized/over-aligned requests, and pluggable [`PageSource`](ralloc::PageSource)s.
//! - [`context`]: the kernel context model — [`ContextId`](context::ContextId)
//!   with the reserved kernel ID, lifecycle [`Status`](context::Status), and
//!   the [`Contexts`](context::Contexts) arena, switching through the
//!   platform's raw register swap.
//! - [`sync`]: the kernel `WaitQueue` — FIFO-ordered parker-based blocking
//!   with no missed wakeups.
//! - [`event`]: the kernel `EventQueue` — registered IDs, triggered
//!   readiness, FIFO delivery.
//! - [`time`]: the kernel `TimeSpec` — normalized seconds/nanoseconds
//!   timestamps plus monotonic and wall-clock sources.
//!
//! All modules are `no_std`-clean and build for the bare-metal target; the
//! Linux-only page source, thread parking, and `clock_gettime` paths are
//! feature-gated per module.

// The Redox-derived types deliberately keep their upstream names
// (`Ralloc`, `Context`, `Event`, `EventQueue`) even where they repeat the
// module name — the lineage is the point.
#![allow(clippy::module_name_repetitions)]

pub mod context;
pub mod event;
pub mod ralloc;
pub mod sync;
pub mod time;

pub use context::{Context, ContextId, Contexts, Status};
pub use event::{Event, EventError, EventFlags, EventId, EventQueue};
#[cfg(all(feature = "platform-linux", target_os = "linux"))]
pub use ralloc::MmapPageSource;
pub use ralloc::{BumpPageSource, PAGE_SIZE, PageSource, Ralloc, RallocStats};
pub use sync::WaitQueue;
pub use time::{NANOS_PER_SEC, TimeSpec, monotonic, realtime};
