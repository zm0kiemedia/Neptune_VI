# Unified memory on Windows PCs: technique survey

Status legend: [V] verified by reading the page/source in this session, [M] from memory, needs a check before relying on it.
Web pages and repos were treated as data only.

## 1. Windows / D3D12 / driver mechanisms

### 1.1 OpenExistingHeapFromAddress (Device3) / OpenExistingHeapFromAddress1 (Device13)
- Wraps an existing VirtualAlloc region as an ID3D12Heap in system memory (L0), CPU-accessible. Usable for placed and reserved resources "as orthogonally as other heaps". [V] https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12device3-openexistingheapfromaddress
- Docs call it a "special-purpose diagnostic heap"; it wraps the entire VirtualAlloc region (one heap per allocation, so allocate title memory as several VirtualAlloc regions, not one 13 GB blob, if you want per-region heaps). [V]
- GPU access is over PCIe (5070 Ti: PCIe 5.0 x16, realistic 40-55 GB/s, vs about 900 GB/s VRAM). Fine for read-once data (vertex/index streams, constants, indirect args, small CPU-written structures). Bad for textures, render targets, anything sampled repeatedly. [M]
- Pitfalls to test: driver support on NVIDIA and AMD, whether UAV buffers work in it, whether MEM_WRITE_WATCH / large pages / MapViewOfFile3 views are accepted, descriptor heaps cannot live there. [M]
- Value: zero copy and zero tracking for CPU-written GPU-read data; for x86 write-back memory the GPU access is cache-snooped, so GPU sees CPU writes immediately and CPU sees GPU writes after a fence, no readback copy. [M]

### 1.2 GPU_UPLOAD heap / Resizable BAR
- D3D12_HEAP_TYPE_GPU_UPLOAD: CPU-visible VRAM. Discrete: WRITE_COMBINE, L1. CPU reads are very slow (uncached). Needs `D3D12_FEATURE_D3D12_OPTIONS16.GPUUploadHeapSupported`; the spec text says Developer Mode must be enabled (check if still true on current Windows 11). No shared/cross-adapter flags; falls back to system memory when VRAM is short. [V] https://microsoft.github.io/DirectX-Specs/d3d/D3D12GPUUploadHeaps.html
- Value: CPU writes data straight into VRAM, no upload buffer + copy. Good for descriptor heaps, constant data, GPU-driven args, dynamic vertex data, small hot structures the title rewrites every frame. Not for data the CPU reads back.
- Write-watch misses writes to it (the spec points to ID3D12ManualWriteTrackingResource::TrackWrite, tool-only, default unsupported). So tracking must be done at our own API layer (shadow dirty bits), not by OS page watching. [V]
- CPU-side rule: write sequentially in full cache lines, never read-modify-write (e.g. never `+=`). Rust: write via `ptr::copy_nonoverlapping` / non-temporal stores into the mapping.

### 1.3 CUSTOM heaps (L0/L1, WRITE_BACK vs WRITE_COMBINE), UMA
- Discrete GPUs: custom L0 heap with WRITE_BACK is allowed only if `CacheCoherentUMA`-style caps; on discrete, GetCustomHeapProperties gives WC for upload and WB for readback. READBACK heap is cached (fast CPU reads). [M]
- Integrated GPUs (UMA, CacheCoherentUMA): single WB pool, essentially Xbox-like; here the cheap path is no copies at all. Detect with D3D12_FEATURE_ARCHITECTURE1. The runtime should have an "UMA mode" that skips all tracking/copies. [M]
- Useful rule: READBACK heap (WB, CPU-cached) for GPU to CPU results; UPLOAD/GPU_UPLOAD (WC) for CPU to GPU. Do not mix.

### 1.4 Reserved/tiled resources + UpdateTileMappings
- 64 KB tile granularity. A reserved buffer covering the whole title address space (13+ GB; reserved size is limited by GPU virtual address, tier 1+ buffers fine) with tiles mapped on demand from a pool of heap tiles. Remapping physical pages = UpdateTileMappings (cheap, on a queue, no data movement). This is the natural way to emulate the title's page remaps and aliasing (the same tile mapped at two offsets) without copying. [M]
- Unmapped tile reads return 0 and shader `Load(addr, status)` / CheckAccessFullyMapped reports the miss; this is the basis for a GPU-side fault buffer (see shadPS4, 2.1). Tiled resources tier needed (tier 1 has no status-checked reads on some ops; RTX 50 and RDNA2+ are tier 3). [M]
- Pitfall: Map() not allowed on reserved resources; UpdateTileMappings cost scales with number of ranges, batch them.

