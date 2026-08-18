"Self-hosted test suite. Run: cargo run --release -- self/test.self
 Any failure raises, which makes the VM exit non-zero.

 Note the parentheses around keyword sends used as keyword arguments. A
 keyword argument is a whole expression (`Parser::parseExpr` recurses into
 one), so a *lower*case keyword there opens a nested message and swallows the
 capitalised parts that follow: `a: b c: d E: f` is `a: (b c:E: d f)`, never
 `a:E:`. The parentheses say which one was meant."

globals _AddSlots: ( | t = (| parent* = traits object.
    n <- 0.
    check: what Is: got Should: want = (
        n: n + 1.
        (got = want) ifFalse: [
            _Error: 'FAILED: ' , what , ' gave ' , got printString
                    , ' want ' , want printString ].
        self ).
| ) | ).

"--- keyword nesting. Every ui2 button script in the 4.4 world is stored as
 source and reparsed on the click, and the world writes them unparenthesised
 (`event sourceHand attach: selfObjectModel newOutlinerFor: m InWorld: w`), so
 an argument that stops at a binary expression loses the whole menu"
globals _AddSlots: ( | k = (| parent* = traits object.
    a: x       = ( 'a:'   , x ).
    b: x C: y  = ( 'b:C:' , x , y ).
| ) | ).
t check: 'kw arg nests'  Is: (k a: k b: 'p' C: 'q')   Should: 'a:b:C:pq'.
t check: 'CapKw is mine' Is: (k b: 'p' C: 'q')        Should: 'b:C:pq'.

"--- literals, arithmetic, precedence (binary is strictly left to right)"
t check: 'add'      Is: 3 + 4            Should: 7.
t check: 'no prec'  Is: 3 + 4 * 2        Should: 14.
t check: 'unary1st' Is: 2 + 3 squared    Should: 11.
t check: 'intdiv'   Is: 7 / 2            Should: 3.
t check: 'mod'      Is: 7 % 2            Should: 1.
t check: 'mixed'    Is: 2.5 + 1          Should: 3.5.
t check: 'radix'    Is: 16r1F            Should: 31.
t check: 'negative' Is: -5 abs           Should: 5.
t check: 'compare'  Is: 3 < 4            Should: true.
t check: 'eq mixed' Is: 3 = 'x'          Should: false.
t check: 'shift'    Is: 1 << 10          Should: 1024.
"a shift runs over the smallint's 63 bits, not the machine's 64: a logical
 right shift of -1 is the largest smallint, and a left shift that leaves the
 field either wraps (logical) or fails (arithmetic), never escaping it"
t check: 'lsr'      Is: (-1 _IntLogicalShiftRight: 1)  Should: 4611686018427387903.
t check: 'lsr0'     Is: (-1 _IntLogicalShiftRight: 0)  Should: -1.
t check: 'lsl wrap' Is: (1 _IntLogicalShiftLeft: 62)   Should: -4611686018427387904.
t check: 'asl ovf'  Is: (1 _IntArithmeticShiftLeft: 62 IfFail: [|:e| 'ovf'])
                    Should: 'ovf'.
"a sum of two smallints can be too wide to be one, and a failing primitive
 names its error the way the world spells it -- `traits smallInt` compares
 against 'overflowError' and retries in bigInts when it matches -- and hands
 the fail block the selector as it was sent"
t check: 'add ovf'  Is: ((1 << 61) _IntAdd: (1 << 61) IfFail: [|:e| e])
                    Should: 'overflowError'.
t check: 'fail name' Is: (1 _IntDiv: 0 IfFail: [|:e. :n| n])
                    Should: '_IntDiv:IfFail:'.
t check: 'float'    Is: 2.0 ** 10        Should: 1024.0.

"--- booleans and blocks. A block in a data slot is data: reading the slot
 answers the block, and only sending it `value` runs it. A method in a slot
 is code, and reading the slot runs it"
t check: 'blockslot' Is: [| o | o: (| b <- 0 |). o b: [|:x| x + 1].
                          (o b) value: 41 ] value                     Should: 42.
