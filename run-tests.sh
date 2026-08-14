#!/bin/sh
# serf test suite: the Self-level checks, then a full image round-trip.
set -e
cd "$(dirname "$0")"
cargo build --release 2>&1 | grep -E '^(error|warning: unused)' && exit 1
cargo test --release --quiet
R=./target/release/serf
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
# the suite runs the VM a few dozen times; it need not listen a few dozen times
export SERF_METRICS=off

$R self/test.self

# again checking every memoised map against a freshly computed shape. A send
# caches on the receiver's shape, so a mutation that changes one without
# calling `forget_map` would dispatch to another object's method -- and the
# Self-level checks cannot see it, because a shape change also bumps
# LOOKUP_GEN and flushes every site before the stale key is consulted
SERF_MAP_VERIFY=1 $R self/test.self >/dev/null
[ -f core.snap ] && SERF_MAP_VERIFY=1 $R --load core.snap \
  -e "(1 to: 200) do: [|:i| (i printString , 'x') hash ]" >/dev/null 2>&1

# again with a young generation small enough to scavenge hundreds of times,
# checking the write barrier against a full scan of the old generation as it
# goes: a missed root shows up as a wrong answer or a panic, a missed barrier
# makes the check itself fail the run
SERF_GC_YOUNG=512 SERF_GC_VERIFY=1 $R self/test.self >/dev/null
# and against a real world, which is where old objects and old->young writes
# actually exist -- test.self's heap is too small to build one
[ -f core.snap ] && SERF_GC_YOUNG=512 SERF_GC_VERIFY=1 $R --load core.snap \
  -e "(1 to: 200) do: [|:i| (i printString , 'x') hash ]" >/dev/null 2>&1
# an annotation lives in a Rust-side table, out of reach of the remembered set,
# so a scavenge finds it only through `Vm::note_anno`. Write a young one, churn
# until it would have been collected, then read it back: without the barrier
# this panics on a freed handle. Stress mode never reuses an id, so a miss is a
# panic rather than an annotation that quietly turns into someone else.
if [ -f core.snap ]; then
  got=$(SERF_GC_STRESS=1 $R --load core.snap --run "[|:x. m. q| \
    m: (reflect: (| y = 1 |)). \
    m: (m _MirrorCopyAnnotation: ('ann-' , 'young')). \
    200 timesRepeat: [ q: ('a' , 'b') ]. \
    m _MirrorAnnotation] value: 0" 2>&1 | tail -1)
  [ "$got" = "'ann-young'" ] || { echo "annotation barrier: got [$got]"; exit 1; }

  # A block holds the activation it closed over, and both the frame that
  # returned and the scavenge that collects the block offer that activation to
  # the pool to be refilled in place. A block still holding one must keep it:
  # close over a local, collect many times, then read it back through the
  # block. Reusing it under the block would answer something other than 42.
  got=$(SERF_GC_STRESS=1 $R --load core.snap --run "[|:x. b. q. n <- 7| \
    b: [|:z| z + n]. \
    200 timesRepeat: [ q: ('a' , 'b') ]. \
    b value: 35] value: 0" 2>&1 | tail -1)
  [ "$got" = "42" ] || { echo "captured activation: got [$got] want [42]"; exit 1; }

  # slot annotations are nested under the object rather than keyed on
  # (object, slot), and a collection walks that outer map to drop dead
  # entries. Write one, collect hard enough to sweep many times, read it back.
  got=$(SERF_GC_STRESS=1 $R --load core.snap --run "[|:x. m. q| \
    m: (reflect: (| y = 1 |)). \
    m: (m _MirrorCopyAt: 'z' Put: (reflect: 9) IsParent: false IsArgument: false Annotation: 'slot-note'). \
    200 timesRepeat: [ q: ('a' , 'b') ]. \
    m _MirrorAnnotationAt: 'z'] value: 0" 2>&1 | tail -1)
  [ "$got" = "'slot-note'" ] || { echo "slot annotation: got [$got]"; exit 1; }
