# um-spikes

Stage U0 spikes S1-S4 of `docs/unified-memory/03-design.md`. Windows only, touches no game files, only its own
allocations. Each spike is isolated: on failure it prints `SPIKE sN error=0x<hr>:<message>` and the next one runs.

    cargo build --release -p um-spikes
    target\release\um-spikes.exe [--gb 8] [--s1-mib 1024] [--only s1,s2,s3,s4]

- `--gb N`: S3 reserves N GiB (default 8) but commits at most half of the free RAM.
- `--s1-mib N`: S1 region size (default 1024, capped at half of free RAM).
- Run S2 twice, Developer Mode off and on, if the first run says `supported=0`.

Output: first `SPIKE gpu adapter=".." driver=..`, then one `SPIKE sN key=value ...` line per result, and all
lines again under "Summary" at the end. Paste that block back.

| Line | Keys |
|---|---|
| `s1` (4 lines: watch=1/0 x api=0 `OpenExistingHeapFromAddress`, api=1 `...1`) | `open` ok/unavailable, `open_ms_per_gib`, `read_gbps` GPU copy from the heap buffer, `cpu_pattern_gpu_read_ok`, `gpu_write_marks_watch_pages` (pages GetWriteWatch reports after a GPU copy-write of 64 KiB; 16 = DMA marks, 0 = invisible, na without watch), `gpu_write_cpu_visible`, `watch_after_open` (GetWriteWatch worked, 2 = n/a), `vprotect` (ok or error code of VirtualProtect on an opened range) |
| `s2` | `supported` (GPUUploadHeapSupported), `devmode` (registry value, 1 = on), `write_gbps` memcpy, `nt_write_gbps` streaming stores, `cpu_read_gbps` (16 MiB only), `gpu_read_gbps` |
| `s3` | per dirty ratio (0.1/1/10 %): `committed_gib`, `dirty_pages`, `found`, `get_ms` (GetWriteWatch, no reset), `get_reset_ms` (with RESET), `after_reset` (must be 0), `get_empty_ms`; then `worker_gbps_idle` and `worker_slowdown_pct_*` of a memcpy thread while the main thread dirties 1 % and queries with RESET, or only dirties (control) |
| `s4` | `tiled_tier`, GPU VA bits; per size 16/64 GiB: `create`, `map_us_per_tile_batchN` (UpdateTileMappings CPU time) and `map_wait_us_per_tile_batchN` (incl. queue drain) for N=1/64/4096, `unmapped_reads_zero`, `alias_ok` (two tiles on one heap tile), `copy_over_4gib_ok`, `root_srv_over_4gib=created_unvalidated` |

Limits: S4 SRV/UAV creation has no error return and no debug layer is enabled, so it only proves no crash; no
compute shader reads are done (copies only). S3 `worker_slowdown` includes memory-bandwidth contention from the
dirtying itself; compare it to the `dirty_only` control.
