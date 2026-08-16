//! Self snapshot ("image") files, byte-compatible with the C++ VM.
//!
//! Section order and encodings come from `vm/src/any/memory/universe.cpp`
//! (`write_snapshot`/`read_snapshot`), `space.cpp` (space bodies),
//! `generation.cpp`, `stringTable.cpp`, `vmStrings.cpp` and `mapVtbls.cpp`.
//!
//! A snapshot is a raw dump of the 32-bit object heap: addresses are stored as
//! written, and the reader relocates if the heap lands somewhere else. This
//! module keeps every word exactly as it appeared on disk; interpreting them
//! as objects is `image_obj.rs`.
//!
//! ponytail: compressed snapshots are piped through the filter the header names,
//! the way the C++ VM does, instead of linking a gzip crate -- and out through
//! `gzip` again on write, so what serf saves is a quarter of the size. Code zones are
//! still refused: `Snapshot code: y` images hold i386 machine code that is
//! meaningless here (the C++ VM regenerates it anyway).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

pub const HEADER_LINE: &str = "exec Self -s $0   $@\n";
pub const BINARY_MARKER: &str = "The rest of this file is binary data.\n\x0c\n";
pub const SNAPSHOT_VERSION: i32 = 13;
pub const VM_MAJOR: i32 = 2023;
pub const VM_MINOR: i32 = 1;
pub const STRING_TABLE_SIZE: usize = 20011;
pub const NUM_VM_STRINGS: usize = 182;
pub const IDEALIZED_PAGE: u32 = 8192;
/// what serf writes, and what the shipped snapshots already name
pub const COMPRESSION_FILTER: &str = "gzip";
pub const DECOMPRESSION_FILTER: &str = "zcat";

/// `APPLY_TO_VM_OOPS`, in order.
pub const VM_OOP_NAMES: [&str; 40] = [
    "lobbyObj", "nilObj", "trueObj", "falseObj", "stringObj", "assignmentObj",
    "objVectorObj", "byteVectorObj", "blockTraitsObj", "deadBlockObj", "processObj",
    "profilerObj", "outerActivationObj", "blockActivationObj", "proxyObj", "fctProxyObj",
    "literalsObj", "slotAnnotationObj", "objectAnnotationObj", "errorObj",
    "assignmentMirrorObj", "blockMirrorObj", "byteVectorMirrorObj", "outerMethodMirrorObj",
    "blockMethodMirrorObj", "floatMirrorObj", "objVectorMirrorObj", "slotsMirrorObj",
    "smiMirrorObj", "stringMirrorObj", "processMirrorObj", "outerActivationMirrorObj",
    "blockActivationMirrorObj", "proxyMirrorObj", "fctProxyMirrorObj", "profilerMirrorObj",
    "mirrorMirrorObj", "objectIDArray", "BugHuntNames", "CompileWithSICNames",
];

/// `FOR_ALL_MAP_TYPES`, in order: the index into the snapshot's vtbl table.
pub const MAP_TYPE_NAMES: [&str; 20] = [
    "slotsMap", "slotsMapDeps", "smiMap", "floatMap", "stringMap", "blockMap",
    "outerMethodMap", "blockMethodMap", "byteVectorMap", "objVectorMap", "mapMap",
    "markMap", "proxyMap", "fctProxyMap", "mirrorMap", "ovframeMap", "bvframeMap",
    "processMap", "profilerMap", "assignmentMap",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum MapType {
    Slots = 0, SlotsDeps, Smi, Float, StringM, Block, OuterMethod, BlockMethod,
    ByteVector, ObjVector, MapM, Mark, Proxy, FctProxy, Mirror, Ovframe, Bvframe,
    Process, Profiler, Assignment,
}

impl MapType {
    pub fn from_index(i: usize) -> Option<MapType> {
        use MapType::*;
        const ALL: [MapType; 20] = [
            Slots, SlotsDeps, Smi, Float, StringM, Block, OuterMethod, BlockMethod,
            ByteVector, ObjVector, MapM, Mark, Proxy, FctProxy, Mirror, Ovframe,
            Bvframe, Process, Profiler, Assignment,
        ];
        ALL.get(i).copied()
    }
    pub fn name(self) -> &'static str {
        MAP_TYPE_NAMES[self as usize]
    }
}

