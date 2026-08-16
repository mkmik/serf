//! Driving the collector: when it runs, and what it is allowed to see.
//!
//! The collector itself is `heap.rs` -- generation scavenging over one arena,
//! with direct tagged pointers and no handle table. What is left here is the
//! part that belongs to the VM rather than to the heap: deciding when a
//! collection is due, refusing to run one while roots are sitting in Rust
//! locals, and handing `Vm` over as the root set.
//!
//! Two rules the rest of the VM has to keep, and they are the whole reason a
//! moving collector is dangerous:
//!
//! * **A collection only happens at a safepoint** -- between two bytecodes,
//!   where everything live is reachable from `Vm`. Allocation never collects.
//! * **Nothing may hold an object across one** except through `Vm`. An object
//!   moves, and a `Value` in a Rust local is not a root. `NoGc` covers the
//!   phases that cannot help it; `Vm::temp_roots` covers the rest.

use std::cell::Cell;

use crate::heap;
use crate::value::{lookup_gen_bump, Vm, VmRoots};

thread_local! {
    /// A collection was asked for, and will happen at the next safepoint.
    static WANT: Cell<bool> = const { Cell::new(false) };
    /// ...and make it a major one (`_GarbageCollect` asked).
    static WANT_MAJOR: Cell<bool> = const { Cell::new(false) };
    /// Nesting depth of phases that keep roots in Rust locals nobody walks:
    /// image load and save, and compilation, which re-enters the interpreter
    /// with half-built literal vectors on the Rust stack. Nonzero means "do
    /// not collect".
    static DISABLED: Cell<u32> = const { Cell::new(0) };
    /// Collect at the first safepoint after *any* allocation, so a root the VM
    /// forgot to tell the collector about is used one collection after it was
    /// dropped rather than a million allocations later.
    static STRESS: bool = std::env::var_os("SERF_GC_STRESS").is_some();
    static OFF: bool = std::env::var("SERF_GC").is_ok_and(|v| v == "off");
    static STATS: bool = std::env::var_os("SERF_GC_STATS").is_some();
}

pub fn disable() {
    DISABLED.with(|d| d.set(d.get() + 1));
}

pub fn enable() {
    DISABLED.with(|d| d.set(d.get() - 1));
}

fn disabled() -> bool {
    DISABLED.with(|d| d.get()) > 0
}

/// Guard for a phase that keeps object references in Rust locals the collector
/// does not walk: image load and save, and compilation.
pub struct NoGc;

impl NoGc {
    pub fn new() -> NoGc {
        disable();
        NoGc
    }
}

impl Default for NoGc {
    fn default() -> NoGc {
        NoGc::new()
    }
}

impl Drop for NoGc {
    fn drop(&mut self) {
        enable();
    }
}

/// Ask for a collection at the next safepoint. `_GarbageCollect` and friends go
/// through this rather than collecting on the spot: a primitive runs with the
/// interpreter's Rust locals underneath it, which are not roots. A request made
/// while collection is disabled stays pending.
pub fn request(major: bool) {
    WANT.with(|w| w.set(true));
    if major {
        WANT_MAJOR.with(|w| w.set(true));
    }
}

/// True when a collection is due. Checked at the interpreter's safepoint.
pub fn wanted() -> bool {
    if OFF.with(|o| *o) || disabled() {
        return false;
    }
    WANT.with(|w| w.get()) || STRESS.with(|s| *s) || heap::heap().wants_collection()
}

/// Collect. `major` sweeps the old generation as well.
///
/// Only ever called from the interpreter's safepoint, or from a primitive's
/// request that the safepoint picks up: everything the VM can still reach has
/// to be in `VmRoots` by then, and `disabled` covers the phases whose roots
/// live in Rust locals instead.
pub fn collect(vm: &mut Vm, major: bool) {
    if OFF.with(|o| *o) || disabled() {
        return;
    }
    let h = heap::heap();
    let t0 = std::time::Instant::now();
    let (young_before, old_before) = (h.young_used(), h.old_live());

    {
        let mut roots = VmRoots { vm };
        h.collect(&mut roots, major);
    }

    // Every memoised lookup, every inline cache and every interned shape is
    // keyed on an address, and a collection has just moved them. Bumping the
    // generation is what drops all three at once.
    lookup_gen_bump();

    WANT.with(|w| w.set(false));
    WANT_MAJOR.with(|w| w.set(false));

    // the pause is the whole collection: one thread, stopped at a safepoint
    crate::metrics::record(crate::metrics::Collection {
        major,
        pause: t0.elapsed(),
        freed: 0,
        promoted: 0,
        allocated: 0,
        young: h.young_used() as u64,
        young_capacity: h.young_capacity() as u64,
        old: h.old_live() as u64,
        remembered: h.remembered_len() as u64,
    });

    if STATS.with(|s| *s) {
        eprintln!(
            "[gc] {} {}us young {}->{} words old {}->{} objects remembered {}",
            if major { "major" } else { "minor" },
            t0.elapsed().as_micros(),
            young_before,
            h.young_used(),
            old_before,
            h.old_live(),
            h.remembered_len(),
        );
    }
}

/// The safepoint's entry point: collect if anything asked for it, majoring when
/// the old generation has outgrown its share.
pub fn collect_if_wanted(vm: &mut Vm) {
    if !wanted() {
        return;
    }
    let major = WANT_MAJOR.with(|w| w.get()) || heap::heap().old_wants_major();
    collect(vm, major);
}

/// Heap occupancy, for `--stats` and the metrics page.
pub fn young_used() -> usize {
    heap::heap().young_used()
}

#[allow(dead_code)]
pub fn young_capacity() -> usize {
    heap::heap().young_capacity()
}

pub fn old_used() -> usize {
    heap::heap().old_live()
}
