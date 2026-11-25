//! Spikes S1-S4 of docs/unified-memory/03-design.md. Windows only; prints `SPIKE sN key=value ...` lines.

#[cfg(windows)]
macro_rules! spike {
    ($n:expr, $($a:tt)*) => {{
        let l = format!("SPIKE {} {}", $n, format!($($a)*));
        println!("{l}");
        $crate::LINES.lock().unwrap().push(l);
    }};
}

#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod s1;
#[cfg(windows)]
mod s2;
#[cfg(windows)]
mod s3;
#[cfg(windows)]
mod s4;

#[cfg(windows)]
static LINES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

#[cfg(not(windows))]
fn main() {
    eprintln!("um-spikes only runs on Windows");
}

#[cfg(windows)]
fn main() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let mut gb = 8u64;
    let mut s1_mib = 1024u64;
    let mut only = String::from("s1,s2,s3,s4");
    let mut a = std::env::args().skip(1);
    while let Some(k) = a.next() {
        let v = a.next().unwrap_or_default();
        match k.as_str() {
            "--gb" => gb = v.parse().unwrap_or(gb),
            "--s1-mib" => s1_mib = v.parse().unwrap_or(s1_mib),
            "--only" => only = v,
            _ => {
                eprintln!("usage: um-spikes [--gb N] [--s1-mib N] [--only s1,s2,s3,s4]");
                return;
            }
        }
    }
    let g = match gpu::Gpu::new() {
        Ok(g) => g,
        Err(e) => {
            spike!("gpu", "error={}", gpu::es(&e));
            return;
        }
    };
    spike!("gpu", "adapter=\"{}\" driver={}", g.name, g.driver);
    // A free-RAM guard shared by S1 and S3: never plan more than half of what is free.
    let free = gpu::avail_phys();
    let s1_mib = s1_mib.min(free / 2 >> 20).max(64);
    for s in only.split(',') {
        let r = catch_unwind(AssertUnwindSafe(|| match s {
            "s1" => s1::run(&g, s1_mib),
            "s2" => s2::run(&g),
            "s3" => s3::run(gb, free),
            "s4" => s4::run(&g),
            _ => Ok(()),
        }));
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => spike!(s, "error={}", gpu::es(&e)),
            Err(_) => spike!(s, "error=panic"),
        }
    }
    println!("\n== Summary (paste these lines back) ==");
    for l in LINES.lock().unwrap().iter() {
        println!("{l}");
    }
}