t check: 'methslot' Is: [| o | o: (| m = ( 7 ) |). o m ] value        Should: 7.
t check: 'ifTrue'   Is: ((3 < 4) ifTrue: ['y'] False: ['n'])   Should: 'y'.
t check: 'block0'   Is: [ 1 + 1 ] value                        Should: 2.
t check: 'block2'   Is: ([|:a. :b| a * b] value: 6 With: 7)     Should: 42.
t check: 'shortcut' Is: false && [ _Error: 'not evaluated' ]    Should: false.
t check: 'blocklcl' Is: [ | q <- 5 | q * 2 ] value             Should: 10.

"--- closures over method locals, three levels deep"
t check: 'nesting'  Is: (| parent* = traits object.
    deep = ( | a <- 1 | [ | b <- 10 | [ | c <- 100 | a + b + c ] value ] value ) |) deep
                    Should: 111.

globals _AddSlots: ( | mkCounter = (| parent* = traits object.
    make = ( | n <- 0 | [ n: n + 1. n ] ) |) | ).
t check: 'closure'  Is: [ | c | c: mkCounter make. c value. c value. c value ] value
                    Should: 3.

"--- non-local return out of a nested block"
globals _AddSlots: ( | finder = (| parent* = traits object.
    find: v In: vec = ( vec do: [|:e| (e = v) ifTrue: [ ^'found' ] ]. 'missing' ) |) | ).
t check: 'nlr hit'  Is: (finder find: 3 In: ((vector copySize: 2) at: 0 Put: 3))
                    Should: 'found'.
t check: 'nlr miss' Is: (finder find: 9 In: ((vector copySize: 2) at: 0 Put: 3))
                    Should: 'missing'.
t check: 'nlr tail' Is: (| parent* = traits object.
    run: b = ( b value ). go = ( run: [ ^'escaped' ] ) |) go
                    Should: 'escaped'.
"go tail-calls run:, so its frame is gone by the time the block returns
 through it -- twice over, with a second tail call in between"
t check: 'nlr through two tail calls' Is: (| parent* = traits object.
    run: b = ( b value. 'wrong' ). mid: b = ( run: b ).
    go = ( mid: [ ^'through' ] ) |) go
                    Should: 'through'.
"a block that outlives the activation it would return to: the tail call must
 not swallow the answer of the send it handed its frame to"
t check: 'tail call keeps the answer' Is: (| parent* = traits object.
    keep <- 0. stash: b = ( keep: b. 'stashed' ).
    go = ( stash: [ ^'wrong' ] ) |) go
                    Should: 'stashed'.

"--- inheritance: undirected and directed resends"
traits _AddSlots: ( | animal = (| parent* = traits object. speak = ( 'noise' ) |) | ).
traits _AddSlots: ( | dog = (| parent* = traits animal. speak = ( 'woof/' , resend.speak ) |) | ).
t check: 'resend'   Is: (| parent* = traits dog |) speak  Should: 'woof/noise'.
t check: 'directed' Is: (| parent* = traits object.
    a* = (| f = ('A') |). b* = (| g = ('B') |). both = ( a.f , b.g ) |) both
                    Should: 'AB'.

"--- assignable slots and cloning"
globals _AddSlots: ( | point = (| parent* = traits object. x <- 0. y <- 0.
    printString = ( 'point(' , x printString , ', ' , y printString , ')' ).
    + p = ( (clone x: x + p x) y: y + p y ) |) | ).
t check: 'assign'   Is: (point copy x: 3) x                    Should: 3.
t check: 'proto'    Is: point x                                Should: 0.
t check: 'operator' Is: ((point copy x: 3) + (point copy x: 4)) x    Should: 7.
t check: 'print'    Is: (point copy y: 2) printString           Should: 'point(0, 2)'.

"--- reflection"
t check: 'hasSlot'  Is: (point hasSlot: 'x:')                   Should: true.
t check: 'slotAt'   Is: (point slotAt: 'x')                     Should: 0.
t check: 'isParent' Is: (point isParentAt: 'parent')            Should: true.
t check: 'perform'  Is: (3 _Perform: '+' With: 4)               Should: 7.