pub const SPACE_SIZE_NAMES: [&str; 7] = [
    "eden_size", "surv_size", "old_size", "code_size", "pic_size", "deps_size", "debug_size",
];

fn delim_bytes(name: &str) -> String {
    format!("\n\x0c\n{}\n\x0c\n!", name)
}

/// The 182 canonical strings the VM keeps direct handles on, in `VMString[]` order.
pub const VM_STRINGS: [&str; NUM_VM_STRINGS] = [
    "primitiveNotDefinedError", "primitiveFailedError", "badTypeError", "badTypeSealError",
    "divisionByZeroError", "overflowError", "badSignError", "alignmentError", "badIndexError",
    "badSizeError", "reflectTypeError", "outOfMemoryError", "stackOverflowError",
    "slotNameError", "slotNameError", "argumentCountError", "parentError",
    "unassignableSlotError", "lonelyAssignmentSlotError", "parallelTWAINSError",
    "noProcessError", "noActivationError", "noReceiverError", "noParentError", "noSenderError",
    "deadProxyError", "liveProxyError", "wrongNoOfArgsError", "nullPointerError",
    "nullCharError", "noDynamicLinkerError", "enumerationTargetError", "noProfilingInfoError",
    "badBranchError", "parent", "codes", "literals", "file", "line", "source", "methodPointer",
    "self", "lexical parent", "value", "value:", "value:With:", "value:With:With:",
    "value:With:With:With:", "_BlockClone", "reflectee", "<top level expr>",
    "primitiveFailedError:Name:", "+", "-", "*", "/", "%", "<", "<=", "=", "!=", ">=", ">",
    "||", "&&", "^^", "min:", "max:", "not", "successor", "succ", "predecessor", "pred",
    "absoluteValue", "inverse", "negate", "complement", "do:", "to:Do:", "to:By:Do:",
    "to:ByPositive:Do:", "to:ByNegative:Do:", "upTo:Do:", "upTo:By:Do:", "downTo:Do:",
    "downTo:By:Do:", "compare:IfLess:Equal:Greater:", "asFloat", "whileTrue:", "whileFalse:",
    "untilTrue:", "untilFalse:", "ifTrue:", "ifFalse:", "ifTrue:False:", "ifFalse:True:",
    "at:", "at:Put:", "size", "_Clone", "_Clone:Filler:", "_Clone0", "_Clone1", "_Clone2",
    "_Clone3", "_Clone4", "_Clone5", "_Clone6", "_Clone7", "_Clone8", "_Clone9", "_IntEQ:",
    "_IntNE:", "_IntLT:", "_IntLE:", "_IntGE:", "_IntGT:", "_IntAdd:", "_IntSub:", "_IntMul:",
    "_IntDiv:", "_IntMod:", "_IntAnd:", "_IntOr:", "_IntXor:", "_IntArithmeticShiftLeft:",
    "_IntLogicalShiftLeft:", "_IntArithmeticShiftRight:", "_IntLogicalShiftRight:", "_Eq:",
    "_At:", "_At:Put:", "_Size", "_ByteAt:", "_ByteAt:Put:", "_ByteSize", "_Restart",
    "_RestartIfFail:", "__DefineLabel:Before:", "__DefineLabel:After:", "<not a string>",
    "undefinedSelector:Receiver:Type:Delegatee:MethodHolder:Arguments:",
    "ambiguousSelector:Receiver:Type:Delegatee:MethodHolder:Arguments:",
    "missingParentSelector:Receiver:Type:Delegatee:MethodHolder:Arguments:",
    "mismatchedArgumentCountSelector:Receiver:Type:Delegatee:MethodHolder:Arguments:",
    "performTypeErrorSelector:Receiver:Type:Delegatee:MethodHolder:Arguments:", "normal",
    "implicitSelf", "undirectedResend", "directedResend", "delegated", "terminated", "aborted",
    "stackOverflow", "_InterruptCheck", "nonLifoBlock", "singleStepped", "finishedActivation",
    "yielded", "signal", "lowOnSpace", "couldntAllocateStack", "sigint", "sigquit", "sigio",
    "siguser1", "siguser2", "sigpipe", "sigterm", "sigurg", "sigchild", "sighup", "sigwinch",
    "sigrealtimer", "sigcputimer", "sigunknown", "nic", "sic", "none", "version",
    "pointerDescriptors", "asSmallInteger",
];

