//! S2: GPU upload heap (resizable BAR style CPU-visible VRAM).
use crate::gpu::*;
use std::ffi::c_void;
use std::time::Instant;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Registry::*;
use windows::core::*;

fn devmode() -> String {
    let (mut v, mut n) = (0u32, 4u32);
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock"),
            w!("AllowDevelopmentWithoutDevLicense"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut c_void),
            Some(&mut n),
        )
    };
    if r.0 == 0 { v.to_string() } else { format!("unknown(err{})", r.0) }
}

pub fn run(g: &Gpu) -> Result<()> {
    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
    unsafe { g.dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS16, &mut o as *mut _ as *mut c_void, size_of_val(&o) as u32)? };
    let sup = o.GPUUploadHeapSupported.as_bool();
    let dm = devmode();
    if !sup {
        spike!("s2", "supported=0 devmode={dm}");
        return Ok(());
    }
    let n: usize = 256 << 20;
    let buf = g.buffer(D3D12_HEAP_TYPE_GPU_UPLOAD, n as u64, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COMMON)?;
    let mut p = std::ptr::null_mut();
    unsafe { buf.Map(0, None, Some(&mut p))? };
    let p = p as *mut u8;
    let src = vec![0x5Au8; n];

    let t = Instant::now();
    unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), p, n) };
    let write = gbps(n as u64, t.elapsed().as_secs_f64());

    let t = Instant::now();
    unsafe {
        use std::arch::x86_64::*;
        for i in (0..n).step_by(16) {
            _mm_stream_si128(p.add(i) as *mut __m128i, _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i));
        }
        _mm_sfence();
    }
    let nt = gbps(n as u64, t.elapsed().as_secs_f64());

    // Reads from write-combined/BAR memory are slow, so only 16 MiB.
    let rn = 16 << 20;
    let mut dstv = vec![0u8; rn];
    let t = Instant::now();
    unsafe { std::ptr::copy_nonoverlapping(p, dstv.as_mut_ptr(), rn) };
    let cread = gbps(rn as u64, t.elapsed().as_secs_f64());
    let ok = dstv.iter().all(|&b| b == 0x5A);

    let d = g.default_buf(n as u64)?;
    g.copy(&d, 0, &buf, 0, n as u64)?;
    let gread = gbps(n as u64, g.copy(&d, 0, &buf, 0, n as u64)?);
    spike!(
        "s2",
        "supported=1 devmode={dm} write_gbps={write:.2} nt_write_gbps={nt:.2} cpu_read_gbps={cread:.3} cpu_readback_ok={} gpu_read_gbps={gread:.2}",
        ok as u8
    );
    Ok(())
}
