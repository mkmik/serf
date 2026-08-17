mod compile;
mod ffi;
mod gc;
mod glue_table;
mod heap;
mod image;
mod image_obj;
mod interp;
mod lexer;
mod metrics;
mod obj;
mod parser;
mod prims;
mod struct_table;
mod value;

use std::io::{BufRead, Write};

use value::{default_print_string, Value, Vm};

const INIT: &str = include_str!("../self/init.self");

fn eval_source(vm: &mut Vm, src: &[u8], file: &str, echo: bool) -> Result<(), String> {
    for e in parser::parse_program(src)? {
        let m = compile::compile_statement(vm, &e, file)?;
        // once an image is loaded, everything runs in its world
        let lobby = vm.image_roots.as_ref().map_or_else(|| vm.lobby.clone(), |r| r[0].clone());
        let scope = interp::new_scope(m, lobby, lobby, &[], None);
        match interp::run(vm, scope) {
            Ok(v) => {
                if echo {
                    println!("{}", show(vm, &v));
                }
            }
            Err(u) => return Err(interp::describe(vm, u)),
        }
    }
    Ok(())
}

fn eval_in_echo(vm: &mut Vm, src: &[u8], file: &str, me: Value) -> Result<(), String> {
    eval_in(vm, src, file, me)
}

/// Evaluate source with a given object as `self` -- for running an image's
/// own code, where implicit sends must land in the image's world.
fn eval_in(vm: &mut Vm, src: &[u8], file: &str, me: Value) -> Result<(), String> {
    // `me` is this function's only reference to the receiver while `show` runs
    // a send of its own, in whose activations it does not appear
    let n_roots = vm.temp_roots.len();
    vm.temp_roots.push(me);
    let r = eval_in_rooted(vm, src, file, me);
    vm.temp_roots.truncate(n_roots);
    r
}

fn eval_in_rooted(vm: &mut Vm, src: &[u8], file: &str, me: Value) -> Result<(), String> {
    for e in parser::parse_program(src)? {
        let m = compile::compile_statement(vm, &e, file)?;
        let scope = interp::new_scope(m, me.clone(), me.clone(), &[], None);
        match interp::run(vm, scope) {
            Ok(v) => println!("{}", show(vm, &v)),
            Err(u) => return Err(interp::describe(vm, u)),
        }
    }
    Ok(())
}

/// Ask the object to print itself; fall back to the VM printer.
fn show(vm: &mut Vm, v: &Value) -> String {
    // `v` is a Rust local across a send, so the collector needs to be told
    let n_roots = vm.temp_roots.len();
    vm.temp_roots.push(*v);
    let printed = interp::send(vm, v.clone(), "printString", vec![]);
    vm.temp_roots.truncate(n_roots);
    if let Ok(s) = printed {
        if let Some(t) = s.as_str() {
            return t;
        }
    }
    default_print_string(vm, v)
}

fn repl(vm: &mut Vm) {
    println!("serf -- a Self VM in Rust. Ctrl-D to leave.");
    let stdin = std::io::stdin();
    let mut pending = String::new();
    loop {
        print!("{}", if pending.is_empty() { "> " } else { "  " });
        std::io::stdout().flush().ok();
        let mut line = String::new();
        loop {
            match stdin.lock().read_line(&mut line) {
                Ok(0) => {
                    println!();
                    return;
                }
                Ok(_) => break,
                // the world puts its files in non-blocking mode
                // (os_file startAsync), and that includes stdin
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => {
                    eprintln!("{}", e);
                    return;
                }
            }
        }
        pending.push_str(&line);
        if pending.trim().is_empty() {
            pending.clear();
            continue;
        }
        // keep reading while the text is merely unfinished
        if let Err(e) = parser::parse_program(pending.as_bytes()) {
            if e.contains("unexpected end of input")
                || e.contains("unterminated")
                || e.contains("Eof")
            {
                continue;
            }
            eprintln!("{}", e);
            pending.clear();
            continue;
        }
        // once a snapshot is booted the prompt belongs to that world
        let me = vm.image_roots.as_ref().map_or_else(|| vm.lobby.clone(), |r| r[0].clone());
        if let Err(e) = eval_in_echo(vm, pending.as_bytes(), "<stdin>", me) {
            eprintln!("{}", e);
        }
        pending.clear();
    }
}

