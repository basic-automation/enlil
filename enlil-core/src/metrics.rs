//! Performance metrics collection for the management console (roadmap 8.5).
//!
//! The console's guest tab claims per-guest CPU/memory display; this module
//! makes it real. Two halves:
//!
//! * Host sampling ([`HostSampler`]): aggregate host CPU %, memory pressure
//!   and swap use from `/proc/stat` + `/proc/meminfo`, plus whole-disk IO
//!   throughput from `/proc/diskstats`. Needs no KVM, so it works on any
//!   Linux host.
//! * Per-guest accounting ([`MetricsRegistry`]): the hypervisor's vCPU run
//!   loop accumulates guest-execution nanoseconds and block-device backends
//!   accumulate IO bytes into [`GuestMetrics`]; the registry turns one
//!   sampling window into the [`ServerMessage::StatusUpdate`] the console
//!   renders.
//!
//! Everything is wall-clock rate math over counter deltas. The first sample
//! after registration has no window yet, so it reports zeros rather than a
//! bogus spike.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use enlil_mgmt_proto::{GuestState, GuestStatus, HostMetrics, ServerMessage};

// ---------------------------------------------------------------------------
// Small math helpers
// ---------------------------------------------------------------------------

/// `part / whole * 100`, clamped to 0.0–100.0; 0.0 when `whole` is 0.
#[allow(clippy::cast_precision_loss)] // jiffy/byte counters never approach 2^53
fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    (part as f64 / whole as f64 * 100.0).clamp(0.0, 100.0)
}

/// Whole bytes-per-second over a fractional-second window.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn rate_per_sec(bytes: u64, secs: f64) -> u64 {
    // Informational rate: sub-byte precision is meaningless here.
    (bytes as f64 / secs) as u64
}

/// [`Duration`] → whole nanoseconds without a lossy `as` cast.
fn duration_nanos(d: Duration) -> u64 {
    d.as_secs()
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::from(d.subsec_nanos()))
}

// ---------------------------------------------------------------------------
// /proc parsing
// ---------------------------------------------------------------------------

/// Block devices we never count toward host IO (virtual or stacked devices
/// whose bytes would double-count the underlying disk).
fn is_virtual_disk(name: &str) -> bool {
    name.starts_with("loop")
        || name.starts_with("ram")
        || name.starts_with("zram")
        || name.starts_with("dm-")
}

/// A block device counts toward host IO when it is a whole disk rather than a
/// partition: whole disks appear directly under `/sys/block`, partitions do
/// not.
fn is_whole_disk(name: &str) -> bool {
    !is_virtual_disk(name) && Path::new("/sys/block").join(name).exists()
}

#[derive(Debug, Clone, Copy, Default)]
struct CpuTimes {
    busy: u64,
    total: u64,
}

/// Parse the aggregate `cpu` line of `/proc/stat` into (busy, total) jiffies.
///
/// `busy = total - idle - iowait`; guest times are already folded into the
/// user/nice fields per `proc(5)`, so the first eight fields suffice.
fn parse_cpu_times(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let mut fields = line
        .split_whitespace()
        .skip(1)
        .map(|f| f.parse::<u64>().unwrap_or(0));
    let user = fields.next()?;
    let nice = fields.next()?;
    let system = fields.next()?;
    let idle = fields.next()?;
    let iowait = fields.next().unwrap_or(0);
    let irq = fields.next().unwrap_or(0);
    let softirq = fields.next().unwrap_or(0);
    let steal = fields.next().unwrap_or(0);
    let total = user + nice + system + idle + iowait + irq + softirq + steal;
    Some(CpuTimes {
        busy: total.saturating_sub(idle).saturating_sub(iowait),
        total,
    })
}

