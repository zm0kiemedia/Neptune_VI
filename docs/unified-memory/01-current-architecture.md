# Neptune VI: how unified memory is emulated today

Status: research map of the code on branch `unified-memory` (worktree `/home/claudeuser/work/nep-um`),
read-only analysis. Sources: `crates/victory-gpu/src/*`, `crates/victory-hle/src/modules/xmem.rs`,
`tools/play-zm.ps1`, the gameplay log `/home/claudeuser/work/nep-um-logs-c3.txt` (28,705 lines,
125 one-second frame lines, 565 per-second diagnostic blocks) and the upstream profile quoted in the task.
All `file:line` references are to the tree as read; paths are relative to `crates/victory-gpu/src/` unless
they start with `victory-hle/`.

## 0. The model in one page

The Xbox has one memory. The runtime keeps that illusion by making **title memory the master copy, always in
PC RAM**, and giving the GPU **derived host copies** that are reconciled by software at submission time:

```
 title CPU threads            victory-hle (xmem)                   victory-gpu (host D3D12 device)
 ---------------------        ----------------------------        --------------------------------------------
 write title memory   ----->  VirtualAlloc2 + MEM_WRITE_WATCH     master copy in RAM (the "title bytes")
 (heap, pool views,           (private memory)                          |  tracked per 64 KiB page
  DirectStorage)              section views of "physical pages"         v
                              (cannot be write-watched: guarded)   refresh at submission (CPU memcpy/kernel read)
                                                                        v
                                                                   CPU-side twin (RAM, "Shadow::data")
                                                                        v   dirty runs / masks
                                                                   card side (VRAM, big buffers/windows)
                                                                        |  shaders read and write here
                                                                        v   compare on GPU (hash per 64 KiB piece)
                                                                   fetch changed pieces back to RAM
                                                                        v   byte-exact merge
 read title memory   <-----   title memory (RAM) updated before the title is told "GPU done"
```

Textures follow a parallel path: title bytes (tiled, RDNA2 layout) -> staging buffer (detiled, RAM) ->
host texture (VRAM). Render targets and UAV textures live only in VRAM; nothing is copied back except a
whitelist (the ocean maps, depth twins).

Two properties shape every cost below:

1. **Everything is keyed to submission time** (`Host::submit`, `host.rs:5933`), not to the moment the title
   writes. The title's recording threads only *declare* what a draw reads or writes (`Step::Refresh`,
   `Step::GpuWrite`, `Step::Texture`, `Step::Copy`, `Step::Fill`, `Step::Run`; `host.rs:6997-7150`). The single
   submitting thread (the title's render thread, unless `VICTORY_ASYNC_SUBMIT=1`, `queue.rs:76`, off) executes
   those steps.
2. **Only declared ranges are synced.** Buffers read through an unindexed heap, and writes outside declared
   ranges, are not seen (documented shortcuts in `shadow.rs:1-16`, `draw.rs:1511-1521`).

## 1. Data flow per memory kind

### 1.0 Shared machinery

**Where title memory comes from** (`victory-hle/src/modules/xmem.rs`)

| Title API | Backing | Tracked how |
|---|---|---|
| `XMemVirtualAlloc` (`xmem.rs:87`) | `VirtualAlloc2` with `MEM_WRITE_WATCH` added when the title reserves at an address of our choice (`xmem.rs:131-137`) | kernel dirty bits per 4 KiB page, read with `GetWriteWatch(RESET)` |
| `XMemAlloc` / `XMemAllocDefault` (`xmem.rs:283-322`) | host heap (`alloc`), blocks >= 256 KiB listed in `titlemem` | not write-watched (not GPU memory in practice) |
| `XMemAllocatePhysicalPages` (`xmem.rs:411`) + `XMemMapPhysicalPagesScatter` (`xmem.rs:801`) + `map_pages` (`xmem.rs:734`) | pagefile sections, 64 KiB pages, mapped as views (`MapViewOfFile3`); one view per run of consecutive slices | cannot be write-watched: **guard pages** (see below) |
| `CopyPageMappingsBatchX` (`queue.rs:1166`, calls `xmem::map_alias`, `xmem.rs:825`) | maps pool pages at tiled-heap addresses, done at once | remap announced to the GPU side (`title_memory_remapped`, `watch.rs:1885`) |
| DirectStorage page destinations (`victory-hle/src/modules/dstorage.rs:1129`) | kernel writes into pool pages | told to us first: `kernel_will_write` (`watch.rs:1608`), then `written_through_view` / `title_memory_written` |

`VICTORY_REMAP_SAME` (default 1, `xmem.rs:929`): a remap that shows the same pages is not redone, only the GPU
side is told bytes may have changed (a title thread was spending 84 % of its time in these calls).

**Write detection** (`watch.rs`)

- Unit: 64 KiB (`PAGE`, `watch.rs:28`); the kernel reports 4 KiB pages, folded into 64 KiB. Finer units were
  measured to cost more than they save. `VICTORY_WATCH_4K=1` keeps 4 KiB stamps and copies only written 4 KiB
  pages inside a stale 64 KiB page (`watch.rs:268-360`, `shadow.rs:3417-3425`); off by default.
- **Journal** of "page N last found written by asking K" (`PARTS` table, `watch.rs:100`). A call to
  `watch::writes` (`watch.rs:1091`) asks the kernel with `GetWriteWatch(WRITE_WATCH_FLAG_RESET)`
  (`watch.rs:1028-1075`) and notes found pages; because asking *clears* the kernel bits, overlapping host copies
  (nested buffers, textures inside buffers) learn from the journal instead (`copied[page] < journal[page]`).
- **Batching the asking.** One kernel call costs 30-70 us whatever the range and flushes every core's TLB
  (`watch.rs:392-396`, `watch.rs:427-440`). So a submission asks once for all it will refresh: `ask_once`
  (`watch.rs:519`, called `host.rs:6013`, on by default), `ask_ahead` per flush (`watch.rs:422`, from
  `Batch::flush`, `host.rs:7480`), and `ask_once_quiet` (`watch.rs:605`, `VICTORY_ASK_QUIET=8`): 1 MiB blocks not
  written for 8 frames are asked about once a frame (the log shows e.g. "2171 blocks of 1 MiB written lately,
  3670 ranges answered from the journal").
- **Guards for views** (`watch.rs:1255-1600`): a copy of view memory leaves each 64 KiB block it took
  `PAGE_READONLY` (`protect`, `watch.rs:1425`); the vectored handler `on_write` (`watch.rs:1931`) takes the first
  write, notes the block written, reopens it (`open`, `watch.rs:1547`; opens `VICTORY_GUARD_OPEN_AHEAD`=1 extra
  blocks), and continues the thread. All other views of the same physical page are made read-only too
  (`VICTORY_VIEW_GUARD`, default on), so a write through any alias faults. A kernel write (DirectStorage,
  `write_physical`) is announced with `watch::writing` (`watch.rs:1608`) so the guard is opened first. Remap
  events reset protection (`title_memory_remapped`, `watch.rs:1885`; `views_mapped`, `watch.rs:1706`). A
  per-16 MB `RwLock` array (`MAPPING`, `watch.rs:1746`) keeps in-place reads and remaps from overlapping.
  Cost seen upstream: write-guard fault handler 8 % of recording-thread time.
- Memory that is neither watched nor guarded is **copied whole at every refresh** (`Shadow::update_free`,
  `shadow.rs:3320-3340`; `VICTORY_UNWATCHED_FRAMES` throttles the heap pass's copies, `draw.rs:1640`).

**Kernel reads.** Title -> host copies go through `copy_mapped` (`host.rs:7530`): a plain `rep movsb` with a
vectored handler that swallows faults on missing pages (`fast_copy`/`plain_copy`, `host.rs:7720-7800`), falling
back to `ReadProcessMemory`-style `read`, then `read_sparse` for holes. Reads of title memory that may be remapped
hold a shared `MAPPING` lock (`title_memory_reading_range`, `watch.rs:1858`).

### 1.1 Title CPU-written buffers (vertex, index, structured data, big heaps)

| Aspect | Today |
|---|---|
| Where bytes live | (1) title memory, RAM. (2) `Shadow::data`, a persistent **CPU-side twin** in RAM (host upload buffer, mapped). (3) for buffers >= 1 MiB (`VICTORY_CARD_BUFFERS_MIN_MB`=1, play-zm: `MIN_KB=64`) and while total < `VICTORY_CARD_BUFFERS_MB` (1024, play-zm 1536) and the card has room: a **VRAM copy** `Shadow::buffer` that shaders see (`cards()`, `card_for`, `shadow.rs:151-345`). Shaders read VRAM; RAM twin exists only to receive refreshes. A buffer shaders write also gets a hash buffer in VRAM (`Card::before`, `card_hash_bytes`, `shadow.rs:269`) or, for RAM-only copies, a CPU snapshot (`Shadow::before`, pooled, `shadow.rs:1240`). So 2 to 3 copies of the same bytes. |
| Granularity of copies | A buffer gets one host copy "on first use" (`Shadow::new`, `shadow.rs:2555`); title address = same offset in the copy. A range of the draw is refreshed lazily (`Shadow::refresh`, `shadow.rs:2550` -> `defer_refresh`, `host.rs:7051`, deduped per list). |
| Detection | `Shadow::update` (`shadow.rs:3267`) -> `update_free` -> `watch::writes(from, to, guarded=true)` -> `copy_stale` (`shadow.rs:3398`): per 64 KiB page, stale if `copied[page] == 0 || journal[page] > copied[page]` (`copied` is a `Mutex<Vec<u64>>` per copy). No hashing of buffers on the CPU. |
| Direction title -> host | at submission, in `Batch::flush` (`host.rs:7480`): coalesce per copy to its covering span, `ask_ahead`, then `Shadow::update` per copy (optionally on worker threads, `VICTORY_WIDE_REFRESH=1`, `host.rs:7401`). `copy()` (`shadow.rs:3441`): plain memcpy into the twin; with `VICTORY_COPY_CHANGED=1` (play-zm) on card copies it reads the title page into a scratch, compares byte-wise (`changed_spans`, `shadow.rs:1936`) and only changed spans are dirtied. Dirty spans go to the card side in `Host::card_before` (`host.rs:4496`) as `CopyBufferRegion` from the twin; many tiny runs in a 4 KiB block go as one **masked** compute pass (`VICTORY_UNDER_WRITES_MASKS`, `host.rs:1991-2070`). |
| What blocks | Nothing waits on the GPU for a normal refresh. The refresh skips ranges that flights in the air may be writing (`Shadow::update`'s `busy` cut-outs, `shadow.rs:3275-3300`) and `stale_under_flights` (`host.rs:3122`) / `merge_under_writes` (`shadow.rs:3147`) handle the title changing bytes under shader writes without settling (stage 2 of late wait). The submitting thread does the CPU copies: this is the "copies" figure on the frame line. |
| Upstream/loop-closing | Title buffer copies (`CopyBufferRegion` between title buffers) on the CPU use `Step::Copy` (`host.rs:6997`); if the source is shader-written, `card_copy` (`list.rs:927`) records it on the card instead (`VICTORY_CARD_COPIES`, on; 25.8 per frame in the log). |

### 1.2 GPU-written buffers and UAVs (the title's compute writes memory the CPU reads next frame)

| Aspect | Today |
|---|---|
| Declaration | A UAV/root-UAV/indirect write that the draw/dispatch may do on a buffer range is declared: `Shadow::gpu_writes` (`shadow.rs:2670`) -> `defer_gpu_write` (`host.rs:7068`) -> `Step::GpuWrite`. Executed at submission: `Batch::flush_one` (refresh first, so shaders start from title data) then `Shadow::writing` (`shadow.rs:2691`). Dispatches declare the same range thousands of times; deduped via `pending.contains`. |
| Detection of what the GPU wrote | **RAM-only copy**: CPU snapshot of the range before the submission (`Shadow::before`, each byte once per submission, pooled, `copy_wide`, `shadow.rs:1363`; deferred to `take_snapshots`, `shadow.rs:1527`, when `VICTORY_WIDE_BACK` on). After the run, `find_all` (`shadow.rs:1574`) compares copy vs snapshot per 64 KiB piece on worker threads (gigabytes a frame, memory-bandwidth bound). **Card copy** (the common case now): no snapshot; a compute pass of ours hashes each declared piece and compares with the hash stored from before (`Card::before`; `make_card_work`, `host.rs:4240`; pass listed `host.rs:4496+`), and only differing pieces are fetched to RAM. Hashes are stored by the compare so each flight flags exactly what changed since the previous compare (`flagged_pieces`, `host.rs:2837`). Log: 17,520 pieces compared per frame, 1,618 fetched (95.7 MB) in one sample, median 86.5 MB fetched, 13.6 extra card waits (4.7 ms) per frame. |
| Direction host -> title | `copy_back_with` (`shadow.rs:2809`) for RAM-only copies; `card_merge` / `card_merge_relay` (`shadow.rs:3656`, `3721`) for card copies. Merge is **byte-exact**: only bytes that differ from the reference (snapshot or CPU twin) go into title memory, so bytes the title wrote concurrently survive (`merge_piece`, `shadow.rs:838`; reason: "putting back the copy's older bytes broke the title's own records, a fence list read as -1", `shadow.rs:2790-2794`). Title pages written by the merge are noted so the journal does not report them as title writes again (`ours_before/ours_after`, `watch.rs:786-860`; `VICTORY_QUIET_BACK`=1 play-zm). |
| When | Default config: the **relay** (`VICTORY_RELAY=2`, `host.rs:2693`): submissions are sent ahead as *flights* (`execute_late`, `host.rs:5692`, `submit(.., late=true)` returns after `ExecuteCommandLists` + `Signal`, `host.rs:6140-6170`). A dedicated thread (`relay_worker`, `host.rs:3425`; `relay_land`, `host.rs:5428`) lands flights in order as soon as the card has run them: fetch (own copy queue, `VICTORY_RELAY_OWN_QUEUE`, `host.rs:4914`, `4971`), merge, then the flight's read-backs, bottom-of-pipe value writes and fence signals (`late_signal`, `host.rs:3182`). The title learns "GPU done" only after landing. |
| What blocks | Settles (`Host::settle`, `host.rs:5789` -> `relay_wait`, `host.rs:3366`) wait until the relay thread landed everything. Reasons (`WHY_*`, `host.rs:2991-2999`): synchronous submission or present (`WHY_SYNC`), title memory read that a flight may write (buffer copy/fill of upload memory/texture, `WHY_READ`, relay stage 2 waits only for flights that declared the range), title bytes changed under shader writes (`WHY_STALE`, only when not write-watched or > `VICTORY_UNDER_WRITES_RUNS` runs; play-zm sets 100000), unmet WaitOnMemory (`late_met`, `host.rs:3214`), too many lists in the air (`WHY_RUN`/`WHY_MOST`, `VICTORY_LATE_WAIT_LISTS`, play-zm 1024), blocking fence wait (`WHY_FENCE`). In the c3 log: 30.6 submissions per frame in **0.0 settles**, 0.0 ms waiting. The relay thread itself landed in 29.5 ms (median) and idled 34.6 ms per frame waiting for the card. |
| Compute that reads GPU writes the title also reads | Textures: see 1.5/1.6. Raw 40-bit pointer shaders (BVH builder) read through `memory.rs`: a directory of 4 MiB windows plus per-slot raw UAVs; a page with no copy reads zero and the fault is only noted for the next frame (`memory.rs:1-60`). |

### 1.3 Memory outside any buffer object ("windows")

The title uses its GPU heap with no resource at all (interface vertices from its own allocator, constant data).
`shadow::loose`/`find_any` (`shadow.rs:4276`) gives such an ask a **window**: a copy of an aligned cell of title
memory.

- Grid: 4 MiB cells (`WINDOW`, `shadow.rs:4148`); `VICTORY_WINDOW_KB` (play-zm 1024) shrinks the grid for asks that
  give a length; `VICTORY_WINDOW_TAILS=1` (play-zm) starts a window at the ask's 256 KiB step to the cell's end
  (`shadow.rs:4201-4230`).
- Placement: windows >= `VICTORY_CARD_WINDOWS_MB` (play-zm 4096; code default 4096 on cards with >= 14 GB, else 0,
  `shadow.rs:352-366`) get a VRAM side like card buffers; **transient windows** with `VICTORY_CARD_WINDOW_UPLOAD=1`
  (play-zm) keep *no* CPU twin: changed pages are copied into pooled upload chunks (`windowup.rs`, pool
  `VICTORY_CARD_WINDOW_UPLOAD_MB`=512) and sent to the card side; the chunk is held until the submission has run
  (`Shadow::copy_to_chunk`, `shadow.rs:3499`; `Host::card_done`, `host.rs:4876`). Log: "window upload pool is full: a slab of
  its own" (21 warnings), 1,028 MB sent from chunks in one ram-line interval; 399 transient windows, 2.5 GB of
  windows in RAM at gameplay (`resident line`: "windows 2583 MB").
- Unused windows are dropped (`let_go_idle`, `shadow.rs:2404`).
- Experimental, off: `VICTORY_WINDOW_REBAR=1` (window is a GPU-upload-heap buffer, one copy, CPU must not read,
  `shadow.rs:401-450`).
- Experimental, off: `VICTORY_INPLACE=1/2` (`inplace.rs:60-100`): open a D3D12 heap *on* title memory
  (`OpenExistingHeapFromAddress1`) and place a buffer over it, so no copy exists either way. Constraints measured in
  `inplace.rs`: only whole private allocations or committed parts, pins pages (the title cannot decommit/protect
  them while open), view memory refused, no tile mapping into such a heap, ALLOW_CROSS_ADAPTER buffers only (PCIe
  reads by the GPU). Level 2 must not be combined with late wait.

### 1.4 Constant buffers and root data

- **Root constants** (`SetGraphicsRoot32BitConstants`): values are copied into the host list when recorded
  (`State::set_constants`, `draw.rs:671`; `host_word`, `draw.rs:917`): no memory sync involved.
- **Root CBV / root SRV addresses** (`State::set_view` -> `root_view`, `draw.rs:578`): finds the host copy for the
  title address (`shadow::find`, thread-local cache `FOUND`, `shadow.rs:3856-3898`), takes at most 64 KiB for a CBV
  (all that is left of the buffer for SRVs; `VICTORY_ROOT_VIEW_MB` can cap). If the host GPU address is not 256
  aligned (console needs 16) the bytes are copied each submission into list **scratch upload memory** (`HostList::scratch`,
  `host.rs:7165`; `defer_constants` -> `Step::Fill`, `host.rs:7040`; waits on flights only with
  `VICTORY_CONSTANTS_WAIT=1`). Otherwise `Shadow::refresh` as 1.1.
- **CBVs in descriptor tables** (`HostHeap::constant_at`, `descriptor/host.rs:2188`; `constant_size` caps at 64 KiB,
  `descriptor/host.rs:2785`): host view onto the copy; the copy is refreshed when the draw's table is made ready
  (`readable`, `draw.rs:3697`).
- Not tracked: constants a shader of this frame writes are read a frame late (`host.rs:7027-7034`).

### 1.5 Textures the title fills on the CPU (streamed pages, layout-library copies, movie frames, glyph atlases)

| Aspect | Today |
|---|---|
| Where bytes live | (1) title memory in console **tiled/swizzled** layout, RAM. (2) a **staging buffer** (`Staging`, `texture.rs:36-100`) in RAM holding the detiled subresources laid out as the host texture expects, pooled (`VICTORY_STAGE_POOL_MB`=256, `texture.rs:434-500`: 293,691 reuses vs 121,868 made in the log). (3) the **host texture** in VRAM (committed, or reserved with tile mappings for levels-on-demand, `tiles.rs:1-60`). The log's ram line at gameplay: 24,997 title-texture host copies alive = 4,051 MB, host textures 25,819 = 7,155 MB, 1,526 MB "filled idle but held", 32 GB of host copies let go so far (churn). |
| Detection | Per texture, per submission that samples it (`Step::Texture`, de-duplicated per submission by `Batch::texture_due`, `host.rs:7469`): `Staging::refresh` (`texture.rs:1030-1200`): (a) in write-watched memory, `watch::writes`: if no page was written since `last.asked`, return 0; (b) in view memory, guards (`VICTORY_TEXTURE_GUARD`, on) say "unwritten"; a texture the guards call quiet is still **BLAKE3-hashed whole** every `VICTORY_TEXTURE_HASH_EVERY` frames (code default 8, play-zm 32) by address-staggered turn, and every check while "unseen writes" were detected (`unseen_write`, `texture.rs:1201`); (c) partly mapped (streaming): re-read at most every 30 frames and hashed; (d) with the remap lock not obtainable `VICTORY_TEXTURE_NO_WAIT=1` (play-zm) reads through the kernel instead of waiting (`refresh_unlocked`, `texture.rs:1234`). |
| Direction | A changed texture is **fully detiled** (`Staging::detile`, `texture.rs:1258`; "a change detiles the whole texture", `texture.rs:14-15`) into the staging buffer, flagged dirty; the first list that samples it records `CopyTextureRegion` staging -> host texture for every level (`texture::upload_from`, `texture.rs:681`; uploads recorded once per dirty state). Levels on demand (`VICTORY_LEVELS_ON_DEMAND=1`, play-zm): only levels the title holds get tiles. |
| Textures picked by index (bindless, SM 6.6 heaps) | `State::heap_buffers`/`heap_textures` (`draw.rs:1522`, `1833`), **once per frame on whichever recording thread binds first**: every buffer view in the heap is made ready, every texture queued from heap writes is created and uploaded within `VICTORY_TEXTURE_STAGE_MB` (192; play-zm 512) and `VICTORY_TEXTURE_STAGE_MS` (250 ms) per pass (catch-up mode: 512 MB / 1.5 s for the first 60 s, `VICTORY_TEXTURE_GATE_S`). This is the frame line's "heap buffers 1.0 passes N ms": median **23.4 ms**, mean 40 ms, p90 88 ms in typical frames of the c3 log (76 ms texture part in loading frames). Admission needs card room (`resource::room`, `VICTORY_TEXTURE_ROOM_PERCENT`=90; "the texture queue is refused, the card has no room" x56). |
| Blocks | CPU only: the thread doing the pass; the staged upload sits behind a pooled staging buffer, the copy is on the queue ahead of the list. No GPU fence wait. |

### 1.6 Render targets and UAV textures (GPU-written textures)

- Live only in VRAM (`HostTexture`, `resource.rs`; target/depth/uav kinds in the ram line:
  "targets 1717 MB, depth 385 MB, uav 857 MB, twins 143 MB").
- **Never copied back to title memory**, except:
  - the whitelisted ocean maps (`texback.rs`, `VICTORY_OCEAN_READBACK=1` set by play-zm; kinds
    `256x256f41,256x256f16,64x64f16,128x128f36`): copied to a read-back buffer when the list closes and tiled into title
    memory after the list ran (`read_backs`, `host.rs:3777`), so the game's physics samples the water;
  - title-recorded texture -> buffer copies into title memory (`list.rs:1215-1260`, `defer_read_back`,
    `host.rs:7126`; counted as "a texture to buffer copy (a read back)").
- **Depth+UAV twins** (`resource.rs:470-500`, `host.rs:6807`, `1435`): a depth texture the title also uses as a UAV gets a
  colour twin; copies between them are recorded in the list *prelude* (`record_twin_copy`) when the next use is the
  other side (`twin_use`, `host.rs:6616`). Log: 131 twins, ~8 copies per frame.
- **Aliasing**: a placed texture in a different format over memory another host texture writes
  (sky, light probes viewed as R9G9B9E5) gets a GPU copy through a buffer of the written texture's texels
  (`texture::alias`, `texture.rs:134`; `resource::alias_of`, `resource.rs:1255`, `VICTORY_ALIAS_COPY`, on).
- Reads of such textures by title CPU code: not supported (documented shortcut `texback.rs:20-24`).

### 1.7 Descriptor heaps

- Heaps are plain zeroed title memory; descriptor "handles" are addresses (`descriptor.rs:1-8`), views 32 B,
  samplers 16 B (RDNA2 size) so title copies of whole descriptors work. A parallel **host heap** (up to
  1,000,000 views, log line 2) holds the real D3D12 descriptors (`HostHeap`, `descriptor/host.rs`).
- Two ways a host slot is made:
  1. `Create*View` calls mirror at the call (`mirror`, `descriptor/host.rs:2288`);
  2. the title also copies records into its heap **as bytes**. `sync_heaps` (`descriptor.rs:172`, called at the
     top of every submission, `host.rs:5959`) takes the write-watch pages of placed shader-visible heaps
     (`written.take`), reads changed pages through the kernel, skips slots "left to their draws"
     (`not_left_to_draws`), and calls `mirror_copied` (`descriptor/host.rs:2225`), which hashes the record with a
     multiply-fold (`record_print`, `descriptor/host.rs:2634`; BLAKE3 here was 35-42 ms/frame) and mirrors only new records.
     Log: median 215 changed pages, 248,285 slots looked at, 693 mirrored, 129,328 left to draws, **4.44 ms** median,
     142 ms max. `VICTORY_HEAP_SYNC=0` disables; `VICTORY_HEAP_SYNC_TABLES=1` mirrors per-draw table slots too (off).
- Per-draw descriptor tables (`State::tables`, `draw.rs:2700`; `readable`, `draw.rs:3697`): each draw that reads a table
  makes host slots in a dynamic region for what the title's table shows, brings the viewed buffers/textures up to date
  and records `SetGraphicsRootDescriptorTable`. Log: median 5,532 tables made per frame (40,881 slots), 3,396 found
  made and made ready again; deduped by table contents (`VICTORY_TABLES_BY_CONTENT`, on).
- Textures that a slot shows without any draw naming them go through the texture queue
  (`TextureQueue`, `descriptor/host.rs:560-780`): written slots get due times; slots whose memory is unmapped are
  re-looked at with backoff; remaps and page writes make slots due (`texture_slots_page_written`,
  `descriptor/host.rs:1405`; `texture_slots_remapped`, `1368`).
- Buffer views are made only "for a draw"; a view not on its copy's elements is given a copy of its own bytes
  (`VICTORY_VIEW_OWN_COPY`, on; `descriptor/host.rs:2652`, log "0.4 copies of their own bytes per frame").

### 1.8 Indirect arguments

- ExecuteIndirect argument/count buffers are title buffers the title's own compute passes often write
  (declared writes, 1.2). Before the GPU reads them, records are **clamped on the GPU** into a scratch buffer
  (`indirect.rs:1-20`; `State::run_indirect`, `draw.rs:3232`), with a per-list budget of 2^27 instances/threads.
- `VICTORY_CLAMP_AHEAD` (default on, `draw.rs:195`): the clamp pass runs in the list's **prelude** (`host.rs:6868-6899`,
  `indirect::record_ahead`), not in place: log "801 clamped ahead, 2,387 clamped in place".
- `VICTORY_INDIRECT_RAW`: unset = clamped; `2` (upstream) draw as written (hung this PC's GPU); **`3`** (play-zm):
  single draws whose 16/20-byte record the CPU can read, that no shader wrote this or the two previous frames, and
  that look sane (`record_sane`, `draw.rs:240`) are drawn raw. `VICTORY_INDIRECT_WINDOW` (off).
- Cost to memory: the CPU reads the record now (not when the list runs), and the "shader wrote it recently" test uses
  the declared-write history (`host::shader_written`, `host.rs:2430`).

### 1.9 Streamed texture pages (pool views, DirectStorage)

Streaming path: DirectStorage reads files into pool pages (`dstorage.rs:1129`, `kernel_will_write` first) -> the
title maps those pages into tiled heaps with `CopyPageMappingsBatchX` (`queue.rs:1166`, "done at once: every earlier
submission has run") -> the descriptor write arrives with no call (heap sync) or a remap event
(`title_memory_remapped` -> `descriptor::host::remapped`, `descriptor/host.rs:467`) -> the texture queue creates the
host texture for the placed view, stages and uploads within the per-frame budget -> memory seen "partly mapped" is
uploaded with what is there and re-looked at 1, 2, 4... frames later (`draw.rs:1950-2010`). When the card is full,
textures not named for `VICTORY_VRAM_IDLE_MS` (play-zm `15000,2500,1000` by pressure, `host.rs:175-240`) are let go
(`let_go_idle`, `texture.rs:534`) and come back black until re-uploaded (`VICTORY_REUPLOAD_WHEN_FULL`, on).

## 2. Cost model

### 2.1 What the log shows (c3, 125 frame lines)

The log mixes a loading phase (0.4-3 fps frames, shader compilation and texture catch-up, max 3,473 ms/frame) with
gameplay. Medians of the 82 "typical" frame lines (50-250 ms/frame; median **109 ms/frame = 9.1 fps**, 3,786 draws):

| Frame-line field | median | p10 / p90 | thread |
|---|---|---|---|
| `preparing` (submit step, sans settles) | **49.2 ms** (45 % of frame) | 21.6 / 65.5 | submission thread (title render thread) |
| of which `copies` (title buffer refresh) | 13.9 ms, 167 MB | 10.9 / 19.2 ms | submission (+ workers if `WIDE_REFRESH`) |
| of which `textures` (staging refresh) | 3.9 ms, 15 MB | 1.1 / 11.2 | submission |
| `on the GPU` (submission thread waiting for the card) | 0.0 ms | 0 / 3.5 | - |
| `copying back` (relay thread landing) | **30.9 ms**, 68 MB | 6 / 48 | relay thread, off the critical path |
| `heap buffers` pass | **23.4 ms** (mean 40.4, p90 88) | 7 / 88 | whichever recording thread binds first |
| process CPU of the whole PC / GPU busy | 34 % / card 46 % busy by its clock | | |

### 2.2 Per-frame mechanisms (typical frames; "up" = title -> host, "down" = host -> title)

Frequencies are per presented frame; ~30 submissions are sent per frame (log: 30.6), almost never settled.

| Mechanism | ms/frame | MB/frame | Frequency / cost driver | On the submission thread's critical path? |
|---|---|---|---|---|
| Title buffer refresh (`Shadow::update`, `copy`) | 13.9 (log), **26 (upstream)** | 167 (log), **194 (upstream)** | all declared ranges, 64 KiB pages stale since last copy; ~3,000 MB of buffer copies alive | Yes: runs in `Batch::flush`, inside `submit` |
| Other preparing: card sends (`card_before`: 50 MB in ~12,000 ranges, 900 masked 4 KiB blocks in 23 passes), snapshots, `ExecuteCommandLists` of ~30 lists, declared-write union | ~27-30 (derived: 49 - 14 - 4 - 4.4) | 47.5 up to the card | per submission | Yes |
| Write-watch asking (`ask_once`, `ask_ahead`, `ask_once_quiet`) | ~5 (upstream; not itemized in the log) | - | 1-30 kernel calls/frame, 30-70 us each plus TLB flush | Yes (inside `Batch::flush`/`submit` start) |
| Descriptor heap sync (`sync_heaps`) | **4.4** median (log), 6 (upstream), 143 max | - | ~215 changed heap pages, ~250,000 slots examined | Yes (start of every `submit`, `host.rs:5959`) |
| Texture refresh (all threads) | 5.0 median (mean 14.4), of which ask 1.0 | 15-21 | textures sampled in the submission; whole-texture detile on change | Yes (`Step::Texture`) except when run from the heap pass |
| Heap pass (`heap_buffers` + `heap_textures`) | **23.4** median | up to the stage budget (192-512 MB) | once per frame | No, but on a recording thread: the render thread waits for lists (upstream: ~48 % of render thread waiting for recording threads) |
| Per-draw bind/tables on recording threads | not itemized in the log; upstream: bind **44 %** of recording time, lock waits **19 %**, write-guard fault handler **8 %** | | 3,800 draws; 5,532 tables and 40,881 slots made per frame; thread-local `FOUND` cache and RW-lock tables already added | No (recording threads), but they set when the lists exist |
| Copy-back: compare on card (412 compare passes, 17,520 pieces), fetch (86.5 MB, 13.6 waits, 4.7 ms), merge (68 MB written to title) | 25-31 on the relay thread; card ~25 % extra | 86.5 down + 68 merged | per flight landed (16 landings, 30 flights per frame) | **No** while no settle happens (log: 0.0 settles, 0.0 ms waiting); **Yes** the moment a settle reason fires |
| Card clock | 41 ms idle median per frame ("card idle behind us" 38 ms while submit thread worked) | | card ~46 % busy (upstream: 35 %) | indicates CPU-bound pipeline |
| Window upload chunks (`windowup`) | included in copies | 1,028 MB between two ram-line samples (several seconds) | pool of 512 MB, overflow slabs | Yes (copy into chunk), then GPU copy |
| Host texture VRAM churn | texture refresh included | "let go 153,246 textures / 32 GB" cumulative; 65 GB of upload buffers made | idle eviction | indirectly |

### 2.3 Upstream profile cross-check (task statement)

| Item | share / value |
|---|---|
| Render thread in our submit step | 41 % |
| ... refreshing copies of game memory | 26 ms, 194 MB per frame |
| ... heap sync | 6 ms |
| ... write-watch | 5 ms |
| ... texture refresh | 4 ms |
| Render thread waiting for game recording threads | 48 % |
| Recording threads: bind / lock waits / guard fault handler | 44 % / 19 % / 8 % |
| GPU busy | 35 % |

The log's own split (49 ms of 109 ms preparing = 45 %) agrees with the 41 % submit share.

### 2.4 Memory footprint (log medians)

PC RAM working set ~12 GB (private commit up to 27 GB); title-buffer host copies 3.0 GB; windows 2.5 GB; texture
staging 47 MB; list scratch 128 MB; card (VRAM) 11.8 of 14.4 GB budget used, of which host textures 7.0 GB, card
buffer copies 1.66 GB, targets 1.7 GB, depth 0.4 GB, UAV 0.9 GB. Host-side: 111,699 upload buffers made (65 GB)
over the run, 13,596 shared buffers (16 GB).

## 3. Correctness hazards handled today

| Hazard | Where handled | How |
|---|---|---|
| GPU writes a buffer the CPU reads next frame | `Shadow::writing` / `copy_back*` / `card_merge*`; read-backs and bottom-of-pipe writes only after landing (`host.rs:3777`, `3182`) | declare ranges, compare, byte-exact merge into title memory, then signal the title |
| CPU writes bytes inside a range the GPU is writing (title and shader both) | `Shadow::update` busy cut-outs (`shadow.rs:3275`); `stale_under_flights` (`host.rs:3122`); `merge_under_writes` (`shadow.rs:3147`, `VICTORY_LATE_WAIT=2+`); `merge_piece` (`shadow.rs:838`); `Flights` counter per range (`shadow.rs:787`) | refresh skips the range; changed title bytes are merged into the copy without overwriting shader bytes; a byte both wrote: shader wins if it wrote later; two flights declaring one range keep it closed until both landed |
| Title writes after the GPU snapshot but before landing | `card_dirty` sends numbered (`shadow.rs:452`, `QUEUE_ORDER`, `host.rs:2727`) | a merge can tell title bytes the card never received from card writes (`card_merge_relay`, `shadow.rs:3721`; test `a_title_byte_sent_after_a_flight_survives_its_slot`) |
| Merge race on the same bytes | `VICTORY_MERGE_RACE` diagnostic (`shadow.rs:908`) | off |
| Title memory aliased by several host copies (nested buffers, textures inside buffers) | journal numbered per asking (`watch.rs:85-105`) | each copy keeps `copied[page]` and compares to the journal; our own copy-back writes are noted so they are not reported as title writes (`ours_after`) |
| Page remapping by the title (XMemMapPhysicalPagesScatter, CopyPageMappingsBatchX, unmap) | `title_memory_remapped` (`watch.rs:1885`); `MAPPING` shard locks (`watch.rs:1746`); `xmem::changing` (`xmem.rs:1030`) | guards reset, placed texture slots made due, in-place views dropped, reads in place never overlap a remap; `VICTORY_REMAP_SAME` skips no-op remaps |
| Writes through other views of the same physical pages (DirectStorage, aliases) | `VICTORY_VIEW_GUARD`, `watch::writing`, `written_through_view` | all views of a guarded page are read-only; kernel writers announce first |
| Texture memory aliasing: same bytes viewed in another format, or 2 title textures at one address | `texture::alias` (`texture.rs:134`), `host_tiled`, `resource::alias_of`, `VICTORY_PLACED_LOOSE_KEY` | GPU copy through a buffer; one host texture per address+shape; copy between identical host textures skipped |
| Texture streamed partly | `texture.rs:1090-1140`, `tiles.rs` | uploaded with zeros for unmapped parts, re-read on backoff, levels on demand |
| A buffer view the host would refuse (not on element, > width) | `buffer_srv`/`buffer_uav` (`descriptor/host.rs:2824`, `2912`), `VICTORY_VIEW_OWN_COPY` | view left null or rebased onto a copy of its own bytes (counted: 24 left null) |
| Indirect garbage records (GPU hangs) | `indirect.rs`, `VICTORY_CLAMP_AHEAD`, `INDIRECT_RAW=3` | clamp on GPU in the prelude or CPU sanity check |
| Title waits on memory values (WaitOnMemory) | `list.rs:2019 unmet_waits`; `late_met` (`host.rs:3214`) | submission held in the queue; or met by a flight's bottom-of-pipe write without waiting |
| Frames whose memory the title unmapped mid-read | `copy_mapped` fault handlers; `Reading` shared lock | missing page ends the copy, retried via kernel reads |
| Card out of VRAM | `resource::room`, card_for 97 % rule, `VICTORY_VRAM_IDLE_MS`, texture eviction | refuse new host textures/copies; evict idle textures (they flash black until back) |
| CPU read of a GPU-written texture (ocean) | `texback.rs` whitelist | only these four kinds |

Known unhandled or documented shortcuts relevant to a redesign: buffers read through directly indexed heaps keep
their first copy (`shadow.rs:11-15`); what picked UAV views write stays in the host copy (`draw.rs:1519`,
`descriptor/host.rs:2909`); a shader store to a window or an undeclared range is lost or seen late
(`shadow.rs:134-149`); RT/BVH pointers only work for pages already copied (`memory.rs`); constants a shader writes are
read a frame late; textures never flow back except the whitelist; unmapped tails of partly streamed textures are zeros.

## 4. Env switches that change memory behaviour

`tools/play-zm.ps1` first clears every `VICTORY_*` variable of the PowerShell window (`play-zm.ps1:45`) and then
sets the block below (`play-zm.ps1:47-88`). "code default" is what the source does with the variable unset.
`-Ram32` adds smaller pools (`play-zm.ps1:155`). `-RawIndirect` sets `INDIRECT_RAW=2` instead of 3.

### 4.1 Write tracking and kernel asking

| Variable | What it does | code default | play-zm |
|---|---|---|---|
| `VICTORY_ASK_ONCE` (`watch.rs:513`) | one write-watch asking for everything a submission refreshes | on | 1 |
| `VICTORY_ASK_QUIET` (`watch.rs:568`) / `_GAP_MB` (`watch.rs:575`) | 1 MiB blocks quiet for N frames asked once a frame; gap 32 MB | 0 (off) / 32 | 8 |
| `VICTORY_ASK_WIDE_MB` (`watch.rs:407`) | ranges within N MiB asked together (fewer TLB flushes) | 0 | unset |
| `VICTORY_WATCH_4K` (`watch.rs:270`) | 4 KiB granularity stamps inside 64 KiB pages | off | unset |
| `VICTORY_GUARD_HOT` (`watch.rs:128`) | blocks written 3 checks running left unguarded for N frames | 0 | unset |
| `VICTORY_GUARD_OPEN_AHEAD` (`watch.rs:1952`) | blocks opened with a fault | 1 | unset |
| `VICTORY_VIEW_GUARD` (`watch.rs:1461`) / `VICTORY_GUARD_HOLES` (`watch.rs:1392`) / `VICTORY_VIEW_ASK`, `GUARD_ASK` (`watch.rs:1264`) | guard every alias of a page; protect holes; ask kernel anyway for view memory | on / on / off | unset |
| `VICTORY_PAGE_WRITES` (`watch.rs:1641`) | how DirectStorage page writes are treated (guards, counts, slots, all) | guards | unset |
| `VICTORY_REMAP_SAME` (`xmem.rs:931`) / `VICTORY_ALIAS_RUNS` (`xmem.rs:957`) | do not redo identical remaps; map runs of aliases | 1 / on | unset |
| `VICTORY_REFRESH_CHECK` (`shadow.rs:3344`) | diagnostic: sample 256 B per block after a refresh to find undetected writes | off | unset |
| `VICTORY_TEXTURE_GUARD` (`texture.rs:1083`), `_HASH_EVERY` (`texture.rs:1092`) | guard textures in view memory; how often a guarded one is still hashed | on / 8 | unset / **32** |
| `VICTORY_TEXTURE_NO_WAIT` (`texture.rs:327`) | read through the kernel instead of waiting for a remap | off | **1** |
| `VICTORY_COMMITTED_QUERY`, `VICTORY_MAPPED_BY_PAGE`, `VICTORY_COPY_ASK` (`host.rs:7553`), `VICTORY_WAIT_ASK` (`list.rs:1968`) | ask VirtualQuery before reads | off | unset |
| `VICTORY_FAST_COPY` (`host.rs:7777`), `VICTORY_TITLE_COPY_PLAIN` (`host.rs:7573`), `VICTORY_DIRECT_WRITES` (`host.rs:7833`) | plain `rep movsb` with fault handler instead of kernel call; title-to-title plain copy; direct write | on | unset |

### 4.2 Buffer copies, windows, card sides

| Variable | What it does | code default | play-zm |
|---|---|---|---|
| `VICTORY_CARD_BUFFERS` (`shadow.rs:153`) | copies of big buffers in VRAM | on | unset |
| `VICTORY_CARD_BUFFERS_MIN_MB` / `_MIN_KB` (`shadow.rs:175`) | smallest buffer with a VRAM copy | 1 MB / unset | **64 KiB** |
| `VICTORY_CARD_BUFFERS_MB` | most bytes of VRAM copies | 1024 | **1536** |
| `VICTORY_CARD_ROOM_FOR_BUFFERS` (`shadow.rs:217`), `_FOR_TARGETS_MB` (`resource.rs:1567`) | texture queue leaves room for card copies / targets | on / (see code) | unset |
| `VICTORY_CARD_WINDOWS_MB` (`shadow.rs:363`) | VRAM side for windows | 4096 if budget >= 14 GB else 0 | **4096** (3072 with `-Ram32`) |
| `VICTORY_CARD_WINDOW_UPLOAD` / `_MB` (`shadow.rs:1896`, `windowup.rs:21`) | transient windows keep no CPU twin, pooled upload chunks | off / 512 | **1** / unset |
| `VICTORY_WINDOW_KB` / `VICTORY_WINDOW_TAILS` (`shadow.rs:4164`, `4203`) | window grid (64-4096 KiB); tail windows | 4096 / off | **1024** / **1** |
| `VICTORY_WINDOW_REBAR` (`shadow.rs:403`) | window = single GPU-upload buffer, no twin | off | unset |
| `VICTORY_INPLACE` / `_MB` (`inplace.rs:90`, `98`) | place buffers over title memory (heap from address) | 0 (off) / 4096 | unset |
| `VICTORY_FOLD_BUFFERS` (`shadow.rs:4135`), `VICTORY_LOOKUP_CACHE` (`shadow.rs:3866`) | small buffers folded into one copy; per-thread lookup cache | on / on | unset |
| `VICTORY_COPY_CHANGED` (`shadow.rs:1886`) | refresh of card copies compares and dirties only changed bytes | off | **1** |
| `VICTORY_WIDE_REFRESH` (`host.rs:7403`), `VICTORY_WIDE_THREADS` (`shadow.rs:1446`), `VICTORY_WIDE_BACK` (`shadow.rs:1408`), `VICTORY_WIDE_MERGE` (`host.rs:2362`) | refresh/compare/merge across worker threads | off / auto 2-6 / on / off | **1** / **12** / unset / **1** |
| `VICTORY_JOIN_COPIES` (`host.rs:3713`) | adjacent title buffer copies recorded as one | off | **1** |
| `VICTORY_UNWATCHED_FRAMES` (`draw.rs:1642`) | interval for whole copies of unwatched memory in the heap pass | 1 | unset |
| `VICTORY_ROOT_VIEW_MB` (`draw.rs:581`), `VICTORY_ROOT_UAV_MB` (`draw.rs:1334`) | cap on bytes a root view brings up to date | unlimited | unset |
| `VICTORY_CONSTANTS_WAIT` (`host.rs:7042`) | constant fills wait for flights | off | unset |
| `VICTORY_SCRATCH_POOL` (`host.rs:1641`), `VICTORY_LIST_POOL` (`list.rs:296`) `_MB`, `_MOST` (`list.rs:259-260`), `VICTORY_STAGE_POOL_MB` (`texture.rs:453`) | pooled scratch/lists/staging | on / on, defaults in code / 256 | LIST_POOL_MB **2048**, MOST **2048** (512/1024 with `-Ram32`), STAGE unset |

### 4.3 Submission, settles, copy-back

| Variable | What it does | code default | play-zm |
|---|---|---|---|
| `VICTORY_RELAY` (`shadow.rs:754`, `host.rs:2693`) | copy-back on its own thread; 1 stage 1, 2 stage 2 (reads wait only for flights that declared the range) | on (stage 2) except in tests | **2** |
| `VICTORY_RELAY_OWN_QUEUE` (`host.rs:2164`), `VICTORY_RELAY_KEPT_PIECES` (`host.rs:2174`) | fetches on a copy queue of their own; pieces kept on card by the compare | off / 2048 | **1** / unset |
| `VICTORY_LATE_WAIT` (`host.rs:2609`) | 0 off, 1-3 stages of sending submissions ahead; relay implies 3 (present sent ahead too) | unset (3 with relay) | unset |
| `VICTORY_LATE_WAIT_LISTS` (`host.rs:2895`) | lists in the air before settling | 256 | **1024** |
| `VICTORY_LATE_WAIT_WRITES` (`host.rs:2905`) | bottom-of-pipe writes tell the title when flights ran | on | unset |
| `VICTORY_UNDER_WRITES_RUNS` (`host.rs:3114`) | title-changed runs under shader writes sent up before settling | 512 | **100000** |
| `VICTORY_UNDER_WRITES_WORDS` (`shadow.rs:2008`) | widen runs to 4-byte words | off | **1** |
| `VICTORY_UNDER_WRITES_MASKS` / `_MASKS_LEAST` (`host.rs:1991`, `1972`) | crowded 4 KiB blocks go up by one masked pass | off / 4 | **1** / **1** |
| `VICTORY_QUIET_BACK` (`watch.rs:773`), `_TOGETHER` (`watch.rs:851`), `_WATCHED` (`watch.rs:780`) | copy-back writes not re-read as title writes | off / on / off | **1** |
| `VICTORY_CARD_REHASH_RUNS` (`host.rs:2344`) | join hash passes after sends | off | **1** |
| `VICTORY_CARD_COPIES` (`list.rs:927`) | title buffer copies of shader-written sources run on the card | on | unset |
| `VICTORY_CARD_FETCH_INFLIGHT` / `_MB` (`host.rs:2211`, `2221`) | each flight fetches into its own slot while in the air | off / 64 | unset |
| `VICTORY_KEEP_SNAPSHOTS` / `_MB` (`shadow.rs:2228`, `2239`) | keep snapshot per 64 KiB piece across submissions (not with relay) | off / 256 | unset |
| `VICTORY_NO_COPY_BACK`, `VICTORY_MERGE_RACE` (`host.rs:2454`, `shadow.rs:910`) | experiment/diagnostic | off | unset |
| `VICTORY_ASYNC_SUBMIT` / `_PRESENT` (`queue.rs:76`, `92`) | submit on a thread of ours, call returns at once | off / on with async | unset |
| `VICTORY_CUT_WAITS`, `VICTORY_CUT_WRITES` (`queue.rs:248`, `list.rs:2170`) | split lists at waits / value writes | on with split / off | unset |
| `VICTORY_SIGNAL_NOW` (`host.rs:3191`) | signal fences when asked, not behind flights | off | unset |
| `VICTORY_D3D12_BACKGROUND` (`host.rs:711`) | host's background shader work | on | **0** |

### 4.4 Descriptors, textures, VRAM budget

| Variable | What it does | code default | play-zm |
|---|---|---|---|
| `VICTORY_HEAP_SYNC` (`descriptor.rs:175`), `_BLOCKS`, `_TABLES` (`descriptor/host.rs:1843`, `2617`) | mirror byte-copied descriptors; per-block; include per-draw table slots | on / on / off | unset |
| `VICTORY_HEAP_TEXTURES`, `VICTORY_HEAP_RECHECK`, `VICTORY_FULL_CARD_PASS`, `VICTORY_REUPLOAD_WHEN_FULL` (`draw.rs:1840`, `495`, `513`, `521`) | texture pass for picked-by-index textures; rechecks; card pass; re-upload when full | on | unset |
| `VICTORY_TABLES_BY_CONTENT`, `VICTORY_READY_TABLES`, `VICTORY_VIEW_OWN_COPY`, `VICTORY_VIEW_EXTENT`, `VICTORY_KEEP_VIEWS`, `VICTORY_KEEP_OTHER_VIEWS`, `VICTORY_STALE_TEXTURES`, `VICTORY_TABLED_TEXTURES` | descriptor table and view reuse rules | on | unset |
| `VICTORY_DESC_DATA` | shaders reading a descriptor as data are translated | (see code) | **1** |
| `VICTORY_TEXTURE_STAGE_MB` / `_MS` (`draw.rs:1848`, `1933`) | bytes/time of texture staging per frame and pass | 192 / 250 | **512** (256 with `-Ram32`) / unset |
| `VICTORY_TEXTURE_CATCHUP` / `_GATE_S` (`draw.rs:1946`, `451`) | 512 MB, 1.5 s passes for the first 60 s | on / 60 | unset |
| `VICTORY_EARLY_TEXTURES_MS`, `VICTORY_BAKE_TEXTURES_MS` (`draw.rs:98`) | textures uploaded early before a draw / before the character bake | 40 / 10000 | **40 / 10000** (same as code) |
| `VICTORY_LEVELS_ON_DEMAND` / `_MIN_KB` (`tiles.rs:81`, `120`) | reserved textures, tiles per level only | off | **1** |
| `VICTORY_TEXTURE_ROOM_PERCENT` (`resource.rs:1606`), `VICTORY_TEXTURE_BUDGET_MB` (`resource.rs:1485`) | share of VRAM budget textures may take | 90 / none | unset |
| `VICTORY_VRAM_IDLE_MS` (`host.rs:204`), `_BY_ROOM` (`host.rs:230`) | idle ms before an unused host texture is let go, by pressure tiers | scaled by pressure / on | **15000,2500,1000** |
| `VICTORY_VRAM_EVICT`, `VICTORY_SLOT_EVICT` (+ `_MB`, `_PERCENT`), `VICTORY_TEXTURE_SWEEP`, `VICTORY_LAZY_TEXTURES` | eviction rules | see code | unset |
| `VICTORY_DEPTH_TWINS` (`resource.rs:493`), `VICTORY_ALIAS_COPY` (`resource.rs:1273`), `VICTORY_PLACED_FILLER` | depth/UAV twins; alias texture copy; filler words | on / on / off | unset |
| `VICTORY_OCEAN_READBACK` / `VICTORY_TEXTURE_BACK` (`texback.rs:37`, `48`), `VICTORY_READ_BACK` (`list.rs:1230`) | GPU texture -> title memory copies | off / four ocean kinds / on | **1** / unset / unset |
| `VICTORY_INDIRECT_RAW` (`draw.rs:218`, `240-257`), `VICTORY_CLAMP_AHEAD` (`draw.rs:195`), `VICTORY_INDIRECT_WINDOW` (`draw.rs:261`) | indirect args: clamped / raw / checked-raw | unset = clamped / on / off | **3** |
| `VICTORY_PIPELINE_WAIT_MS` (`pipeline/make.rs:183`) | frame waits at most this long for a shader being compiled | 12000 | **2500** |

## 5. Things the map makes obvious (observations, not proposals)

- Total per-frame bytes moved by CPU in the submit step are of the order of 200 MB (copies) + 15-50 MB (textures)
  + 50 MB up to the card + 87 MB fetched down + 68 MB merged, against an `~9 fps` frame. Most of those bytes are
  re-copies of memory that did not change in meaning (64 KiB page granularity; `copy_changed` already trims card
  sends to differing bytes but the CPU still reads and compares the whole stale page).
- The only synchronous GPU->CPU mechanism left on the hot path is the relay landing, and it already runs on its own
  thread; the log shows the submission thread not waiting for the card at all. The CPU-side costs therefore
  dominate: refresh (buffers and windows), descriptor sync/mirroring, per-draw table making and the once-per-frame heap pass.
- Three kinds of memory are copied three times (title RAM, CPU twin RAM, VRAM): buffers >= 64 KiB and windows with
  card sides, and textures (title tiled, staging, VRAM). `inplace.rs` and `window_rebar` are the existing experiments
  that remove the RAM twin; both are off, with documented driver constraints.
- Correctness machinery (flights, relay, hashes, numbering of sends) exists because one thread, one lock and one
  snapshot per copy were the original model; any change of sync direction must keep the guarantees in section 3.