#[derive(Clone, Default)]
pub struct Space {
    pub objs_bottom: u32,
    pub objs_top: u32,
    pub bytes_bottom: u32,
    pub bytes_top: u32,
    /// one entry per word between objs_bottom and objs_top, as stored
    pub objs: Vec<u32>,
    pub bytes: Vec<u8>,
}

impl Space {
    pub fn objs_byte_len(&self) -> u32 {
        self.objs_top.wrapping_sub(self.objs_bottom)
    }
    pub fn bytes_len(&self) -> u32 {
        self.bytes_top.wrapping_sub(self.bytes_bottom)
    }
}

pub struct Snapshot {
    pub major: i32,
    pub minor: i32,
    pub version: i32,
    pub timestamp: i32,
    pub snapshot_code: bool,
    pub vm_date: String,
    pub sizes: [i32; 7],
    pub page_aligned: bool,
    /// true if the binary section came through a decompression filter
    pub compressed: bool,
    pub maps_canonical: bool,
    pub vm_oops: Vec<u32>,
    pub tenuring_threshold: u32,
    pub vm_maps: Vec<u32>,
    /// eden, from, to
    pub new_gen: Vec<Space>,
    pub old: Vec<Space>,
    pub string_table: Vec<Vec<u32>>,
    pub vm_strings: Vec<u32>,
    pub vtbls: Vec<u32>,
    /// true if the file was written by a big-endian VM (sparc/ppc)
    pub was_swapped: bool,
}

#[cfg(test)]
mod tests {
    /// the shipped snapshots are gzipped, so this fails if the filter pipe breaks
    #[test]
    fn reads_a_compressed_snapshot() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Clean-4.4.snap");
        let s = super::Snapshot::read(&p).unwrap();
        assert_eq!(s.vm_oops.len(), super::VM_OOP_NAMES.len());
        assert!(s.old.iter().any(|s| !s.objs.is_empty()));
    }

    /// and what the writer gzips must come back out of the reader unchanged
    #[test]
    fn writes_a_compressed_snapshot() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Clean-4.4.snap");
        let s = super::Snapshot::read(&p).unwrap();
        let out = std::env::temp_dir().join(format!("serf-write-{}.snap", std::process::id()));
        s.write(&out).unwrap();
        let back = super::Snapshot::read(&out).unwrap();
        std::fs::remove_file(&out).unwrap();
        assert!(back.compressed && !back.page_aligned);
        assert_eq!(back.binary_section().unwrap(), s.binary_section().unwrap());
    }
}

// ------------------------------------------------------------------- reading

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
    swap: bool,
}

impl<'a> Rd<'a> {
    fn need(&self, n: usize) -> Result<(), String> {
        if self.i + n > self.b.len() {
            Err(format!("snapshot truncated at offset {}", self.i))
        } else {
            Ok(())
        }
    }
    fn u32(&mut self) -> Result<u32, String> {
        self.need(4)?;
        let v = u32::from_le_bytes([self.b[self.i], self.b[self.i + 1], self.b[self.i + 2], self.b[self.i + 3]]);
        self.i += 4;
        Ok(if self.swap { v.swap_bytes() } else { v })
    }
    fn u8(&mut self) -> Result<u8, String> {
        self.need(1)?;
        self.i += 1;
        Ok(self.b[self.i - 1])
    }
    fn raw(&mut self, n: usize) -> Result<&'a [u8], String> {
        self.need(n)?;
        self.i += n;
        Ok(&self.b[self.i - n..self.i])
    }
    fn delim(&mut self, name: &str, page_aligned: bool) -> Result<(), String> {
        if page_aligned {
            return Ok(()); // check_delim is a no-op for page-aligned snapshots
        }
        let want = delim_bytes(name);
        let n = want.len();
        self.need(n)?;
        if &self.b[self.i..self.i + n] != want.as_bytes() {
            return Err(format!("snapshot corrupt: expected the '{}' delimiter at offset {}", name, self.i));
        }
        self.i += n;
        Ok(())
    }
    fn align_page(&mut self) {
        let p = IDEALIZED_PAGE as usize;
        self.i = (self.i + p - 1) / p * p;
    }
}

