//! Prometheus metrics, over an HTTP server on a port the OS picks.
//!
//! Collection is stop-the-world -- serf runs one thread, and a collection only
//! happens at a safepoint between two bytecodes -- so the time a collection
//! takes *is* the pause, and `serf_gc_pause_seconds` is the number to watch.
//!
//! The counters live here rather than in `gc.rs` because the heap is a
//! thread-local: the serving thread has no business touching it, and reads
//! these atomics instead.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// Every trip to the system allocator, counted.
///
/// The point of the memory subsystem work is that a running interpreter should
/// not make any: Self objects, their slots, their bytes and their activations
/// all belong in the VM's own arenas, and `malloc` should be reached for only
/// to grow one. That is a claim about a number, so the number is a metric --
/// two relaxed atomics on a path that already costs hundreds of cycles.
///
/// It counts the whole process, including the metrics thread and Rust's own
/// startup, which is a few dozen allocations and not worth excluding.
struct Counted;

static MALLOCS: AtomicU64 = AtomicU64::new(0);
static FREES: AtomicU64 = AtomicU64::new(0);
static MALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        MALLOCS.fetch_add(1, Relaxed);
        MALLOC_BYTES.fetch_add(l.size() as u64, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        FREES.fetch_add(1, Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        MALLOCS.fetch_add(1, Relaxed);
        MALLOC_BYTES.fetch_add(n.saturating_sub(l.size()) as u64, Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static ALLOCATOR: Counted = Counted;

/// `SERF_MEM_TRACE=1`: a line at exit, for benchmarks that do not stay up long
/// enough to be scraped.
pub fn trace_mem(tag: &str) {
    if std::env::var_os("SERF_MEM_TRACE").is_none() {
        return;
    }
    let (m, f, b) = (MALLOCS.load(Relaxed), FREES.load(Relaxed), MALLOC_BYTES.load(Relaxed));
    eprintln!("[mem] {}: mallocs {} frees {} live {} bytes {}", tag, m, f, m.saturating_sub(f), b);
}

/// Upper bounds in seconds. A scavenge of a small young generation is tens of
/// microseconds; a major collection of a loaded world is tens of milliseconds.
const BUCKETS: [f64; 11] =
    [0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25];

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
    maps: AtomicU64,
    switched: AtomicU64,
    site_hits: AtomicU64,
    site_map_hits: AtomicU64,
    site_misses: AtomicU64,
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
    maps: AtomicU64::new(0),
    switched: AtomicU64::new(0),
    site_hits: AtomicU64::new(0),
    site_map_hits: AtomicU64::new(0),
    site_misses: AtomicU64::new(0),
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

/// A shape nothing had seen before. A world that mints these in a loop is
/// reshaping objects rather than cloning them, and gains nothing from the
/// map-keyed caches.
pub fn map_minted() {
    M.maps.fetch_add(1, Relaxed);
}

/// One `switch_pointers`: a walk of both generations to make the world stop
/// naming an object that a wider one replaced. Cheap because it happens while
/// a world is being programmed, not while it runs -- this is what says so.
pub fn switched() {
    M.switched.fetch_add(1, Relaxed);
}

/// One send-site inline cache probe. The hit rate is what says whether keying
/// on the map rather than on the receiver was worth it.
pub fn site(hit: bool, by_map: bool) {
    if hit {
        M.site_hits.fetch_add(1, Relaxed);
        if by_map {
            M.site_map_hits.fetch_add(1, Relaxed);
        }
    } else {
        M.site_misses.fetch_add(1, Relaxed);
    }
}

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
        ("serf_mem_mallocs_total", "Trips to the system allocator. The memory subsystem's goal is that a running interpreter makes none.", "counter", MALLOCS.load(Relaxed)),
        ("serf_mem_frees_total", "Blocks returned to the system allocator.", "counter", FREES.load(Relaxed)),
        ("serf_mem_malloc_bytes_total", "Bytes asked of the system allocator.", "counter", MALLOC_BYTES.load(Relaxed)),
        ("serf_gc_objects_allocated_total", "Objects allocated.", "counter", M.allocated.load(Relaxed)),
        ("serf_maps_total", "Distinct object shapes interned.", "counter", M.maps.load(Relaxed)),
        ("serf_switch_pointers_total", "Heap walks to replace every reference to one object with another, as _AddSlots: needs.", "counter", M.switched.load(Relaxed)),
        ("serf_send_site_hits_total", "Send-site inline cache probes that hit.", "counter", M.site_hits.load(Relaxed)),
        ("serf_send_site_map_hits_total", "Hits on a receiver the site had not seen, of a shape it had -- the ones keying on the receiver alone would have missed.", "counter", M.site_map_hits.load(Relaxed)),
        ("serf_send_site_misses_total", "Send-site inline cache probes that missed.", "counter", M.site_misses.load(Relaxed)),
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
