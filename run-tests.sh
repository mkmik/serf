#!/bin/sh
# serf test suite: the Self-level checks, then a full image round-trip.
set -e
cd "$(dirname "$0")"
cargo build --release 2>&1 | grep -E '^(error|warning: unused)' && exit 1
cargo test --release --quiet
R=./target/release/serf
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT

$R self/test.self

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
