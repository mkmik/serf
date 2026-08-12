"serf bootstrap world.

The VM hands us: lobby, globals, traits {object smallInt float string
byteVector vector block}, nil, true, false, and the string/byteVector/vector
prototypes. Everything below is ordinary Self."


"------------------------------------------------------------------ objects"

traits object _AddSlots: ( |
    "every object can reach the global namespace"
    globals* = globals.

    clone = ( _Clone ).
    copy = ( clone ).

    = x = ( _Eq: x ).
    == x = ( _Eq: x ).
    != x = ( (self = x) not ).
    hash = ( _Hash ).

    isNil = ( false ).
    notNil = ( isNil not ).
    isNumber = ( false ).
    isString = ( false ).
    isBlock = ( false ).
    isVector = ( false ).
    isBoolean = ( false ).

    printString = ( _PrintString ).
    print = ( printString _StringPrint. self ).
    printLine = ( print. '\n' _StringPrint. self ).

    error: msg = ( _Error: msg ).

    perform: sel = ( _Perform: sel ).
    perform: sel With: a = ( _Perform: sel With: a ).
    perform: sel With: a With: b = ( _Perform: sel With: a With: b ).

    addSlots: obj = ( _AddSlots: obj ).
    removeSlot: n = ( _RemoveSlot: n ).
    slotNames = ( _SlotNames ).
    hasSlot: n = ( _HasSlot: n ).
    slotAt: n = ( _SlotAt: n ).
    slotAt: n Put: v = ( _SlotAt: n Put: v ).
    isParentAt: n = ( _IsParentAt: n ).

    ifNil: blk = ( self ).
    ifNotNil: blk = ( blk value: self ).
| ).

globals _AddSlots: ( | parent* = traits object | ).
lobby   _AddSlots: ( | parent* = traits object | ).

nil _AddSlots: ( |
    parent* = traits object.
    isNil = ( true ).
    printString = ( 'nil' ).
    ifNil: blk = ( blk value ).
    ifNotNil: blk = ( nil ).
| ).


"----------------------------------------------------------------- booleans"

traits _AddSlots: ( | boolean = () | ).
traits boolean _AddSlots: ( |
    parent* = traits object.
    isBoolean = ( true ).
| ).

true _AddSlots: ( |
    parent* = traits boolean.
    printString = ( 'true' ).
    not = ( false ).
    ifTrue: b = ( b value ).
    ifFalse: b = ( nil ).
    ifTrue: t False: f = ( t value ).
    ifFalse: f True: t = ( t value ).
    && b = ( b value ).
    || b = ( self ).
    and: b = ( b value ).
    or: b = ( self ).
    asInteger = ( 1 ).
| ).

false _AddSlots: ( |
    parent* = traits boolean.
    printString = ( 'false' ).
    not = ( true ).
    ifTrue: b = ( nil ).
    ifFalse: b = ( b value ).
    ifTrue: t False: f = ( f value ).
    ifFalse: f True: t = ( f value ).
    && b = ( self ).
    || b = ( b value ).
    and: b = ( self ).
    or: b = ( b value ).
    asInteger = ( 0 ).
| ).


"------------------------------------------------------------------- blocks

 whileTrue: is written so its recursive call is in tail position: the
 interpreter drops the frame, so loops run in constant space."

traits block _AddSlots: ( |
    parent* = traits object.
    isBlock = ( true ).
    printString = ( '<block>' ).

    whileTrue: body = (
        value ifFalse: [ ^nil ].
        body value.
        whileTrue: body ).

    whileFalse: body = (
        value ifTrue: [ ^nil ].
        body value.
        whileFalse: body ).

    whileTrue  = ( whileTrue:  [] ).
    whileFalse = ( whileFalse: [] ).
    loop = ( value. loop ).
| ).


"------------------------------------------------------------------ numbers"

traits _AddSlots: ( | number = () | ).

