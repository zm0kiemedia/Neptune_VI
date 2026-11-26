//! `VICTORY_GPU_TIME=1`: GPU time per command, by shader hashes. In one
//! frame of `VICTORY_GPU_TIME_EVERY` (default 30) every host list stamps the
//! GPU clock before each draw, dispatch and copy (and before our own
//! passes: the ambient light, texture uploads, the present copy and the UI
//! plane's composition) and resolves the stamps into a read-back buffer; the
//! list's reset (after its fences) reads them. Every 5 s the 25 keys with the
//! most time are logged ("gpu time top"), with a total line and a line per
//! kind. Off, a list pays one cached flag test per command.
//! Only the host's direct queue is timed. Our own lists that are not
//! `HostList`s (the card's copy lists) are timed whole, by [`span_begin`]
//! and [`span_end`]; the relay's copy queue is not.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

/// Stamps per list; commands past them are not timed (counted in the log).
pub(crate) const CAP: u32 = 4096;
/// Entries logged.
const TOP: usize = 25;

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GPU_TIME").is_some_and(|v| v != "0"))
}

pub(crate) fn sampled(frame: u64) -> bool {
    static EVERY: OnceLock<u64> = OnceLock::new();
    let every = *EVERY.get_or_init(|| {
        std::env::var("VICTORY_GPU_TIME_EVERY").ok().and_then(|v| v.parse().ok()).filter(|&n| n > 0).unwrap_or(30)
    });
    frame % every == 0
}

/// What a stamp's interval is charged to.
pub(crate) struct Key {
    /// The command, or the label of our own work ("ambient", "present copy").
    pub kind: &'static str,
    /// The shader hashes of the pipeline (empty for our own work).
    pub name: String,
    /// Vertices, indices or groups, as the command counts them.
    pub n: u64,
}

/// A list's stamps, kept across resets.
#[derive(Default)]
pub(crate) struct ListProf {
    /// Whether this recording has been looked at for sampling.
    pub decided: bool,
    pub sampled: bool,
    pub frame: u64,
    pub heap: Option<(ID3D12QueryHeap, ID3D12Resource)>,
    pub keys: Vec<Key>,
    /// Stamps resolved at close (0: none).
    pub resolved: u32,
}

/// A query heap of `count` timestamps and its read-back buffer.
fn heap_of(device: &ID3D12Device, count: u32) -> Option<(ID3D12QueryHeap, ID3D12Resource)> {
    let mut heap: Option<ID3D12QueryHeap> = None;
    let mut back: Option<ID3D12Resource> = None;
    let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: u64::from(count) * 8,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    // SAFETY: plain object creation.
    unsafe {
        device
            .CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: count, NodeMask: 0 }, &mut heap)
            .ok()?;
        device
            .CreateCommittedResource(&readback, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut back)
            .ok()?;
    }
    Some((heap?, back?))
}

/// A host list's heap and read-back buffer.
pub(crate) fn make(device: &ID3D12Device) -> Option<(ID3D12QueryHeap, ID3D12Resource)> {
    heap_of(device, CAP)
}

#[derive(Default)]
struct Entry {
    ms: f64,
    calls: u64,
    n: u64,
}

struct Agg {
    keys: HashMap<(&'static str, String), Entry>,
    frames: BTreeSet<u64>,
    since: Option<Instant>,
}

static AGG: Mutex<Option<Agg>> = Mutex::new(None);
/// Lists whose stamps were all zero (never run).
static EMPTY: AtomicU64 = AtomicU64::new(0);
/// Commands that got no stamp (a list was past [`CAP`]).
pub(crate) static OVER: AtomicU64 = AtomicU64::new(0);

/// The summary line's group of a key.
fn category(kind: &str) -> &'static str {
    match kind {
        "draw" => "draw",
        "mesh" => "mesh",
        "dispatch" => "dispatch",
        "ambient" => "ambient",
        k if k.starts_with("indirect") => "indirect",
        k if k.starts_with("present") => "present",
        _ => "copy",
    }
}

fn add(agg: &mut Agg, frame: u64, kind: &'static str, name: &str, ticks: u64, frequency: u64, n: u64) {
    agg.frames.insert(frame);
    let e = agg.keys.entry((kind, name.to_owned())).or_default();
    e.ms += ticks as f64 * 1e3 / frequency.max(1) as f64;
    e.calls += 1;
    e.n += n;
}