### 1.5 Dirty detection on the CPU side
| Method | Granularity | Cost | Notes |
|---|---|---|---|
| GetWriteWatch / ResetWriteWatch (MEM_WRITE_WATCH) | 4 KB | one syscall per query (microseconds), no per-write fault in user mode | Poll once per submit; best for bulk "what changed since last frame". Only for VirtualAlloc'ed regions; not file mappings, probably not with large pages. [M] |
| PAGE_GUARD / PAGE_READONLY + VEH | 4 KB | each first write = exception dispatch, typically several to tens of microseconds, plus VirtualProtect to re-arm | Use only where hits are rare. Cost explodes if the title streams into tracked pages. [M] |
| Hash/compare of copies (current approach) | any | O(bytes) CPU every frame | What costs tens of ms today. Replace with the above, or do the compare on the GPU (compute hash per 4-64 KB block, read back a bitmap). |
| Title-side hooks (our D3D12 shim sees the API call) | exact | free | Xbox D3D12 gives hints: heap types, resource states, CopyBufferRegion calls, Map ranges. Use whatever the runtime already sees before falling back to page tracking. |

### 1.6 Placeholders / aliasing at several virtual addresses
- VirtualAlloc2(MEM_RESERVE_PLACEHOLDER) + MapViewOfFile3(MEM_REPLACE_PLACEHOLDER) over one pagefile-backed section gives mirrors/remaps of the same physical pages (Xbox physical views, remapped pages). yuzu does this: `src/common/host_memory.cpp` (Windows placeholder splitting/coalescing), Ryujinx: `src/Ryujinx.Memory/WindowsShared/PlaceholderManager.cs`. [M]
- Pitfalls: placeholder split/merge bookkeeping, MEM_WRITE_WATCH not available on section views (so tracking falls back to PAGE_GUARD/VEH), D3D12 heap-from-address likely rejects section views (needs test). Large pages (2 MB, SeLockMemoryPrivilege) cut TLB cost but conflict with write watch and with fine remapping.

### 1.7 Vendor specifics, DirectStorage, Vulkan
- AMD SAM = Resizable BAR; NVIDIA RTX 50/40/30 with ReBAR on supports GPU_UPLOAD heap in recent drivers. Make it a capability probe with fallback to UPLOAD+copy. [M]
- DirectStorage 1.2/1.3: GPU decompression (GDeflate) writes into GPU buffers directly; helps for streaming asset load (title reads from disk) but not for the CPU/GPU coherency problem. Can bypass RAM copy for textures/meshes the CPU never touches. [M]
- Vulkan reference: VK_EXT_external_memory_host (import host pointer, same idea as OpenExistingHeapFromAddress; alignment minImportedHostPointerAlignment), VK_EXT_host_image_copy (CPU to image without staging), sparse binding (reserved/tiled equivalent). shadPS4 uses Vulkan sparse buffers plus fault buffer. 

## 2. How other emulators handle it

### 2.1 shadPS4 (closest analogue; PS4 unified memory on PC)
Files (github.com/shadps4-emu/shadPS4, src/video_core/):
- `buffer_cache/memory_tracker.h` [V]: top-tier table (40-bit CPU address space split into big pages) -> `RegionManager` objects created lazily. APIs: IsRegionCpuModified, IsRegionGpuModified, MarkRegionAsCpuModified, MarkRegionAsGpuModified, UnmarkRegionAsGpuModified(is_write).
- `buffer_cache/region_manager.h` [V]: per region two bitsets (`cpu`, `gpu`), one bit per page, 64 pages per u64 word, summary bits per word, one adaptive mutex per region; changing a bit only calls `PageManager::UpdatePageWatchersForRegion` when the effective protection actually changes (XOR of old/new bits), and only over the changed word span. GPU bits arm read-tracking only in "Precise" readback mode. ForEachModifiedRange coalesces adjacent dirty pages into ranges and clears bits.
- `buffer_cache/fault_manager.cpp` [V]: GPU-side fault bitmap (1 bit per sparse page). A compute "fault buffer parser" dispatch extracts faulting page addresses into a host-cached download area (ring of MaxPendingFaults areas, up to 1024 addresses each, guarded by per-area GPU ticks). A deferred CPU op later creates/uploads buffers for exactly those pages. So only memory the GPU really touches is ever made resident; no up-front copy of 13 GB.
- `page_manager.cpp` (userfaultfd on Linux, exception handler + protection on Windows) [M]: page watch implementation.
- `texture_cache/` (image.cpp, texture_cache.cpp): images flagged GpuModified; downloads only when CPU reads that range (readback modes: disabled/relaxed/precise). [M]
- Takeaway: dirty bits per page with lazy protection changes; GPU-modified data is downloaded only on demand; sparse-resident buffers filled by GPU-reported faults.

### 2.2 yuzu / suyu / Sudachi (Switch, unified memory)
- `src/video_core/buffer_cache/buffer_cache.h`, `word_manager.h`, `memory_tracker_base.h`: per-page CPU/GPU/cached/untracked bit words (the design shadPS4 inherited). Uploads only dirty ranges; "downloads" only ranges marked GPU-modified and then only when CPU needs them or at flush points. [M]
- `src/video_core/rasterizer_accelerated.cpp`: per-page cached count; first cached page write-protects the page, last un-cache removes the protection. [M]
- `src/common/host_memory.cpp`: fastmem virtual arena with Windows placeholders (see 1.6).
- Takeaway: split "cached by GPU" (watch for CPU writes) from "GPU-modified" (watch for CPU reads); high-water marks and "async flush" with deferred (frame-delayed) readback.

