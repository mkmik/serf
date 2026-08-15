# Open problems

Known, reproducible, not fixed. One section each; delete a section when it goes.

## The interpreter is ~1.3x slower than the cell heap

Measured against the last pre-flip commit (2411e7f), built as its own binary:

| | cells | arena |
|---|---|---|
| 1M-iteration integer loop | 0.95s | 1.23s |
| 200k clone-and-send loop | 0.29s | 0.35s |
| `self/test.self` | 0.18s | 0.24s |
| `--load core.snap --save` | 0.24s | **0.18s** |
| stress, 5 iterations on a loaded world | -- | 7.1s, was 100.9s |

Image work is faster and the collector itself is about four times faster per
object (`cargo test --release -- --ignored heap::bench`). What is left is a
consistent ~1.3x on interpreter loops, and it is worth closing, because the
whole point of direct pointers was that an interpreter should not pay for the
indirection a handle table charged.

Where it probably is, in the order a profile last put them:

* **`heap::heap()` is a thread-local.** `LocalKey::with` on every allocation and
  every write barrier. Removing it from field reads and from the barrier is what
  took this from 32x to 1.3x, and it is still on `alloc_or_tenure`. The fix is
  to hand the interpreter the `&'static Heap` once per `run_stack` rather than
  fetching it per operation, which means threading it through `obj.rs`.
* **`obj::from_oop` reads the object's header** on every `Value` read that is
  not an immediate, only to ask whether it is a boxed float. Objects are
  8-aligned and bits 1 and 2 of an `Oop` are still spare: tagging a boxed float
  would make that a bit test. The collector would have to mask the tag when
  following a reference, which is a real cost on its own hot path -- measure
  before assuming it wins.
* **`Value` converting at the boundary at all.** The enum exists so that 180
  call sites kept compiling. Moving the hot paths to `Oop` directly would remove
  the conversion rather than make it cheaper.

`sample` on a 20M-iteration loop is how the above was found; it has been
reliable where reasoning about it was not.
