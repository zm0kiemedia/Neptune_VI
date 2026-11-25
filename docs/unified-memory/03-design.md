# Unified memory for Neptune VI: staged design

Status: design, nothing implemented. Inputs: `01-current-architecture.md` (cost map), `02-techniques.md` (survey).
Paths are relative to `crates/victory-gpu/src/` unless stated. Every stage is independently shippable behind a
default-off `VICTORY_UM_*` switch, and `tools/play-zm.ps1` only gets a line once the stage has proven itself.
Gains are estimates with the reasoning shown; the spikes (section 2) and the probes decide which stages are worth doing.

Baseline (c3 log, gameplay medians): 109 ms/frame, submission step 49 ms (buffer refresh 13.9 ms/167 MB, textures
3.9, heap sync 4.4, write-watch asking ~5, other ~27-30), recording-thread heap pass 23 ms median, copy-back 31 ms
on the relay thread (off the critical path while no settle fires), GPU 46 % busy, RAM 13-17 GB.

## 1. Findings that shape the design (verified in code)

### 1.1 Is the per-frame bind/table work memory-emulation driven? Mostly not.

The biggest item (recording threads: bind 44 %, lock waits 19 %, guard faults 8 %; render thread waits 48 % of its
time for them) splits into five parts. Only parts (c) and (d) are memory emulation, and none of the five is cached
across frames by per-page dirty bits as the code stands.

| Part | Evidence | Memory driven? |
|---|---|---|
| (a) Tables with a CBV are always rebuilt per draw | `State::tables` forces `fresh` for every table that has a `ResourceType::Cbv` entry (`draw.rs:2706-2712`); each CBV is copied to list scratch (`list.scratch`, `draw.rs:2925`; `defer_constants`, `draw.rs:2929`) because the host wants 256-byte aligned CBVs, the console 16 (`descriptor/host.rs:2785` `constant_size`, `host.rs:7168` rounds to 256). | No. It is an alignment/representation cost, not a coherency one. |
| (b) Descriptor words hashed per draw to find a table "by content" | `DefaultHasher` (SipHash) over `n * 12` bytes per table (`draw.rs:2794-2803`). 5,532 tables, 40,881 slots a frame in the log. | No. Pure CPU. |
| (c) Views made ready per draw | `readable()` (`draw.rs:3697`) calls `shadow.refresh` -> `defer_refresh` (`host.rs:7051`: per-list mutex + HashSet insert + `Arc` clone) for each buffer view, `upload_from` for textures, `ensure_state`. The ready cache `ready_tables` is per `State` (per recording thread), per list, and is dropped when `list.changes` moves (`draw.rs:2845-2885`). Log: of ~3,880 tables made, 2,537 "not noted ready", 665 "after a change in the list". | Partly. The work exists because buffer/texture copies must be refreshed at submission, but most of it re-declares ranges that are clean. |
| (d) Lock waits | `Shadow::copied` is a `Mutex<Vec<u64>>` per copy (`shadow.rs:37`, `2565`), `watch::JOURNAL` is a global `RwLock` (`watch.rs:74`), `MAPPING` shards (`watch.rs:1746`), `deferred()` mutex per list (`host.rs:6992`). | Yes: page-state bookkeeping. This is where lock-free page state pays. |
| (e) Heap pass | `heap_buffers` (`draw.rs:1522`) once per frame on the first binding thread; the buffer part looks at ~1,300 slots (median) and the texture part (`heap_textures`, `draw.rs:1833`) does the real work: detile + stage + upload within 192-512 MB / 250 ms. Log: "773,288 looks found them unchanged, 501 changed". | Yes, and mostly texture streaming, not buffers. |

Can per-page dirty bits let tables be cached across frames? Not alone, for two reasons seen in the code. First, the
cache key includes the title's descriptor words and the title writes each draw's table at a place of its own
(`draw.rs:2800-2803` comment; 585-622 tables per frame "at a new address that took another's slots"), so the key
changes unless the title's allocator repeats addresses. Second, a made table points at list-local scratch CBV copies
and dynamic host slots (`host.dynamic`, `draw.rs:2903`) that die with the list. A cross-frame cache therefore needs
(1) a persistent slot region and persistent constant storage, (2) an epoch per viewed resource so a table is valid
only while its views did not change, (3) the heap-page write epoch from `sync_heaps` to skip rehashing unchanged
table pages. Per-page dirty state is one of three ingredients, and probe P3 (section 2) measures how much repeats
before anything is built. Cheap wins (stage U3: faster hash, constant dedupe, skipping clean re-declarations)
come first and need none of it.

### 1.2 Other facts the stages rely on

- Write watch is 4 KiB in the kernel, folded to 64 KiB (`watch.rs:28`, `1028-1075`). `copy_stale` copies whole
  stale 64 KiB pages unless `VICTORY_WATCH_4K` (`shadow.rs:3398-3438`). 167-194 MB/frame of copies means roughly
  2,700-3,100 stale pages a frame; how much of that is real change is unknown (probe P1).
- The card side already hashes and fetches by piece: 14,715-17,520 pieces compared, 1,618-1,813 fetched (10-12 %).
- `card_before` sent 72.4 MB in **39,596 ranges** per frame in one sample (12,000 in another): per-range cost
  (bookkeeping + `CopyBufferRegion`) is likely a real part of the 27-30 ms "other prep", not itemised today.