/// Run the snapshot's own decompression filter over the binary tail.
///
/// ponytail: only the handful of decompressors people actually put in a
/// snapshot are allowed -- the name comes out of the file, and a file should
/// not get to pick which program we exec.
fn decompress(body: &[u8], filter: &str) -> Result<Vec<u8>, String> {
    const KNOWN: [&str; 8] =
        ["zcat", "gzcat", "gunzip", "bzcat", "bunzip2", "xzcat", "unxz", "zstdcat"];
    if !KNOWN.contains(&filter) {
        return Err(format!(
            "snapshot asks for the decompression filter {:?}, which is not one of {:?}; \
             decompress it youserf",
            filter, KNOWN
        ));
    }
    pipe_through(filter, body)
}

/// Feed the binary tail through an external filter and collect what comes back.
fn pipe_through(filter: &str, body: &[u8]) -> Result<Vec<u8>, String> {
    let mut child = Command::new(filter)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run the filter {:?}: {}", filter, e))?;
    let mut sink = child.stdin.take().expect("stdin was piped");
    // the tail is megabytes and the pipe holds a few pages, so feed the filter
    // while its output is being read, or both ends block forever
    let out = std::thread::scope(|s| {
        s.spawn(move || sink.write_all(body)); // a filter that dies early breaks the pipe; its exit status tells us
        child.wait_with_output()
    })
    .map_err(|e| format!("{}: {}", filter, e))?;
    if !out.status.success() {
        return Err(format!("{} failed on the snapshot's binary section: {}", filter, out.status));
    }
    Ok(out.stdout)
}

/// The binary section, decompressed if the header named a filter: the bytes the
/// reader really parses. Two snapshots hold the same world when these match --
/// the header text and the compression around them are only framing.
pub fn tail(data: &[u8]) -> Result<Vec<u8>, String> {
    let marker = BINARY_MARKER.as_bytes();
    let split = data
        .windows(marker.len())
        .position(|w| w == marker)
        .ok_or("not a Self snapshot: no binary-data marker")?;
    let body = &data[split + marker.len()..];
    let head = String::from_utf8_lossy(&data[..split]);
    match head.lines().find_map(|l| l.strip_prefix("Decompression filter: ")) {
        Some(f) => decompress(body, f.trim()),
        None => Ok(body.to_vec()),
    }
}

fn ascii_field<'a>(lines: &mut std::slice::Iter<'a, &'a str>, key: &str) -> Result<&'a str, String> {
    let l = lines.next().ok_or_else(|| format!("snapshot header ended before '{}'", key))?;
    l.strip_prefix(&format!("{}: ", key))
        .ok_or_else(|| format!("snapshot header line {:?} is not '{}: ...'", l, key))
}

