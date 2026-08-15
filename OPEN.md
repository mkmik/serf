# Open problems

Known, reproducible, not fixed. One section each; delete a section when it goes.

## A scavenge costs 1.9ms with nothing to collect

`SERF_GC_STRESS=1` on a *loaded world* is around a hundred times slower than it
should be. The answers are right; the wait is not.

```sh
# 100.9s.  Answers 1, correctly.
SERF_GC_STRESS=1 ./target/release/serf --load core.snap \
  --run "[|:x. q| 5 timesRepeat: [ q: ('a' , 'b') ]. 1] value: 0"
```

It is why `run-tests.sh` no longer runs its annotation checks under stress.

### What is actually happening

Not the loop, and not the image load:

| | |
|---|---|
| `--run "1"` | 1.35s |
| the block above, 5 iterations | 100.9s |
| the same, 10 iterations | 102.7s |

Five iterations and ten cost the same, so it is a fixed cost, not a per-iteration
one. `SERF_GC_STATS=1` says where it is:

```
61,701 collections, ~1.9ms each  ->  ~117s
[gc] minor 1918us young 0->0 words old 108916->108916 objects remembered 0
```

Stress collects after every allocation, so 61,701 collections is expected and
fine. **1.9ms for a collection with an empty young space and an empty remembered
set is not.** The scavenge has nothing to copy and nothing to scan, and still
takes two milliseconds. Worst seen: 25ms.

So the cost is in the root scan, and it is paid whether or not there is anything
to collect.

### The likely cause, and how to check it

`VmRoots::each` walks every root on every collection, and two of them are large
on a loaded world:

* `vm.canonical` — the canonicalised string table.
* `vm.image_strings` — the string table read out of the image.

`core.snap` has 14,922 byte objects, and the two tables between them hold most
of that. Each root goes through `rewrite`, which calls `obj::from_oop`, which
reads the object's header to see whether it is a boxed float -- a cache miss per
root. Thirty thousand of those is about 1.5ms, which is the number.

To confirm, time a collection with the two tables skipped, or just count: put a
counter in `rewrite` and divide.

### What the fix probably is

The old cell heap had this exact problem with annotations, and solved it the way
this wants solving. `Vm::each_root` took a `major` flag and walked the
annotation tables only on a major collection, because an old object neither
moves nor dies outside one; a scavenge took a small list of *young* annotations
instead, kept by a write barrier (`Vm::note_anno`). The commit that did it took
a morphic scavenge from 5.8ms to 3.9ms.

`VmRoots` dropped that distinction in the switch-over and walks everything every
time. The same shape applies: a scavenge should see only the canonical strings
that might be young, and a major should see them all. `heap::Roots::each` would
need to know which kind of collection it is being asked about, which it
currently does not.

Two things to be careful of, and they are why this is not a five-minute change:

* With direct pointers a root that is skipped is not merely untraced -- if the
  object *is* young, it is a pointer into a space that has been abandoned. The
  young-list barrier has to be exactly right, and `SERF_GC_STRESS` on serf's own
  world is what would say so.
* `vm.string()` inserts into `canonical` at runtime, and that string is young.
  That is the barrier's whole job.

### Why it has been left

It is a debug mode, and the mode still works -- 100s is slow, not wrong. Normal
runs do not pay it: a scavenge only happens when the young space fills, so the
fixed cost is amortised over tens of thousands of allocations instead of one.

But it is worth fixing rather than tolerating, because it is the same number
that decides how often a *normal* collection can afford to happen. A scavenge
that costs 1.9ms before it does any work is a scavenge you cannot run often, and
that constrains the young space size for every world serf loads.

### A correction to the record

An earlier note said the collector had been ruled out, on the strength of 12µs
pauses. Those were measured before the image was loaded -- 85 objects in the old
generation, not 108,916 -- so they said nothing about the case in question. The
real pauses are 1.9ms and the collector is exactly where the time goes.
