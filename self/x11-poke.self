"Resize another client's window, to check that a Morphic world notices and
 repaints. Core X only -- no XTEST, which XQuartz does not enable.

   DISPLAY=... serf self/x11-poke.self

 Finds the first child of the root that is not 1x1 and resizes it, which the
 server turns into ConfigureNotify and Expose for whoever owns it."

globals _AddSlots: ( | poke = (| parent* = traits object.
  d.
  resize: w To: width By: height = (
    d: (nil _XOpenDisplayxOpenDisplayResultProxy: _ProxyNew).
    d _ProxyIsNull ifTrue: [ ^'FAILED: no display' ].
    d _XResizeWindowxResize: w W: width H: height.
    d _XFlushxFlush.
    nil _Sleep: 1500.
    'resized ' , w printString ).
| ) | ).

(poke resize: 4194407 To: 560 By: 380) printLine.
