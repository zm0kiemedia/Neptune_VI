//! The pipeline pre-build, the warm-up's second half: every pipeline of the
//! title's shader pack made ahead of a run into the host pipeline library
//! the run reads (`<cache>/pipelines`), so that a first run, and a first
//! visit to any place, compiles nothing the pack lists.
//!
//! Each pack record becomes the creation stream the engine would hand the
//! device for that pipeline (`victory_game::pipelines`), which goes through
//! the run's own code (`victory_gpu::pipeline::Prebuild`): a pipeline is
//! only ever used if its host description is the run's, byte for byte.
//! Records whose states cannot be named, or whose shaders are refused, are
//! counted by cause and skipped. The library is saved as it grows: an
//! interrupted pre-build keeps the last saved batch.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use victory_game::pipelines::{self, Named, Unnamed};
use victory_game::{sgd5, shaderpack};
use victory_gpu::pipeline::{self, Prebuild};
use victory_shader::{Container, FourCC, HwStage, native_programs};

use crate::shader::{MAX_FILE, Source, sources};

/// Where a native program's container is: file, source, and its stage.
type Found = (usize, usize, HwStage);

/// One pipeline to make: a graphics or compute record of the pack.
#[derive(Clone, Copy, Debug)]
enum Job {
    Graphics(usize),
    Compute(usize),
}