/// Read a C++-format snapshot and bind its lobby as `snapshotLobby` in globals.
fn load_image(vm: &mut Vm, path: &str) -> Result<usize, String> {
    // The whole image graph hangs off the loader's own tables until it is
    // installed in the Vm, and objects are published half-built, so nothing may
    // collect until the load is done.
    let _g = gc::NoGc::new();
    let snap = image::Snapshot::read(std::path::Path::new(path))?;
    // The header's Timestamp is the world's programming timestamp, so a loaded
    // world carries on where it left off. Starting from 0 instead leaves every
    // cache that stamps itself looking current when it is empty.
    vm.timestamp = snap.timestamp as i64;
    let mut ld = image_obj::Loader::new(&snap, vm);
    let lobby = ld.value(snap.vm_oops[0])?;
    let t = ld.value(snap.vm_oops[2])?;
    let f = ld.value(snap.vm_oops[3])?;
    let mut roots = Vec::with_capacity(snap.vm_oops.len());
    for &o in &snap.vm_oops {
        roots.push(ld.value(o)?);
    }
    // Everything serf hands to the image -- error strings, vectors -- must
    // inherit from the image's traits, or it dispatches back into serf's own
    // world. Immediates take theirs from smi_map/float_map; the rest from the
    // parent of the matching prototype root.
    if let Some(t) = ld.immediate_traits(snap.vm_maps[0])? {
        vm.t_smallint = t;
    }
    if let Some(t) = ld.immediate_traits(snap.vm_maps[1])? {
        vm.t_float = t;
    }
    for (root, dest) in [(4usize, 0usize), (6, 1), (7, 2), (8, 3)] {
        let proto = ld.value(snap.vm_oops[root])?;
        let parent = proto.as_obj().and_then(|o| {
            o.borrow().slots.iter().find(|s| s.kind == value::SlotKind::Parent).map(|s| s.value)
        });
        if let Some(p) = parent {
            match dest {
                0 => vm.t_string = p,
                1 => vm.t_vector = p,
                2 => vm.t_bytevector = p,
                _ => vm.t_block = p,
            }
        }
    }
    let n = ld.count();
    vm.image_true = Some(t);
    vm.image_false = Some(f);
    let mut vstr = Vec::with_capacity(snap.vm_strings.len());
    for &o in &snap.vm_strings {
        vstr.push(ld.value(o)?);
    }
    // the image's canonical strings, so anything serf hands back is the
    // world's own object and compares equal to its literals
    let mut canon = std::collections::HashMap::new();
    for bucket in &snap.string_table {
        for &a in bucket {
            if let Ok(v) = ld.value(a) {
                if let Some(b) = v.bytes() {
                    canon.insert(b, v);
                }
            }
        }
    }
    vm.canonical = canon;
    vm.image_roots = Some(roots);
    vm.image_strings = Some(vstr);

    // a load hands back young objects, so every annotation starts on the
    // barrier list; the first few scavenges prune it to nothing as they
    // promote them
    // each of these widens, which means each builds a new object and switches
    // every pointer to it -- so re-read the target each time round
    for which in 0..2 {
        let target = if which == 0 { vm.globals } else { lobby };
        vm.put_slot(target, value::slot("snapshotLobby", value::SlotKind::Data, lobby));
    }
    Ok(n)
}

/// Read a snapshot and re-serialise it: the bytes must come back identical,
/// which is the check that the format layer is an exact inverse of itself.
fn verify_image(path: &str) -> Result<String, String> {
    let p = std::path::Path::new(path);
    let orig = std::fs::read(p).map_err(|e| e.to_string())?;
    let snap = image::Snapshot::read(p)?;
    // a page-aligned image is padded to 8K boundaries and carries no section
    // delimiters; serf always writes the compact form, so only compare bytes
    // when the source was compact too. Compression is framing, so compare the
    // binary sections with both sides decompressed.
    let (was, again) = (image::tail(&orig)?, snap.binary_section()?);
    if !snap.page_aligned && was != again {
        let at = was.iter().zip(&again).position(|(a, b)| a != b);
        return Err(format!(
            "re-serialised image differs (len {} vs {}, first difference at {:?})",
            was.len(),
            again.len(),
            at
        ));
    }
    let heap = image_obj::Heap::new(&snap);
    let mut objs = 0;
    for s in snap.new_gen.iter().chain(snap.old.iter()) {
        objs += heap.walk(s)?;
    }
    Ok(format!(
        "{}: {} bytes, {} objects tile {} old space(s) exactly; \
         {} canonical strings; version {}, code {}, {}{}",
        path,
        if snap.page_aligned {
            format!("{} (page aligned, byte compare skipped)", orig.len())
        } else if snap.compressed {
            format!("{} compressed, {} round-trip identical", orig.len(), was.len())
        } else {
            format!("{} round-trip identical", orig.len())
        },
        objs,
        snap.old.len(),
        snap.string_table.iter().map(|b| b.len()).sum::<usize>(),
        snap.version,
        if snap.snapshot_code { "yes" } else { "no" },
        if snap.page_aligned { "page aligned" } else { "not page aligned" },
        if snap.was_swapped { ", byte-swapped source" } else { "" }
    ))
}