- `window_rebar`, `gpu_upload_supported`, `gpu_upload_buffer` (`shadow.rs:397-450`, `host.rs:8981-9000`) and the
  in-place machinery with a built-in probe (`inplace.rs:705-1000`, `VICTORY_INPLACE_PROBE`) already exist. The c3 log
  has no probe output and no "host GPU upload heap support" line: they were never run on the target PC.
- Existing probe constraints (from `inplace.rs:10-50`, measured on WARP only): heap-from-address needs a whole
  committed part with one protection, locks pages, forbids tile mapping into it (device removal), buffers need
  `ALLOW_CROSS_ADAPTER`, the title cannot decommit/protect while it stands.
- Reserved resources are already used for textures (`tiles.rs:994`, `UpdateTileMappings` at `tiles.rs:557`: 13,063
  calls / 381 ms total in the log, about 29 us a call), so the tile machinery is proven on this driver, for textures.

## 2. Spikes and probes (stage U0, run first)

Two kinds: **tools** (one tiny standalone binary `tools/um-spikes`, subcommands `s1`..`s5`, printing lines
`SPIKE sN key=value ...`, runnable without the title) and **probes** (env-gated in the runtime, one info line per
10 s, `UM-PROBE pN ...`). The user runs each once and pastes the `SPIKE`/`UM-PROBE` lines. Effort: S each, M for s4.

| Id | Question | How | Decides |
|---|---|---|---|
| **S1** heap-from-address on NVIDIA with write-watched memory | Does `OpenExistingHeapFromAddress1` accept `MEM_WRITE_WATCH` private memory on the RTX 5070 Ti? Does `GetWriteWatch` still work, and do GPU (UAV) writes set write-watch bits? PCIe read speed? | First run the existing `VICTORY_INPLACE_PROBE=1` (it already allocates with `MEM_WRITE_WATCH`, `inplace.rs:761-789`, and measures a copy speed, `inplace.rs:898-910`) and report. Extend it, or the tool, with: (1) CPU writes a pattern, GPU compute copies it out, CPU verifies; (2) GPU UAV-writes a 64 KiB block, then `GetWriteWatch` on it (does DMA mark pages? it decides whether our own copy-back stays invisible to the journal); (3) compute pass reads 1 GiB through the heap buffer: GB/s; (4) 100 `VirtualAlloc` regions of 16 MiB opened as heaps: open time, working-set growth; (5) `VirtualProtect` and `VirtualFree` on an opened part: error code. Log `SPIKE s1 open=ok|fail(hr) watch_after_open=ok gpu_write_marks_watch=0|1 read_gbps=.. open_ms_per_gib=..`. | U10, and whether U7 may use heap-from-address for constants |
| **S2** GPU_UPLOAD without Developer Mode | Is `GPUUploadHeapSupported` true on this driver, Windows 11 24H2+, Developer Mode off? CPU write speed (sequential, non-temporal) into a mapped GPU_UPLOAD buffer, GPU read speed, behaviour when VRAM is nearly full (spec says it falls back to system memory). | Tool: `CheckFeatureSupport(OPTIONS16)`, create a 256 MiB `D3D12_HEAP_TYPE_GPU_UPLOAD` buffer, `Map`, write 256 MiB with `copy_nonoverlapping` and with `_mm_stream_si128`, fence, compute pass reads it. Run it twice by the user: Developer Mode off, and on, if the first says unsupported. Also in the runtime: `VICTORY_UM_PROBE=1` prints the existing `host GPU upload heap support` line plus `host.video_memory()` and ReBAR size (DXGI adapter dedicated visible memory if available). Log `SPIKE s2 supported=0|1 devmode=? write_gbps=.. nt_write_gbps=.. gpu_read_gbps=..`. | U7 |
| **S3** `GetWriteWatch` over 13 GB | Cost of one asking over the whole 13 GB, and of 1,000 askings of 1 MiB blocks (the `ask_once_quiet` pattern), with 0.1 %, 1 %, 10 % pages dirty; TLB flush effect on a busy second thread. | Tool: `VirtualAlloc(MEM_WRITE_WATCH)` 13 GiB (commit only touched 6 GiB), a worker thread streaming memory, main thread timing `GetWriteWatch(RESET)` for whole-range, per-64 MiB and per-1 MiB blocks. Log `SPIKE s3 whole_ms=.. per64mb_ms=.. per1mb_us=.. dirty=..%  worker_slowdown=..%`. | Whether to ask the kernel once per frame for everything and keep a bitset (U4), or keep the 1 MiB quiet scheme |
| **S4** reserved-buffer mirror with 64 KiB tiles | Can a reserved buffer of 16 and 64 GiB be created on this card? `UpdateTileMappings` cost for 1, 100, 10,000 tiles in one call; raw SRV/UAV over a window of it; unmapped read returns 0 (and `CheckAccessFullyMapped` reports it); two tile mappings of one heap tile at two offsets (aliasing); CopyBufferRegion into a mapped tile from an upload buffer; GPU VA limit. | Tool: `CreateReservedResource` buffer, a 1 GiB heap pool, map tiles in batches, upload a pattern, compute shader reads at a sampled set of offsets including tile boundaries and the 4 GiB mark. Log `SPIKE s4 max_gib=.. map_us_per_tile=.. unmapped_reads_zero=.. alias_ok=.. root_srv_over_4gib=..`. | U9 |
| **S5** frame-level page waste | How much of the 167 MB/frame is actually different from what the copy held? How many 4 KiB pages the kernel reports vs 64 KiB pages copied? | Probe `VICTORY_UM_PROBE=1` in `Shadow::copy_stale`: per frame, count 4 KiB pages found written (`watch.rs` `COUNTS[7]` already), 64 KiB pages copied, bytes copied, and (sampled, 1 copy in 16) bytes that differ from the previous copy contents using the existing `changed_spans`. Log `UM-PROBE p1 pages4k=.. pages64k=.. copied_mb=.. differing_mb=.. by_kind(card|ram|window)`. | U5 and U4 |