"--- indexables"
t check: 'concat'   Is: 'he' , 'llo'                            Should: 'hello'.
t check: 'reverse'  Is: 'hello' reverse                         Should: 'olleh'.
t check: 'copyFrom' Is: ('hello' copyFrom: 1 To: 4)             Should: 'ell'.
t check: 'size'     Is: 'hello' size                            Should: 5.
t check: 'escapes'  Is: 'a\tb\nc' size                          Should: 5.
t check: 'strcmp'   Is: 'abc' < 'abd'                           Should: true.
t check: 'vecprint' Is: ((vector copySize: 2) at: 0 Put: 1) printString
                    Should: '(1. nil. )'.
t check: 'collect'  Is: ((((vector copySize: 0) copyAddLast: 1) copyAddLast: 2)
                         collect: [|:e| e * 10]) printString
                    Should: '(10. 20. )'.
t check: 'inject'   Is: ((((vector copySize: 0) copyAddLast: 1) copyAddLast: 2)
                         inject: 0 Into: [|:a. :b| a + b])
                    Should: 3.
t check: 'includes' Is: (((vector copySize: 1) at: 0 Put: 7) includes: 7)
                    Should: true.
"a C integer inside a byte vector: the width is in bits, the index in bytes,
 the order the machine's. This is what a bigInt keeps its digits in"
t check: 'cint'     Is: [| b | b: 'abcd' copy.
                          b _CUnsignedIntSize: 32 At: 0 Put: 305419896.
                          b _CUnsignedIntSize: 32 At: 0 ] value       Should: 305419896.
t check: 'cint sgn' Is: [| b | b: 'abcd' copy.
                          b _CSignedIntSize: 8 At: 0 Put: 255.
                          b _CSignedIntSize: 8 At: 0 ] value          Should: -1.
t check: 'cint end' Is: [| b | b: 'abcd' copy.
                          b _CUnsignedIntSize: 32 At: 1 IfFail: [|:e| e] ] value
                    Should: 'badIndexError'.

"--- loops run in constant space: whileTrue: recurses in tail position"
t check: 'bigloop'  Is: ((| parent* = traits object.
    sum: n = ( | i <- 0. s <- 0 | [ i < n ] whileTrue: [ s: s + i. i: i + 1 ]. s ) |)
    sum: 200000)
                    Should: 19999900000.
t check: 'timesRep' Is: [ | k | k: 0. 5 timesRepeat: [ k: k + 2 ]. k ] value
                    Should: 10.
t check: 'to:By:'   Is: [ | k | k: 0. 10 to: 1 By: -3 Do: [|:i| k: k + i ]. k ] value
                    Should: 22.

"--- recursion"
t check: 'fib'      Is: ((| parent* = traits object.
    fib: n = ( n < 2 ifTrue: [ n ] False: [ (fib: n - 1) + (fib: n - 2) ] ) |) fib: 20)
                    Should: 6765.

"--- inline caches: one send site, sent to over and over. Every check below
     runs the *same* `o v` in `fetch:`, so the site's cached hit is what is
     under test: it has to notice an unrelated receiver, an inherited slot,
     an immediate (whose lookup starts in its traits, not in itself), and a
     slot added later that shadows what it found."
globals _AddSlots: ( | ic = (| parent* = traits object.
    fetch: o = ( o v ).
    twice: x = ( x + x ).
    proto = (| parent* = traits object. v = 'proto' |).
    other = (| parent* = traits object. v = 'other' |).
| ) | ).
globals _AddSlots: ( | icChild = (| parent* = ic proto |) | ).

t check: 'ic fill'   Is: (ic fetch: ic proto) Should: 'proto'.
t check: 'ic poly'   Is: (ic fetch: ic other) Should: 'other'.
t check: 'ic parent' Is: (ic fetch: icChild)  Should: 'proto'.
icChild _AddSlots: (| v = 'own' |).
t check: 'ic shadow' Is: (ic fetch: icChild)  Should: 'own'.
t check: 'ic int'    Is: (ic twice: 3)        Should: 6.
t check: 'ic float'  Is: (ic twice: 2.5)      Should: 5.0.

