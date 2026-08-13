//! Prometheus metrics, over an HTTP server on a port the OS picks.
//!
//! Collection is stop-the-world -- serf runs one thread, and a collection only
//! happens at a safepoint between two bytecodes -- so the time a collection
//! takes *is* the pause, and `serf_gc_pause_seconds` is the number to watch.
//!
//! The counters live here rather than in `gc.rs` because the heap is a
//! thread-local: the serving thread has no business touching it, and reads
//! these atomics instead.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// Upper bounds in seconds. A scavenge of a small young generation is tens of
/// microseconds; a major collection of a loaded world is tens of milliseconds.
const BUCKETS: [f64; 11] = [
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25,
];

const YOUNG: usize = 0;
const OLD: usize = 1;

#[derive(Default)]
struct Gen {
    collections: AtomicU64,
    /// nanoseconds, so the sum stays exact until it is divided out at the end
    pause_ns: AtomicU64,
    pause_max_ns: AtomicU64,
    buckets: [AtomicU64; BUCKETS.len()],
}

#[derive(Default)]
struct Metrics {
    gens: [Gen; 2],
    allocated: AtomicU64,
    promoted: AtomicU64,
    freed: AtomicU64,
    young: AtomicU64,
    old: AtomicU64,
    young_capacity: AtomicU64,
    remembered: AtomicU64,
}

static M: Metrics = Metrics {
    gens: [
        Gen {
            collections: AtomicU64::new(0),
            pause_ns: AtomicU64::new(0),
            pause_max_ns: AtomicU64::new(0),
            buckets: [const { AtomicU64::new(0) }; BUCKETS.len()],
        },
        Gen {
            collections: AtomicU64::new(0),
            pause_ns: AtomicU64::new(0),
            pause_max_ns: AtomicU64::new(0),
            buckets: [const { AtomicU64::new(0) }; BUCKETS.len()],
        },
    ],
    allocated: AtomicU64::new(0),
    promoted: AtomicU64::new(0),
    freed: AtomicU64::new(0),
    young: AtomicU64::new(0),
    old: AtomicU64::new(0),
    young_capacity: AtomicU64::new(0),
    remembered: AtomicU64::new(0),
};

/// What one collection did. The counts are totals since the VM started, except
/// `freed` and `promoted`, which are for this collection.
pub struct Collection {
    pub major: bool,
    pub pause: Duration,
    pub freed: u64,
    pub promoted: u64,
    pub allocated: u64,
    pub young: u64,
    pub young_capacity: u64,
    pub old: u64,
    pub remembered: u64,
}

/// `M` is one set of counters for the whole process, but `cargo test` runs
/// tests as threads: any test that collects records into the same totals the
/// exposition test asserts exact values on. Both sides take this first, so the
/// two never interleave. Not a lock the VM itself ever touches.
#[cfg(test)]
pub static TOTALS: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn record(c: Collection) {
    let g = &M.gens[if c.major { OLD } else { YOUNG }];
    let ns = c.pause.as_nanos() as u64;
    g.collections.fetch_add(1, Relaxed);
    g.pause_ns.fetch_add(ns, Relaxed);
    g.pause_max_ns.fetch_max(ns, Relaxed);
    let secs = c.pause.as_secs_f64();
    for (i, b) in BUCKETS.iter().enumerate() {
        if secs <= *b {
            g.buckets[i].fetch_add(1, Relaxed);
        }
    }
    M.freed.fetch_add(c.freed, Relaxed);
    M.promoted.fetch_add(c.promoted, Relaxed);
    M.allocated.store(c.allocated, Relaxed);
    M.young.store(c.young, Relaxed);
    M.young_capacity.store(c.young_capacity, Relaxed);
    M.old.store(c.old, Relaxed);
    M.remembered.store(c.remembered, Relaxed);
}

/// The buckets are already cumulative -- `record` bumps every bound the pause
/// fits under -- so they go out as they are, and `+Inf` is just the count.
fn histogram(out: &mut String, name: &str, gen: &str, g: &Gen) {
    for (i, b) in BUCKETS.iter().enumerate() {
        let n = g.buckets[i].load(Relaxed);
        out.push_str(&format!("{}_bucket{{generation=\"{}\",le=\"{}\"}} {}\n", name, gen, b, n));
    }
    let n = g.collections.load(Relaxed);
    out.push_str(&format!("{}_bucket{{generation=\"{}\",le=\"+Inf\"}} {}\n", name, gen, n));
    let sum = g.pause_ns.load(Relaxed) as f64 / 1e9;
    out.push_str(&format!("{}_sum{{generation=\"{}\"}} {}\n", name, gen, sum));
    out.push_str(&format!("{}_count{{generation=\"{}\"}} {}\n", name, gen, n));
}

