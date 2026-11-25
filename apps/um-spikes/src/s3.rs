//! S3: GetWriteWatch cost over a large MEM_WRITE_WATCH region.
use crate::gpu::*;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};
use windows::Win32::System::Memory::*;
use windows::core::*;

const CHUNK: usize = 256 << 20;
const PAGE: usize = 4096;

fn query(flags: u32, base: *mut u8, len: usize, buf: &mut [*mut c_void]) -> (usize, f64, u32) {
    let mut cnt = buf.len();
    let mut gran = 0u32;
    let t = Instant::now();
    let r = unsafe { GetWriteWatch(flags, base as *const c_void, len, Some(buf.as_mut_ptr()), Some(&mut cnt), Some(&mut gran)) };
    (if r == 0 { cnt } else { usize::MAX }, t.elapsed().as_secs_f64() * 1e3, gran)
}

fn dirty(base: *mut u8, pages: usize, stride: usize) {
    for i in (0..pages).step_by(stride) {
        unsafe { base.add(i * PAGE).write_volatile(1) };
    }
}

pub fn run(gb: u64, free: u64) -> Result<()> {
    let reserve = (gb * GIB) as usize;
    // Commit at most half of the free RAM (rounded down to whole chunks).
    let commit = ((reserve as u64).min(free / 2) as usize / CHUNK * CHUNK).max(CHUNK);
    let base = unsafe { VirtualAlloc(None, reserve, MEM_RESERVE | MEM_WRITE_WATCH, PAGE_READWRITE) } as *mut u8;
    if base.is_null() {
        return Err(Error::from_thread());
    }
    for off in (0..commit).step_by(CHUNK) {
        if unsafe { VirtualAlloc(Some(base.add(off) as *const c_void), CHUNK, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
            return Err(Error::from_thread());
        }
    }
    let pages = commit / PAGE;
    let mut buf = vec![std::ptr::null_mut::<c_void>(); pages / 10 + pages / 40 + 16];

    for (pct, stride) in [(0.1, 1000usize), (1.0, 100), (10.0, 10)] {
        unsafe { ResetWriteWatch(base as *const c_void, commit) };
        dirty(base, pages, stride);
        let (n1, ms, gran) = query(0, base, commit, &mut buf);
        let (n2, ms_reset, _) = query(WRITE_WATCH_FLAG_RESET, base, commit, &mut buf);
        let (n3, ms_empty, _) = query(0, base, commit, &mut buf);
        spike!(
            "s3",
            "reserved_gib={:.1} committed_gib={:.1} dirty_pct={pct} dirty_pages={} found={n1} found_reset={n2} after_reset={n3} granularity={gran} get_ms={ms:.2} get_reset_ms={ms_reset:.2} get_empty_ms={ms_empty:.2}",
            reserve as f64 / GIB as f64, commit as f64 / GIB as f64, pages.div_ceil(stride)
        );
    }

    // A second thread streams memcpy while the main thread asks the kernel (1 % dirty per round).
    let stop = AtomicBool::new(false);
    let bytes = AtomicU64::new(0);
    let rate = |dur: Duration, work: &mut dyn FnMut()| {
        let b0 = bytes.load(Relaxed);
        let t = Instant::now();
        while t.elapsed() < dur {
            work();
        }
        (bytes.load(Relaxed) - b0) as f64 / t.elapsed().as_secs_f64()
    };
    let (idle, with_query, dirty_only) = std::thread::scope(|s| {
        s.spawn(|| {
            let (a, mut b) = (vec![1u8; 64 << 20], vec![0u8; 64 << 20]);
            while !stop.load(Relaxed) {
                for (x, y) in a.chunks(1 << 20).zip(b.chunks_mut(1 << 20)) {
                    y.copy_from_slice(x);
                    bytes.fetch_add(1 << 20, Relaxed);
                }
            }
        });
        let d = Duration::from_millis(1000);
        let idle = rate(d, &mut || std::thread::sleep(Duration::from_millis(10)));
        let wq = rate(d, &mut || {
            dirty(base, pages, 100);
            query(WRITE_WATCH_FLAG_RESET, base, commit, &mut buf);
        });
        let dd = rate(d, &mut || dirty(base, pages, 100));
        stop.store(true, Relaxed);
        (idle, wq, dd)
    });
    spike!(
        "s3",
        "worker_gbps_idle={:.2} worker_slowdown_pct_dirty_and_query={:.1} worker_slowdown_pct_dirty_only={:.1}",
        idle / 1e9, (1.0 - with_query / idle) * 100.0, (1.0 - dirty_only / idle) * 100.0
    );
    unsafe {
        let _ = VirtualFree(base as *mut c_void, 0, MEM_RELEASE);
    }
    Ok(())
}