pub fn run(paths: &[PathBuf], threads: usize, used_pipelines: bool) -> Result<()> {
    let started = Instant::now();
    let dir = victory_core::Config::load()?.cache_subdir("pipelines")?;
    let keys_only = std::env::var_os("VICTORY_PREBUILD_KEYS");
    let requested = if used_pipelines && keys_only.is_none() {
        let history = dir.join("early-pipelines.txt");
        let listed = match std::fs::read_to_string(&history) {
            Ok(listed) => listed,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                println!(
                    "pipelines    no recorded requests at {}; nothing to prebuild",
                    history.display(),
                );
                return Ok(());
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", history.display())),
        };
        let keys: HashSet<String> = listed
            .lines()
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
            .collect();
        if keys.is_empty() {
            println!(
                "pipelines    recorded request list is empty at {}; nothing to prebuild",
                history.display(),
            );
            return Ok(());
        }
        Some(keys)
    } else {
        None
    };
    pipeline::use_pipeline_cache(dir);
    // The files that hold shaders (or the pack itself).
    let files: Vec<(PathBuf, Vec<u8>)> = paths
        .iter()
        .flat_map(|root| {
            walkdir::WalkDir::new(root)
                .into_iter()
                .filter_map(Result::ok)
        })
        .filter(|e| e.file_type().is_file() && e.metadata().is_ok_and(|m| m.len() <= MAX_FILE))
        .filter_map(|e| Some((e.path().to_owned(), std::fs::read(e.path()).ok()?)))
        .filter(|(_, d)| d.starts_with(shaderpack::MAGIC) || !sources(d).is_empty())
        .collect();
    // It ships beside the shader databases.
    let found = (files.iter())
        .filter(|(_, d)| d.starts_with(shaderpack::MAGIC) || sgd5::is_sgd5(d))
        .find_map(|(p, d)| Some((p, shaderpack::find(d)?)));
    let Some((pack_path, pack)) = found else {
        println!("pipelines    no shader pack among the files: nothing to pre-build");
        return Ok(());
    };
    let pack = shaderpack::Pack::parse(&pack).map_err(anyhow::Error::msg)?;
    let graphics: Vec<shaderpack::Graphics> = pack.graphics().collect();
    let compute: Vec<shaderpack::Compute> = pack.compute().collect();

    // Every container by the hash instructions its native program starts
    // with, which the pack's programs start with too.
    let sources: Vec<Vec<Source>> = files.iter().map(|(_, d)| sources(d)).collect();
    let programs: Mutex<HashMap<[u8; 24], Found>> = Mutex::new(HashMap::new());
    for (f, list) in sources.iter().enumerate() {
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads.min(list.len()) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(source) = list.get(i) else { break };
                        let Some(bytes) = source.bytes() else {
                            continue;
                        };
                        for (id, stage) in program_ids(&bytes) {
                            lock(&programs).entry(id).or_insert((f, i, stage));
                        }
                    }
                });
            }
        });
    }
    let programs = programs.into_inner().unwrap_or_else(|e| e.into_inner());
    let databases: Vec<sgd5::Database> =
        files.iter().flat_map(|(_, d)| sgd5::databases(d)).collect();
    let named = Named::find(&pack, &databases);
    eprintln!(
        "{}: shader pack, {} graphics and {} compute pipelines; states named: {} depth-stencil, {} rasteriser, {} blend",
        pack_path.display(),
        graphics.len(),
        compute.len(),
        named.depth.len(),
        named.raster.len(),
        named.blend.len(),
    );

    let container = |code: &[u8]| -> Option<(Vec<u8>, HwStage)> {
        let &(f, i, stage) = programs.get(&shaderpack::program_id(code)?)?;
        let bytes = sources[f][i].bytes()?;
        // As the title passes it: the container's own size.
        let size = u32::from_le_bytes(bytes.get(0x18..0x1c)?.try_into().ok()?) as usize;
        Some((bytes.get(..size)?.to_vec(), stage))
    };
    // A job's pipeline, or why it has none.
    let prepare = |job: Job| -> Result<Prebuild, String> {
        let g = match job {
            Job::Graphics(i) => &graphics[i],
            Job::Compute(i) => {
                let (cs, _) = container(compute[i].stage.program).ok_or("shader not found")?;
                return Ok(Prebuild::compute(&cs));
            }
        };
        let [ps, vs, gs, ..] = g.stages;
        if g.stages[3..].iter().any(Option::is_some) {
            return Err("stages other than vertex, mesh and pixel".into());
        }
        let (first, stage) = vs
            .or(gs)
            .and_then(|s| container(s.program))
            .ok_or("shader not found")?;
        let pixel = match ps {
            Some(ps) => Some(container(ps.program).ok_or("shader not found")?.0),
            None => None,
        };
        let mesh = match stage {
            HwStage::Ms => true,
            HwStage::Vs => false,
            _ => return Err("first stage not a vertex or mesh shader".into()),
        };
        let stream = pipelines::stream(g, &named, &first, mesh, pixel.as_deref()).map_err(|u| {
            match u {
                Unnamed::DepthStencil => "depth-stencil state not named",
                Unnamed::Rasterizer => "rasteriser state not named",
                Unnamed::Blend => "blend state not named",
                Unnamed::Metadata => "metadata not read",
            }
            .to_owned()
        })?;
        // SAFETY: the stream points at `first`, `pixel` and its own input
        // elements, all alive here.
        Ok(unsafe { Prebuild::graphics(stream.bytes()) })
    };

    // A job's shaders as a run names them (diagnostics).
    let shaders_of = |job: Job| {
        let stages: Vec<(&str, &[u8])> = match job {
            Job::Compute(i) => vec![("compute", compute[i].stage.program)],
            Job::Graphics(i) => {
                let [ps, vs, gs, ..] = graphics[i].stages;
                let first = vs
                    .map(|s| ("vertex", s.program))
                    .or(gs.map(|s| ("mesh", s.program)));
                first
                    .into_iter()
                    .chain(ps.map(|s| ("pixel", s.program)))
                    .collect()
            }
        };
        let names: Vec<String> = (stages.iter())
            .map(|(stage, code)| match container(code) {
                Some((c, _)) => format!("{stage} {}", &blake3::hash(&c).to_hex()[..16]),
                None => format!("{stage} ?"),
            })
            .collect();
        format!("[{}]", names.join(", "))
    };
    let mut jobs: Vec<Job> = (0..compute.len()).map(Job::Compute).collect();
    jobs.extend((0..graphics.len()).map(Job::Graphics));
    // Diagnostics, `VICTORY_PREBUILD_KEYS=<file>`: make nothing, list each
    // record's key (what a run lists pipelines by) with its shaders and
    // state as a run's `VICTORY_PIPELINE_LOG` has them, or why it is
    // skipped; and the records each source named states for.
    if keys_only.is_some() {
        let mut by: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        for g in &graphics {
            for (kind, h) in [
                ("depth-stencil", g.depth_stencil_hash),
                ("rasteriser", g.rasterizer_hash),
                ("blend", g.blend_hash),
            ] {
                *by.entry((kind, named.source.get(&h).copied().unwrap_or("not named")))
                    .or_default() += 1;
            }
        }
        for ((kind, source), n) in by {
            println!("  {kind:<14} {source:<12} {n:>6} records");
        }
    }
    let skipped: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());
    let skip = |why: String| *lock(&skipped).entry(why).or_default() += 1;
    // Calculate keys before translating. Give priority to pipelines used
    // in an earlier run, and let the host library check each description.
    let lines: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let todo: Mutex<Vec<(bool, Job)>> = Mutex::new(Vec::new());
    let matched = Mutex::new(HashSet::new());
    let unrequested = AtomicUsize::new(0);
    each(&jobs, threads, |job| match prepare(job) {
        Ok(p) if keys_only.is_some() => {
            lock(&lines).push(format!("{}\t{job:?}\t{}", p.key(), p.describe()))
        }
        Ok(p) => {
            let key = p.key();
            if let Some(requested) = &requested {
                if !requested.contains(&key) {
                    unrequested.fetch_add(1, Ordering::Relaxed);
                } else if lock(&matched).insert(key) {
                    lock(&todo).push((false, job));
                } else {
                    skip("duplicate requested pipeline key".into());
                }
            } else {
                lock(&todo).push((!pipeline::wanted_early(&key), job));
            }
        }
        Err(why) => {
            if keys_only.is_some() {
                lock(&lines).push(format!("-\t{job:?}\t{}\t{why}", shaders_of(job)));
            }
            skip(why);
        }
    });
    if let Some(file) = keys_only {
        std::fs::write(file, lock(&lines).join("\n"))?;
    }
    let mut todo = todo.into_inner().unwrap_or_else(|e| e.into_inner());
    todo.sort_by_key(|&(later, _)| later);
    let todo: Vec<Job> = todo.into_iter().map(|(_, job)| job).collect();
    if let Some(requested) = &requested {
        if todo.is_empty() {
            println!(
                "pipelines    none of {} recorded keys matched a reconstructable shader-pack pipeline; nothing to prebuild",
                requested.len(),
            );
        } else {
            println!(
                "pipelines    matched {} of {} recorded keys; {} pack records were not requested",
                todo.len(),
                requested.len(),
                unrequested.into_inner(),
            );
        }
    }

    let next = AtomicUsize::new(0);
    let (made0, loaded0) = pipeline::pipeline_counts();
    let counts = || {
        let (made, loaded) = pipeline::pipeline_counts();
        (made - made0, loaded - loaded0)
    };
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads.min(todo.len()))
            .map(|_| {
                s.spawn(|| {
                    while let Some(&job) = todo.get(next.fetch_add(1, Ordering::Relaxed)) {
                        if let Err(why) = prepare(job).and_then(|p| p.make()) {
                            skip(why);
                        }
                    }
                })
            })
            .collect();
        let (n, what) = (todo.len(), "pipelines to make");
        crate::translate::report(pack_path, n, what, &next, started, &workers, || {
            let (made, loaded) = counts();
            format!("; {made} made, {loaded} found in the library")
        });
    });
    pipeline::save_pipeline_cache();
    let (made, loaded) = counts();
    println!(
        "pipelines    {} in the pack, {:.1} s: {made} made, {} already in the library",
        jobs.len(),
        started.elapsed().as_secs_f64(),
        loaded,
    );
    for (why, n) in lock(&skipped).iter() {
        println!("  {n:>7}  skipped: {why}");
    }
    Ok(())
}

/// Runs `f` on every job, on `threads` threads.
fn each(jobs: &[Job], threads: usize, f: impl Fn(Job) + Sync) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.min(jobs.len()) {
            s.spawn(|| {
                while let Some(&job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    f(job);
                }
            });
        }
    });
}

/// The hash instructions each native program of a container starts with.
fn program_ids(bytes: &[u8]) -> Vec<([u8; 24], HwStage)> {
    let Ok(c) = Container::parse(bytes) else {
        return Vec::new();
    };
    let Some(part) = c.part(FourCC::DXIL) else {
        return Vec::new();
    };
    native_programs(part.data)
        .map(|programs| {
            (programs.iter())
                .filter_map(|p| Some((shaderpack::program_id(&p.code)?, p.stage)))
                .collect()
        })
        .unwrap_or_default()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
