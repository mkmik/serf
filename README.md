# serf — a Self VM in Rust

A from-scratch reimplementation of [Self](https://selflanguage.org/), the
prototype-based language and programming environment from Stanford and Sun,
that runs on modern macOS and Linux. It reads and writes the original C++ VM's
snapshot format, so it boots the Self 4.4 worlds as shipped — Morphic desktop
included.

Tested on:

| OS | arch | GUI |
|---|---|---|
| macOS 14 | arm64 | XQuartz |
| Ubuntu 24.04 (CI) | x86_64 | Xvfb |

The VM is portable Rust with no dependencies, so other targets `rustc` supports
should work. It can also draw the world itself, with no X server anywhere and
text from the fonts installed on the host — that part takes three crates and is
off by default; see [INTERNALS.md](INTERNALS.md).

There is no JIT — serf is a bytecode interpreter with per-send inline caches
and a generational collector, so it is far behind the real thing on a
benchmark. It is quick enough to boot the demo world and play with the Morphic
system: open outliners, edit methods, click things.

## Install

```sh
cargo install selflang     # crates.io already had a `serf`; the binary is still `serf`
```

## Run the Self 4.4 demo world

The snapshots are too big to ship in the crate, so fetch one from the repo, and
start XQuartz (macOS) or use your X display (Linux):

```sh
curl -LO https://github.com/mkmik/serf/raw/main/Demo-4.4.snap
serf Demo-4.4.snap
```

The world takes the process over from there: it prints the Self 4.4 banner on
the terminal and opens its desktop on `$DISPLAY`.

![The Self 4.4 demo world, booted by serf](https://raw.githubusercontent.com/mkmik/serf/main/shots/19-demo-4.4.png)

`serf` with no arguments is a REPL for serf's own small world; the other flags —
loading an image without booting it, saving one, the GC and metrics knobs — are
in [INTERNALS.md](INTERNALS.md).

## More

* [INTERNALS.md](INTERNALS.md) — building from source, the CLI, images, maps, the collector, and what it took to run Morphic
* [OPEN.md](OPEN.md) — known problems, reproducible and not fixed
* [MEMORY.md](MEMORY.md) — the design of the heap

## License

Apache 2.0, see [LICENSE](LICENSE). The `*.snap` images are part of the Self
system and keep its own licence, see [LICENSE.self](LICENSE.self).
