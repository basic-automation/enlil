//! On-screen mirror of the kernel log.
//!
//! Every kernel log line goes out COM1 ([`crate::serial`]), which most desktop
//! boards do not expose — so on real hardware a first boot would show a blank
//! screen whether it succeeded, hung, or faulted. This module mirrors the same
//! bytes onto the GOP framebuffer through the [`TextConsole`], so progress, the
//! last line before a hang, and fault reports are visible without a cable.
//!
//! Three pieces:
//!
//! - a lock-free **ring** of the most recent log bytes ([`record`]) — always
//!   fed, even before the framebuffer is up, so nothing logged early is lost;
//! - **paged** console writes ([`write_paged`]): when the page fills, clear it
//!   and start at the top instead of scrolling. Scrolling moves the whole
//!   frame buffer once per line, which on an uncached framebuffer costs
//!   minutes over a boot; a page flip every screenful costs a clear;
//! - **tail redraw** ([`tail_start`]): after something else draws over the
//!   screen (the framebuffer self-tests) the last screenful of the ring is
//!   replayed.
//!
//! The ring, the paging policy and the tail search are pure and host-tested;
//! the live framebuffer console is `uefi`-only.

use crate::framebuffer::TextConsole;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// Size of the log ring in bytes (the most recent 64 KiB of log text).
pub const RING_BYTES: usize = 64 * 1024;

static RING: [AtomicU8; RING_BYTES] = [const { AtomicU8::new(0) }; RING_BYTES];
/// Total bytes ever recorded (the next write index, unwrapped).
static HEAD: AtomicUsize = AtomicUsize::new(0);

/// Append one log byte to the ring. Lock-free and safe from any CPU or
/// exception context; concurrent writers each claim a distinct slot. Returns
/// the byte's absolute position in the stream.
pub fn record(byte: u8) -> usize {
    let i = HEAD.fetch_add(1, Ordering::Relaxed);
    RING[i % RING_BYTES].store(byte, Ordering::Relaxed);
    i
}

/// Byte at absolute (unwrapped) position `i` of the ring.
fn ring_byte(i: usize) -> u8 {
    RING[i % RING_BYTES].load(Ordering::Relaxed)
}

/// Absolute position (into the unwrapped stream) where a replay of the last
/// `rows` complete lines should begin.
///
/// `head` is the total bytes recorded, `ring_len` how many of them the ring
/// still holds, and `byte_at` reads absolute position `i`. A trailing newline
/// does not start a new (empty) line. Returns the oldest available position
/// when fewer than `rows` lines exist.
pub fn tail_start(
    head: usize,
    ring_len: usize,
    rows: usize,
    byte_at: impl Fn(usize) -> u8,
) -> usize {
    let oldest = head.saturating_sub(ring_len);
    if rows == 0 {
        return head;
    }
    let mut end = head;
    // Ignore the newline that terminates the final line.
    if end > oldest && byte_at(end - 1) == b'\n' {
        end -= 1;
    }
    let mut lines = 1usize;
    let mut i = end;
    while i > oldest {
        if byte_at(i - 1) == b'\n' {
            if lines >= rows {
                return i;
            }
            lines += 1;
        }
        i -= 1;
    }
    oldest
}

/// The byte to draw for log byte `b`, or `None` to draw nothing.
///
/// The console only has ASCII glyphs, and log text uses UTF-8 punctuation
/// (`—`, `→`). Each multi-byte sequence is drawn as one `-`: the lead byte
/// (`0xC0..`) becomes `-` and the continuation bytes (`0x80..=0xBF`) are
/// dropped. Serial and the ring keep the raw bytes.
#[must_use]
pub const fn screen_byte(b: u8) -> Option<u8> {
    match b {
        0x00..=0x7F => Some(b),
        0x80..=0xBF => None,
        _ => Some(b'-'),
    }
}

/// A read-only view of the log stream.
///
/// Holds the position just past the bytes that may be replayed, and how to read
/// byte `i` of it. Lets the replay and paging logic run over the real ring on
/// hardware and over a plain slice in tests.
pub struct LogView<'a> {
    /// Replay may use positions `[.., end)`.
    pub end: usize,
    /// Read the byte at absolute position `i` (`i < end`).
    pub byte_at: &'a dyn Fn(usize) -> u8,
}

/// Replay up to `lines` of the log just before `view.end` onto `console`.
///
/// Never scrolls: the replay stops two rows short of the bottom, so it can run
/// on a freshly cleared page without triggering anything itself.
pub fn replay_lines(console: &mut TextConsole<'_>, view: &LogView<'_>, lines: usize) {
    let ring_len = view.end.min(RING_BYTES);
    let start = tail_start(view.end, ring_len, lines, view.byte_at);
    for i in start..view.end {
        if console.cursor().1 + 2 >= console.rows() {
            break;
        }
        if let Some(b) = screen_byte((view.byte_at)(i)) {
            console.write_byte(b);
        }
    }
}