Additional runtime probes (all passive, logging only, no behaviour change):

- **P2 submission itemisation.** Timers around `card_before` (ranges, bytes, us), `Batch::flush` ask phase, and
  `ExecuteCommandLists`: `UM-PROBE p2 card_before_ms=.. ranges=.. exec_ms=..`. Decides U1 gain.
- **P3 table stability.** At the end of each frame (per recording thread, merged), count tables whose content hash
  was also made in the previous frame, tables at the same title address with the same hash, tables whose title page
  was not written since last frame (heap `written` epoch), and CBV bytes copied to scratch vs bytes of those CBV
  ranges whose page journal says unchanged. `UM-PROBE p3 tables=.. same_hash_prev=.. same_addr_hash=.. page_clean=.. cbv_copy_mb=.. cbv_clean_mb=..`. Decides U3 and U6.
- **P4 texture looks.** Per frame: looks, looks that found change, ns spent, split by guard / hash / unmapped
  (counters beside `TextureQueue`, `descriptor/host.rs:560-780`; `texture.rs:1083-1200`). Decides U2.
- **P5 CPU reads of GPU-written pages (for U8).** After a relay merge, for a 2 % random sample of merged 64 KiB pages
  (only private, non-view memory), set `PAGE_NOACCESS` through a vectored handler registered *before* the
  `fast_copy` handler (`host.rs:7720-7800` swallows faults on missing pages: that swallow must not eat ours),
  restore on the first touch and log the touch kind (read/write), thread, and frames since the merge. Data is already
  fetched, so correctness is unaffected. `UM-PROBE p5 sampled=.. touched_read=.. touched_write=.. never_touched_in_8_frames=..`.
  Risk: protect/fault cost on sampled pages only, and the fault storm if the title polls them; 2 % keeps it small.

Spike results route the plan: if S1 and S2 are negative, U7/U10 are dropped and nothing else changes; if S4 is
negative, U9 is dropped. U1-U6 depend on no spike (S3 only tunes U4, S5 only sizes U5).

## 3. Stages (ordered by gain over effort)

Common data structure used from U4 on, the **page state table** (one entry per 64 KiB title page, per reservation
shard like `PARTS`, `watch.rs:100`), kept as lock-free atomics:

```
struct PageState {            // 16 bytes, one per 64 KiB title page
    cpu_epoch: AtomicU32,     // journal number of the last asking that found the page written (replaces JOURNAL[page])
    mask4k:    AtomicU16,     // 4 KiB sub-pages written since the page was last taken by anyone (stale_mask today)
    gpu_dirty: AtomicU8,      // 0 clean, 1 declared writable by an unlanded flight, 2 landed and merged-pending
    resident:  AtomicU8,      // 0 none, 1 card copy valid for [epoch..], 2 pinned (in-place / rebar)
    owner:     AtomicU8,      // see state machine
    flights:   AtomicU8,      // unlanded flights declaring this page (today's Flights, shadow.rs:787)
    uploaded:  AtomicU32,     // cpu_epoch value the card side last received (card side only)
}
```

Ownership state machine per page (transitions are the existing events, made explicit):

```
CPU_ONLY --declare read--> SHARED_CLEAN --title write (journal)--> CPU_DIRTY --refresh/upload--> SHARED_CLEAN
SHARED_CLEAN --declare GPU write--> GPU_DIRTY(flights>0)
GPU_DIRTY --title write--> BOTH_DIRTY (merge_under_writes / byte-exact merge, today's rules, shadow.rs:3147, 838)
GPU_DIRTY/BOTH_DIRTY --relay lands (compare, fetch, merge)--> SHARED_CLEAN
any --remap/free (title_memory_remapped, watch.rs:1885)--> CPU_ONLY
```

Rule of the table: **no stage may change the byte-exact merge or the landing order**; the table only answers "is
there work to do for this page" faster than today's locks and scans, and records the facts later stages need.

### U1. Coalesced card sends  `VICTORY_UM_COALESCE_KB=<n>` (default off, 0)

- Attacks: the 39,596 ranges/frame of `Host::card_before` (`host.rs:4496`), part of the 27-30 ms "other prep" and
  of 72 MB sent up.