/// Logs and clears what was gathered, every 5 s.
fn report(agg: &mut Agg) {
    let since = *agg.since.get_or_insert_with(Instant::now);
    if since.elapsed() < Duration::from_secs(5) {
        return;
    }
    agg.since = Some(Instant::now());
    let frames = agg.frames.len().max(1) as f64;
    let total: f64 = agg.keys.values().map(|e| e.ms).sum();
    let mut kinds: HashMap<&str, f64> = HashMap::new();
    for ((kind, _), e) in &agg.keys {
        *kinds.entry(category(kind)).or_default() += e.ms;
    }
    let mut all: Vec<_> = agg.keys.drain().collect();
    agg.frames.clear();
    all.sort_by(|a, b| b.1.ms.total_cmp(&a.1.ms));
    let shown: f64 = all.iter().take(TOP).map(|(_, e)| e.ms).sum();
    tracing::info!(
        "gpu time top total: {:.2} ms/frame sampled over {frames} frames, top {TOP} cover {:.0}%, {} lists never ran, {} commands past the stamp cap",
        total / frames,
        100.0 * shown / total.max(1e-9),
        EMPTY.swap(0, Ordering::Relaxed),
        OVER.swap(0, Ordering::Relaxed)
    );
    let mut kinds: Vec<_> = kinds.into_iter().collect();
    kinds.sort_by(|a, b| b.1.total_cmp(&a.1));
    let line: Vec<String> = kinds
        .iter()
        .map(|(k, ms)| format!("{k} {:.2} ms ({:.0}%)", ms / frames, 100.0 * ms / total.max(1e-9)))
        .collect();
    tracing::info!("gpu time top kinds: {}", line.join(", "));
    for (rank, ((kind, name), e)) in all.iter().take(TOP).enumerate() {
        tracing::info!(
            "gpu time top {}: {:.3} ms/frame, {:.1} calls/frame, {:.0} us/call, avg n {:.0}, {kind} {name}",
            rank + 1,
            e.ms / frames,
            e.calls as f64 / frames,
            e.ms * 1e3 / e.calls.max(1) as f64,
            e.n as f64 / e.calls.max(1) as f64,
        );
    }
}

/// A finished list's stamps (`stamps[i + 1] - stamps[i]` is `keys[i]`).
pub(crate) fn land(frame: u64, frequency: u64, keys: &[Key], stamps: &[u64]) {
    if stamps.first() == Some(&0) {
        EMPTY.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut guard = AGG.lock().unwrap_or_else(|e| e.into_inner());
    let agg = guard.get_or_insert_with(|| Agg { keys: HashMap::new(), frames: BTreeSet::new(), since: None });
    for (i, key) in keys.iter().enumerate() {
        add(agg, frame, key.kind, &key.name, stamps[i + 1].saturating_sub(stamps[i]), frequency, key.n);
    }
    spans_land(agg, frequency);
    report(agg);
}

/// Our lists that are not host lists, timed whole: a ring of begin/end
/// stamp pairs. A pair is read when a host list lands later (the queue is
/// in order, so a pair with both stamps written has run); read slots are
/// zeroed again.
const SPANS: u32 = 64;

struct Ring {
    heap: ID3D12QueryHeap,
    back: ID3D12Resource,
    next: u32,
    /// (slot, label, frame) of the pairs resolved and not yet read.
    pending: Vec<(u32, &'static str, u64)>,
}

static RING: Mutex<Option<Option<Ring>>> = Mutex::new(None);

/// Before the first command of our own open list: the slot of a begin stamp,
/// if this frame is sampled.
pub(crate) fn span_begin(list: &ID3D12GraphicsCommandList) -> Option<u32> {
    if !on() || !sampled(crate::frame::current()) {
        return None;
    }
    let mut guard = RING.lock().unwrap_or_else(|e| e.into_inner());
    let ring = guard
        .get_or_insert_with(|| {
            let (heap, back) = crate::host::host().and_then(|h| heap_of(&h.device, SPANS * 2))?;
            Some(Ring { heap, back, next: 0, pending: Vec::new() })
        })
        .as_mut()?;
    let slot = ring.next % SPANS;
    if ring.pending.iter().any(|p| p.0 == slot) {
        return None;
    }
    ring.next = ring.next.wrapping_add(1);
    // SAFETY: an open list and our heap.
    unsafe { list.EndQuery(&ring.heap, D3D12_QUERY_TYPE_TIMESTAMP, slot * 2) };
    Some(slot)
}

/// After the last command of that list, before it closes.
pub(crate) fn span_end(list: &ID3D12GraphicsCommandList, slot: Option<u32>, label: &'static str) {
    let Some(slot) = slot else { return };
    let mut guard = RING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(ring)) = guard.as_mut() else { return };
    // SAFETY: an open list, our heap and read-back buffer.
    unsafe {
        list.EndQuery(&ring.heap, D3D12_QUERY_TYPE_TIMESTAMP, slot * 2 + 1);
        list.ResolveQueryData(&ring.heap, D3D12_QUERY_TYPE_TIMESTAMP, slot * 2, 2, &ring.back, u64::from(slot) * 16);
    }
    ring.pending.push((slot, label, crate::frame::current()));
}

fn spans_land(agg: &mut Agg, frequency: u64) {
    let mut guard = RING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(ring)) = guard.as_mut() else { return };
    if ring.pending.is_empty() {
        return;
    }
    let mut data = std::ptr::null_mut();
    let all = D3D12_RANGE { Begin: 0, End: SPANS as usize * 16 };
    // SAFETY: a read-back buffer; the slots read are written back as zero
    // before the unmap, and the GPU writes only slots still pending.
    unsafe {
        if ring.back.Map(0, Some(&all), Some(&mut data)).is_err() {
            return;
        }
        let words = data.cast::<u64>();
        ring.pending.retain(|&(slot, label, frame)| {
            let at = slot as usize * 2;
            let (a, b) = (words.add(at).read(), words.add(at + 1).read());
            if a == 0 || b == 0 {
                return true;
            }
            words.add(at).write(0);
            words.add(at + 1).write(0);
            add(agg, frame, label, "", b.saturating_sub(a), frequency, 0);
            false
        });
        ring.back.Unmap(0, Some(&all));
    }
}