fi
echo "gc checks ok"

# scrape a running VM on the port it picked for itself
if command -v curl >/dev/null; then
  SERF_METRICS=on $R -e "[ | i <- 0. v | [ i < 2000000 ] whileTrue: [ v: (vector copySize: 8). i: i + 1 ]. i ] value" \
    >/dev/null 2>"$T/m" &
  pid=$!
  n=0
  while [ $n -lt 50 ] && ! grep -q 127.0.0.1 "$T/m" 2>/dev/null; do sleep 0.1; n=$((n + 1)); done
  port=$(sed -n 's|.*127\.0\.0\.1:\([0-9]*\).*|\1|p' "$T/m")
  curl -s "http://127.0.0.1:$port/metrics" > "$T/scrape" || true
  wait $pid
  grep -q '^serf_gc_pause_seconds_count{generation="young"} [1-9]' "$T/scrape" || {
    echo "metrics: nothing scraped from port [$port]"; cat "$T/scrape"; exit 1; }
  echo "metrics ok"
fi

cat > "$T/w.self" <<'EOF'
globals _AddSlots: ( | demo = (| parent* = traits object.
    n <- 7.
    fact: k = ( k < 2 ifTrue: [ 1 ] False: [ k * (fact: k - 1) ] ).
    twice = ( [ |:x| x * 2 ] value: n ).
    greet = ( 'hello: ' , (fact: 6) printString ) |) | ).
EOF

$R "$T/w.self" --save "$T/a.snap" >/dev/null
$R --verify-image "$T/a.snap" >/dev/null

got=$($R --load "$T/a.snap" 2>/dev/null \
  -e "((snapshotLobby slotAt: 'globals') slotAt: 'demo') greet printLine" \
  -e "((snapshotLobby slotAt: 'globals') slotAt: 'demo') twice printLine" \
  -e "(((snapshotLobby slotAt: 'globals') slotAt: 'demo') fact: 10) printLine" \
  | sed -n '1p;3p;5p' | tr '\n' ' ')
want="hello: 720 14 3628800 "
[ "$got" = "$want" ] || { echo "image round-trip: got [$got] want [$want]"; exit 1; }

# a snapshot written from a loaded snapshot must still work
$R --load "$T/a.snap" >/dev/null 2>&1
echo "image round-trip ok"