- Mechanism: after `Shadow::card_dirty` (`shadow.rs:452`) returns runs per copy, join runs whose gap is under `n` KiB
  (try 4, 16) when the gap pages are not GPU-dirty or in a flight (`Shadow::flights` and the busy cut-outs, `shadow.rs:3275-3300`);
  gap bytes come from the CPU twin, which equals title bytes for any page the journal says is clean, so the send is
  harmless where nothing runs on it. Masked passes (`host.rs:1991-2070`) stay for dense sub-4 KiB crowds. Record
  the joined runs as one `CopyBufferRegion` each; with several shadows in one card buffer already adjacent they
  already join (`VICTORY_JOIN_COPIES`).
- Files: `host.rs` (`card_before`, plan building), `shadow.rs` (`card_dirty`, the `sent` numbering: a joined send gets
  one number; `card_merge_relay` must see the gap bytes as card-held, so note the whole joined span in `sent`).
- Risks: a gap that is GPU-dirty in a flight overwrites shader output (guarded by the busy test); gap bytes sent
  that the card already changed in an earlier flight that has landed but whose merge not yet applied to the twin
  (the twin is updated at the merge, so wait for that, same rule as `QUEUE_ORDER`, `host.rs:2727`).
  Test: unit test next to `a_title_byte_sent_after_a_flight_survives_its_slot` with a gap inside a dirty flight; a
  debug check (`VICTORY_UM_VERIFY=1`) that after landing a sampled copy's card side equals twin on clean pages.
- Expected gain: 3-8 ms on the submission thread if P2 shows `card_before` at 8+ ms; ranges/frame down 3-5x; the
  extra bytes are bounded by the gap (a few MB). Effort **S**.

### U2. Texture look diet  `VICTORY_UM_TEXLOOK=1`

- Attacks: heap pass textures part (median 5.6 ms, mean 14+, p90 88 ms with 233 ms in loading frames) and the
  texture refresh 3.9 ms median; "773,288 looks found unchanged vs 501 changed", BLAKE3 whole-texture hashes every
  8/32 frames (`texture.rs:1083-1200`), re-looks of partly mapped textures every 30 frames.
- Mechanism: a texture's validity becomes `(mapping_epoch, write_epoch_max)` of its pages, taken from the page
  state table (U4) or, before U4, from the existing journal and guards: no hash while guards cover the pages, hash
  only (a) once at creation, (b) when the guard state says "unseen writes" (`unseen_write`, `texture.rs:1201`),
  (c) on a long timer (256 frames) as a safety net. Partly mapped textures are re-looked only when the page range
  gets a `title_memory_remapped` event or a DirectStorage `written_through_view` (the events exist: 451,198 pool
  writes in the log), not by timer.
