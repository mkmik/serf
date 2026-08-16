//! Reading a snapshot's words as objects, and laying serf objects back out as
//! a snapshot heap.
//!
//! Layouts come from vm/src/any/objects: an object is `[mark][mapOop] ...`,
//! the `Map*` sits two words into its mapOop, and a map's slot descriptors
//! follow its fields (4 words each: name, type, data, annotation). Assignable
//! slots are an object word plus a constant `x:` slot holding an assignment
//! object -- exactly serf's `SlotKind::Assign`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::heap::Kind as HKind;
use crate::image::*;
use crate::value::*;

#[allow(dead_code)]
pub fn value_key(v: &Value) -> usize {
    match v {
        Value::Obj(o) => o.id(),
        _ => 0,
    }
}

pub const MEM_TAG: u32 = 1;
pub const MARK_TAG: u32 = 3;
const TAG_MASK: u32 = 3;

pub fn smi(w: u32) -> i32 {
    (w as i32) >> 2
}
pub fn as_smi(v: i32) -> u32 {
    (v as u32) << 2
}

/// Self floats are 30-bit: sign, 6-bit exponent, 23-bit fraction, 2 tag bits.
pub fn float_of(w: u32) -> f32 {
    let self_exp = (w >> 25) & 0x3f;
    let fract = (w >> 2) & 0x7f_ffff;
    let mut r = w & 0x8000_0000;
    if self_exp == 0 {
        // zero
    } else if self_exp >= 0x3f {
        r |= 0xff << 23 | fract;
    } else {
        r |= ((self_exp as i32 - 31 + 127) as u32) << 23 | fract;
    }
    f32::from_bits(r)
}

pub fn as_float(v: f32) -> u32 {
    let i = v.to_bits();
    let exp = ((i >> 23) & 0xff) as i32;
    let fract = i & 0x7f_ffff;
    let mut r = (i & 0x8000_0000) | 2;
    let self_exp = exp - 127 + 31;
    if self_exp <= 0 {
        // underflow, rounds to zero
    } else if self_exp >= 0x3f {
        r |= 0x3f << 25;
        if exp == 0xff && fract != 0 {
            r |= fract << 2; // NaN
        }
    } else {
        r |= (self_exp as u32) << 25 | fract << 2;
    }
    r
}

/// Where a map's slot descriptors start, in words from the `Map*`.
fn slots_offset(k: MapType) -> usize {
    use MapType::*;
    match k {
        Smi | Float | Mark | MapM => 1, // Map has only a vtbl
        Slots => 4,                     // + object_length, slots_length, annotation
        Block => 4,                     // + desc, valueMethodName, valueMethod
        // blockMethodMap adds _sourceOffset and _sourceLen on top of methodMap,
        // which is why it overrides slots() in codeSlotsMap.hh
        BlockMethod => 11,
        // methodMap adds codes/file/line/source/literals to slotsMap;
        // slotsMapDeps adds two nmlns and a dependents pointer. Both land at 9.
        _ => 9,
    }
}