/// Write one byte to `console` without ever scrolling.
///
/// When the byte would need a new row (or a wrap) past the last row, the page
/// is cleared and the last quarter-screen of the log (everything in `view`
/// before this byte) is replayed first, so a flip keeps recent context on
/// screen instead of leaving a blank page with a single line.
pub fn write_paged(console: &mut TextConsole<'_>, byte: u8, view: &LogView<'_>) {
    let Some(byte) = screen_byte(byte) else {
        return;
    };
    let (cx, cy) = console.cursor();
    let on_last_row = cy + 1 >= console.rows();
    let flip = match byte {
        b'\n' => on_last_row,
        0x20..=0x7E => on_last_row && cx + 1 >= console.cols(),
        _ => false,
    };
    if flip {
        console.clear();
        let keep = usize::try_from(console.rows() / 4).unwrap_or(0);
        replay_lines(console, view, keep);
    }
    console.write_byte(byte);
}

/// Replay the last screenful of the ring onto `console` (which the caller has
/// just cleared), up to the current end of the log.
pub fn replay_tail(console: &mut TextConsole<'_>) {
    let end = HEAD.load(Ordering::Relaxed);
    let view = LogView {
        end,
        byte_at: &ring_byte,
    };
    let rows = usize::try_from(console.rows())
        .unwrap_or(usize::MAX)
        .saturating_sub(2);
    replay_lines(console, &view, rows);
}