impl Snapshot {
    pub fn read(path: &Path) -> Result<Snapshot, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        Snapshot::parse(&data)
    }

    pub fn parse(data: &[u8]) -> Result<Snapshot, String> {
        let marker = BINARY_MARKER.as_bytes();
        let split = data
            .windows(marker.len())
            .position(|w| w == marker)
            .ok_or("not a Self snapshot: no binary-data marker")?;
        let head = String::from_utf8_lossy(&data[..split]).into_owned();
        let bin_at = split + marker.len();

        let all: Vec<&str> = head.lines().collect();
        let mut lines = all.iter();
        let first = lines.next().ok_or("empty snapshot")?;
        // only the prefix is checked, so the line can carry extra VM flags
        if !first.starts_with(&HEADER_LINE[..9]) {
            return Err(format!("not a Self snapshot; first line was {:?}", first));
        }

        let v = ascii_field(&mut lines, "Version")?;
        let vs: Vec<i32> = v.split('.').map(|x| x.trim().parse().unwrap_or(-1)).collect();
        if vs.len() != 3 {
            return Err(format!("bad Version line {:?}", v));
        }
        let timestamp: i32 = ascii_field(&mut lines, "Timestamp")?.trim().parse().map_err(|_| "bad Timestamp")?;
        let snapshot_code = ascii_field(&mut lines, "Snapshot code")?.trim() == "y";
        let vm_date = ascii_field(&mut lines, "VM date")?.to_string();
        let mut sizes = [0i32; 7];
        for (i, name) in SPACE_SIZE_NAMES.iter().enumerate() {
            sizes[i] = ascii_field(&mut lines, name)?.trim().parse().map_err(|_| format!("bad {}", name))?;
        }
        let compressed = ascii_field(&mut lines, "Compressed")?.trim() == "y";
        // the filter's output replaces the compressed tail, so the binary data
        // now starts at 0 -- just as it does for the C++ VM, which reads it from
        // a pipe (universe.cpp read_snapshot, os_unix.cpp start_decompressing_snapshot)
        let piped: Vec<u8>;
        let (data, bin_at) = if compressed {
            let _ = ascii_field(&mut lines, "Compression filter")?;
            let filter = ascii_field(&mut lines, "Decompression filter")?.trim();
            piped = decompress(&data[bin_at..], filter)?;
            (&piped[..], 0usize)
        } else {
            (data, bin_at)
        };
        let page_aligned = ascii_field(&mut lines, "Page aligned")?.trim() == "y";

        if vs[2] < 12 {
            return Err(format!(
                "snapshot version {} is too old (maps are not embedded in map objects before 12)",
                vs[2]
            ));
        }
        if snapshot_code {
            return Err("this snapshot contains compiled code; re-save it with 'Snapshot code: n'".into());
        }

        let mut r = Rd { b: data, i: bin_at, swap: false };
        r.delim("Misc data", page_aligned)?;
        let maps_canonical = r.u8()? != 0;

        // endianness is decided the way check_endianness_of_vm_oops does it:
        // every VM root is an object, so the reading that leaves no
        // non-object roots is the right one
        let oops_at = r.i;
        let mut vm_oops = Vec::with_capacity(VM_OOP_NAMES.len());
        for _ in 0..VM_OOP_NAMES.len() {
            vm_oops.push(r.u32()?);
        }
        let bad = |v: &[u32], swap: bool| {
            v.iter()
                .map(|&x| if swap { x.swap_bytes() } else { x })
                .filter(|&x| x != 0 && x & 3 != 1)
                .count()
        };
        let was_swapped = bad(&vm_oops, false) != 0 && bad(&vm_oops, true) == 0;
        if was_swapped {
            r = Rd { b: data, i: oops_at, swap: true };
            vm_oops.clear();
            for _ in 0..VM_OOP_NAMES.len() {
                vm_oops.push(r.u32()?);
            }
        }
        let tenuring_threshold = r.u32()?;
        let mut vm_maps = vec![];
        for _ in 0..4 {
            vm_maps.push(r.u32()?);
        }

        r.delim("New generation", page_aligned)?;
        let mut new_gen = vec![];
        for _ in 0..3 {
            new_gen.push(read_space_header(&mut r)?);
        }
        for s in new_gen.iter_mut() {
            read_space_body(&mut r, s, page_aligned)?;
        }

        r.delim("Old generation", page_aligned)?;
        let n = r.u32()?;
        if n > 1000 {
            return Err(format!("snapshot corrupt: {} old spaces", n));
        }
        let mut old = vec![];
        for _ in 0..n {
            old.push(read_space_header(&mut r)?);
        }
        for s in old.iter_mut() {
            read_space_body(&mut r, s, page_aligned)?;
        }

        r.delim("String table", page_aligned)?;
        let mut string_table = Vec::with_capacity(STRING_TABLE_SIZE);
        for _ in 0..STRING_TABLE_SIZE {
            let len = r.u32()? as i32;
            if !(0..=1_000_000).contains(&len) {
                return Err(format!("snapshot corrupt: string table bucket length {}", len));
            }
            let mut b = Vec::with_capacity(len as usize);
            for _ in 0..len {
                b.push(r.u32()?);
            }
            string_table.push(b);
        }

        r.delim("VM strings", page_aligned)?;
        let mut vm_strings = Vec::with_capacity(NUM_VM_STRINGS);
        for _ in 0..NUM_VM_STRINGS {
            vm_strings.push(r.u32()?);
        }

        r.delim("vtbls", page_aligned)?;
        let mut vtbls = Vec::with_capacity(MAP_TYPE_NAMES.len());
        for _ in 0..MAP_TYPE_NAMES.len() {
            vtbls.push(r.u32()?);
        }

        r.delim("Code", page_aligned)?; // empty: Snapshot code was 'n'
        r.delim("Processes", page_aligned)?; // always empty: Process::write_snapshot is a no-op
        r.delim("End of snapshot", page_aligned)?;

        Ok(Snapshot {
            major: vs[0],
            minor: vs[1],
            version: vs[2],
            timestamp,
            snapshot_code,
            vm_date,
            sizes,
            page_aligned,
            compressed,
            maps_canonical,
            vm_oops,
            tenuring_threshold,
            vm_maps,
            new_gen,
            old,
            string_table,
            vm_strings,
            vtbls,
            was_swapped,
        })
    }

    // ----------------------------------------------------------------- writing

    /// Write a snapshot with its binary section gzipped, the way the shipped
    /// images are: a world is mostly zeros and pointers, so it is a third of
    /// the size, and the reader pipes it back through `zcat`.
    pub fn write(&self, path: &Path) -> Result<(), String> {
        let mut o = self.header();
        o.extend_from_slice(&pipe_through(COMPRESSION_FILTER, &self.binary_section()?)?);
        std::fs::write(path, o).map_err(|e| format!("{}: {}", path.display(), e))
    }

    fn header(&self) -> Vec<u8> {
        let mut o: Vec<u8> = vec![];
        o.extend_from_slice(HEADER_LINE.as_bytes());
        o.extend_from_slice(format!("Version: {}.{}.{}\n", self.major, self.minor, self.version).as_bytes());
        o.extend_from_slice(format!("Timestamp: {}\n", self.timestamp).as_bytes());
        o.extend_from_slice(b"Snapshot code: n\n");
        o.extend_from_slice(format!("VM date: {}\n", self.vm_date).as_bytes());
        for (i, name) in SPACE_SIZE_NAMES.iter().enumerate() {
            o.extend_from_slice(format!("{}: {}\n", name, self.sizes[i]).as_bytes());
        }
        o.extend_from_slice(b"Compressed: y\n");
        o.extend_from_slice(format!("Compression filter: {}\n", COMPRESSION_FILTER).as_bytes());
        o.extend_from_slice(format!("Decompression filter: {}\n", DECOMPRESSION_FILTER).as_bytes());
        o.extend_from_slice(b"Page aligned: n\n");
        o.extend_from_slice(BINARY_MARKER.as_bytes());
        o
    }

    /// Everything after the binary-data marker, uncompressed: see `tail`.
    pub fn binary_section(&self) -> Result<Vec<u8>, String> {
        let mut o: Vec<u8> = vec![];
        let w = |o: &mut Vec<u8>, v: u32| o.extend_from_slice(&v.to_le_bytes());

        push_delim(&mut o, "Misc data");
        o.push(if self.maps_canonical { 1 } else { 0 });
        if self.vm_oops.len() != VM_OOP_NAMES.len() {
            return Err(format!("need exactly {} VM roots", VM_OOP_NAMES.len()));
        }
        for &x in &self.vm_oops {
            w(&mut o, x);
        }
        w(&mut o, self.tenuring_threshold);
        if self.vm_maps.len() != 4 {
            return Err("need exactly 4 VM maps".into());
        }
        for &x in &self.vm_maps {
            w(&mut o, x);
        }

        push_delim(&mut o, "New generation");
        if self.new_gen.len() != 3 {
            return Err("need exactly 3 new-generation spaces".into());
        }
        for s in &self.new_gen {
            write_space_header(&mut o, s);
        }
        for s in &self.new_gen {
            write_space_body(&mut o, s)?;
        }

        push_delim(&mut o, "Old generation");
        w(&mut o, self.old.len() as u32);
        for s in &self.old {
            write_space_header(&mut o, s);
        }
        for s in &self.old {
            write_space_body(&mut o, s)?;
        }

        push_delim(&mut o, "String table");
        if self.string_table.len() != STRING_TABLE_SIZE {
            return Err(format!("string table must have {} buckets", STRING_TABLE_SIZE));
        }
        for b in &self.string_table {
            w(&mut o, b.len() as u32);
            for &x in b {
                w(&mut o, x);
            }
        }

        push_delim(&mut o, "VM strings");
        if self.vm_strings.len() != NUM_VM_STRINGS {
            return Err(format!("need exactly {} VM strings", NUM_VM_STRINGS));
        }
        for &x in &self.vm_strings {
            w(&mut o, x);
        }

        push_delim(&mut o, "vtbls");
        if self.vtbls.len() != MAP_TYPE_NAMES.len() {
            return Err("need one vtbl value per map type".into());
        }
        for &x in &self.vtbls {
            w(&mut o, x);
        }

        push_delim(&mut o, "Code");
        push_delim(&mut o, "Processes");
        push_delim(&mut o, "End of snapshot");
        Ok(o)
    }
}