/// Parse `/proc/meminfo`, returning (memory-pressure %, swap-used MiB).
///
/// Pressure is `(MemTotal - MemAvailable) / MemTotal * 100` — how hard the
/// host is squeezing, which is what an operator sizing guests cares about.
fn parse_meminfo(meminfo: &str) -> Option<(f64, u64)> {
    let mut total_kb = None;
    let mut avail_kb = None;
    let mut swap_total_kb = 0u64;
    let mut swap_free_kb = 0u64;
    for line in meminfo.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next()?;
        let value: u64 = parts.next()?.parse().ok()?;
        match key {
            "MemTotal:" => total_kb = Some(value),
            "MemAvailable:" => avail_kb = Some(value),
            "SwapTotal:" => swap_total_kb = value,
            "SwapFree:" => swap_free_kb = value,
            _ => {}
        }
    }
    let total = total_kb?;
    let avail = avail_kb?;
    if total == 0 {
        return None;
    }
    let pressure = percent(total.saturating_sub(avail), total);
    let swap_used_mib = swap_total_kb.saturating_sub(swap_free_kb) / 1024;
    Some((pressure, swap_used_mib))
}

/// Parse `/proc/diskstats` into per-device (read bytes, write bytes).
///
/// The sector counters are fields 6 and 10 (1-based) of each line and each
/// sector is 512 bytes. Only devices accepted by `is_whole_disk` are kept so
/// partitions and stacked devices are not double-counted.
fn parse_diskstats(
    diskstats: &str,
    is_whole_disk: &dyn Fn(&str) -> bool,
) -> HashMap<String, (u64, u64)> {
    let mut out = HashMap::new();
    for line in diskstats.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 14 {
            continue;
        }
        let name = fields[2];
        if !is_whole_disk(name) {
            continue;
        }
        let read_sectors: u64 = fields[5].parse().unwrap_or(0);
        let write_sectors: u64 = fields[9].parse().unwrap_or(0);
        out.insert(
            name.to_string(),
            (
                read_sectors.saturating_mul(512),
                write_sectors.saturating_mul(512),
            ),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Host sampling
// ---------------------------------------------------------------------------

/// Samples host-wide CPU, memory-pressure and disk-IO from `/proc`.
///
/// All rates are deltas between consecutive [`sample`](Self::sample) calls;
/// the first call reports 0 for every rate so the console never shows a
/// startup spike.
#[derive(Debug, Default)]
pub struct HostSampler {
    prev_cpu: Option<CpuTimes>,
    prev_disk: HashMap<String, (u64, u64)>,
    prev_at: Option<Instant>,
}

impl HostSampler {
    /// A sampler with no history yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sample the live host: `/proc/stat`, `/proc/meminfo`, `/proc/diskstats`.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] when any of the proc files cannot be read.
    pub fn sample(&mut self) -> io::Result<HostMetrics> {
        let stat = fs::read_to_string("/proc/stat")?;
        let meminfo = fs::read_to_string("/proc/meminfo")?;
        let diskstats = fs::read_to_string("/proc/diskstats")?;
        Ok(self.sample_str(&stat, &meminfo, &diskstats, Instant::now()))
    }

    /// [`sample`](Self::sample) over caller-supplied proc contents (the seam
    /// the unit tests drive).
    fn sample_str(
        &mut self,
        stat: &str,
        meminfo: &str,
        diskstats: &str,
        now: Instant,
    ) -> HostMetrics {
        self.sample_parts(stat, meminfo, diskstats, now, &is_whole_disk)
    }

    /// Core sampling math with an injectable whole-disk predicate so tests
    /// do not depend on the host's `/sys/block`.
    fn sample_parts(
        &mut self,
        stat: &str,
        meminfo: &str,
        diskstats: &str,
        now: Instant,
        is_whole_disk: &dyn Fn(&str) -> bool,
    ) -> HostMetrics {
        let cpu = parse_cpu_times(stat).unwrap_or_default();
        let (pressure, swap_mib) = parse_meminfo(meminfo).unwrap_or((0.0, 0));
        let disks = parse_diskstats(diskstats, is_whole_disk);

        let mut metrics = HostMetrics {
            cpu_percent: 0.0,
            memory_pressure_percent: pressure,
            swap_used_mib: swap_mib,
            disk_read_bytes_per_sec: 0,
            disk_write_bytes_per_sec: 0,
        };

        if let (Some(prev_cpu), Some(prev_at)) = (self.prev_cpu, self.prev_at) {
            let busy_delta = cpu.busy.saturating_sub(prev_cpu.busy);
            let total_delta = cpu.total.saturating_sub(prev_cpu.total);
            metrics.cpu_percent = percent(busy_delta, total_delta);

            let elapsed = now.saturating_duration_since(prev_at).as_secs_f64();
            if elapsed > 0.0 {
                let mut read_delta = 0u64;
                let mut write_delta = 0u64;
                for (name, (read_bytes, write_bytes)) in &disks {
                    if let Some((prev_read, prev_write)) = self.prev_disk.get(name) {
                        read_delta =
                            read_delta.saturating_add(read_bytes.saturating_sub(*prev_read));
                        write_delta =
                            write_delta.saturating_add(write_bytes.saturating_sub(*prev_write));
                    }
                }
                metrics.disk_read_bytes_per_sec = rate_per_sec(read_delta, elapsed);
                metrics.disk_write_bytes_per_sec = rate_per_sec(write_delta, elapsed);
            }
        }

        self.prev_cpu = Some(cpu);
        self.prev_disk = disks;
        self.prev_at = Some(now);
        metrics
    }
}

// ---------------------------------------------------------------------------
// Per-guest accounting
// ---------------------------------------------------------------------------

/// Previous-sample state for one guest's rate math.
#[derive(Debug, Default)]
struct GuestSampleState {
    at: Option<Instant>,
    exec_nanos: u64,
    read_bytes: u64,
    write_bytes: u64,
}

/// One sampling window for a guest, as the console displays it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuestSample {
    /// Guest-CPU busy % normalized across the guest's vCPUs (0.0–100.0,
    /// where 100 means every vCPU saturated the whole window).
    pub cpu_percent: f64,
    /// Memory pressure: used / total * 100.
    pub memory_pressure_percent: f64,
    /// Disk read throughput, bytes/sec.
    pub io_read_bytes_per_sec: u64,
    /// Disk write throughput, bytes/sec.
    pub io_write_bytes_per_sec: u64,
}

/// Counters the hypervisor feeds for one guest.
///
/// The vCPU run loop adds measured guest-execution time via
/// [`note_guest_exec`](Self::note_guest_exec`) once per guest entry/exit
/// slice; block-device backends add IO bytes via
/// [`note_io_read`](Self::note_io_read)/[`note_io_write`](Self::note_io_write).
/// [`sample`](Self::sample) turns the counter deltas into the per-second
/// rates the console displays. All counters are atomic so the run loop,
/// device backends, and the sampling thread never block each other.
#[derive(Debug)]
pub struct GuestMetrics {
    guest_id: String,
    name: String,
    total_mem_mib: u64,
    vcpu_count: u32,
    exec_nanos: AtomicU64,
    io_read_bytes: AtomicU64,
    io_write_bytes: AtomicU64,
    prev: Mutex<GuestSampleState>,
}

impl GuestMetrics {
    /// Fresh counters for a guest (no history: the first sample is zeros).
    #[must_use]
    pub fn new(guest_id: &str, name: &str, total_mem_mib: u64, vcpu_count: u32) -> Self {
        Self {
            guest_id: guest_id.to_string(),
            name: name.to_string(),
            total_mem_mib,
            vcpu_count,
            exec_nanos: AtomicU64::new(0),
            io_read_bytes: AtomicU64::new(0),
            io_write_bytes: AtomicU64::new(0),
            prev: Mutex::new(GuestSampleState::default()),
        }
    }

    /// The guest this handle accounts for.
    #[must_use]
    pub fn guest_id(&self) -> &str {
        &self.guest_id
    }

    /// The guest's display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The guest's total configured memory in MiB.
    #[must_use]
    pub const fn total_mem_mib(&self) -> u64 {
        self.total_mem_mib
    }

    /// Add measured guest-execution time (called by the vCPU run loop once
    /// per guest entry/exit slice).
    pub fn note_guest_exec(&self, elapsed: Duration) {
        self.exec_nanos
            .fetch_add(duration_nanos(elapsed), Ordering::Relaxed);
    }

    /// Add bytes the guest read from a block device.
    pub fn note_io_read(&self, bytes: u64) {
        self.io_read_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Add bytes the guest wrote to a block device.
    pub fn note_io_write(&self, bytes: u64) {
        self.io_write_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Sample this guest's rates over the window since the previous sample.
    /// `memory_used_mib` is read from the guest's memory manager at sample
    /// time. The first call returns zeros (no window yet).
    #[must_use]
    pub fn sample(&self, now: Instant, memory_used_mib: u64) -> GuestSample {
        let exec = self.exec_nanos.load(Ordering::Relaxed);
        let read = self.io_read_bytes.load(Ordering::Relaxed);
        let written = self.io_write_bytes.load(Ordering::Relaxed);
        let mut prev = self.prev.lock().expect("metrics mutex poisoned");

        let mut sample = GuestSample {
            cpu_percent: 0.0,
            memory_pressure_percent: percent(memory_used_mib, self.total_mem_mib),
            io_read_bytes_per_sec: 0,
            io_write_bytes_per_sec: 0,
        };

        if let Some(at) = prev.at {
            let wall_nanos = duration_nanos(now.saturating_duration_since(at));
            let exec_delta = exec.saturating_sub(prev.exec_nanos);
            if wall_nanos > 0 && self.vcpu_count > 0 {
                // Normalize across vCPUs so 100% means "all vCPUs saturated".
                let capacity = u64::from(self.vcpu_count).saturating_mul(wall_nanos);
                sample.cpu_percent = percent(exec_delta, capacity);
            }
            let elapsed = now.saturating_duration_since(at).as_secs_f64();
            if elapsed > 0.0 {
                sample.io_read_bytes_per_sec =
                    rate_per_sec(read.saturating_sub(prev.read_bytes), elapsed);
                sample.io_write_bytes_per_sec =
                    rate_per_sec(written.saturating_sub(prev.write_bytes), elapsed);
            }
        }

        *prev = GuestSampleState {
            at: Some(now),
            exec_nanos: exec,
            read_bytes: read,
            write_bytes: written,
        };
        sample
    }
}

// ---------------------------------------------------------------------------
// Registry: the daemon's handle on all guests' metrics
// ---------------------------------------------------------------------------

/// The daemon's per-guest facts for one status window; combined with the
/// sampled [`GuestMetrics`] by [`MetricsRegistry::status_update`].
#[derive(Debug, Clone, Copy)]
pub struct GuestInfo<'a> {
    /// Matches the [`GuestMetrics`] registration key.
    pub id: &'a str,
    /// Display name for the console.
    pub name: &'a str,
    /// Lifecycle state for the console.
    pub state: GuestState,
    /// Bytes currently allocated to the guest (from its memory manager).
    pub memory_used_mib: u64,
    /// Seconds since the guest started (0 when not running).
    pub uptime_secs: u64,
}

/// Owns the [`GuestMetrics`] for every live guest.
///
/// The daemon holds one registry and hands the `Arc<GuestMetrics>` for a
/// guest to that guest's vCPU run loop and block-device backends; on each
/// console refresh tick it calls [`status_update`](Self::status_update) to
/// build the wire message the console renders.
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    guests: RwLock<HashMap<String, Arc<GuestMetrics>>>,
}

impl MetricsRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a guest and return the handle its run loop and device
    /// backends feed. Re-registering an id replaces the old handle (and its
    /// history).
    pub fn register(
        &self,
        guest_id: &str,
        name: &str,
        total_mem_mib: u64,
        vcpu_count: u32,
    ) -> Arc<GuestMetrics> {
        let metrics = Arc::new(GuestMetrics::new(guest_id, name, total_mem_mib, vcpu_count));
        self.guests
            .write()
            .expect("metrics registry poisoned")
            .insert(guest_id.to_string(), Arc::clone(&metrics));
        metrics
    }

    /// Drop a guest's counters (on guest destroy).
    pub fn unregister(&self, guest_id: &str) {
        self.guests
            .write()
            .expect("metrics registry poisoned")
            .remove(guest_id);
    }

    /// The live handle for `guest_id`, if registered.
    #[must_use]
    pub fn get(&self, guest_id: &str) -> Option<Arc<GuestMetrics>> {
        self.guests
            .read()
            .expect("metrics registry poisoned")
            .get(guest_id)
            .cloned()
    }

    /// Build the console's [`ServerMessage::StatusUpdate`] from one sampling
    /// window: samples every guest in `infos` and converts the result to the
    /// wire [`GuestStatus`]. Guests in `infos` with no registered metrics get
    /// zeroed counters (CPU/memory/IO all 0, memory total unknown).
    #[must_use]
    pub fn status_update(&self, now: Instant, infos: &[GuestInfo<'_>]) -> ServerMessage {
        let guests = self.guests.read().expect("metrics registry poisoned");
        let statuses = infos
            .iter()
            .map(|info| {
                let mut status = GuestStatus {
                    id: info.id.to_string(),
                    name: info.name.to_string(),
                    state: info.state,
                    cpu_percent: 0.0,
                    memory_used_mib: info.memory_used_mib,
                    memory_total_mib: 0,
                    uptime_secs: info.uptime_secs,
                    io_read_bytes_per_sec: 0,
                    io_write_bytes_per_sec: 0,
                };
                if let Some(metrics) = guests.get(info.id) {
                    let sample = metrics.sample(now, info.memory_used_mib);
                    status.cpu_percent = sample.cpu_percent;
                    status.memory_total_mib = metrics.total_mem_mib();
                    status.io_read_bytes_per_sec = sample.io_read_bytes_per_sec;
                    status.io_write_bytes_per_sec = sample.io_write_bytes_per_sec;
                }
                status
            })
            .collect();
        ServerMessage::StatusUpdate(statuses)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // stat A: busy = 100+0+100 = 200, total = 1000.
    const STAT_A: &str = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 50 0 50 400 0 0 0 0 0 0\n";
    // stat B: busy = 150+0+150 = 300, total = 1200 → deltas 100/200 = 50%.
    const STAT_B: &str = "cpu  150 0 150 900 0 0 0 0 0 0\n";
    const MEMINFO: &str = "\
MemTotal:        8000000 kB
MemFree:         1000000 kB
MemAvailable:    2000000 kB
SwapTotal:       1000000 kB
SwapFree:         800000 kB
";
    // sda: 2000/4000 sectors; sda1 (partition): 1000/1000; loop0: virtual.
    const DISKSTATS_A: &str = "\
   8       0 sda 0 0 2000 0 0 0 4000 0 0 0 0 0 0 0
   8       1 sda1 0 0 1000 0 0 0 1000 0 0 0 0 0 0 0
   7       0 loop0 0 0 5000 0 0 0 5000 0 0 0 0 0 0 0
";
    // sda doubled: read 2_048_000 B, write 4_096_000 B total.
    const DISKSTATS_B: &str = "\
   8       0 sda 0 0 4000 0 0 0 8000 0 0 0 0 0 0 0
   8       1 sda1 0 0 1000 0 0 0 1000 0 0 0 0 0 0 0
   7       0 loop0 0 0 9000 0 0 0 9000 0 0 0 0 0 0 0
";

    /// Test seam: only `sda` counts as a whole disk.
    fn sda_only(name: &str) -> bool {
        name == "sda"
    }

    #[test]
    fn first_sample_reports_zero_rates_but_live_pressure() {
        let mut sampler = HostSampler::new();
        let m = sampler.sample_str(STAT_A, MEMINFO, DISKSTATS_A, Instant::now());
        assert_eq!(m.cpu_percent, 0.0, "no window yet: no CPU rate");
        assert_eq!(m.disk_read_bytes_per_sec, 0);
        assert_eq!(m.disk_write_bytes_per_sec, 0);
        // Pressure is a point-in-time ratio, not a rate: live on first sample.
        assert_eq!(m.memory_pressure_percent, 75.0);
    }

    #[test]
    fn second_sample_computes_cpu_and_disk_rates() {
        let mut sampler = HostSampler::new();
        let t0 = Instant::now();
        let _ = sampler.sample_parts(STAT_A, MEMINFO, DISKSTATS_A, t0, &sda_only);
        let t1 = t0 + Duration::from_secs(2);
        let m = sampler.sample_parts(STAT_B, MEMINFO, DISKSTATS_B, t1, &sda_only);
        assert_eq!(m.cpu_percent, 50.0, "busy delta 100 / total delta 200");
        // sda read delta: (4000-2000)*512 = 1_024_000 B over 2 s.
        assert_eq!(m.disk_read_bytes_per_sec, 512_000);
        // sda write delta: (8000-4000)*512 = 2_048_000 B over 2 s.
        assert_eq!(m.disk_write_bytes_per_sec, 1_024_000);
    }

    #[test]
    fn diskstats_skips_partitions_and_virtual_disks() {
        let disks = parse_diskstats(DISKSTATS_A, &sda_only);
        assert_eq!(disks.len(), 1, "only sda passes the test predicate");
        assert_eq!(disks["sda"], (2000 * 512, 4000 * 512));

        // The production predicate keeps whole disks, drops partitions and
        // loop/ram/dm devices by name even before the /sys/block check.
        assert!(is_virtual_disk("loop0"));
        assert!(is_virtual_disk("dm-0"));
        assert!(!is_virtual_disk("sda"));
        assert!(!is_virtual_disk("nvme0n1"));
    }

    #[test]
    fn meminfo_pressure_math() {
        let (pressure, swap_mib) = parse_meminfo(MEMINFO).unwrap();
        assert_eq!(pressure, 75.0, "(8M - 2M) / 8M");
        assert_eq!(swap_mib, 195, "(1M - 800k) kB / 1024");
    }

    #[test]
    fn meminfo_missing_fields_yield_no_sample() {
        assert!(parse_meminfo("MemTotal: 8000000 kB\n").is_none());
        assert!(parse_meminfo("").is_none());
        // Zero total is not a pressure of NaN.
        assert!(parse_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB\n").is_none());
    }

    #[test]
    fn cpu_line_must_be_the_aggregate() {
        // Per-CPU lines must not be mistaken for the aggregate.
        assert!(parse_cpu_times("cpu0 50 0 50 400 0 0 0 0 0 0\n").is_none());
        assert!(parse_cpu_times("").is_none());
        let t = parse_cpu_times(STAT_A).unwrap();
        assert_eq!((t.busy, t.total), (200, 1000));
    }

    #[test]
    fn guest_cpu_normalizes_across_vcpus() {
        let g = GuestMetrics::new("g1", "linux", 1024, 2);
        let t0 = Instant::now();
        let first = g.sample(t0, 512);
        assert_eq!(first.cpu_percent, 0.0, "first sample has no window");

        // 1 s of guest execution over a 1 s window on 2 vCPUs = 50%.
        g.note_guest_exec(Duration::from_secs(1));
        let s = g.sample(t0 + Duration::from_secs(1), 512);
        assert_eq!(s.cpu_percent, 50.0);
        assert_eq!(s.memory_pressure_percent, 50.0, "512 / 1024 MiB");
    }

    #[test]
    fn guest_io_rates_come_from_counter_deltas() {
        let g = GuestMetrics::new("g1", "linux", 1024, 1);
        let t0 = Instant::now();
        let _ = g.sample(t0, 0);
        g.note_io_read(3_000_000);
        g.note_io_write(1_000_000);
        let s = g.sample(t0 + Duration::from_secs(2), 0);
        assert_eq!(s.io_read_bytes_per_sec, 1_500_000);
        assert_eq!(s.io_write_bytes_per_sec, 500_000);
        // Counters keep accumulating; a quiet window reports zero.
        let s2 = g.sample(t0 + Duration::from_secs(3), 0);
        assert_eq!(s2.io_read_bytes_per_sec, 0);
        assert_eq!(s2.io_write_bytes_per_sec, 0);
    }

    #[test]
    fn guest_sample_with_zero_vcpus_does_not_divide_by_zero() {
        let g = GuestMetrics::new("g1", "linux", 1024, 0);
        let t0 = Instant::now();
        let _ = g.sample(t0, 512);
        g.note_guest_exec(Duration::from_secs(10));
        let s = g.sample(t0 + Duration::from_secs(1), 512);
        assert_eq!(s.cpu_percent, 0.0);
    }

    #[test]
    fn registry_status_update_wires_proto_fields() {
        let registry = MetricsRegistry::new();
        let handle = registry.register("vm1", "Ubuntu", 4096, 2);
        assert!(Arc::ptr_eq(&handle, &registry.get("vm1").unwrap()));

        let t0 = Instant::now();
        let infos = [GuestInfo {
            id: "vm1",
            name: "Ubuntu",
            state: GuestState::Running,
            memory_used_mib: 1024,
            uptime_secs: 60,
        }];
        // First window: zeros.
        let ServerMessage::StatusUpdate(first) = registry.status_update(t0, &infos) else {
            panic!("expected StatusUpdate");
        };
        assert_eq!(first[0].cpu_percent, 0.0);

        // Second window: 2 s of guest exec over 2 s on 2 vCPUs = 50%;
        // 4 MiB read over 2 s = 2 MiB/s.
        handle.note_guest_exec(Duration::from_secs(2));
        handle.note_io_read(4 * 1024 * 1024);
        let ServerMessage::StatusUpdate(second) =
            registry.status_update(t0 + Duration::from_secs(2), &infos)
        else {
            panic!("expected StatusUpdate");
        };
        let st = &second[0];
        assert_eq!(st.id, "vm1");
        assert_eq!(st.state, GuestState::Running);
        assert_eq!(st.cpu_percent, 50.0);
        assert_eq!(st.memory_used_mib, 1024);
        assert_eq!(st.memory_total_mib, 4096);
        assert_eq!(st.uptime_secs, 60);
        assert_eq!(st.io_read_bytes_per_sec, 2 * 1024 * 1024);
        assert_eq!(st.io_write_bytes_per_sec, 0);
    }

    #[test]
    fn unregistered_guest_gets_zeroed_status() {
        let registry = MetricsRegistry::new();
        let infos = [GuestInfo {
            id: "ghost",
            name: "Ghost",
            state: GuestState::Stopped,
            memory_used_mib: 0,
            uptime_secs: 0,
        }];
        let ServerMessage::StatusUpdate(statuses) = registry.status_update(Instant::now(), &infos)
        else {
            panic!("expected StatusUpdate");
        };
        let st = &statuses[0];
        assert_eq!(st.cpu_percent, 0.0);
        assert_eq!(st.memory_total_mib, 0);
        assert_eq!(st.io_read_bytes_per_sec, 0);
    }

    #[test]
    fn unregister_drops_the_handle() {
        let registry = MetricsRegistry::new();
        registry.register("vm1", "Ubuntu", 1024, 1);
        assert!(registry.get("vm1").is_some());
        registry.unregister("vm1");
        assert!(registry.get("vm1").is_none());
    }

    #[test]
    fn live_host_sample_is_sane() {
        // Needs no KVM: pure /proc reads. May fail only on non-Linux.
        let mut sampler = HostSampler::new();
        let first = sampler.sample().expect("/proc must be readable");
        assert_eq!(first.cpu_percent, 0.0);
        assert!((0.0..=100.0).contains(&first.memory_pressure_percent));
        std::thread::sleep(Duration::from_millis(100));
        let second = sampler.sample().expect("/proc must be readable");
        assert!((0.0..=100.0).contains(&second.cpu_percent));
        assert!((0.0..=100.0).contains(&second.memory_pressure_percent));
    }
}
