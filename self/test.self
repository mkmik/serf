"Self-hosted test suite. Run: cargo run --release -- self/test.self
 Any failure raises, which makes the VM exit non-zero.

 Note the parentheses around keyword sends used as keyword arguments: as in
 the real Self grammar, `a: b c: d` is one message, never two nested ones."

globals _AddSlots: ( | t = (| parent* = traits object.
    n <- 0.
    check: what Is: got Should: want = (
        n: n + 1.
        (got = want) ifFalse: [
            _Error: 'FAILED: ' , what , ' gave ' , got printString
                    , ' want ' , want printString ].
        self ).
| ) | ).

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
t check: 'float'    Is: 2.0 ** 10        Should: 1024.0.

"A failing primitive answers a bare VM error name and the selector it was
 sent: `traits smallInt` reads both, and retries on bigInts when it overflows"
t check: 'overflow' Is: ((1 << 62) _IntMul: 4 IfFail: [|:e. :n| e])
                    Should: 'overflowError'.
t check: 'failname' Is: ((1 << 62) _IntMul: 4 IfFail: [|:e. :n| n])
                    Should: '_IntMul:IfFail:'.
t check: 'cint'     Is: (((byteVector copySize: 8) _CUnsignedIntSize: 32 At: 2
                          Put: 70000) _CSignedIntSize: 8 At: 2)
                    Should: 112.

"--- booleans and blocks"
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

t n print. ' tests passed' printLine.