fn push_delim(o: &mut Vec<u8>, name: &str) {
    o.extend_from_slice(delim_bytes(name).as_bytes());
}

fn read_space_header(r: &mut Rd) -> Result<Space, String> {
    let objs_bottom = r.u32()?;
    let objs_top = r.u32()?;
    if objs_top < objs_bottom {
        return Err("snapshot corrupt: space objs_top below objs_bottom".into());
    }
    let bytes_bottom = r.u32()?;
    let bytes_top = r.u32()?;
    if bytes_top < bytes_bottom {
        return Err("snapshot corrupt: space bytes_top below bytes_bottom".into());
    }
    Ok(Space { objs_bottom, objs_top, bytes_bottom, bytes_top, objs: vec![], bytes: vec![] })
}

fn read_space_body(r: &mut Rd, s: &mut Space, page_aligned: bool) -> Result<(), String> {
    let nobjs = s.objs_byte_len() as usize;
    let nbytes = s.bytes_len() as usize;
    if nobjs % 4 != 0 {
        return Err("snapshot corrupt: object region is not word sized".into());
    }
    if page_aligned {
        // whole pages were dumped, including the gap between the two regions
        r.align_page();
        let objs_page_end = page_end(s.objs_top);
        let bytes_page_start = page_start(s.bytes_bottom);
        let base = r.i;
        if bytes_page_start <= objs_page_end {
            let n = (s.bytes_top - s.objs_bottom) as usize;
            let all = r.raw(n)?;
            s.objs = words(&all[..nobjs], r.swap);
            let off = (s.bytes_bottom - s.objs_bottom) as usize;
            s.bytes = all[off..off + nbytes].to_vec();
        } else {
            let n1 = (objs_page_end - s.objs_bottom) as usize;
            let a = r.raw(n1)?;
            s.objs = words(&a[..nobjs], r.swap);
            let _ = base;
            r.align_page();
            let n2 = (s.bytes_top - bytes_page_start) as usize;
            let b = r.raw(n2)?;
            let off = (s.bytes_bottom - bytes_page_start) as usize;
            s.bytes = b[off..off + nbytes].to_vec();
        }
    } else {
        s.objs = words(r.raw(nobjs)?, r.swap);
        s.bytes = r.raw(nbytes)?.to_vec();
    }
    Ok(())
}

fn words(b: &[u8], swap: bool) -> Vec<u32> {
    b.chunks_exact(4)
        .map(|c| {
            let v = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            if swap { v.swap_bytes() } else { v }
        })
        .collect()
}

fn page_start(a: u32) -> u32 {
    a / IDEALIZED_PAGE * IDEALIZED_PAGE
}
fn page_end(a: u32) -> u32 {
    (a + IDEALIZED_PAGE - 1) / IDEALIZED_PAGE * IDEALIZED_PAGE
}

fn write_space_header(o: &mut Vec<u8>, s: &Space) {
    for v in [s.objs_bottom, s.objs_top, s.bytes_bottom, s.bytes_top] {
        o.extend_from_slice(&v.to_le_bytes());
    }
}

fn write_space_body(o: &mut Vec<u8>, s: &Space) -> Result<(), String> {
    if s.objs.len() * 4 != s.objs_byte_len() as usize {
        return Err("space object region does not match its header".into());
    }
    if s.bytes.len() != s.bytes_len() as usize {
        return Err("space byte region does not match its header".into());
    }
    for &x in &s.objs {
        o.extend_from_slice(&x.to_le_bytes());
    }
    o.extend_from_slice(&s.bytes);
    Ok(())
}