"--- slots live in the object's cell while they fit and spill to a vector when
     they do not, so both sides of that branch need exercising: an object born
     with more than fits, and one grown past it a slot at a time."
globals _AddSlots: ( | big = (| parent* = traits object.
    a = 1. b = 2. c = 3. d = 4. e = 5. f = 6.
    sum = ( a + b + c + d + e + f ) |) | ).
t check: 'spilled'    Is: big sum              Should: 21.
big _AddSlots: (| g = 7 |).
t check: 'spill grew' Is: big sum + big g      Should: 28.
globals _AddSlots: ( | grew = (| parent* = traits object. a = 1 |) | ).
grew _AddSlots: (| b = 2 |). grew _AddSlots: (| c = 3 |).
grew _AddSlots: (| d = 4 |). grew _AddSlots: (| e = 5 |).
t check: 'grew past'  Is: grew a + grew e      Should: 6.

"--- a tail call is where most activations end, and it hands them to the pool
     to be refilled in place. One a block still holds must not go: `whileTrue:`
     is recursive in tail position, so the `^` below returns non-locally out of
     a block, through frames that were tail-called away, to a home activation
     that has to still be itself when it gets there."
globals _AddSlots: ( | tc = (| parent* = traits object.
    find: n = ( | i <- 0 |
        [ i < 1000 ] whileTrue: [ (i = n) ifTrue: [ ^'found' ]. i: i + 1 ].
        'missing' ) |) | ).
t check: 'nlr thru tail' Is: (tc find: 500)    Should: 'found'.
t check: 'tail no nlr'   Is: (tc find: 5000)   Should: 'missing'.
t check: 'nlr at once'   Is: (tc find: 0)      Should: 'found'.

"--- maps: the shape a send caches on, and what has to invalidate it.
 `askWho:` has a single send site for `who`, so every check below reuses one
 inline cache entry. Two objects with the same slot names but different parents
 must not share it -- which is why a parent slot's value is part of the shape."
globals _AddSlots: ( | mA = (| parent* = traits object. who = 'A'. |).
                       mB = (| parent* = traits object. who = 'B'. |).
                       askWho: x = ( x who ). | ).
globals _AddSlots: ( | mk1 <- (| parent* <- mA. |).
                       mk2 <- (| parent* <- mB. |). | ).
t check: 'one site, parent A'  Is: (askWho: mk1)  Should: 'A'.
t check: 'one site, parent B'  Is: (askWho: mk2)  Should: 'B'.
t check: 'one site, back to A' Is: (askWho: mk1)  Should: 'A'.

"a clone shares its prototype's shape, so one cache entry answers for both"
globals _AddSlots: ( | mk3 <- mk1 _Clone. | ).
t check: 'clone shares shape'  Is: (askWho: mk3)  Should: 'A'.

"rewiring a parent changes what that site has to find"
mk3 parent: mB.
t check: 'parent rewired'      Is: (askWho: mk3)  Should: 'B'.
t check: 'sibling unaffected'  Is: (askWho: mk1)  Should: 'A'.

"adding a slot shadows the parent's, and removing it falls back again"
mk3 _AddSlots: (| who = 'own'. |).
t check: 'added slot shadows'  Is: (askWho: mk3)  Should: 'own'.
"the sibling still has the old shape: if mk3 kept the memoised map it no longer
 matches, the site would answer for mk3 here and find a slot mk1 does not have"
t check: 'sibling after add'   Is: (askWho: mk1)  Should: 'A'.
mk3 _RemoveSlot: 'who'.
t check: 'removed slot falls back' Is: (askWho: mk3) Should: 'B'.
t check: 'sibling after remove' Is: (askWho: mk1)  Should: 'A'.


t n print. ' tests passed' printLine.
