//! (Neptune) `VICTORY_UM_TEXLOOK=1`: write epochs for texture looks (stage U2).
//!
//! CPU writes already carry a number per 64 KiB page (the write-watch journal, `watch::writes`).
//! What the journal does not see is title memory that is mapped, unmapped, or written by the
//! runtime or by our own copy-back (`descriptor::host::remapped`, `page_written`): those bump
//! the epoch of their pages here. A texture remembers both numbers at its last look and is
//! looked at again only if one advanced.
//! shortcut: pages share slots by address modulo the table (a collision looks too often, never
//! too rarely); ranges over `WIDE` pages bump everything.
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

const SLOTS: usize = 1 << 18;
const WIDE: usize = 1 << 14;
const PAGE: usize = 1 << 16;

static NEXT: AtomicU64 = AtomicU64::new(0);
/// Epoch below which everything counts as changed (a bump of a very wide range).
static FLOOR: AtomicU64 = AtomicU64::new(0);
static PAGES: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

/// Looks done, looks skipped by epoch, looks that found a change, since the last log.
pub(crate) static LOOKS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_UM_TEXLOOK").is_some_and(|v| v == "1"))
}

/// Notes title memory `[at, at + len)` mapped, unmapped or written outside the journal.
pub(crate) fn bump(at: usize, len: usize) {
    if !on() || len == 0 {
        return;
    }
    let e = NEXT.fetch_add(1, Relaxed) + 1;
    let (first, last) = (at / PAGE, at.saturating_add(len).div_ceil(PAGE));
    if last - first > WIDE {
        FLOOR.fetch_max(e, Relaxed);
        return;
    }
    for p in first..last {
        PAGES[p % SLOTS].fetch_max(e, Relaxed);
    }
}

/// The highest epoch of the pages of `[at, at + len)`.
pub(crate) fn max(at: usize, len: usize) -> u64 {
    let (first, last) = (at / PAGE, at.saturating_add(len).div_ceil(PAGE));
    if last - first > WIDE {
        return u64::MAX;
    }
    (first..last).map(|p| PAGES[p % SLOTS].load(Relaxed)).fold(FLOOR.load(Relaxed), u64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bump_advances_only_pages_it_touches() {
        // An address no other test uses (the table is global).
        let at = 0x7ff0_0000_0000usize;
        let before = max(at, 3 * PAGE);
        NEXT.fetch_add(1, Relaxed);
        PAGES[(at / PAGE + 1) % SLOTS].fetch_max(NEXT.load(Relaxed), Relaxed);
        assert!(max(at, 3 * PAGE) > before);
        assert_eq!(max(at + 2 * PAGE, PAGE), max(at + 2 * PAGE, PAGE));
        assert_eq!(max(at + 2 * PAGE, PAGE), FLOOR.load(Relaxed).max(PAGES[(at / PAGE + 2) % SLOTS].load(Relaxed)));
        assert_eq!(max(at, usize::MAX / 2), u64::MAX);
    }
}