- Files: `texture.rs` (`Staging::refresh`), `descriptor/host.rs` (`TextureQueue`, `texture_slots_page_written`).
- Risks: stale texture when a write path is invisible to guards (DirectStorage is announced, aliases are guarded by
  `VICTORY_VIEW_GUARD`); the existing `VICTORY_REFRESH_CHECK` diagnostic extended to textures (sample 256 B of each
  guarded texture's first page every 64 frames, flag a mismatch) is the test. Soak test: 20 minutes with
  `HASH_EVERY=1` vs `UM_TEXLOOK=1`, compare screenshots at fixed spots (the project has `screenshot.rs`).
- Gain: P4 decides; expect heap pass median 23 -> 12-16 ms and p90 88 -> 40 (the recording thread that binds first
  is released sooner), submit-side texture refresh -1 to -2 ms. Effort **S-M**.

### U3. Table and bind diet  `VICTORY_UM_TABLES=1` (lite) then `=2` (cache, see U6)

- Attacks: parts (a), (b), (c) of 1.1: recording-thread bind 44 %.
- Mechanism (level 1, no cross-frame state):
  1. Replace SipHash with a 64-bit multiply-fold or xxh3 over the descriptor words (`draw.rs:2794-2803`); the same
     fold already exists as `record_print` (`descriptor/host.rs:2634`) and BLAKE3 was found 35-42 ms/frame vs the fold.
  2. Skip rehashing a table when the heap page that holds it was not written since this thread last hashed it:
     the heap's write-watch `written` ranges feed `sync_heaps`; add a per-heap-page `epoch` bump at `written.take`
     and cache `(table address, n) -> (epoch, hash)` in `State`.
  3. CBV scratch dedupe: key `(address, size)` per list; a second table naming the same CBV range gets the first
     copy's GPU address (`list.scratch` + `defer_constants` once per list per range instead of per fresh table).
  4. Clean re-declaration skip: in `readable()`, test the dedupe set first with a thread-local, lock-free
     structure (per-`State` `HashSet<(shadow id, from, to)>` folded into `deferred.refreshed` at list close) so the
     `deferred()` mutex is taken once per list, not once per view.
  5. When the console CBV address is already 256 aligned inside a card or rebar copy, bind the host copy directly
     (as `root_view` does) instead of a scratch copy.
- Files: `draw.rs` (`tables`, `readable`, `State`), `host.rs` (`HostList::scratch`, `defer_refresh`, `defer_constants`),
  `descriptor/host.rs`, `descriptor.rs` (`sync_heaps` epoch).
- Risks: a table whose descriptor words did not change but whose CBV *target data* did: still correct, because the
  data is refreshed at submission from title memory, not cached; a rehash skip wrongly trusting a page epoch if the
  title writes a table page between our epoch read and the hash (read epoch before hashing; a write after gives a
  newer epoch next time). Test: the existing table counters (`descriptor tables per frame`) must give the same
  made/ready totals within 1 %; run with `VICTORY_UM_TABLES=1` and diff screenshots plus `draws/frame`.
- Gain: P3 sizes it. Reasoning: bind is 44 % of recording time; hash + scratch + declarations are plausibly half of
  that; the render thread waits 48 % of its time on recording threads, so 15-25 % less recording time is worth
  about 5-10 ms of frame. Effort **S-M**.

### U4. Lock-free page state  `VICTORY_UM_PAGESTATE=1`

- Attacks: lock waits 19 % of recording time, the global `JOURNAL` `RwLock` (`watch.rs:74`), `Shadow::copied`
  mutex scans, and gives the base for U5/U6/U8/U9.
- Mechanism: the `PageState` table above. `watch::writes`/`ask_*` write `cpu_epoch` with relaxed atomics (the
  journal's `found_set` numbering is already a monotonic u64; narrow to u32 with wrap compare). `Shadow::copy_stale`
  reads `cpu_epoch` and compares with `uploaded`/`copied` without taking a lock: `copied` becomes `Vec<AtomicU32>`.
  `refresh` can answer "clean" for a range in `O(pages)` with no lock, so `defer_refresh` could skip clean ranges at
  *submission* (not at record time: a title write between record and submit must still be seen).
  Keep the kernel asking batching (`ask_once`, `ask_ahead`, `ask_once_quiet`) unchanged; S3 may later allow
  one whole-range asking per frame.
- Files: `watch.rs` (JOURNAL/PARTS), `shadow.rs` (`copied`, `copy_stale`, `update_free`, `card_dirty`), a new `pagestate.rs`.
- Interactions: journal numbers and `ours_before/ours_after` (copy-back writes noted so they are not read back as
  title writes, `watch.rs:786-860`) keep their meaning: `ours_after` writes `cpu_epoch` to the same value as before.
  Guard faults (`on_write`, `watch.rs:1931`) set `cpu_epoch` too, so views behave like private memory for readers.
- Risks: epoch wrap (u32 at ~1,000 askings/s is 49 days; compare with wrap arithmetic), memory ordering between a
  guard fault noting a write and a refresh reading it (the fault handler stores with `Release` before reopening the
  page, readers `Acquire`; tests with a stress thread writing views while refreshing). Test: run the existing shadow
  and watch unit tests against both implementations with an `UM_PAGESTATE` feature matrix; and `VICTORY_REFRESH_CHECK=1`
  for 10 minutes: zero "differs on three refreshes".
- Gain: 3-6 ms on the submission thread (copy_stale and `watch::writes` lock traffic), and most of the 19 %
  lock-wait share on recording threads (frame -3 to -6 ms). Effort **M**.

### U5. 4 KiB masks inside page state  `VICTORY_UM_4K=1` (needs U4)

- Attacks: copies 13.9-26 ms / 167-194 MB per frame. `VICTORY_WATCH_4K` measured worse before (stamps were a
  per-copy mutexed Vec); a 16-bit mask per page in the shared table is one atomic OR.
- Mechanism: the kernel's 4 KiB write list sets `mask4k` bits (already available: `stale_mask`, `watch.rs`); `copy_stale`
  copies only set 4 KiB sub-pages inside a stale 64 KiB page and clears bits it consumed per copy by comparing
  `mask4k` generations (a copy keeps `seen_mask` per page for nested copies: use per-copy 16-bit `taken` mask array).
- Files: `pagestate.rs`, `shadow.rs` (`copy_stale`, `copy`), `watch.rs`. Keep `COPY_CHANGED` as is.
- Risks: nested copies each need their own consumption mask or one copy eats another's bits (that is why the journal is numbered);
  therefore keep per-copy `taken: Vec<AtomicU16>`. Test: property test comparing per-copy results of 64 KiB vs
  4 KiB tracking on random write sequences; `VICTORY_REFRESH_CHECK`.
- Gain: S5 decides. If differing bytes are about 25-35 % of stale pages (typical for dynamic buffers sharing pages
  with static data) copies drop 13.9 -> 6-8 ms and 167 -> ~70 MB. If S5 shows >70 % differing, drop this stage.
  Effort **S** after U4.

### U6. Cross-frame table cache  `VICTORY_UM_TABLES=2` (needs U3, U4; P3 must show 30 %+ of tables repeating)

- Attacks: the rest of bind: re-making 3,880 tables/frame (36,000 slots) and re-running `readable` for 2,537 of them.
- Mechanism: a persistent host dynamic region (not wrapped per list; sized e.g. 64K slots, LRU by frame) holding tables
  keyed by content hash. Entry = `{host first slot, n, view epochs[]}` where a view epoch is
  `(resource generation)` for textures/buffer views (the host `HostTexture`/`Shadow` id plus a generation bumped on
  upload, remap, twin copy, alias update; today `list.changes` is the coarse stand-in, `draw.rs:2850`). CBVs in a
  cached table point at a persistent constant ring (GPU_UPLOAD if S2 passed, else a pooled upload buffer in
  VRAM-resident chunks) refreshed by `defer_constants` only when the CBV's pages are `cpu_epoch`-newer than the
  ring copy (page state). Valid entry => bind and skip `readable`'s declarations only for resources whose epoch is
  unchanged *and* whose ranges are still declared for refresh at submission (declarations are cheap with U4).
- Files: `draw.rs` (`tables`), `descriptor/host.rs` (persistent region, `dynamic`), `resource.rs`/`texture.rs`/`shadow.rs`
  (generation counters), `host.rs` (constant ring, lifetime: a ring slot lives until the last list using it has run).
- Risks: stale view after an upload or alias change that forgot to bump a generation (the dangerous failure mode:
  a black or wrong texture); lifetime of persistent slots vs lists in flight (use per-frame fences/`QUEUE_ORDER`
  numbers, never reuse a slot until its last list landed). Test: `VICTORY_UM_VERIFY=1` rebuilds each hit the old way
  1 time in 32 and compares the resulting host slot contents (descriptor bytes via a shadow table in debug) and logs mismatches.
- Gain: only if P3 shows reuse; reasoning: if half of the tables repeat, recording-thread bind shrinks by up to 20 %
  more than U3, i.e. 3-8 ms of frame. Effort **M-L**. Stop if verify mismatches do not reach zero.

### U7. GPU_UPLOAD windows and constants  `VICTORY_UM_REBAR=1` (gated on S2 positive)

- Attacks: windows 2.5 GB in RAM at gameplay, window upload chunks (1,028 MB sent from chunks; 21 "pool is full"
  warnings), the CPU copy into chunk + GPU `CopyBufferRegion`; plus per-list CBV scratch.
- Mechanism: already built, off, never validated: `VICTORY_WINDOW_REBAR` (`shadow.rs:397-450`): a window is a
  `D3D12_HEAP_TYPE_GPU_UPLOAD` buffer, refresh writes title pages straight to VRAM, one copy. U7 hardens and
  extends it: (1) sequential non-temporal stores in `copy()` for rebar destinations, never a read-modify-write
  and never a CPU read of the rebar mapping (`refresh_check` already skips rebar, `shadow.rs:3349`); (2) the
  `UM_COALESCE` joining must not read the rebar twin (there is none: gaps come from title memory); (3) the
  list scratch (`HostList::scratch`) becomes GPU_UPLOAD chunks when supported; (4) the existing RAM-fallback when
  VRAM is short ("falls back to system memory", spec) is detected by `video_memory()` as `window_rebar_for` does.
- Interactions: no byte-exact merge for windows (no shader writes declared; `shadow.rs:134-149`), no copy-back;
  windows with shader writes keep the card side path. Write watch is not needed for the rebar side (it is only a
  destination); source tracking is unchanged.
- Risks: write-combined mapping slow if code reads it (a stray `memcmp` or `COPY_CHANGED` compare against a rebar
  destination reads WC memory: forbid, compare against title memory instead); driver evicts to system memory
  silently (S2 measures); Developer Mode requirement (S2).
- Gain: RAM -2.5 GB (windows twins) and -1 GB pool; submission -2 to -5 ms (one fewer memcpy, no chunk, no GPU
  copy); VRAM +2 GB used so the card-room rules (`room`, `VICTORY_TEXTURE_ROOM_PERCENT`) must count it. Effort **M**.

### U8. Lazy copy-back  `VICTORY_UM_LAZY_BACK=1` (gated on P5; needs U4)

- Attacks: relay thread 25-31 ms, 87 MB fetched + 68 MB merged per frame, 17,520 pieces compared on the card;
  matters most when any settle reason fires (the relay is then on the critical path) and for the 25 % extra card time.
- Mechanism (shadPS4/yuzu "GPU-modified, download on CPU access"): after a flight lands, pages the GPU wrote are
  not fetched eagerly if P5 says the CPU almost never reads them soon; they get `gpu_dirty=2` and a `PAGE_NOACCESS`
  guard. A title read or write faults; the handler fetches that page (wait for the card if needed), merges byte-exact,
  clears the guard, continues. Pages never touched cost nothing; a periodic sweep (every N frames, 8 initially)
  fetches the rest so state does not accumulate. Only private, non-view memory first.
- Files: `host.rs` (`relay_land`, `relay_worker`), `shadow.rs` (`card_merge_relay`), `watch.rs` (guard machinery:
  reuse `protect`/`open`/`on_write`, add a read-fault kind), `host.rs` fault swallow in `fast_copy`/`plain_copy`.
- Risks: this is the highest-risk stage. Guard faults on the title's threads (the title may poll flags in GPU-written
  buffers every frame: P5 will show), faults inside our own `copy_mapped` readers (must pre-resolve before reading
  a range: `ensure_cpu(range)` called from `Shadow::update` and `defer_read_back`), fences and WaitOnMemory values
  written by shaders must stay eager (class them by `late_met`, `host.rs:3214`, and bottom-of-pipe writes), deadlock if
  the faulting thread holds a lock the relay needs. Test: replay the c3 scenario with `UM_LAZY_BACK=1` and
  `VICTORY_REFRESH_CHECK=1`, compare screenshots and hang counters over 30 min; a debug mode that does lazy *and*
  eager fetches and compares.
- Gain: if P5 shows 70 % of merged pages untouched within 8 frames: relay 31 -> ~12 ms, fetched 87 -> ~30 MB, merged
  68 -> ~20 MB, GPU compare/fetch work -50 %; frame time moves only when settles occur (0.0 in the log), so expect
  0-3 ms of frame but a lot of headroom and memory bandwidth. Effort **L**.

### U9. Reserved-buffer mirror of title memory (Xenia style)  `VICTORY_UM_RESERVED=1` (gated on S4, needs U4)

- Attacks: the structure itself: ~3 GB title-buffer copies + 2.5 GB windows in RAM (and the 4,002 card copies, 1.6 GB
  VRAM), `Shadow` lookup per bind (`shadow::find`, thread-local `FOUND` cache), `find_any`/window grid heuristics,
  unindexed-heap blindness, raw-pointer shaders reading zero from uncopied pages (`memory.rs`).
- Mechanism: one reserved buffer whose GPU address = base + title address (64 GiB if S4 says so, else a
  two-level scheme). 64 KiB tiles = our `PAGE`. A page becomes **resident** (tile mapped from a VRAM heap pool with
  `UpdateTileMappings`, batched per submission) the first time a draw declares it (same declarations as today:
  `Shadow::refresh` ranges, `heap_buffers` spans); it is uploaded when `resident` is 0 or `cpu_epoch > uploaded`,
  directly from title memory through the upload ring (or GPU_UPLOAD if S2 passed), no CPU twin. Eviction: clean
  tiles are dropped under VRAM pressure (title RAM is the master), GPU-dirty tiles flushed first. Every buffer view
  and root view becomes `base + address`; per-buffer `Shadow` objects stay only as bookkeeping of ranges (flights,
  snapshots) or disappear. Remaps by the title (`title_memory_remapped`) become `UpdateTileMappings` or an
  invalidation of `resident`. Declared-write compare and merge work per page exactly as today (hash per piece
  on card; the reserved buffer is the card side).
- Phased delivery (each shippable): phase 1, only windows (they are the unindexed, no-resource case, gain and
  risk smallest); phase 2, buffers >= 64 KiB; phase 3, raw-pointer shaders read the same buffer, removing
  `memory.rs` windows; phase 4, optional GPU fault buffer (shadPS4) so memory the GPU touches without a declaration
  is made resident next frame instead of read as zero.
- Files: new `mirror.rs`; `shadow.rs` (find/windows/cards), `descriptor/host.rs` (buffer views on the mirror),
  `draw.rs` (`root_view`), `memory.rs`, `host.rs` (`card_before`/relay use tiles), `tiles.rs` (share the heap pool code).
- Risks: reserved-buffer SRV/UAV limits (element counts, 40-bit addresses, S4), `UpdateTileMappings` count (29 us/call
  today for textures; batch to a handful per submission), tile pool residency vs texture tiles competing for VRAM,
  mirror of title memory in 64 KiB units means a 4 KiB hot page drags 64 KiB (U5 masks fix the bytes), TDR risk from
  unmapped reads on older drivers (S4 on this driver only). Test: golden-image comparison per phase, c3 scenario
  soak, `VICTORY_UM_VERIFY` that checks a resident page against title memory by hash every N frames.
- Gain: RAM -4 to -5 GB (no twins), per-draw buffer resolution turns into arithmetic (a fraction of bind), no window
  grid, uploads only what is dirty and declared, plausible -10 to -20 ms of submission and -5 ms of bind. Largest and
  least certain. Effort **L** (phase 1: M).

### U10. In-place heaps for read-once title data  `VICTORY_UM_INPLACE=1` (gated on S1; optional)

- Attacks: copies of data the GPU reads once and the title rewrites each frame (vertex streams, small structured
  data, indirect args), without twin, upload, hash, or card copy.
- Mechanism: the existing `VICTORY_INPLACE` (`inplace.rs`) with the constraints it documents: parts of title
  allocations that are whole committed `VirtualAlloc` private memory, `ALLOW_CROSS_ADAPTER` buffers, PCIe reads (S1
  measures GB/s; expect 40-55). Only choose pages by a **classifier** from page state: pages with no `gpu_dirty` ever, read
  at most once per frame (`P1`-style counter) and write-heavy by the title. Never for textures, never combined
  with late wait stage 2 as the file warns (`inplace.rs:84-86`).
- Risks: pinning blocks title `VirtualFree`/`VirtualProtect` (error 87); view memory refused; write watch plus in-place
  heap interplay (S1 item 2); PCIe-bound shaders. Test: S1 first, then a one-hour soak on windows only.
- Gain: only for classified pages; unknown until S1 and U7 data; likely smaller than U7 on this PC, which is why it is last.
  Effort **M**.

## 4. What not to do

| Rejected | Why |
|---|---|
| Whole-title-memory OpenExistingHeapFromAddress | 13 GB pinned, `VirtualProtect`/`VirtualFree` fail while open (`inplace.rs:46-49`), view memory refused, no tile mapping, everything over PCIe at ~50 GB/s; textures and RTs would crawl. |
| Page protection (PAGE_GUARD/READONLY + VEH) for all private memory instead of write watch | Each first write is an exception (8 % of recording time already goes to guard faults on views); keep `GetWriteWatch` for private memory and guards only where views force it. |
| More hashing of buffers or textures on the CPU | BLAKE3 was 35-42 ms/frame for descriptors, whole-texture hashing is the 773k-looks problem; the fix is fewer looks, not faster hashes. |
| Smaller write-watch granularity as a flat switch (`WATCH_4K` as is) | Measured worse in `01`; only worth it with lock-free masks (U5). |
| A single huge `Shadow` for everything | Same problem as today at bigger scale; U9 does it with sparse residency, not with a flat 13 GB copy (VRAM is 16 GB). |
| Moving all copy-back to lazy at once | Fences, WaitOnMemory values and fence lists read as -1 when the merge was not byte-exact (`shadow.rs:2790-2794`); U8 is per-class, probe-gated. |
| Large pages (2 MB) for title memory | Conflicts with write watch and 64 KiB remapping; TLB gains small relative to the work. |
| Caching descriptor tables by title address alone | Title writes tables at new addresses each draw (585-622 per frame at new address); content plus epochs (U6) or nothing. |
| Treating `ExecuteCommandLists`/threading as the first lever | GPU is 46 % busy and the submission thread never waits for it (0.0 ms on the GPU); the costs are CPU-side bookkeeping and copies. |
| A UMA/CacheCoherentUMA fast path | Target PC is discrete; only valuable on iGPUs, out of scope. |

## 5. Measurement plan

Every stage is judged on the same 3-minute gameplay segment (same save, same spot, `play-zm.ps1` plus the switch),
three runs, medians of the 82-style "typical" frames. Existing lines that prove a stage:

| Stage | Line / value (field) | Pass criterion |
|---|---|---|
| U0 | `SPIKE s1..s4`, `UM-PROBE p1..p5`, existing `host GPU upload heap support` | results pasted, plan routed |
| U1 | frame line `preparing`; new `UM-PROBE p2 card_before_ms, ranges`; host-copies line "N MB sent up in M ranges" | ranges down 3x, `preparing` down 3 ms, no new "WHY_STALE" settles |
| U2 | frame line `heap buffers 1.0 passes N ms`; "the pass over the heap per frame ... ms in its pass over textures"; "looks found them unchanged" counter; frame `textures X ms` | heap pass median under 16 ms, p90 under 45; no visual regression |
| U3 | `descriptor tables per frame` line; overlay `Draws`; profiler split bind/lock (upstream profile, 44 %/19 %/8 %); `UM-PROBE p3 cbv_copy_mb` | bind share of recording time under 35 %, same `made` count within 1 % |
| U4 | `UM-PROBE p1`; frame `copies`; lock-wait share in the upstream profile; `descriptor records ... ms` (sync_heaps) | lock waits under 8 %, `copies` -2 ms, `VICTORY_REFRESH_CHECK` clean |
| U5 | frame line `copies X ms Y MB` | MB/frame under 100, ms under 9; no `REFRESH_CHECK` hit |
| U6 | `descriptor tables per frame` "found ready" up, "made" down; verify mismatches = 0 | made under half, mismatches 0 over 30 min |
| U7 | `ram line` "windows N MB"; "window upload pool is full" warnings (21); REBAR counters (`shadow.rs` `REBAR`); VRAM overlay value | windows RAM under 500 MB, warnings 0, VRAM headroom still above 10 % |
| U8 | frame line `copying back X ms Y MB`; card line "pieces compared / fetched"; settle counters (`WHY_*` reasons) | relay ms -50 %, no new settles, no hangs, screenshots equal |
| U9 | `ram line` host copies and windows; "host copies in the card's memory"; `UpdateTileMappings` calls/us line (`tiles.rs:1465`) | RAM -4 GB, frame -10 ms, tile call time under 5 ms per second |
| U10 | S1 results; new in-place counters (`inplace.rs`); frame `copies` | only if it beats U7 on the same segment |

Whole-frame acceptance: `ms/frame`, `preparing`, `copies`, `heap buffers`, `copying back`, `cpu`, `gpu busy`, `ram`
all come from the one frame line (`host.rs` frame line, sample at `nep-um-logs-c3.txt:24485`); the overlay's `fps`, `Draws`,
`RAM`, `VRAM` rows (`overlay.rs:409-424`) cover the on-screen check. Target after U1-U6: preparing 49 -> ~34 ms, recording
bind share down by a third, frame 109 -> ~80 ms (12 fps); U7-U9 add RAM headroom and another 10-20 ms. These are estimates: P2/P3/S5
are there to replace them with measured numbers before any stage after U3 is started.

## 6. Delivery order

1. U0 tools and probes (S): `tools/um-spikes` plus P1-P5 logging; user runs once.
2. U1, U2, U3 (each S, independent, can go in parallel).
3. U4 (M), then U5 (S) when S5 supports it.
4. U7 if S2 is positive (M), U6 if P3 supports it (M-L).
5. U8 if P5 supports it (L), U9 if S4 is positive (L, phase 1 first), U10 last (M).