traits number _AddSlots: ( |
    parent* = traits object.
    isNumber = ( true ).

    + x = ( _IntAdd: x ).
    - x = ( _IntSub: x ).
    * x = ( _IntMul: x ).
    / x = ( _IntDiv: x ).
    % x = ( _IntMod: x ).

    <  x = ( _IntLT: x ).
    <= x = ( _IntLE: x ).
    >  x = ( _IntGT: x ).
    >= x = ( _IntGE: x ).
    =  x = ( x isNumber ifTrue: [ _IntEQ: x ] False: [ false ] ).
    != x = ( (self = x) not ).
    hash = ( _Hash ).

    "no `|` selector: a bare bar always closes a slot list"
    & x = ( _IntAnd: x ).
    bitAnd: x = ( _IntAnd: x ).
    bitOr:  x = ( _IntOr: x ).
    bitXor: x = ( _IntXor: x ).
    << x = ( _IntArithmeticShiftLeft: x ).
    >> x = ( _IntArithmeticShiftRight: x ).

    min: x = ( self < x ifTrue: [ self ] False: [ x ] ).
    max: x = ( self > x ifTrue: [ self ] False: [ x ] ).
    abs = ( self < 0 ifTrue: [ 0 - self ] False: [ self ] ).
    negate = ( 0 - self ).
    isNegative = ( self < 0 ).
    isZero = ( self = 0 ).
    even = ( (self % 2) = 0 ).
    odd = ( even not ).
    squared = ( self * self ).
    between: lo And: hi = ( (self >= lo) && [ self <= hi ] ).

    sqrt = ( _Sqrt ).
    sin = ( _Sin ).
    cos = ( _Cos ).
    exp = ( _Exp ).
    ln = ( _Ln ).
    ** x = ( _Pow: x ).
    floor = ( _Floor _AsInteger ).
    ceiling = ( _Ceiling _AsInteger ).
    truncated = ( _AsInteger ).
    asFloat = ( _AsFloat ).
    asInteger = ( _AsInteger ).
    asString = ( printString ).

    to: hi Do: blk = ( | i |
        i: self.
        [ i <= hi ] whileTrue: [ blk value: i. i: i + 1 ].
        self ).

    to: hi By: st Do: blk = ( | i |
        i: self.
        st > 0
            ifTrue: [ [ i <= hi ] whileTrue: [ blk value: i. i: i + st ] ]
             False: [ [ i >= hi ] whileTrue: [ blk value: i. i: i + st ] ].
        self ).

    timesRepeat: blk = ( | i |
        i: 0.
        [ i < self ] whileTrue: [ blk value. i: i + 1 ].
        self ).
| ).

traits smallInt _AddSlots: ( | parent* = traits number | ).
traits float    _AddSlots: ( | parent* = traits number | ).


"--------------------------------------------------------------- indexables"

traits _AddSlots: ( | indexable = () | ).

traits indexable _AddSlots: ( |
    parent* = traits object.
    size = ( _Size ).
    at: i = ( _At: i ).
    at: i Put: v = ( _At: i Put: v ).
    , x = ( _Concatenate: x ).
    isEmpty = ( size = 0 ).
    first = ( at: 0 ).
    last = ( at: size - 1 ).

    do: blk = ( | i |
        i: 0.
        [ i < size ] whileTrue: [ blk value: (at: i). i: i + 1 ].
        self ).

    do: blk WithIndex: iblk = ( | i |
        i: 0.
        [ i < size ] whileTrue: [ iblk value: (at: i) With: i. i: i + 1 ].
        self ).

    copyFrom: a To: b = ( | r |
        r: (copySize: b - a).
        a to: b - 1 Do: [ |:i| r at: i - a Put: (at: i) ].
        r ).

    copyAddLast: v = ( | r |
        r: (copySize: size + 1).
        0 to: size - 1 Do: [ |:i| r at: i Put: (at: i) ].
        r at: size Put: v.
        r ).

    reverse = ( | r |
        r: (copySize: size).
        0 to: size - 1 Do: [ |:i| r at: size - 1 - i Put: (at: i) ].
        r ).

    detect: blk IfNone: none = (
        do: [ |:e| (blk value: e) ifTrue: [ ^e ] ].
        none value ).

    inject: acc Into: blk = ( | a |
        a: acc.
        do: [ |:e| a: (blk value: a With: e) ].
        a ).

    includes: x = (
        do: [ |:e| (e = x) ifTrue: [ ^true ] ].
        false ).

    count: blk = ( | n |
        n: 0.
        do: [ |:e| (blk value: e) ifTrue: [ n: n + 1 ] ].
        n ).
| ).

traits string _AddSlots: ( |
    parent* = traits indexable.
    isString = ( true ).
    copySize: n = ( _Clone: n Filler: 32 ).
    = x = ( x isString ifTrue: [ (_ByteVectorCompare: x) = 0 ] False: [ false ] ).
    <  x = ( (_ByteVectorCompare: x) <  0 ).
    >  x = ( (_ByteVectorCompare: x) >  0 ).
    <= x = ( (_ByteVectorCompare: x) <= 0 ).
    >= x = ( (_ByteVectorCompare: x) >= 0 ).
    hash = ( _Hash ).
    printString = ( self ).
    asString = ( self ).
| ).

traits byteVector _AddSlots: ( |
    parent* = traits indexable.
    copySize: n = ( _Clone: n Filler: 0 ).
| ).

traits vector _AddSlots: ( |
    parent* = traits indexable.
    isVector = ( true ).
    copySize: n = ( _Clone: n Filler: nil ).

    collect: blk = ( | r |
        r: (copySize: size).
        0 to: size - 1 Do: [ |:i| r at: i Put: (blk value: (at: i)) ].
        r ).

    select: blk = ( | r |
        r: (copySize: 0).
        do: [ |:e| (blk value: e) ifTrue: [ r: (r copyAddLast: e) ] ].
        r ).

    printString = ( | s |
        s: '('.
        do: [ |:e| s: s , e printString , '. ' ].
        s , ')' ).
| ).