pub fn encode() -> String {
    let mut o = String::with_capacity(2048);
    o.push_str(
        "# HELP serf_gc_collections_total Collections run since the VM started.\n\
         # TYPE serf_gc_collections_total counter\n",
    );
    for (gen, i) in [("young", YOUNG), ("old", OLD)] {
        o.push_str(&format!(
            "serf_gc_collections_total{{generation=\"{}\"}} {}\n",
            gen,
            M.gens[i].collections.load(Relaxed)
        ));
    }
    o.push_str(
        "# HELP serf_gc_pause_seconds Stop-the-world pause per collection. The VM \
         is single-threaded and collects only at a safepoint, so this is the whole pause.\n\
         # TYPE serf_gc_pause_seconds histogram\n",
    );
    for (gen, i) in [("young", YOUNG), ("old", OLD)] {
        histogram(&mut o, "serf_gc_pause_seconds", gen, &M.gens[i]);
    }
    o.push_str(
        "# HELP serf_gc_pause_seconds_max Longest single stop-the-world pause so far.\n\
         # TYPE serf_gc_pause_seconds_max gauge\n",
    );
    for (gen, i) in [("young", YOUNG), ("old", OLD)] {
        o.push_str(&format!(
            "serf_gc_pause_seconds_max{{generation=\"{}\"}} {}\n",
            gen,
            M.gens[i].pause_max_ns.load(Relaxed) as f64 / 1e9
        ));
    }
    for (name, help, kind, v) in [
        ("serf_gc_objects_allocated_total", "Objects allocated.", "counter", M.allocated.load(Relaxed)),
        ("serf_gc_objects_freed_total", "Objects reclaimed.", "counter", M.freed.load(Relaxed)),
        ("serf_gc_objects_promoted_total", "Objects tenured into the old generation.", "counter", M.promoted.load(Relaxed)),
        ("serf_gc_young_objects", "Objects in the young generation, as of the last collection.", "gauge", M.young.load(Relaxed)),
        ("serf_gc_young_capacity_objects", "Objects one semispace holds.", "gauge", M.young_capacity.load(Relaxed)),
        ("serf_gc_old_objects", "Objects in the old generation, as of the last collection.", "gauge", M.old.load(Relaxed)),
        ("serf_gc_remembered_objects", "Old objects a scavenge must scan because they were written to.", "gauge", M.remembered.load(Relaxed)),
    ] {
        o.push_str(&format!("# HELP {} {}\n# TYPE {} {}\n{} {}\n", name, help, name, kind, name, v));
    }
    o
}

/// Serve on a port the OS picks, so several VMs can run at once. Answers the
/// same page whatever the path: a scraper asks for /metrics, and there is
/// nothing else to serve.
pub fn serve() -> std::io::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let _ = answer(s);
        }
    });
    Ok(port)
}

fn answer(mut s: TcpStream) -> std::io::Result<()> {
    // one read is enough for a request line and its headers, and the timeout
    // keeps a client that opens a socket and says nothing from parking a thread
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = [0u8; 1024];
    let _ = s.read(&mut buf)?;
    let body = encode();
    write!(
        s,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_collection_shows_up_in_the_exposition() {
        // the totals are the whole process's, and other tests collect into
        // them: hold the lock and measure what this one call added, rather
        // than asserting numbers only a first-to-run test could see
        let _totals = TOTALS.lock().unwrap_or_else(|e| e.into_inner());
        let g = &M.gens[YOUNG];
        let at_1ms = g.buckets[3].load(Relaxed);
        let at_2ms5 = g.buckets[4].load(Relaxed);
        let count = g.collections.load(Relaxed);
        let sum_ns = g.pause_ns.load(Relaxed);
        let freed = M.freed.load(Relaxed);

        record(Collection {
            major: false,
            pause: Duration::from_micros(1500),
            freed: 40,
            promoted: 2,
            allocated: 100,
            young: 7,
            young_capacity: 512,
            old: 3,
            remembered: 1,
        });

        assert_eq!(BUCKETS[3], 0.001);
        assert_eq!(BUCKETS[4], 0.0025);
        // 1.5ms falls in the 2.5ms bucket and not in the 1ms one
        assert_eq!(g.buckets[3].load(Relaxed) - at_1ms, 0);
        assert_eq!(g.buckets[4].load(Relaxed) - at_2ms5, 1);
        assert_eq!(g.collections.load(Relaxed) - count, 1);
        assert_eq!(g.pause_ns.load(Relaxed) - sum_ns, 1_500_000);
        assert_eq!(M.freed.load(Relaxed) - freed, 40);
        assert!(g.pause_max_ns.load(Relaxed) >= 1_500_000);

        // gauges are last-write-wins, and under the lock this call was last
        let o = encode();
        assert!(o.contains("serf_gc_old_objects 3\n"), "{o}");
        // an untouched generation still reports itself, so a scrape never gaps
        assert!(o.contains("serf_gc_collections_total{generation=\"old\"} "), "{o}");
    }
}