fn dump_image(path: &str) -> Result<(), String> {
    let snap = image::Snapshot::read(std::path::Path::new(path))?;
    println!(
        "version {}.{}.{} timestamp {} code {} page_aligned {} swapped {}",
        snap.major,
        snap.minor,
        snap.version,
        snap.timestamp,
        snap.snapshot_code,
        snap.page_aligned,
        snap.was_swapped
    );
    println!("sizes {:?}", snap.sizes);
    for (n, s) in snap.new_gen.iter().chain(snap.old.iter()).enumerate() {
        println!(
            "space {}: objs {:#x}..{:#x} ({} words)  bytes {:#x}..{:#x} ({} bytes)",
            n,
            s.objs_bottom,
            s.objs_top,
            s.objs.len(),
            s.bytes_bottom,
            s.bytes_top,
            s.bytes.len()
        );
    }
    println!("vm_maps {:x?}", snap.vm_maps);
    println!("vtbls {:x?}", snap.vtbls);
    for (i, o) in snap.vm_oops.iter().enumerate().take(6) {
        println!("  root {} {} = {:#x}", i, image::VM_OOP_NAMES[i], o);
    }
    let heap = image_obj::Heap::new(&snap);
    let mut hist: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut annotated = 0usize;
    for (n, s) in snap.new_gen.iter().chain(snap.old.iter()).enumerate() {
        if s.objs.is_empty() {
            continue;
        }
        let mut a = s.objs_bottom;
        let mut count = 0usize;
        let mut recent: Vec<String> = vec![];
        while a < s.objs_top {
            let mark = match heap.word(a) {
                Ok(w) => w,
                Err(e) => {
                    println!("space {}: {}", n, e);
                    break;
                }
            };
            let info = heap.map_star_of(a).and_then(|m| heap.read_map(m));
            let (kindname, size) = match &info {
                Ok(m) => (m.kind.name().to_string(), heap.object_size(a, m)),
                Err(e) => (format!("<{}>", e), Ok(0)),
            };
            let size = match size {
                Ok(s) if s >= 2 => s,
                _ => {
                    for r in &recent {
                        println!("  ...{}", r);
                    }
                    println!(
                        "space {} object {} at {:#x}: bad size, kind {}",
                        n, count, a, kindname
                    );
                    let start = a - 64;
                    for k in 0..32u32 {
                        let w = heap.word(start + 4 * k).unwrap_or(0);
                        println!(
                            "    {:#x}: {:#010x} tag {} smi {}",
                            start + 4 * k,
                            w,
                            w & 3,
                            image_obj::smi(w)
                        );
                    }
                    break;
                }
            };
            let line = format!(
                "space {} obj {:5} at {:#x} mark {:#x} kind {:<14} size {}",
                n, count, a, mark, kindname, size
            );
            recent.push(line);
            if recent.len() > 8 {
                recent.remove(0);
            }
            if mark & 3 != 3 {
                for r in &recent {
                    println!("  ...{}", r);
                }
            }
            if mark & 3 != 3 || count < 12 || info.is_err() {
                println!(
                    "space {} obj {:5} at {:#x} mark {:#x} kind {:<14} size {}",
                    n, count, a, mark, kindname, size
                );
            }
            if mark & 3 != 3 {
                break;
            }
            if let Ok(m) = &info {
                *hist.entry(m.kind.name().to_string()).or_insert(0) += 1;
                if m.annotation & 3 == 1 {
                    annotated += 1;
                }
                if m.slots.iter().any(|sd| sd.anno & 3 == 1) {
                    annotated += 1;
                }
            }
            a += 4 * size as u32;
            count += 1;
        }
        println!(
            "space {}: walked {} objects, stopped at {:#x} (top {:#x})",
            n, count, a, s.objs_top
        );
    }
    println!("map kinds: {:?}", hist);
    println!("maps with a non-nil annotation (object or slot): {}", annotated);
    Ok(())
}