# if a real C++-built world is present, round-trip that too and require the
# reachable graph to come back with the same shape
if [ -f core.snap ]; then
  $R --verify-image core.snap >/dev/null
  $R --load core.snap --save "$T/c.snap" >/dev/null 2>&1
  $R --verify-image "$T/c.snap" >/dev/null
  # Methods, bytecodes, literals, blocks, floats and a checksum over every
  # integer and float in the reachable graph must come back identical, as must
  # the parent-slot counts on methods, plain objects, blocks and vectors.
  # (Byte-object counts may drop: the world holds canonical strings with equal
  # content, and writing canonicalises them together. See README.)
  $R --load core.snap    --stats 2>/dev/null > "$T/s1"
  $R --load "$T/c.snap"  --stats 2>/dev/null > "$T/s2"
  for f in "$T/s1" "$T/s2"; do
    sed -n '3p' "$f" > "$f.k"
    sed -n '4p' "$f" | sed 's/"bytes": [0-9]*, //' >> "$f.k"
  done
  diff "$T/s1.k" "$T/s2.k" || { echo "core.snap round-trip changed the object graph"; exit 1; }
  echo "core.snap round-trip ok"

  # A method mirror must answer its source -- through the world's own
  # protocol, and through a snapshot serf wrote. This is what an outliner
  # shows when you expand a slot holding code.
  src="[| m | m: (((reflect: traits smallInt) at: 'asBigInteger') contents).
        (m source , '|' , m sourceOffset printString , '\n') _StringPrint ] value"
  for f in core.snap "$T/c.snap"; do
    got=$($R --load "$f" --run "$src" 2>/dev/null | head -1)
    want=" bigInt fromInt: asSmallInteger|0"
    [ "$got" = "$want" ] || { echo "method source from $f: got [$got] want [$want]"; exit 1; }
  done
  echo "method source ok"

  # the world parsing its own source: text in, a mirror out, evaluated in a
  # context; and a syntax error that says what was wrong rather than leaving
  # the prototype's 'the prototypical syntax error' behind
  got=$($R --load core.snap 2>/dev/null \
    -e "lobby asMirror evaluate: ('3 + 4' parseObjectBodyIfFail: [|:e| e])" \
    -e "'3 +' parseObjectBodyIfFail: [|:e| e message]" | tr '\n' ' ')
  want="7 'line 1: expected an expression (near Eof)' "
  [ "$got" = "$want" ] || { echo "parsing: got [$got] want [$want]"; exit 1; }
  echo "parsing ok"

  # Only a define moves the programming timestamp, as in the C++ VM, where
  # define_prim holds the sole increment_programming_timestamp call. Bumping it
  # for _AddSlots: too would leave the module cache obsolete after changes the
  # real VM ignores, so every refill re-walks the lobby and re-warns about
  # slots that belong to no module.
  got=$($R --load core.snap 2>/dev/null \
    -e "[| t. o | o: (| x = 3 |). t: 0 _ProgrammingTimestamp. o _Mirror _MirrorDefine: (| y = 4 |) _Mirror. 0 _ProgrammingTimestamp - t] value" \
    -e "[| t. o | o: (| x = 3 |). t: 0 _ProgrammingTimestamp. o _AddSlots: (| y = 4 |). 0 _ProgrammingTimestamp - t] value" | tr '\n' ' ')
  want="1 0 "
  [ "$got" = "$want" ] || { echo "programming timestamp: got [$got] want [$want]"; exit 1; }
  echo "programming timestamp ok"
fi

# A loaded world keeps its header's programming timestamp, so the caches that
# stamp themselves with it are obsolete while empty. The outliner's button
# cache is one: filled on cmd+click, and looked up by name right after.
if [ -f Clean-4.4.snap ]; then
  got=$($R --load Clean-4.4.snap 2>/dev/null \
    --run "[selfObjectModel ensureButtonCacheIsFull. selfObjectModel buttonCache includesKey: 'addSlot'] value" | tail -1)
  [ "$got" = "true" ] || { echo "button cache never filled: got [$got]"; exit 1; }
  echo "button cache ok"
fi

# X11 foreign calls. Headless by default: our own Xvfb, which -displayfd lets
# pick a free display and tells us when it is ready to accept connections.
# SERF_X11=real runs the same check against $DISPLAY instead, so the window
# actually appears (XQuartz); SERF_X11=off skips it.
XVFB=${XVFB:-/opt/X11/bin/Xvfb}
[ -x "$XVFB" ] || XVFB=$(command -v Xvfb || true)
D=
case "${SERF_X11:-headless}" in
  off) ;;
  real) D=$DISPLAY ;;
  *) if [ -x "$XVFB" ]; then
       "$XVFB" -displayfd 3 -screen 0 640x480x24 3>"$T/dpy" >/dev/null 2>&1 &
       xvfb=$!
       trap 'kill $xvfb 2>/dev/null; rm -rf "$T"' EXIT
       n=0
       while [ ! -s "$T/dpy" ] && [ $n -lt 100 ]; do sleep 0.1; n=$((n + 1)); done
       [ -s "$T/dpy" ] && D=":$(cat "$T/dpy")"
     fi ;;
esac
if [ -n "$D" ]; then
  out=$(DISPLAY=$D $R self/x11-demo.self 2>&1 | tail -1)
  [ "$out" = "drew" ] || { echo "x11 demo failed on $D: $out"; exit 1; }
  echo "x11 demo ok ($D)"
fi
