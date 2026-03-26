"""Fix remaining 9 clippy warnings."""
import re

def fix_file(path, replacements):
    with open(path, 'r', encoding='utf-8', errors='replace') as f:
        content = f.read()
    for old, new in replacements:
        if old not in content:
            print(f"  WARNING: pattern not found in {path}")
            print(f"    looking for: {repr(old[:80])}")
            continue
        content = content.replace(old, new, 1)
        print(f"  Fixed in {path}")
    with open(path, 'w', encoding='utf-8') as f:
        f.write(content)

# 1. enlil-platform/src/threading/mod.rs
print("Fixing enlil-platform/src/threading/mod.rs...")
fix_file('D:/Development/enlil/enlil-platform/src/threading/mod.rs', [
    # Fix 1: Debug impl missing fields - use finish_non_exhaustive
    (
        """impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("name", &self.name)
            .field("priority", &self.priority)
            .field("affinity", &self.affinity)
            .field("pending", &self.is_pending())
            .finish()
    }
}""",
        """impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("name", &self.name)
            .field("priority", &self.priority)
            .field("affinity", &self.affinity)
            .field("pending", &self.is_pending())
            .finish_non_exhaustive()
    }
}"""
    ),
    # Fix 2: spawn_hosted missing # Panics doc
    (
        """/// Spawn a task on a real OS thread (hosted mode).
///
/// This is the backend used when running under a hosted OS (Linux, Windows,
/// macOS).  Each task gets its own `std::thread`.
#[must_use]
pub fn spawn_hosted(task: Task) -> std::thread::JoinHandle<()> {""",
        """/// Spawn a task on a real OS thread (hosted mode).
///
/// This is the backend used when running under a hosted OS (Linux, Windows,
/// macOS).  Each task gets its own `std::thread`.
///
/// # Panics
///
/// Panics if the OS thread cannot be spawned.
#[must_use]
pub fn spawn_hosted(task: Task) -> std::thread::JoinHandle<()> {"""
    ),
    # Fix 3: submit missing # Panics and # Errors doc
    (
        """    /// Submit a task, respecting its affinity.
    pub fn submit(&self, task: Task) -> Result<(), &'static str> {""",
        """    /// Submit a task, respecting its affinity.
    ///
    /// # Errors
    ///
    /// Returns an error if the pinned CPU index is out of range or
    /// no valid CPU exists in the affinity set.
    ///
    /// # Panics
    ///
    /// Panics if a queue mutex is poisoned.
    pub fn submit(&self, task: Task) -> Result<(), &'static str> {"""
    ),
    # Fix 4: run_one missing # Panics doc
    (
        """    /// Run one task on the given CPU. Returns `true` if work was found.
    ///
    /// If the local queue is empty, attempts to steal from the busiest
    /// neighbour.
    #[must_use]
    pub fn run_one(&self, cpu: usize) -> bool {""",
        """    /// Run one task on the given CPU. Returns `true` if work was found.
    ///
    /// If the local queue is empty, attempts to steal from the busiest
    /// neighbour.
    ///
    /// # Panics
    ///
    /// Panics if a queue mutex is poisoned.
    #[must_use]
    pub fn run_one(&self, cpu: usize) -> bool {"""
    ),
    # Fix 5: total_pending missing # Panics doc
    (
        """    /// Total pending tasks across all CPUs.
    #[must_use]
    pub fn total_pending(&self) -> usize {""",
        """    /// Total pending tasks across all CPUs.
    ///
    /// # Panics
    ///
    /// Panics if a queue mutex is poisoned.
    #[must_use]
    pub fn total_pending(&self) -> usize {"""
    ),
])

# 2. enlil-devices/src/display/mod.rs
print("Fixing enlil-devices/src/display/mod.rs...")
fix_file('D:/Development/enlil/enlil-devices/src/display/mod.rs', [
    # Fix 6: type_complexity - add type alias
    (
        """/// VirtIO-GPU framebuffer source
#[derive(Debug)]
pub struct VirtioGpuSource {
    frames: Arc<RwLock<VecDeque<(u64, Vec<u8>)>>>,""",
        """/// Shared frame buffer: list of `(frame_id, pixel_data)` pairs.
type FrameQueue = Arc<RwLock<VecDeque<(u64, Vec<u8>)>>>;

/// VirtIO-GPU framebuffer source
#[derive(Debug)]
pub struct VirtioGpuSource {
    frames: FrameQueue,"""
    ),
    # Fix 7: field_reassign_with_default in test
    (
        """        let mut config = DisplayConfig::default();
        config.width = 1920;
        config.height = 1080;
        let compositor = DisplayCompositor::new(config);""",
        """        let config = DisplayConfig {
            width: 1920,
            height: 1080,
            ..DisplayConfig::default()
        };
        let compositor = DisplayCompositor::new(config);"""
    ),
])

# 3. enlil-core/src/vcpu.rs
print("Fixing enlil-core/src/vcpu.rs...")
fix_file('D:/Development/enlil/enlil-core/src/vcpu.rs', [
    # Fix 8: field_reassign_with_default in test
    (
        """        let mut ctx = VcpuContext::default();
        ctx.rip = 0xDEADBEEF;
        ctx.gp_regs.rax = 42;
        sched.save_context("guest1", 0, ctx);""",
        """        let mut ctx = VcpuContext {
            rip: 0xDEADBEEF,
            ..VcpuContext::default()
        };
        ctx.gp_regs.rax = 42;
        sched.save_context("guest1", 0, ctx);"""
    ),
])

print("Done.")