/// A summary of everything reachable from a loaded image, for comparing an
/// original against its round trip.
fn world_stats(vm: &Vm) -> String {
    use std::collections::HashSet;
    let root = match &vm.image_roots {
        Some(r) => r.clone(),
        None => vec![vm.lobby.clone()],
    };
    let (mut objs, mut slots, mut parents, mut assigns) = (0usize, 0usize, 0usize, 0usize);
    let (mut strs, mut strbytes, mut vecs, mut vecelems) = (0usize, 0usize, 0usize, 0usize);
    let (mut meths, mut code, mut lits, mut blocks) = (0usize, 0usize, 0usize, 0usize);
    let (mut ints, mut floats, mut annos) = (0i64, 0usize, 0usize);
    let mut bykind: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut seen: HashSet<usize> = HashSet::new();
    let mut shapes: HashSet<value::MapRef> = HashSet::new();
    let mut work: Vec<value::Value> = root;
    while let Some(v) = work.pop() {
        match &v {
            value::Value::Int(i) => {
                ints = ints.wrapping_add(*i);
                continue;
            }
            value::Value::Float(f) => {
                floats += 1;
                ints = ints.wrapping_add(f.to_bits() as i64);
                continue;
            }
            value::Value::Obj(o) => {
                if !seen.insert(o.id()) {
                    continue;
                }
            }
        }
        objs += 1;
        let at = v.as_obj().unwrap();
        if value::obj_anno(at).is_some() {
            annos += 1;
        }
        let o = at.borrow();
        shapes.insert(o.map());
        for sl in &o.slots {
            slots += 1;
            match sl.kind {
                value::SlotKind::Parent => parents += 1,
                value::SlotKind::Assign => assigns += 1,
                _ => {}
            }
            if value::slot_anno(at, slots - 1).is_some() {
                annos += 1;
            }
            if sl.kind == value::SlotKind::Parent {
                let k = match o.payload.kind() {
                    value::PayKind::Bytes => "bytes",
                    value::PayKind::Vector => "vector",
                    value::PayKind::Method => "method",
                    value::PayKind::Block => "block",
                    value::PayKind::Mirror => "mirror",
                    value::PayKind::Proxy => "proxy",
                    _ => "plain",
                };
                *bykind.entry(k).or_insert(0usize) += 1;
            }
            work.push(sl.value);
        }
        match o.payload.kind() {
            value::PayKind::Bytes => {
                strs += 1;
                strbytes += o.payload.byte_len()
            }
            value::PayKind::Vector => {
                let x = o.payload.vector().unwrap();
                vecs += 1;
                vecelems += x.len();
                work.extend(x);
            }
            value::PayKind::Method => {
                let m = o.payload.method().unwrap();
                meths += 1;
                code += m.code.len();
                lits += m.lits.borrow().len();
                work.extend(m.lits.borrow().iter().cloned());
                work.extend(m.slot_inits.borrow().iter().cloned());
            }
            value::PayKind::Block => {
                let m = o.payload.method().unwrap();
                blocks += 1;
                work.extend(m.lits.borrow().iter().cloned());
                work.extend(m.slot_inits.borrow().iter().cloned());
            }
            value::PayKind::Mirror => work.push(o.payload.mirror().unwrap()),
            _ => {}
        }
    }
    format!(
        "objects {} slots {} (parent {} assign {}) annotations {}\n\
         byte objects {} ({} bytes)  vectors {} ({} elements)\n\
         methods {} ({} bytecodes, {} literals)  blocks {}  floats {}  int-checksum {}\n\
         parents by payload {:?}\n\
         maps {} (one per {:.1} objects)\n\
         heap {} objects (young {} old {})",
        objs,
        slots,
        parents,
        assigns,
        annos,
        strs,
        strbytes,
        vecs,
        vecelems,
        meths,
        code,
        lits,
        blocks,
        floats,
        ints,
        bykind,
        shapes.len(),
        objs as f64 / shapes.len().max(1) as f64,
        (gc::young_used() + gc::old_used()),
        gc::young_used(),
        gc::old_used()
    )
}

/// What the C++ VM does after reading a snapshot: evaluate
/// `snapshotAction postRead`. Trouble is reported, not fatal.
fn boot(vm: &mut Vm) {
    let lobby = match vm.image_roots.as_ref() {
        Some(r) => r[0].clone(),
        None => return,
    };
    if let Err(e) = eval_in(vm, b"snapshotAction postRead", "<postRead>", lobby) {
        eprintln!("snapshotAction postRead: {}", e);
    }
}

