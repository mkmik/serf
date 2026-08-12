"Drive a real X server from Self through serf's foreign-call layer.

   Xvfb :99 -screen 0 1024x768x24 &
   DISPLAY=:99 serf self/x11-demo.self

 The window has to be kept alive and repainted: when the process exits the X
 connection closes and the server destroys the window, which is why an earlier
 version of this demo drew and then vanished."

globals _AddSlots: ( | x = (| parent* = traits object.
  d. scr <- 0. root <- 0. win <- 0. gc <- 0.
  open = (
    d: (nil _XOpenDisplayxOpenDisplayResultProxy: _ProxyNew).
    d _ProxyIsNull ifTrue: [ ^'FAILED: no display' ].
    scr: (d _DefaultScreendefaultScreen).
    root: (d _RootWindowrootWindow: scr).
    'display ok, screen ' , scr printString , ' root ' , root printString ).
  makeWindow = ( | black. white |
    black: (d _BlackPixelblackPixel: scr).
    white: (d _WhitePixelwhitePixel: scr).
    win: (d _XCreateSimpleWindowxCreate: root X: 0 Y: 0 W: 300 H: 200 B: 1 Bd: black Bg: white).
    gc: (d _XCreateGCxCreateGC: win Mask: 0 Values: 0 ResultProxy: _ProxyNew).
    d _XMapWindowxMap: win.
    d _XFlushxFlush.
    'window ' , win printString ).
  draw = (
    d _XSetForegroundxSetFg: gc To: (d _BlackPixelblackPixel: scr).
    d _XFillRectanglexFill: win GC: gc X: 20 Y: 20 W: 100 H: 60.
    d _XDrawStringxDrawString: win GC: gc X: 30 Y: 120 S: 'hello from serf' L: 14.
    d _XFlushxFlush.
    self ).
  "repaint for a while, since nothing here handles Expose"
  showFor: seconds = ( (seconds * 4) timesRepeat: [ draw. nil _Sleep: 250 ]. 'drew' ).
| ) | ).

x open printLine.
x makeWindow printLine.
(x showFor: 8) printLine.
