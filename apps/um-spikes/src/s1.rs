//! S1: OpenExistingHeapFromAddress on VirtualAlloc memory, with and without MEM_WRITE_WATCH.
use crate::gpu::*;
use std::ffi::c_void;
use std::time::Instant;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Memory::*;
use windows::core::*;

struct Region(*mut c_void);
impl Drop for Region {
    fn drop(&mut self) {
        unsafe {
            let _ = VirtualFree(self.0, 0, MEM_RELEASE);
        }
    }
}

pub fn run(g: &Gpu, mib: u64) -> Result<()> {
    let size = mib << 20;
    let dst = g.default_buf(size)?;
    for watch in [true, false] {
        for api in [0, 1] {
            if let Err(e) = variant(g, size, watch, api, &dst) {
                spike!("s1", "watch={} api={} error={}", watch as u8, api, es(&e));
            }
        }
    }
    Ok(())
}

fn variant(g: &Gpu, size: u64, watch: bool, api: u32, dst: &ID3D12Resource) -> Result<()> {
    unsafe {
        let mut fl = MEM_RESERVE | MEM_COMMIT;
        if watch {
            fl |= MEM_WRITE_WATCH;
        }
        let p = VirtualAlloc(None, size as usize, fl, PAGE_READWRITE);
        if p.is_null() {
            return Err(Error::from_thread());
        }
        let _free = Region(p);
        let mem = std::slice::from_raw_parts_mut(p as *mut u8, size as usize);
        for (i, page) in mem.chunks_mut(4096).enumerate() {
            page.fill(i as u8);
        }

        let t = Instant::now();
        let heap: ID3D12Heap = if api == 0 {
            g.dev.cast::<ID3D12Device3>()?.OpenExistingHeapFromAddress(p)?
        } else {
            match g.dev.cast::<ID3D12Device13>() {
                Ok(d) => d.OpenExistingHeapFromAddress1(p, size as usize)?,
                Err(_) => {
                    spike!("s1", "watch={} api=1 open=unavailable", watch as u8);
                    return Ok(());
                }
            }
        };
        let open_ms = t.elapsed().as_secs_f64() * 1e3;
        let mut res: Option<ID3D12Resource> = None;
        g.dev.CreatePlacedResource(&heap, 0, &buf_desc(size, D3D12_RESOURCE_FLAG_NONE), D3D12_RESOURCE_STATE_COMMON, None, &mut res)?;
        let res = res.unwrap();

        // GPU reads the whole range (twice, keep the second), then check the first 16 pages.
        g.copy(dst, 0, &res, 0, size)?;
        let secs = g.copy(dst, 0, &res, 0, size)?;
        let rb = g.readback(65536)?;
        g.copy(&rb, 0, dst, 0, 65536)?;
        let pat_ok = g.read(&rb, 65536, |b| b.chunks(4096).enumerate().all(|(i, c)| c.iter().all(|&x| x == i as u8)))?;

        // GPU writes 64 KiB into the middle; did the write watch see it, and does the CPU see the bytes?
        let off = (size / 2) & !0xffff;
        let up = g.upload(65536, |_| 0xCD)?;
        if watch {
            ResetWriteWatch(p, size as usize);
        }
        g.copy(&res, off, &up, 0, 65536)?;
        let marks = if watch {
            let mut addrs = [std::ptr::null_mut::<c_void>(); 64];
            let mut cnt = addrs.len();
            let mut gran = 0u32;
            let r = GetWriteWatch(0, p, size as usize, Some(addrs.as_mut_ptr()), Some(&mut cnt), Some(&mut gran));
            if r == 0 { cnt.to_string() } else { "err".into() }
        } else {
            "na".into()
        };
        let cpu_sees = mem[off as usize..off as usize + 65536].iter().all(|&b| b == 0xCD);

        let mut old = PAGE_PROTECTION_FLAGS(0);
        let vp = match VirtualProtect(p, 65536, PAGE_READONLY, &mut old) {
            Ok(()) => {
                let _ = VirtualProtect(p, 65536, PAGE_READWRITE, &mut old);
                "ok".to_string()
            }
            Err(e) => es(&e),
        };
        let wsa = if watch { (marks != "err") as u8 } else { 2 }; // 2 = n/a
        spike!(
            "s1",
            "watch={} api={} open=ok open_ms_per_gib={:.2} read_gbps={:.2} cpu_pattern_gpu_read_ok={} gpu_write_marks_watch_pages={} gpu_write_cpu_visible={} watch_after_open={} vprotect={}",
            watch as u8, api, open_ms / (size as f64 / GIB as f64), gbps(size, secs), pat_ok as u8, marks, cpu_sees as u8, wsa, vp
        );
    }
    Ok(())
}