/// Where fetched images live: `$SERF_CACHE`, else the usual cache directory.
fn cache_dir() -> Result<std::path::PathBuf, String> {
    let var = |k| std::env::var_os(k).filter(|v| !v.is_empty()).map(std::path::PathBuf::from);
    var("SERF_CACHE")
        .or_else(|| var("XDG_CACHE_HOME").map(|d| d.join("serf")))
        .or_else(|| var("HOME").map(|d| d.join(".cache/serf")))
        .ok_or_else(|| "no HOME to keep a cache in; set SERF_CACHE".to_string())
}

/// A URL is a fine name for an image: fetch it into the cache directory and
/// hand back the local path. Later runs revalidate rather than download again
/// -- If-Modified-Since and If-None-Match, which is curl's job, not ours.
///
/// ponytail: curl rather than an HTTP client of our own. The crate has no
/// dependencies and TLS is not something to write for a convenience.
fn fetch(url: &str) -> Result<std::path::PathBuf, String> {
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    // the whole URL is the key, punched into one filesystem-safe name
    let key: String = url
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    let (file, etag, part) =
        (dir.join(&key), dir.join(format!("{}.etag", key)), dir.join(format!("{}.part", key)));
    // a partial download from a run that died would otherwise look like a body
    let _ = std::fs::remove_file(&part);
    let mut curl = std::process::Command::new("curl");
    // -R keeps the server's mtime, which is what -z then asks the next run about
    curl.arg("-fsSLR").arg("-o").arg(&part);
    if file.exists() {
        curl.arg("-z").arg(&file);
        // and If-None-Match, but only with an etag to match: a server that sends
        // none leaves the file empty, and an empty If-None-Match still takes
        // precedence over the date -- and then matches nothing
        if std::fs::metadata(&etag).is_ok_and(|m| m.len() > 0) {
            curl.arg("--etag-compare").arg(&etag);
        }
    }
    curl.arg("--etag-save").arg(&etag).arg("--").arg(url);
    let ok = curl.status().map_err(|e| format!("cannot run curl: {}", e))?.success();
    match (ok, part.exists()) {
        // 200: the new body replaces the old one, whole or not at all
        (true, true) => std::fs::rename(&part, &file).map_err(|e| format!("{}: {}", key, e))?,
        (true, false) => {} // 304: no body sent, the copy on disk stands
        (false, _) if file.exists() => {
            eprintln!("serf: {} unreachable, using the cached copy", url)
        }
        (false, _) => return Err(format!("cannot fetch {}", url)),
    }
    Ok(file)
}