fn map_word_size(k: MapType, slots_len: usize) -> usize {
    use MapType::*;
    match k {
        Smi | Float => 5, // immediateMap + one parent slotDesc
        Mark | MapM => 1,
        Block => 4,
        _ => slots_offset(k) + 4 * slots_len,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotClass {
    Obj,
    Const,
    Arg,
}

#[derive(Clone)]
#[allow(dead_code)] // is_vm/anno are decoded to prove the 4-word stride is right
pub struct SlotDescr {
    pub name: u32,
    pub class: SlotClass,
    pub is_parent: bool,
    pub is_vm: bool,
    pub data: u32,
    pub anno: u32,
}

pub fn slot_type_bits(class: SlotClass, is_parent: bool, is_vm: bool) -> u32 {
    let t = match class {
        SlotClass::Obj => 0,
        SlotClass::Const => 1,
        SlotClass::Arg => 2,
    };
    (t << 2) | ((is_vm as u32) << 4) | ((is_parent as u32) << 5)
}

#[derive(Clone)]
#[allow(dead_code)] // annotation is part of the map, unused by serf itself
pub struct MapInfo {
    pub kind: MapType,
    pub object_length: i32,
    pub annotation: u32,
    pub slots: Vec<SlotDescr>,
    /// outer/block method maps: codes, file, line, source, literals
    pub method: Option<[u32; 5]>,
    /// block maps: desc, valueMethodName, valueMethod
    pub block: Option<[u32; 3]>,
}

// ------------------------------------------------------------------ the heap

pub struct Heap<'a> {
    spaces: Vec<&'a Space>,
    /// snapshot vtbl value -> map type
    vtbl: HashMap<u32, MapType>,
}

impl<'a> Heap<'a> {
    pub fn new(snap: &'a Snapshot) -> Heap<'a> {
        let mut vtbl = HashMap::new();
        for (i, &v) in snap.vtbls.iter().enumerate() {
            if let Some(t) = MapType::from_index(i) {
                vtbl.insert(v, t);
            }
        }
        Heap { spaces: snap.new_gen.iter().chain(snap.old.iter()).collect(), vtbl }
    }

    pub fn word(&self, addr: u32) -> Result<u32, String> {
        for s in &self.spaces {
            if addr >= s.objs_bottom && addr + 4 <= s.objs_top {
                return Ok(s.objs[((addr - s.objs_bottom) / 4) as usize]);
            }
        }
        Err(format!("address {:#x} is not in any object region", addr))
    }

    pub fn bytes(&self, addr: u32, n: usize) -> Result<Vec<u8>, String> {
        for s in &self.spaces {
            if addr >= s.bytes_bottom && addr as u64 + n as u64 <= s.bytes_top as u64 {
                let o = (addr - s.bytes_bottom) as usize;
                return Ok(s.bytes[o..o + n].to_vec());
            }
        }
        Err(format!("byte range {:#x}+{} is not in any byte region", addr, n))
    }

    /// The `Map*` of the object at `addr` (two words into its mapOop).
    pub fn map_star_of(&self, addr: u32) -> Result<u32, String> {
        let m = self.word(addr + 4)?;
        if m & TAG_MASK != MEM_TAG {
            return Err(format!("object at {:#x} has a non-object map word {:#x}", addr, m));
        }
        Ok((m & !TAG_MASK) + 8)
    }

    pub fn read_map(&self, map_star: u32) -> Result<MapInfo, String> {
        let v = self.word(map_star)?;
        let kind = *self
            .vtbl
            .get(&v)
            .ok_or_else(|| format!("map at {:#x} has unknown vtbl {:#x}", map_star, v))?;
        let (object_length, slots_len, annotation) = match kind {
            MapType::Smi | MapType::Float => (0, 1, 0),
            MapType::Mark | MapType::MapM => (0, 0, 0),
            MapType::Block => (3, 0, 0),
            _ => (
                smi(self.word(map_star + 4)?),
                smi(self.word(map_star + 8)?) as usize,
                self.word(map_star + 12)?,
            ),
        };
        if slots_len > 100_000 {
            return Err(format!("map at {:#x} claims {} slots", map_star, slots_len));
        }
        let off = slots_offset(kind) as u32 * 4;
        let mut slots = Vec::with_capacity(slots_len);
        for i in 0..slots_len {
            let b = map_star + off + (i as u32) * 16;
            let t = self.word(b + 4)?;
            slots.push(SlotDescr {
                name: self.word(b)?,
                class: match (t >> 2) & 3 {
                    0 => SlotClass::Obj,
                    1 => SlotClass::Const,
                    _ => SlotClass::Arg,
                },
                is_vm: t & (1 << 4) != 0,
                is_parent: t & (1 << 5) != 0,
                data: self.word(b + 8)?,
                anno: self.word(b + 12)?,
            });
        }
        let method = if matches!(kind, MapType::OuterMethod | MapType::BlockMethod) {
            let mut f = [0u32; 5];
            for (i, x) in f.iter_mut().enumerate() {
                *x = self.word(map_star + 16 + 4 * i as u32)?;
            }
            Some(f)
        } else {
            None
        };
        let block = if kind == MapType::Block {
            Some([self.word(map_star + 4)?, self.word(map_star + 8)?, self.word(map_star + 12)?])
        } else {
            None
        };
        Ok(MapInfo { kind, object_length, annotation, slots, method, block })
    }

    /// Words occupied by the object at `addr`, so the heap can be walked.
    pub fn object_size(&self, addr: u32, m: &MapInfo) -> Result<usize, String> {
        Ok(match m.kind {
            MapType::MapM => {
                let inner = self.read_map(addr + 8)?;
                2 + map_word_size(inner.kind, inner.slots.len())
            }
            MapType::Block => 3,
            MapType::ObjVector => {
                m.object_length as usize + smi(self.word(addr + 8)?).max(0) as usize
            }
            _ => m.object_length.max(2) as usize,
        })
    }

    /// Walk a space's objects linearly, as the C++ enumeration does. Every
    /// object must start with a mark word and the sizes must tile the region
    /// exactly -- if they do not, some map layout here is wrong.
    pub fn walk(&self, s: &Space) -> Result<usize, String> {
        let mut a = s.objs_bottom;
        let mut n = 0;
        while a < s.objs_top {
            let mark = self.word(a)?;
            if mark & TAG_MASK != MARK_TAG {
                return Err(format!(
                    "object at {:#x} does not start with a mark word ({:#x})",
                    a, mark
                ));
            }
            let m = self.read_map(self.map_star_of(a)?)?;
            let size = self.object_size(a, &m)?;
            if size < 2 {
                return Err(format!("object at {:#x} ({}) has size {}", a, m.kind.name(), size));
            }
            a += 4 * size as u32;
            n += 1;
        }
        if a != s.objs_top {
            return Err(format!(
                "objects overrun the space: ended at {:#x}, top is {:#x}",
                a, s.objs_top
            ));
        }
        Ok(n)
    }

    pub fn string_at(&self, addr: u32) -> Result<String, String> {
        if addr & TAG_MASK != MEM_TAG {
            return Err(format!("{:#x} is not an object", addr));
        }
        let a = addr & !TAG_MASK;
        let len = smi(self.word(a + 8)?);
        let ptr = self.word(a + 12)?;
        if len < 0 {
            return Err("negative string length".into());
        }
        Ok(String::from_utf8_lossy(&self.bytes(ptr, len as usize)?).into_owned())
    }
}

// --------------------------------------------------------- snapshot -> serf

pub struct Loader<'a> {
    heap: Heap<'a>,
    done: HashMap<u32, Value>,
    methods: HashMap<u32, Rc<Method>>,
    nil: Value,
    block_traits: u32,
}

impl<'a> Loader<'a> {
    pub fn new(snap: &'a Snapshot, vm: &Vm) -> Loader<'a> {
        Loader {
            heap: Heap::new(snap),
            done: HashMap::new(),
            methods: HashMap::new(),
            nil: vm.nil.clone(),
            block_traits: snap.vm_oops[8],
        }
    }

    /// The traits object an immediate inherits from, taken from the image's
    /// smi_map / float_map (their single slot is the parent).
    pub fn immediate_traits(&mut self, map_star: u32) -> Result<Option<Value>, String> {
        let m = self.heap.read_map(map_star)?;
        match m.slots.first() {
            Some(s) if s.class == SlotClass::Const => Ok(Some(self.value(s.data)?)),
            _ => Ok(None),
        }
    }

    pub fn count(&self) -> usize {
        self.done.len()
    }

    /// Decode one tagged word.
    pub fn value(&mut self, w: u32) -> Result<Value, String> {
        match w & TAG_MASK {
            0 => Ok(Value::Int(smi(w) as i64)),
            2 => Ok(Value::Float(float_of(w) as f64)),
            3 => Ok(self.nil.clone()), // markOops (badOop &c) are not user values
            _ => self.object(w & !TAG_MASK),
        }
    }

    /// How big the object will be, from its map alone -- no recursion, which
    /// is what lets it be allocated before its contents are read.
    ///
    /// Counting the slots twice, here and in the loop that fills them, is the
    /// price of a cyclic graph in a heap where shape is fixed at allocation.
    /// ponytail: the two counts must agree; `obj::fill` asserts they do.
    fn shape_of(&mut self, m: &MapInfo, addr: u32) -> Result<(HKind, usize, usize, bool), String> {
        let mut n = 0usize;
        if m.kind == MapType::Block {
            n += 2; // parent, and the `value...` selector
        }
        let mut annotated = m.annotation & TAG_MASK == MEM_TAG;
        for s in &m.slots {
            if s.class == SlotClass::Arg {
                continue;
            }
            if self.heap.string_at(s.name).is_err() {
                continue;
            }
            n += 1;
            if s.class == SlotClass::Obj {
                n += 1; // the derived `x:`
            }
            annotated |= s.anno & TAG_MASK == MEM_TAG;
        }
        let ilen = |me: &mut Self| -> Result<usize, String> {
            Ok(smi(me.heap.word(addr + 8)?).max(0) as usize)
        };
        let (k, len) = match m.kind {
            MapType::StringM | MapType::ByteVector => (HKind::Bytes, ilen(self)?),
            MapType::ObjVector => (HKind::ObjVector, ilen(self)?),
            MapType::Mirror => (HKind::Mirror, 0),
            MapType::Proxy | MapType::FctProxy => (HKind::Proxy, 0),
            MapType::OuterMethod | MapType::BlockMethod => (HKind::Method, 0),
            MapType::Block => (HKind::Block, 0),
            _ => (HKind::Slots, 0),
        };
        Ok((k, n, len, annotated))
    }

    pub fn object(&mut self, addr: u32) -> Result<Value, String> {
        if let Some(v) = self.done.get(&addr) {
            return Ok(v.clone());
        }
        let map_star = self.heap.map_star_of(addr)?;
        let m = self.heap.read_map(map_star)?;

        // The object graph is cyclic, so something has to be published before
        // its contents are read. An arena object's shape is fixed when it is
        // allocated, though, so a placeholder cannot be filled in later -- and
        // it does not have to be: the map says how many slots and how long the
        // indexable part is, and reading *that* needs no recursion at all.
        let (k, ns, ilen, anno) = self.shape_of(&m, addr)?;
        let shell = Value::Obj(crate::obj::make_shape(k, ns, ilen, anno));
        self.done.insert(addr, shell);

        // mark word is `marked<1> age<7> hash<22> tag<2>` (markOop.hh); a
        // hash of 0 is `no_hash`, meaning the VM never assigned one. serf keeps
        // it in its own mark word, where the C++ VM keeps it -- a Rust-side
        // table keyed on identity cannot survive the identity moving.
        let hash = ((self.heap.word(addr)? >> 2) & 0x3f_ffff) as u32;
        if hash != 0 {
            crate::heap::set_hash(shell.as_obj().unwrap(), hash);
        }

        let at = shell.as_obj().unwrap();
        let payload = match m.kind {
            MapType::StringM | MapType::ByteVector => {
                let len = smi(self.heap.word(addr + 8)?).max(0) as usize;
                let ptr = self.heap.word(addr + 12)?;
                Payload::Bytes(self.heap.bytes(ptr, len)?)
            }
            MapType::ObjVector => {
                let len = smi(self.heap.word(addr + 8)?).max(0) as usize;
                let base = addr + 4 * m.object_length.max(2) as u32;
                let mut v = Vec::with_capacity(len);
                for i in 0..len {
                    let w = self.heap.word(base + 4 * i as u32)?;
                    v.push(self.value(w)?);
                }
                Payload::Vector(v)
            }
            MapType::Mirror => Payload::Mirror(self.value(self.heap.word(addr + 8)?)?),
            // foreignOop keeps its C pointer in the first word after the header
            // space::fixup_killables kills every foreign object on the way
            // into a snapshot, so proxies always come back dead and the world
            // revives the ones it still wants
            MapType::Proxy | MapType::FctProxy => Payload::Proxy(None),
            MapType::OuterMethod | MapType::BlockMethod => {
                let me = self.method(map_star, addr)?;
                Payload::Method(me)
            }
            MapType::Block => {
                let vm_addr = m.block.unwrap()[2];
                if vm_addr & TAG_MASK != MEM_TAG {
                    return Err("block has no value method".into());
                }
                let vaddr = vm_addr & !TAG_MASK;
                let vmap = self.heap.map_star_of(vaddr)?;
                Payload::Block(self.method(vmap, vaddr)?, None)
            }
            _ => Payload::None,
        };

        let mut slots = vec![];
        let mut slot_annos: Vec<(usize, Value)> = vec![];
        if let Payload::Block(bm, _) = &payload {
            let bm = bm.clone();
            // the blockMap carries the real selector; deriving it from the
            // argument count gets it wrong for blocks the world names itself
            let sel = m
                .block
                .map(|b| b[1])
                .and_then(|w| self.heap.string_at(w).ok())
                .unwrap_or_else(|| crate::compile::block_selector(bm.nargs));
            if std::env::var_os("SERF_TRACE_BLOCKS").is_some() {
                let derived = crate::compile::block_selector(bm.nargs);
                if derived != sel {
                    eprintln!(
                        "block: map says {:?} but arity {} implies {:?}",
                        sel, bm.nargs, derived
                    );
                }
            }
            let mv = match m.block.map(|b| b[2]).filter(|w| w & TAG_MASK == MEM_TAG) {
                Some(w) => self.object(w & !TAG_MASK)?,
                None => Value::obj([], Payload::Method(bm)),
            };
            let bt = self.value(self.block_traits)?;
            slots.push(Slot { name: "parent".into(), kind: SlotKind::Parent, value: bt });
            slots.push(Slot { name: sel.as_str().into(), kind: SlotKind::Data, value: mv });
        }
        for s in &m.slots {
            if s.class == SlotClass::Arg {
                continue; // only meaningful inside an activation
            }
            let name = match self.heap.string_at(s.name) {
                Ok(n) => n,
                Err(_) => continue,
            };
            let raw = match s.class {
                SlotClass::Const => s.data,
                _ => self.heap.word(addr + 4 * (smi(s.data).max(0) as u32))?,
            };
            let kind = if s.is_parent { SlotKind::Parent } else { SlotKind::Data };
            let value = self.value(raw)?;
            if s.anno & TAG_MASK == MEM_TAG {
                let a = self.value(s.anno)?;
                slot_annos.push((slots.len(), a));
            }
            slots.push(Slot { name: name.as_str().into(), kind, value });
            // Self stores only the data slot; `x:` is derived from it being an
            // obj slot (slotDesc::assignment_slot_name), so make it explicit
            if s.class == SlotClass::Obj {
                slots.push(Slot {
                    name: format!("{}:", name).as_str().into(),
                    kind: SlotKind::Assign,
                    value: self.nil.clone(),
                });
            }
        }

        // the shape was right, so the contents just go in
        crate::obj::fill(at, &slots, payload);
        for (i, a) in slot_annos {
            crate::heap::set_slot_anno(at, i, crate::obj::to_oop(a));
        }
        if m.annotation & TAG_MASK == MEM_TAG {
            let a = self.value(m.annotation)?;
            crate::heap::set_obj_anno(at, crate::obj::to_oop(a));
        }
        crate::heap::heap().record(at);
        // mirrors, proxies, processes and vframes are ordinary slot objects to
        // serf; remember their map kind so writing does not demote them --
        // in the object, because an address is not a key across a collection
        crate::heap::set_aux(at, m.kind as u8);
        Ok(shell)
    }

    fn method(&mut self, map_star: u32, obj: u32) -> Result<Rc<Method>, String> {
        if let Some(m) = self.methods.get(&map_star) {
            return Ok(m.clone());
        }
        let mi = self.heap.read_map(map_star)?;
        let f = mi.method.ok_or("not a method map")?;
        let [codes, file, line, source, literals] = f;
        // a block method's map carries the slice of the enclosing method's
        // source that is the block, two words past the methodMap fields
        let (soff, slen) = if mi.kind == MapType::BlockMethod {
            (smi(self.heap.word(map_star + 36)?), smi(self.heap.word(map_star + 40)?))
        } else {
            (0, 0)
        };
        let source = match source & TAG_MASK == MEM_TAG {
            true => Some((self.value(source)?, soff as i64, slen as i64)),
            false => None,
        };

        let code = {
            let a = codes & !TAG_MASK;
            let len = smi(self.heap.word(a + 8)?).max(0) as usize;
            let ptr = self.heap.word(a + 12)?;
            self.heap.bytes(ptr, len)?
        };
        let mut lits = vec![];
        let mut lit_strs = vec![];
        if literals & TAG_MASK == MEM_TAG {
            let a = literals & !TAG_MASK;
            let lm = self.heap.read_map(self.heap.map_star_of(a)?)?;
            let len = smi(self.heap.word(a + 8)?).max(0) as usize;
            let base = a + 4 * lm.object_length.max(2) as u32;
            for i in 0..len {
                let w = self.heap.word(base + 4 * i as u32)?;
                lit_strs
                    .push(self.heap.string_at(w).ok().map(|s| -> Rc<str> { s.as_str().into() }));
                lits.push(self.value(w)?);
            }
        }

        let mut arg_slots: Vec<(usize, usize)> = vec![];
        let mut names = vec![];
        let mut flags = vec![];
        let mut inits = vec![];
        for (i, s) in mi.slots.iter().enumerate() {
            if s.class == SlotClass::Arg {
                arg_slots.push((smi(s.data).max(0) as usize, i));
            }
            let v = match s.class {
                SlotClass::Const => self.value(s.data)?,
                // an arg slot's data is its argument index, not an offset
                SlotClass::Arg => self.nil.clone(),
                SlotClass::Obj => {
                    let w = self.heap.word(obj + 4 * (smi(s.data).max(0) as u32))?;
                    self.value(w)?
                }
            };
            flags.push(slot_type_bits(s.class, s.is_parent, s.is_vm));
            inits.push(v);
            names.push(self.heap.string_at(s.name).unwrap_or_default().as_str().into());
        }

        arg_slots.sort();
        let m = Rc::new(Method {
            sel: "<loaded>".into(),
            nargs: arg_slots.len(),
            arg_slots: arg_slots.iter().map(|&(_, i)| i).collect(),
            slot_names: names,
            slot_flags: flags,
            slot_inits: RefCell::new(inits),
            code,
            lits: RefCell::new(lits),
            lit_strs,
            is_block: mi.kind == MapType::BlockMethod,
            file: self.heap.string_at(file).unwrap_or_default().as_str().into(),
            line: smi(line).max(0) as u32,
            source: Cell::new(source),
            sites: Default::default(),
            max_stack: std::cell::Cell::new(u32::MAX),
        });
        self.methods.insert(map_star, m.clone());
        Ok(m)
    }
}

// --------------------------------------------------------- serf -> snapshot

// --------------------------------------------------------- serf -> snapshot

const OLD_BASE: u32 = 0x0400_0000 + NEW_SIZE; // right above the new generation
const NEW_SIZE: u32 = EDEN + 2 * SURV;
const EDEN: u32 = 512 * 1024;
const SURV: u32 = 256 * 1024;
/// Any distinct values work: the C++ reader translates them through its own
/// table whenever they differ from the running VM's (mapVtbls.cpp).
const VTBL_SENTINEL: u32 = 0x5e1f_0000;

fn mark_word(hash: u32) -> u32 {
    ((hash & 0x3f_ffff) << 2) | MARK_TAG
}

/// hashpjw over at most 32 evenly spaced bytes -- stringTable.cpp's `hash`.
pub fn string_hash(b: &[u8]) -> u32 {
    let inc = if b.len() < 32 { 1 } else { b.len() >> 5 };
    let mut h: u32 = 0;
    let mut i = 0;
    while i < b.len() {
        h = (h << 4).wrapping_add(b[i] as i8 as i32 as u32);
        let g = h & 0xf000_0000;
        if g != 0 {
            h ^= g | (g >> 24);
        }
        i += inc;
    }
    h
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Slots,
    Str,
    ByteVec,
    Vector,
    Method,
    Block,
}

struct Item {
    v: Value,
    kind: Kind,
    /// names of slots stored in the object (assignable ones)
    obj_slots: Vec<Sym>,
    words: usize,
    addr: u32,
}

pub struct Builder<'a> {
    vm: &'a Vm,
    items: Vec<Item>,
    index: HashMap<usize, usize>,
    /// canonical strings, interned by content
    strs: HashMap<Vec<u8>, usize>,
    bytes: Vec<u8>,
    byte_off: HashMap<usize, u32>,
    /// method ptr -> item indices of its literals vector and its code, file
    /// and source strings
    mparts: HashMap<usize, (usize, usize, usize, usize)>,
    next_hash: u32,
}

/// Canonical string or plain byte vector? A loaded object remembers the map
/// class it came from; an serf-native one is a string if its parent is
/// `traits string`.
fn is_str(vm: &Vm, v: &Value, o: &ObjRef) -> bool {
    let _ = v;
    match MapType::from_index(crate::heap::aux(*o) as usize) {
        Some(MapType::StringM) => return true,
        Some(MapType::ByteVector) => return false,
        _ => {}
    }
    o.borrow().slots.iter().any(|s| s.kind == SlotKind::Parent && s.value.id_eq(&vm.t_string))
}

impl<'a> Builder<'a> {
    pub fn new(vm: &'a Vm) -> Builder<'a> {
        Builder {
            vm,
            items: vec![],
            index: HashMap::new(),
            strs: HashMap::new(),
            bytes: vec![],
            byte_off: HashMap::new(),
            mparts: HashMap::new(),
            next_hash: 1,
        }
    }

    fn hash(&mut self) -> u32 {
        self.next_hash = self.next_hash.wrapping_add(0x9e37).max(1) & 0x3f_ffff;
        self.next_hash.max(1)
    }

    /// Intern a canonical string, returning its item index.
    fn string(&mut self, text: &[u8]) -> usize {
        if let Some(&i) = self.strs.get(text) {
            return i;
        }
        let v = self.vm.bytes_with(self.vm.t_string.clone(), text.to_vec());
        let i = self.add(v, Kind::Str, vec![], 4);
        self.strs.insert(text.to_vec(), i);
        let off = self.bytes.len() as u32;
        self.bytes.extend_from_slice(text);
        while self.bytes.len() % 4 != 0 {
            self.bytes.push(0);
        }
        self.byte_off.insert(i, off);
        i
    }

    fn add(&mut self, v: Value, kind: Kind, obj_slots: Vec<Sym>, words: usize) -> usize {
        let key = match &v {
            Value::Obj(o) => o.id(),
            _ => 0,
        };
        let i = self.items.len();
        if key != 0 {
            self.index.insert(key, i);
        }
        self.items.push(Item { v, kind, obj_slots, words, addr: 0 });
        i
    }

    /// Walk the graph, sizing every object. Returns this value's item index.
    fn visit(&mut self, v: &Value) -> Result<Option<usize>, String> {
        let o = match v.as_obj() {
            Some(o) => o.clone(),
            None => return Ok(None), // immediates live in the referring word
        };
        if let Some(&i) = self.index.get(&o.id()) {
            return Ok(Some(i));
        }
        // assignable slots are the ones with a matching `name:` writer
        let (assignable, kind, extra) = {
            let b = o.borrow();
            let mut a: Vec<Sym> = vec![];
            for s in &b.slots {
                if s.kind == SlotKind::Assign {
                    let t = sym_str(s.name).trim_end_matches(':');
                    if let Some(d) =
                        b.slots.iter().find(|x| sym_str(x.name) == t && x.kind != SlotKind::Assign)
                    {
                        a.push(d.name);
                    }
                }
            }
            let (k, e) = match b.payload.kind() {
                PayKind::Bytes => (
                    if is_str(self.vm, v, &o) { Kind::Str } else { Kind::ByteVec },
                    b.payload.byte_len(),
                ),
                PayKind::Vector => (Kind::Vector, b.payload.vector_len()),
                PayKind::Method => (Kind::Method, 0),
                PayKind::Block => {
                    if b.payload.block_scope().is_some() {
                        return Err("cannot write a live block: its home activation is not part of the heap".into());
                    }
                    (Kind::Block, 0)
                }
                // a mirror is a slots object with one extra fixed word; a dead
                // proxy on the way out, because the pointer is not portable
                _ => (Kind::Slots, 0),
            };
            (a, k, e)
        };

        if kind == Kind::Str {
            let text = v.bytes().unwrap();
            // a string that came from an image keeps its own identity; only
            // serf-native strings are canonicalised by content here
            if crate::heap::aux(o) != 0 {
                let i = self.add(v.clone(), Kind::Str, vec![], 4);
                let off = self.bytes.len() as u32;
                self.bytes.extend_from_slice(&text);
                while self.bytes.len() % 4 != 0 {
                    self.bytes.push(0);
                }
                self.byte_off.insert(i, off);
                self.strs.entry(text).or_insert(i);
                return Ok(Some(i));
            }
            let i = self.string(&text);
            self.index.insert(o.id(), i);
            return Ok(Some(i));
        }

        // only obj slots take a word: constants live in the map and arg slots
        // hold an index into the activation's argument list
        let nmslots = match o.borrow().payload.method() {
            Some(m) if o.borrow().payload.kind() == PayKind::Method => (0..m.slot_names.len())
                .filter(|&k| (m.slot_flags.get(k).copied().unwrap_or(0) >> 2) & 3 == 0)
                .count(),
            _ => 0,
        };
        let is_mirror = o.borrow().payload.kind() == PayKind::Mirror;
        let words = match kind {
            Kind::Method => 2 + nmslots,
            Kind::Slots => 2 + assignable.len() + is_mirror as usize,
            Kind::ByteVec => 4 + assignable.len(),
            Kind::Vector => 3 + assignable.len() + extra,
            Kind::Block => 3,
            Kind::Str => unreachable!(),
        };
        let me = self.add(v.clone(), kind, assignable, words);

        if kind == Kind::ByteVec {
            let off = self.bytes.len() as u32;
            self.bytes.extend_from_slice(&v.bytes().unwrap());
            while self.bytes.len() % 4 != 0 {
                self.bytes.push(0);
            }
            self.byte_off.insert(me, off);
        }

        // now the children: slot names, slot values, payload contents
        let (slots, payload_kids, methods) = {
            let b = o.borrow();
            let mut kids: Vec<Value> = vec![];
            let mut ms: Vec<Rc<Method>> = vec![];
            match b.payload.kind() {
                PayKind::Mirror => kids.push(b.payload.mirror().unwrap()),
                PayKind::Vector => kids.extend(b.payload.vector().unwrap()),
                PayKind::Method | PayKind::Block => ms.push(b.payload.method().unwrap()),
                _ => {}
            }
            let slots: Vec<Slot> = b.slots.iter().collect();
            (slots, kids, ms)
        };
        for s in &slots {
            self.string(sym_str(s.name).as_bytes());
            self.visit(&s.value)?;
        }
        for i in 0..slots.len() {
            if let Some(a) = crate::value::slot_anno(o, i) {
                self.visit(&a)?;
            }
        }
        if let Some(a) = crate::value::obj_anno(o) {
            self.visit(&a)?;
        }
        for k in payload_kids {
            self.visit(&k)?;
        }
        for m in methods {
            self.visit_method(&m)?;
        }
        Ok(Some(me))
    }

    fn visit_method(&mut self, m: &Rc<Method>) -> Result<(), String> {
        let key = Rc::as_ptr(m) as *const u8 as usize;
        if self.mparts.contains_key(&key) {
            return Ok(());
        }
        let code = self.string(&m.code);
        self.string(crate::compile::block_selector(m.nargs).as_bytes());
        let file = self.string(m.file.as_bytes());
        for n in &m.slot_names {
            self.string(n.as_bytes());
        }
        for v in m.slot_inits.borrow().iter() {
            self.visit(v)?;
        }
        for l in m.lits.borrow().iter() {
            self.visit(l)?;
        }
        let lv = self.vm.vector(m.lits.borrow().clone());
        let lits = self.visit(&lv)?.unwrap();
        let source = match m.source.get() {
            Some((s, _, _)) => self.visit(&s)?.unwrap(),
            None => self.string(b""),
        };
        self.mparts.insert(key, (lits, code, file, source));
        Ok(())
    }
}

// ------------------------------------------------------------ emitting

fn round_page(n: u32) -> u32 {
    (n + IDEALIZED_PAGE - 1) / IDEALIZED_PAGE * IDEALIZED_PAGE
}

struct Emit {
    words: Vec<u32>,
    base: u32,
}

impl Emit {
    fn put(&mut self, at: u32, v: u32) {
        let i = ((at - self.base) / 4) as usize;
        self.words[i] = v;
    }
}

impl<'a> Builder<'a> {
    fn tagged(&self, v: &Value) -> Result<u32, String> {
        Ok(match v {
            Value::Int(i) => {
                let i = *i;
                if !(-(1 << 29)..(1 << 29)).contains(&i) {
                    return Err(format!("{} does not fit in a 30-bit Self integer", i));
                }
                as_smi(i as i32)
            }
            Value::Float(f) => as_float(*f as f32),
            Value::Obj(o) => {
                let i = *self.index.get(&o.id()).ok_or("object was not visited")?;
                self.items[i].addr | MEM_TAG
            }
        })
    }

    /// Lay everything out and produce a snapshot.
    pub fn finish(mut self, roots: &[Value]) -> Result<Snapshot, String> {
        // 1. addresses for objects, then bytes at the top of the old space
        let mut w = 0usize;
        for it in self.items.iter_mut() {
            it.addr = OLD_BASE + 4 * w as u32;
            w += it.words;
        }
        let obj_words = w;
        // upper bound: every object gets its own map, 9 header words + 4/slot
        let map_bound: usize =
            self.items.iter().map(|i| 11 + 4 * (i.obj_slots.len() + slot_count(&i.v) + 8)).sum();
        let byte_len = self.bytes.len() as u32;
        let old_size = round_page((obj_words + map_bound) as u32 * 4 + byte_len + 64 * 1024);
        let bytes_top = OLD_BASE + old_size;
        let bytes_bottom = bytes_top - byte_len;

        if std::env::var("SERF_DUMP_ITEMS").is_ok() {
            for (n, it) in self.items.iter().enumerate() {
                let k = match it.kind {
                    Kind::Slots => "slots",
                    Kind::Str => "str",
                    Kind::ByteVec => "bytevec",
                    Kind::Vector => "vector",
                    Kind::Method => "method",
                    Kind::Block => "block",
                };
                eprintln!(
                    "item {:6} addr {:#x} words {:3} kind {} objslots {}",
                    n,
                    it.addr,
                    it.words,
                    k,
                    it.obj_slots.len()
                );
            }
        }
        let frozen = self.items.len();
        let mut e = Emit { words: vec![0; obj_words], base: OLD_BASE };
        // maps are appended after the objects
        let mut maps: HashMap<Vec<u32>, u32> = HashMap::new();
        let mut map_words: Vec<u32> = vec![];
        let map_base = OLD_BASE + 4 * obj_words as u32;

        let nil = self.tagged(&self.vm.nil.clone())?;

        // the shared assignment object, its map, and map_map come first so
        // their addresses are stable
        let intern =
            |body: Vec<u32>, maps: &mut HashMap<Vec<u32>, u32>, mw: &mut Vec<u32>, h: u32| -> u32 {
                if let Some(&a) = maps.get(&body) {
                    return a | MEM_TAG;
                }
                let a = map_base + 4 * mw.len() as u32;
                mw.push(mark_word(h));
                mw.push(0); // patched to map_map below
                mw.extend_from_slice(&body);
                maps.insert(body, a);
                a | MEM_TAG
            };

        let h1 = self.hash();
        let map_map =
            intern(vec![VTBL_SENTINEL + MapType::MapM as u32 * 4], &mut maps, &mut map_words, h1);
        let h2 = self.hash();
        let assign_map = intern(
            vec![
                VTBL_SENTINEL + MapType::Assignment as u32 * 4,
                as_smi(2),
                as_smi(0),
                nil,
                0,
                0,
                0,
                0,
                0,
            ],
            &mut maps,
            &mut map_words,
            h2,
        );
        // the assignment object itself is a plain two-word object; park it in
        // the map region, it is shaped like any other heap object
        let assign_obj = {
            let a = map_base + 4 * map_words.len() as u32;
            let h = self.hash();
            map_words.push(mark_word(h));
            map_words.push(assign_map);
            a | MEM_TAG
        };
        let h3 = self.hash();
        let pi = self.string(b"parent");
        let parent_str = self.items[pi].addr | MEM_TAG;
        let t_int = self.tagged(&self.vm.t_smallint.clone())?;
        let t_flt = self.tagged(&self.vm.t_float.clone())?;
        let smi_map = intern(
            vec![
                VTBL_SENTINEL + MapType::Smi as u32 * 4,
                parent_str,
                slot_type_bits(SlotClass::Const, true, false),
                t_int,
                nil,
            ],
            &mut maps,
            &mut map_words,
            h3,
        );
        let h4 = self.hash();
        let float_map = intern(
            vec![
                VTBL_SENTINEL + MapType::Float as u32 * 4,
                parent_str,
                slot_type_bits(SlotClass::Const, true, false),
                t_flt,
                nil,
            ],
            &mut maps,
            &mut map_words,
            h4,
        );
        let h5 = self.hash();
        let mark_map =
            intern(vec![VTBL_SENTINEL + MapType::Mark as u32 * 4], &mut maps, &mut map_words, h5);

        // 2. every object: header, payload words, and its map
        for i in 0..self.items.len() {
            let (kind, addr, words, obj_slots) = {
                let it = &self.items[i];
                (it.kind, it.addr, it.words, it.obj_slots.clone())
            };
            let v = self.items[i].v.clone();
            let h = self.hash();
            e.put(addr, mark_word(h));

            let mut descs: Vec<u32> = vec![];
            let reflectee = v.as_obj().unwrap().borrow().payload.mirror();
            let mut next_obj_word = match kind {
                Kind::Str | Kind::ByteVec => 4,
                Kind::Vector => 3,
                _ => 2 + reflectee.is_some() as usize,
            };
            if let Some(r) = &reflectee {
                let t = self.tagged(r)?;
                e.put(addr + 8, t);
            }
            let map_kind = match kind {
                // only the kinds that are ordinary slot objects with a
                // different map class; everything else is decided by payload
                Kind::Slots if reflectee.is_some() => MapType::Mirror,
                Kind::Slots => {
                    match MapType::from_index(crate::heap::aux(v.as_obj().unwrap()) as usize) {
                        Some(k)
                            if matches!(
                                k,
                                MapType::Mirror
                                    | MapType::Proxy
                                    | MapType::FctProxy
                                    | MapType::Process
                                    | MapType::Profiler
                                    | MapType::Ovframe
                                    | MapType::Bvframe
                                    | MapType::Assignment
                            ) =>
                        {
                            k
                        }
                        _ => MapType::SlotsDeps,
                    }
                }
                Kind::Str => MapType::StringM,
                Kind::ByteVec => MapType::ByteVector,
                Kind::Vector => MapType::ObjVector,
                Kind::Method => {
                    let blk =
                        v.as_obj().unwrap().borrow().payload.method().is_some_and(|m| m.is_block);
                    if blk {
                        MapType::BlockMethod
                    } else {
                        MapType::OuterMethod
                    }
                }
                Kind::Block => MapType::Block,
            };

            match kind {
                Kind::Str | Kind::ByteVec => {
                    let b = v.bytes().unwrap();
                    e.put(addr + 8, as_smi(b.len() as i32));
                    e.put(addr + 12, bytes_bottom + self.byte_off[&i]);
                }
                Kind::Vector => {
                    let items: Vec<Value> =
                        v.as_obj().unwrap().borrow().payload.vector().unwrap_or_default();
                    e.put(addr + 8, as_smi(items.len() as i32));
                    let base = addr + 4 * (words - items.len()) as u32;
                    for (k, x) in items.iter().enumerate() {
                        let t = self.tagged(x)?;
                        e.put(base + 4 * k as u32, t);
                    }
                }
                _ => {}
            }

            let mut method_fields: Option<Vec<u32>> = None;
            if kind == Kind::Method {
                let m = match v.as_obj().unwrap().borrow().payload.method() {
                    Some(m) => m,
                    _ => unreachable!(),
                };
                let (lits, code, file, source) =
                    self.mparts[&(Rc::as_ptr(&m) as *const u8 as usize)];
                {
                    let mut mf = vec![
                        self.items[code].addr | MEM_TAG,
                        self.items[file].addr | MEM_TAG,
                        as_smi(m.line as i32),
                        self.items[source].addr | MEM_TAG,
                        self.items[lits].addr | MEM_TAG,
                    ];
                    if m.is_block {
                        // blockMethodMap adds _sourceOffset and _sourceLen
                        let (o, l) = m.source.get().as_ref().map_or((0, 0), |&(_, o, l)| (o, l));
                        mf.push(as_smi(o as i32));
                        mf.push(as_smi(l as i32));
                    }
                    method_fields = Some(mf);
                    for (k, name) in m.slot_names.iter().enumerate() {
                        let ni = self.string(name.as_bytes());
                        let nm = self.items[ni].addr | MEM_TAG;
                        let t = m.slot_flags.get(k).copied().unwrap_or(0);
                        let init = self.tagged(&m.slot_inits.borrow()[k])?;
                        if (t >> 2) & 3 == 2 {
                            // an arg slot's data is its position in the arglist
                            let ai = m.arg_slots.iter().position(|&x| x == k).unwrap_or(0);
                            descs.extend_from_slice(&[nm, t, as_smi(ai as i32), nil]);
                        } else if (t >> 2) & 3 == 1 {
                            // a constant slot keeps its value in the map
                            descs.extend_from_slice(&[nm, t, init, nil]);
                        } else {
                            descs.extend_from_slice(&[nm, t, as_smi(next_obj_word as i32), nil]);
                            e.put(addr + 4 * next_obj_word as u32, init);
                            next_obj_word += 1;
                        }
                    }
                }
            }

            if kind != Kind::Method && kind != Kind::Block {
                let at = v.as_obj().unwrap();
                let slots: Vec<Slot> = at.borrow().slots.iter().collect();
                for (si, s) in slots.iter().enumerate() {
                    let ni = self.string(sym_str(s.name).as_bytes());
                    let nm = self.items[ni].addr | MEM_TAG;
                    let sa = match crate::value::slot_anno(at, si) {
                        Some(a) => self.tagged(&a)?,
                        None => nil,
                    };
                    if s.kind == SlotKind::Assign {
                        let _ = sa;
                        continue; // derived from the data slot being an obj slot
                    } else if obj_slots.iter().any(|n| *n == s.name) {
                        descs.extend_from_slice(&[
                            nm,
                            slot_type_bits(SlotClass::Obj, s.kind == SlotKind::Parent, false),
                            as_smi(next_obj_word as i32),
                            sa,
                        ]);
                        let t = self.tagged(&s.value)?;
                        e.put(addr + 4 * next_obj_word as u32, t);
                        next_obj_word += 1;
                    } else {
                        let t = self.tagged(&s.value)?;
                        descs.extend_from_slice(&[
                            nm,
                            slot_type_bits(SlotClass::Const, s.kind == SlotKind::Parent, false),
                            t,
                            sa,
                        ]);
                    }
                }
            }

            let nslots = descs.len() / 4;
            let body: Vec<u32> = if kind == Kind::Block {
                let m = match v.as_obj().unwrap().borrow().payload.method() {
                    Some(m) => m,
                    _ => unreachable!(),
                };
                let vsel = crate::compile::block_selector(m.nargs);
                let vi = self.string(vsel.as_bytes());
                let vname = self.items[vi].addr | MEM_TAG;
                // the value method is whatever the block's value slot holds
                let mv = v
                    .as_obj()
                    .unwrap()
                    .borrow()
                    .slots
                    .iter()
                    .find(|s| s.value.method().is_some())
                    .map(|s| s.value)
                    .ok_or("block object has no value method slot")?;
                let vmeth = self.tagged(&mv)?;
                e.put(addr + 8, as_smi(0)); // scopeHomeFr: no live home
                vec![VTBL_SENTINEL + MapType::Block as u32 * 4, as_smi(0), vname, vmeth]
            } else {
                let anno = match crate::value::obj_anno(v.as_obj().unwrap()) {
                    Some(a) => self.tagged(&a)?,
                    None => nil,
                };
                let mut b = vec![
                    VTBL_SENTINEL + map_kind as u32 * 4,
                    as_smi(
                        words as i32 - if kind == Kind::Vector { vec_len(&v) as i32 } else { 0 },
                    ),
                    as_smi(nslots as i32),
                    anno,
                ];
                match &method_fields {
                    Some(f) => b.extend_from_slice(f),
                    None => b.extend_from_slice(&[0, 0, 0, 0, 0]),
                }
                b.extend_from_slice(&descs);
                b
            };
            let hm = self.hash();
            let mo = intern(body, &mut maps, &mut map_words, hm);
            e.put(addr + 4, mo);
        }

        // 3. every mapOop's own map is map_map
        let mut i = 0usize;
        while i < map_words.len() {
            let a = map_base + 4 * i as u32;
            // the assignment object shares this region but is not a map
            let size = if a | MEM_TAG == assign_obj {
                2
            } else {
                map_words[i + 1] = map_map;
                2 + map_body_len(&map_words[i + 2..])
            };
            i += size;
        }

        if self.items.len() != frozen {
            return Err("internal: an object was interned after layout".into());
        }
        let mut objs = e.words;
        objs.extend_from_slice(&map_words);
        let objs_top = OLD_BASE + 4 * objs.len() as u32;
        if objs_top > bytes_bottom {
            return Err("heap layout overflowed the old space".into());
        }

        let old = Space {
            objs_bottom: OLD_BASE,
            objs_top,
            bytes_bottom,
            bytes_top,
            objs,
            bytes: self.bytes.clone(),
        };
        let empty = |a: u32| Space {
            objs_bottom: a,
            objs_top: a,
            bytes_bottom: a,
            bytes_top: a,
            objs: vec![],
            bytes: vec![],
        };

        let mut vm_oops = vec![nil; VM_OOP_NAMES.len()];
        for (k, r) in roots.iter().enumerate() {
            vm_oops[k] = self.tagged(r)?;
        }
        vm_oops[5] = assign_obj; // assignmentObj

        let mut string_table = vec![vec![]; STRING_TABLE_SIZE];
        for (text, &i) in &self.strs {
            let b = (string_hash(text) % STRING_TABLE_SIZE as u32) as usize;
            string_table[b].push(self.items[i].addr | MEM_TAG);
        }
        let vm_strings: Vec<u32> = match &self.vm.image_strings {
            Some(vs) => vs.iter().map(|v| self.tagged(v)).collect::<Result<_, _>>()?,
            None => VM_STRINGS
                .iter()
                .map(|s| {
                    self.items[*self.strs.get(s.as_bytes()).expect("VM string not interned")].addr
                        | MEM_TAG
                })
                .collect(),
        };

        Ok(Snapshot {
            major: VM_MAJOR,
            minor: VM_MINOR,
            version: SNAPSHOT_VERSION,
            timestamp: self.vm.timestamp as i32,
            snapshot_code: false,
            vm_date: "serf".into(),
            sizes: [
                EDEN as i32,
                SURV as i32,
                old_size as i32,
                1024 * 1024,
                512 * 1024,
                512 * 1024,
                512 * 1024,
            ],
            page_aligned: false,
            maps_canonical: true,
            vm_oops,
            tenuring_threshold: as_smi(8),
            // MAP_READ_SNAPSHOT_TEMPLATE takes these as Map*, not as tagged
            // mapOops: the map body starts two words into its object
            vm_maps: [smi_map, float_map, mark_map, map_map].iter().map(|m| (m & !3) + 8).collect(),
            new_gen: vec![
                empty(0x0400_0000),
                empty(0x0400_0000 + EDEN),
                empty(0x0400_0000 + EDEN + SURV),
            ],
            old: vec![old],
            string_table,
            vm_strings,
            vtbls: (0..20).map(|i| VTBL_SENTINEL + i * 4).collect(),
            was_swapped: false,
        })
    }
}

fn vec_len(v: &Value) -> usize {
    match v.as_obj().map(|o| match o.borrow().payload.kind() {
        PayKind::Vector => o.borrow().payload.vector_len(),
        _ => 0,
    }) {
        Some(n) => n,
        None => 0,
    }
}

fn slot_count(v: &Value) -> usize {
    v.as_obj().map(|o| o.borrow().slots.len()).unwrap_or(0)
}

/// Words of a map body (everything after the mapOop's mark and map words).
fn map_body_len(body: &[u32]) -> usize {
    let k =
        MapType::from_index(((body[0] - VTBL_SENTINEL) / 4) as usize).unwrap_or(MapType::SlotsDeps);
    match k {
        MapType::Smi | MapType::Float => 5,
        MapType::Mark | MapType::MapM => 1,
        MapType::Block => 4,
        _ => slots_offset(k) + 4 * smi(body[2]).max(0) as usize,
    }
}

/// Build a snapshot: of a loaded image's roots if one is present, else of
/// serf's own world.
pub fn build(vm: &Vm) -> Result<Snapshot, String> {
    // the builder's work list is a Rust local holding every object it has laid
    // out so far, and it allocates strings and vectors as it goes
    let _g = crate::gc::NoGc::new();
    let mut b = Builder::new(vm);
    if let Some(vs) = &vm.image_strings {
        // reuse the image's own canonical strings, so identities survive
        for v in vs.clone() {
            b.visit(&v)?;
        }
    }
    for s in VM_STRINGS {
        b.string(s.as_bytes());
    }
    b.string(b"parent");
    if let Some(rs) = &vm.image_roots {
        let roots = rs.clone();
        for r in &roots {
            b.visit(r)?;
        }
        return b.finish(&roots);
    }
    let empty_str = vm.string("");
    let empty_vec = vm.vector(vec![]);
    let empty_bytes = vm.bytes_with(vm.t_string.clone(), vec![]);
    let mut roots =
        vec![vm.lobby.clone(), vm.nil.clone(), vm.tru.clone(), vm.fals.clone(), empty_str];
    roots.push(vm.nil.clone()); // assignmentObj, patched in finish
    roots.push(empty_vec.clone());
    roots.push(empty_bytes);
    while roots.len() < VM_OOP_NAMES.len() {
        roots.push(vm.nil.clone());
    }
    roots[8] = vm.t_block.clone(); // blockTraitsObj
    roots[37] = empty_vec; // objectIDArray
    for r in &roots {
        b.visit(r)?;
    }
    b.finish(&roots)
}
