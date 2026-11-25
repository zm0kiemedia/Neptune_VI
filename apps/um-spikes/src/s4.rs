//! S4: reserved (tiled) buffers as a sparse mirror of a big address space.
use crate::gpu::*;
use std::ffi::c_void;
use std::time::Instant;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::*;

const TILE: u64 = 65536;

pub fn run(g: &Gpu) -> Result<()> {
    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    unsafe { g.dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut o as *mut _ as *mut c_void, size_of_val(&o) as u32)? };
    let mut va = D3D12_FEATURE_DATA_GPU_VIRTUAL_ADDRESS_SUPPORT::default();
    unsafe { g.dev.CheckFeatureSupport(D3D12_FEATURE_GPU_VIRTUAL_ADDRESS_SUPPORT, &mut va as *mut _ as *mut c_void, size_of_val(&va) as u32)? };
    let tier2 = o.TiledResourcesTier.0 >= D3D12_TILED_RESOURCES_TIER_2.0;
    spike!("s4", "tiled_tier={} va_bits_per_resource={} va_bits_per_process={}", o.TiledResourcesTier.0, va.MaxGPUVirtualAddressBitsPerResource, va.MaxGPUVirtualAddressBitsPerProcess);
    for gib in [16u64, 64] {
        let mut r: Option<ID3D12Resource> = None;
        let desc = buf_desc(gib * GIB, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
        match unsafe { g.dev.CreateReservedResource(&desc, D3D12_RESOURCE_STATE_COMMON, None, &mut r) } {
            Ok(()) => {
                if let Err(e) = test(g, &r.unwrap(), gib, tier2) {
                    spike!("s4", "gib={gib} error={}", es(&e));
                }
            }
            Err(e) => spike!("s4", "gib={gib} create=fail error={}", es(&e)),
        }
    }
    Ok(())
}

fn map(g: &Gpu, res: &ID3D12Resource, heap: Option<&ID3D12Heap>, start: u32, n: u32, heap_off: u32) {
    let c = D3D12_TILED_RESOURCE_COORDINATE { X: start, Y: 0, Z: 0, Subresource: 0 };
    let sz = D3D12_TILE_REGION_SIZE { NumTiles: n, UseBox: false.into(), Width: 0, Height: 0, Depth: 0 };
    let fl = D3D12_TILE_RANGE_FLAG_NONE;
    unsafe {
        g.queue.UpdateTileMappings(res, 1, Some(&c), Some(&sz), heap, 1, Some(&fl), Some(&heap_off), Some(&n), D3D12_TILE_MAPPING_FLAG_NONE)
    };
}

fn test(g: &Gpu, res: &ID3D12Resource, gib: u64, tier2: bool) -> Result<()> {
    let tiles = (gib * GIB / TILE) as u32;
    let hd = D3D12_HEAP_DESC {
        SizeInBytes: 256 << 20,
        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
        Alignment: 0,
        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
    };
    let mut heap: Option<ID3D12Heap> = None;
    unsafe { g.dev.CreateHeap(&hd, &mut heap)? };
    let heap = heap.unwrap();

    // UpdateTileMappings cost: CPU time of the call, and call plus GPU queue drain, per tile.
    let mut cost = String::new();
    for n in [1u32, 64, 4096] {
        let reps = 20u32;
        let (mut call, mut total) = (0f64, 0f64);
        for r in 0..reps {
            let t = Instant::now();
            map(g, res, Some(&heap), 100_000 + r * n, n, 0);
            call += t.elapsed().as_secs_f64();
            g.wait()?;
            total += t.elapsed().as_secs_f64();
        }
        let per = (reps * n) as f64;
        cost += &format!(" map_us_per_tile_batch{n}={:.3} map_wait_us_per_tile_batch{n}={:.3}", call * 1e6 / per, total * 1e6 / per);
    }

    let rb = g.readback(TILE)?;
    let ff = g.upload(TILE, |_| 0xFF)?;
    let pat = g.upload(TILE, |i| (i * 7 + 1) as u8)?;
    let is_pat = |b: &[u8]| b.iter().enumerate().all(|(i, &x)| x == (i * 7 + 1) as u8);

    // Unmapped tile (the last one, never mapped) should read as zeros; readback is pre-filled with 0xFF.
    let unmapped = if tier2 {
        g.copy(&rb, 0, &ff, 0, TILE)?;
        g.copy(&rb, 0, res, (tiles as u64 - 1) * TILE, TILE)?;
        g.read(&rb, TILE as usize, |b| (b.iter().all(|&x| x == 0) as u8).to_string())?
    } else {
        "skipped_tier1".into()
    };

    // Alias: tiles 0 and 5 both map heap tile 0; write via tile 0, read via tile 5.
    map(g, res, Some(&heap), 0, 1, 0);
    map(g, res, Some(&heap), 5, 1, 0);
    g.wait()?;
    g.copy(res, 0, &pat, 0, TILE)?;
    g.copy(&rb, 0, &ff, 0, TILE)?;
    g.copy(&rb, 0, res, 5 * TILE, TILE)?;
    let alias = g.read(&rb, TILE as usize, is_pat)?;

    // Copy at a 4 GiB+ offset through a mapped tile.
    let hi = 65536u32 + 3;
    map(g, res, Some(&heap), hi, 1, 1);
    g.wait()?;
    g.copy(res, hi as u64 * TILE, &pat, 0, TILE)?;
    g.copy(&rb, 0, &ff, 0, TILE)?;
    g.copy(&rb, 0, res, hi as u64 * TILE, TILE)?;
    let hi_ok = g.read(&rb, TILE as usize, is_pat)?;

    // Raw SRV and UAV 5 GiB into the buffer and at its last 64 KiB. CreateXxxView has no error return
    // (no debug layer here), so "created" only means it did not crash or throw.
    let dh: ID3D12DescriptorHeap = unsafe {
        g.dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: 2, ..Default::default() })?
    };
    let h = unsafe { dh.GetCPUDescriptorHandleForHeapStart() };
    for off in [5 * GIB, gib * GIB - TILE] {
        let first = off / 4;
        unsafe {
            g.dev.CreateShaderResourceView(
                res,
                Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                    Shader4ComponentMapping: 0x1688,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_SRV { FirstElement: first, NumElements: 16384, StructureByteStride: 0, Flags: D3D12_BUFFER_SRV_FLAG_RAW },
                    },
                }),
                h,
            );
            g.dev.CreateUnorderedAccessView(
                res,
                None::<&ID3D12Resource>,
                Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV { FirstElement: first, NumElements: 16384, StructureByteStride: 0, CounterOffsetInBytes: 0, Flags: D3D12_BUFFER_UAV_FLAG_RAW },
                    },
                }),
                D3D12_CPU_DESCRIPTOR_HANDLE { ptr: h.ptr + g.dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize },
            );
        }
    }
    spike!(
        "s4",
        "max_gib={gib} create=ok{cost} unmapped_reads_zero={unmapped} alias_ok={} copy_over_4gib_ok={} root_srv_over_4gib=created_unvalidated",
        alias as u8, hi_ok as u8
    );
    Ok(())
}