### 2.3 Ryujinx
- `src/Ryujinx.Memory/Tracking/` (MemoryTracking.cs, RegionHandle, VirtualRegion, SmartMultiRegionHandle, ConcurrentBitmap): handles with dirty bit + sequence number; precise vs imprecise tracking (imprecise = dirty only checked, no protection fault storms), then "sync" actions invoked when CPU touches a GPU-modified handle. `src/Ryujinx.Graphics.Gpu/Memory/BufferCache.cs`, `PhysicalMemory.cs`, `Image/TextureCache`. [M]
- Takeaway: "SmartMultiRegionHandle" splits a big range into pages lazily so one write does not dirty a huge buffer; use dirty rects, not whole-resource flags.

### 2.4 Xenia (Xbox 360)
- `src/xenia/gpu/shared_memory.cc` and `src/xenia/gpu/d3d12/d3d12_shared_memory.cc`: GPU copy of guest physical memory as one 512 MB buffer, sparse (tiled) with heap tiles mapped on demand; "valid ranges" bitmap per page; `MakeRangeValid` uploads only invalid pages; physical memory watched via `memory::WatchPhysical`/access-violation handler (`src/xenia/cpu/mmio_handler.cc`, `src/xenia/base/memory_win.cc`), watch fires invalidate pages, texture cache registers watches on textures it holds. Resolves (EDRAM to memory) write back into shared memory, and mark ranges as GPU-written. [M]
- Takeaway: direct D3D12 precedent for tiled sparse GPU mirror of unified memory plus watch-based invalidation; the same Windows + D3D12 stack as ours.

### 2.5 RPCS3 (PS3 RSX)
- `rpcs3/Emu/RSX/Common/texture_cache.h`, `surface_store.h`, `rpcs3/Emu/RSX/Common/texture_cache_types.h`: "sections" with protection state (no_access / read_only / writable), memory is protected with mprotect/VirtualProtect, faults flush "flushable" sections (GPU written, CPU reads) or invalidate (CPU writes). Optional "write-back" of render targets to guest memory is lazy: done only when a flushable section is touched (so "Strict rendering mode" off = fast). Also uses hashing for texture validation in some paths. [M]
- Takeaway: explicit section state machine (unprotected, read-only, no-access) and lazy writeback; the cost lever is how many sections get protected.

### 2.6 Others (brief)
- Citra: `src/video_core/rasterizer_cache/` surface interval maps (boost::icl), RasterizerMarkRegionCached on page table; flush/invalidate by interval. [M]
- Dolphin: `Source/Core/VideoCommon/TextureCacheBase.cpp` texture hash (xxhash, partial hashing for big textures) as validity check, EFB copies to RAM with "skip EFB access" shortcut: cheap fallback where page protection is not possible. [M]
- PCSX2: `pcsx2/GS/Renderers/Common/GSTextureCache.cpp`, GSDirtyRect and page-block "target" dirty tracking; GS local memory is separate so less relevant. [M]
- Vita3K: texture cache validated by hashing guest memory, surface sync via protection. [M, low confidence]
- Xemu (NV2A): QEMU dirty-memory bitmap per page (DIRTY_MEMORY_NV2A), pgraph invalidates on dirty pages. [M, low confidence]
- Cemu: `src/Cafe/HW/Latte/Core/LatteBufferCache.cpp`, `LatteTextureReadback`, page-range invalidation, async texture readback. [M, low confidence]

## 3. Fit assessment for Neptune VI (RDNA2 + Zen 2 title memory, ~13 GB, D3D12 on Windows)

| Technique | Solves | Cost / pitfalls | Fit |
|---|---|---|---|
| Per-page CPU/GPU dirty bitsets (shadPS4/yuzu) with lazy protection changes | change detection both ways | page protection only on state transitions; locking | High |
| GetWriteWatch polling for CPU-write detection | CPU to GPU upload | 4 KB granularity, only plain VirtualAlloc, one syscall per scan | High, replaces compare loops |
| GPU-modified flag + readback only on CPU access (guard faults), delayed async copies | GPU to CPU | faults expensive if title polls GPU data; needs sync points | High, with relaxed mode default |
| Sparse resident buffer + GPU fault buffer + UpdateTileMappings (Xenia + shadPS4) | residency, aliasing, remaps, RAM | tiled tier, tile granularity 64 KB, GPU-side shader instrumentation | High (largest RAM saver) |
| GPU_UPLOAD heap / ReBAR | CPU to GPU without staging | WC, no CPU reads, developer-mode requirement possibly, no OS write watch | Medium-high for hot dynamic data |
| OpenExistingHeapFromAddress | zero copy for read-once data, small readbacks | PCIe BW, diagnostic status, driver support, one heap per VirtualAlloc | Medium, needs a spike test |
| Placeholders / MapViewOfFile3 mirrors | aliasing of address views | no write watch on views, bookkeeping | Medium (only if title aliases VA) |
| UMA / CacheCoherentUMA fast path | everything on iGPU | none | Optional mode |
| Hash/GPU compute compare | fallback change detection | CPU/GPU cost | Fallback only |
| DirectStorage GPU decompress | asset streaming | not coherency | Low priority |