#[cfg(target_os = "uefi")]
pub use hw::{init, mirror, pause, resume};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{LogView, record, replay_tail, ring_byte, write_paged};
    use crate::framebuffer::{Canvas, Color, TextConsole};
    use crate::handoff::Framebuffer;
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, Ordering};

    /// The live framebuffer console, guarded by [`BUSY`].
    struct Live(UnsafeCell<Option<TextConsole<'static>>>);
    // SAFETY: access is serialized by the BUSY try-lock below; a contended
    // writer drops its screen update (the byte is still in the ring).
    unsafe impl Sync for Live {}

    static LIVE: Live = Live(UnsafeCell::new(None));
    static BUSY: AtomicBool = AtomicBool::new(false);
    /// Whether log bytes are currently painted to the screen.
    static ENABLED: AtomicBool = AtomicBool::new(false);

    /// Try to take the console lock; `false` if another CPU/context holds it.
    fn lock() -> bool {
        BUSY.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    fn unlock() {
        BUSY.store(false, Ordering::Release);
    }

    /// Bring the screen mirror up on the handed-off framebuffer: clear it and
    /// replay everything logged so far. Returns `false` if the framebuffer
    /// geometry cannot host a text console.
    ///
    /// # Safety
    ///
    /// `fb` must describe the live GOP linear framebuffer, mapped writable for
    /// the kernel's lifetime (it is identity-mapped on both the firmware's and
    /// the kernel's own page tables).
    pub unsafe fn init(fb: &Framebuffer) -> bool {
        // SAFETY: contract delegated to the caller.
        let Some(canvas) = (unsafe { Canvas::from_framebuffer(fb) }) else {
            return false;
        };
        let Some(console) = TextConsole::new(canvas, Color::WHITE, Color::BLACK) else {
            return false;
        };
        if !lock() {
            return false;
        }
        // SAFETY: BUSY is held, so no other context touches LIVE.
        unsafe { *LIVE.0.get() = Some(console) };
        unlock();
        resume();
        true
    }

    /// Feed one log byte: always into the ring, and onto the screen when the
    /// mirror is live and uncontended. Called by the serial writer.
    pub fn mirror(byte: u8) {
        let pos = record(byte);
        if !ENABLED.load(Ordering::Relaxed) || !lock() {
            return;
        }
        // SAFETY: BUSY is held.
        if let Some(console) = unsafe { (*LIVE.0.get()).as_mut() } {
            let view = LogView {
                end: pos,
                byte_at: &ring_byte,
            };
            write_paged(console, byte, &view);
        }
        unlock();
    }

    /// Stop painting the log (e.g. while a framebuffer self-test draws over
    /// the screen). Bytes keep landing in the ring.
    pub fn pause() {
        ENABLED.store(false, Ordering::Relaxed);
    }

    /// Clear the screen, replay the last screenful of the log, and go live.
    pub fn resume() {
        if !lock() {
            return;
        }
        // SAFETY: BUSY is held.
        if let Some(console) = unsafe { (*LIVE.0.get()).as_mut() } {
            console.clear();
            replay_tail(console);
            ENABLED.store(true, Ordering::Relaxed);
        }
        unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::{Canvas, Color, TextConsole};
    use crate::handoff::PixelFormat;

    fn at(s: &'static [u8]) -> impl Fn(usize) -> u8 {
        move |i| s[i]
    }

    #[test]
    fn screen_byte_maps_utf8_to_one_dash_per_sequence() {
        assert_eq!(screen_byte(b'a'), Some(b'a'));
        assert_eq!(screen_byte(b'\n'), Some(b'\n'));
        // "—" is E2 80 94: lead -> '-', continuations dropped.
        let drawn: Vec<u8> = "a — b".bytes().filter_map(screen_byte).collect();
        assert_eq!(drawn, b"a - b");
        let drawn: Vec<u8> = "uefi→kernel".bytes().filter_map(screen_byte).collect();
        assert_eq!(drawn, b"uefi-kernel");
    }

    #[test]
    fn tail_start_returns_the_beginning_of_the_last_n_lines() {
        let s = b"one\ntwo\nthree\nfour\n";
        // Last 2 complete lines: "three\nfour\n" starts at index 8.
        assert_eq!(tail_start(s.len(), 1024, 2, at(s)), 8);
        // Last 1 line: "four\n" starts at 14.
        assert_eq!(tail_start(s.len(), 1024, 1, at(s)), 14);
    }

    #[test]
    fn tail_start_without_a_trailing_newline_counts_the_partial_line() {
        let s = b"one\ntwo\nthr";
        assert_eq!(tail_start(s.len(), 1024, 2, at(s)), 4); // "two\nthr"
    }

    #[test]
    fn tail_start_with_fewer_lines_than_rows_starts_at_the_oldest_byte() {
        let s = b"a\nb\n";
        assert_eq!(tail_start(s.len(), 1024, 10, at(s)), 0);
    }

    #[test]
    fn tail_start_never_goes_before_the_ring_window() {
        let s = b"aaaa\nbbbb\ncccc\n";
        // Only the last 7 bytes survive in the ring: oldest = 8.
        assert_eq!(tail_start(s.len(), 7, 99, at(s)), 8);
    }

    #[test]
    fn tail_start_zero_rows_replays_nothing() {
        let s = b"x\ny\n";
        assert_eq!(tail_start(s.len(), 1024, 0, at(s)), s.len());
    }

    /// A 16-column x 4-row console over a heap canvas (8x8 cells).
    fn console(buf: &mut [u8]) -> TextConsole<'_> {
        let canvas =
            Canvas::new(buf, 16 * 8, 4 * 8, 16 * 8, 4, PixelFormat::Rgb).expect("valid geometry");
        TextConsole::new(canvas, Color::WHITE, Color::BLACK).expect("console")
    }

    /// A 16-column x 8-row console over a heap canvas (8x8 cells).
    fn console8(buf: &mut [u8]) -> TextConsole<'_> {
        let canvas =
            Canvas::new(buf, 16 * 8, 8 * 8, 16 * 8, 4, PixelFormat::Rgb).expect("valid geometry");
        TextConsole::new(canvas, Color::WHITE, Color::BLACK).expect("console")
    }

    /// Feed `text` through `write_paged`, with the stream so far as the view.
    fn write_all(c: &mut TextConsole<'_>, stream: &mut Vec<u8>, text: &[u8]) {
        for &b in text {
            let pos = stream.len();
            stream.push(b);
            let snapshot = stream.clone();
            let at = move |i: usize| snapshot[i];
            let view = LogView {
                end: pos,
                byte_at: &at,
            };
            write_paged(c, b, &view);
        }
    }

    #[test]
    fn paging_flips_instead_of_scrolling_and_keeps_context() {
        let mut buf = vec![0u8; 16 * 8 * 8 * 8 * 4];
        let mut c = console8(&mut buf); // 8 rows
        let mut log = Vec::new();
        // Fill all 8 rows: lines l0..l7, cursor on the last row (row 7).
        write_all(&mut c, &mut log, b"l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7");
        assert_eq!(c.cursor(), (2, 7));
        // The next newline flips the page: it keeps the last rows/4 = 2 lines
        // (l6, l7) at the top, then moves to the row after them.
        write_all(&mut c, &mut log, b"\n");
        assert_eq!(c.cursor(), (0, 2), "two context lines + a fresh row");
        write_all(&mut c, &mut log, b"x");
        assert_eq!(c.cursor(), (1, 2));
    }

    #[test]
    fn paging_flips_when_a_wrapped_line_would_pass_the_last_row() {
        let mut buf = vec![0u8; 16 * 8 * 4 * 8 * 4];
        let mut c = console(&mut buf);
        let mut log = Vec::new();
        write_all(&mut c, &mut log, b"\n\n\n"); // row 3 (last of 4)
        write_all(&mut c, &mut log, b"0123456789abcde"); // 15 of 16 columns
        assert_eq!(c.cursor(), (15, 3));
        // The 16th cell would wrap past the last row -> flip, keep context,
        // then draw: the cursor must land back inside the page, not scroll.
        write_all(&mut c, &mut log, b"X");
        let (_, row) = c.cursor();
        assert!(row < 3, "page flipped, cursor row {row}");
    }

    #[test]
    fn replay_tail_is_bounded_to_the_screen() {
        // Feeds the shared ring; only checks the replay stays within the page
        // and leaves a free row (no page flip during the replay itself).
        for b in b"one\ntwo\nthree\n" {
            record(*b);
        }
        let mut buf = vec![0u8; 16 * 8 * 4 * 8 * 4];
        let mut c = console(&mut buf);
        replay_tail(&mut c);
        let (_, row) = c.cursor();
        assert!(row < c.rows(), "cursor row {row}");
    }
}