fn main() {
    // A port the OS picks, so any number of VMs can run at once; it goes to
    // stderr, where the rest of the VM's own chatter goes. SERF_METRICS=off
    // for somewhere that cannot or should not listen.
    if std::env::var("SERF_METRICS").as_deref() != Ok("off") {
        match metrics::serve() {
            Ok(p) => eprintln!("serf: metrics on http://127.0.0.1:{}/metrics", p),
            Err(e) => eprintln!("serf: no metrics server: {}", e),
        }
    }

    let mut vm = Vm::new();
    if let Err(e) = eval_source(&mut vm, INIT.as_bytes(), "init.self", false) {
        eprintln!("bootstrap failed: {}", e);
        std::process::exit(1);
    }

    // any argument naming a file may name a URL instead: fetch it first, so
    // everything downstream -- --load, --dump-image, a bare snapshot -- sees a
    // path and needs to know nothing about where it came from
    let args: Vec<String> = std::env::args()
        .skip(1)
        .map(|a| {
            if !a.starts_with("http://") && !a.starts_with("https://") {
                return a;
            }
            match fetch(&a) {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(1)
                }
            }
        })
        .collect();
    let mut interactive = args.is_empty();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-i" => interactive = true,
            "--save" => {
                i += 1;
                let f = args.get(i).cloned().unwrap_or_default();
                match image_obj::build(&vm).and_then(|s| s.write(std::path::Path::new(&f))) {
                    Ok(()) => eprintln!("wrote {}", f),
                    Err(e) => {
                        eprintln!("save failed: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            // rewrite any snapshot in serf's own form -- compact and gzipped --
            // without booting it, so no unreachable object is lost on the way
            "--recompress" => {
                let src = args.get(i + 1).cloned().unwrap_or_default();
                let dst = args.get(i + 2).cloned().unwrap_or_default();
                i += 2;
                let go = image::Snapshot::read(std::path::Path::new(&src))
                    .and_then(|s| s.write(std::path::Path::new(&dst)));
                match go {
                    Ok(()) => eprintln!("wrote {}", dst),
                    Err(e) => {
                        eprintln!("recompress failed: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            "--dump-image" => {
                i += 1;
                let f = args.get(i).cloned().unwrap_or_default();
                if let Err(e) = dump_image(&f) {
                    eprintln!("dump failed: {}", e);
                    std::process::exit(1);
                }
            }
            "--stats" => {
                println!("{}", world_stats(&vm));
            }
            "--verify-image" => {
                i += 1;
                let f = args.get(i).cloned().unwrap_or_default();
                match verify_image(&f) {
                    Ok(m) => println!("{}", m),
                    Err(e) => {
                        eprintln!("verify failed: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            "--prims" => {
                let mut n: std::collections::BTreeMap<String, usize> = Default::default();
                let mut seen = std::collections::HashSet::new();
                let mut work: Vec<value::Value> = vm.image_roots.clone().unwrap_or_default();
                while let Some(v) = work.pop() {
                    let o = match v.as_obj() {
                        Some(o) => o.clone(),
                        None => continue,
                    };
                    if !seen.insert(o.id()) {
                        continue;
                    }
                    let b = o.borrow();
                    for sl in &b.slots {
                        work.push(sl.value)
                    }
                    match b.payload.kind() {
                        value::PayKind::Vector => work.extend(b.payload.vector().unwrap()),
                        value::PayKind::Method | value::PayKind::Block => {
                            let m = b.payload.method().unwrap();
                            for (k, l) in m.lit_strs.iter().enumerate() {
                                if let Some(t) = l {
                                    if t.starts_with('_') {
                                        *n.entry(t.to_string()).or_insert(0) += 1
                                    }
                                }
                                let _ = k;
                            }
                            work.extend(m.lits.borrow().iter().cloned());
                            work.extend(m.slot_inits.borrow().iter().cloned());
                        }
                        _ => {}
                    }
                }
                let mut v: Vec<_> = n.into_iter().collect();
                v.sort_by(|a, b| b.1.cmp(&a.1));
                println!("{} distinct primitives", v.len());
                for (k, c) in &v {
                    println!("{:6} {}", c, k)
                }
            }
            "--run" => {
                i += 1;
                let src = args.get(i).cloned().unwrap_or_default();
                let lobby = match &vm.image_roots {
                    Some(r) => r[0].clone(),
                    None => {
                        eprintln!("--run needs an image; use --load first");
                        std::process::exit(1)
                    }
                };
                if let Err(e) = eval_in(&mut vm, src.as_bytes(), "<--run>", lobby) {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            }
            "--load" => {
                i += 1;
                let f = args.get(i).cloned().unwrap_or_default();
                match load_image(&mut vm, &f) {
                    Ok(n) => eprintln!("loaded {} ({} objects reachable from the lobby)", f, n),
                    Err(e) => {
                        eprintln!("load failed: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            "-e" => {
                i += 1;
                let src = args.get(i).cloned().unwrap_or_default();
                if let Err(e) = eval_source(&mut vm, src.as_bytes(), "<-e>", true) {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            }
            f => match std::fs::read(f) {
                Ok(src) => {
                    // a snapshot is not Self source: recognise one and boot it,
                    // the way the C++ VM does with `Self -s snapshot`
                    if src.starts_with(b"exec Self") {
                        match load_image(&mut vm, f) {
                            Ok(n) => {
                                eprintln!("loaded {} ({} objects reachable from the lobby)", f, n)
                            }
                            Err(e) => {
                                eprintln!("load failed: {}", e);
                                std::process::exit(1)
                            }
                        }
                        boot(&mut vm);
                        // booting a snapshot lands you at a prompt, unless the
                        // command line already says what to do
                        interactive = !args.iter().any(|a| a == "-e");
                    } else if let Err(e) = eval_source(&mut vm, &src, f, false) {
                        eprintln!("{}", e);
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("{}: {}", f, e);
                    std::process::exit(1);
                }
            },
        }
        i += 1;
    }
    metrics::trace_mem("exit");
    if interactive {
        repl(&mut vm);
    }
}
