//! Primitives. Names follow the C++ VM where an equivalent exists
//! (`_IntAdd:`, `_Clone`, `_AddSlots:`, `_Perform:`, ...).
//!
//! ponytail: integer primitives promote to float when either side is a float,
//! so the Self-level `+` needs no coercion protocol.

use std::io::{BufRead, Write};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::value::*;

/// `struct tm`, as far as `_DateTime:` needs it. The trailing gmtoff/zone
/// fields are left off: nothing here reads past `tm_isdst`.
#[repr(C)]
struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
}

extern "C" {
    #[link_name = "localtime"]
    fn libc_localtime(t: *const i64) -> *const Tm;
    #[link_name = "gmtime"]
    fn libc_gmtime(t: *const i64) -> *const Tm;
}

/// Unix errno names, as ErrorCodes::os_error_name answers them on macOS
/// (vm/src/unix/os/errorCodes_unix.cpp). A failing `int_or_errno` primitive
/// answers this name and the world reads it: `while_EINTR_do:IfFail:` retries
/// on 'EINTR', and the console waits when a non-blocking read says 'EAGAIN'.
const ERRNO_NAMES: &[&str] = &[
    "NOERR", "EPERM", "ENOENT", "ESRCH", "EINTR", "EIO", "ENXIO", "E2BIG", "ENOEXEC", "EBADF",
    "ECHILD", "EDEADLK", "ENOMEM", "EACCES", "EFAULT", "ENOTBLK", "EBUSY", "EEXIST", "EXDEV", "ENODEV",
    "ENOTDIR", "EISDIR", "EINVAL", "ENFILE", "EMFILE", "ENOTTY", "ETXTBSY", "EFBIG", "ENOSPC", "ESPIPE",
    "EROFS", "EMLINK", "EPIPE", "EDOM", "ERANGE", "EAGAIN", "EINPROGRESS", "EALREADY", "ENOTSOCK", "EDESTADDRREQ",
    "EMSGSIZE", "EPROTOTYPE", "ENOPROTOOPT", "EPROTONOSUPPORT", "ESOCKTNOSUPPORT", "ENOTSUP", "EPFNOSUPPORT", "EAFNOSUPPORT", "EADDRINUSE", "EADDRNOTAVAIL",
    "ENETDOWN", "ENETUNREACH", "ENETRESET", "ECONNABORTED", "ECONNRESET", "ENOBUFS", "EISCONN", "ENOTCONN", "ESHUTDOWN", "ETOMANYREFS",
    "ETIMEDOUT", "ECONNREFUSED", "ELOOP", "ENAMETOOLONG", "EHOSTDOWN", "EHOSTUNREACH", "ENOTEMPTY", "EPROCLIM", "EUSERS", "EDQUOT",
    "ESTALE", "EREMOTE", "EBADRPC", "ERPCMISMATCH", "EPROGUNAVAIL", "EPROGMISMATCH", "EPROCUNAVAIL", "ENOLCK", "ENOSYS", "EFTYPE",
    "EAUTH", "ENEEDAUTH", "EPWROFF", "EDEVERR", "EOVERFLOW", "EBADEXEC", "EBADARCH", "ESHLIBVERS", "EBADMACHO",
];

extern "C" {
    #[link_name = "__error"]
    fn libc_errno() -> *mut i32;
}

fn os_error() -> String {
    let e = unsafe { *libc_errno() };
    match ERRNO_NAMES.get(e as usize) {
        Some(n) => (*n).to_string(),
        None => format!("UNKNOWN {}", e),
    }
}

pub enum P {
    Val(Value),
    /// `_Yield:` -- park this stack and return to whoever ran it
    Yield(Value, Value),
    /// the primitive turned into an ordinary send (`_Perform:` and friends)
    Send(Value, String, Vec<Value>),
    /// run this method with the given receiver -- a method nobody holds in a
    /// slot, so no send can reach it (`_MirrorEvaluate:`)
    Activate(Rc<Method>, Value),
}

/// Primitive failures answer one of the VM's own error names and nothing
/// else: the world tests them with `==` as often as with `isPrefixOf:`
/// (`vector at:IfAbsent:`, `indexable if:IsIndex:...`), so a message with
/// detail appended reads to it as an unknown error. Set SERF_TRACE_PRIMS to
/// see which primitive failed.
fn as_f(v: &Value, who: &str) -> Result<f64, String> {
    let _ = who;
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        _ => Err("badTypeError".into()),
    }
}
fn as_i(v: &Value, who: &str) -> Result<i64, String> {
    let _ = who;
    match v {
        Value::Int(i) => Ok(*i),
        _ => Err("badTypeError".into()),
    }
}
fn as_obj<'a>(v: &'a Value, who: &str) -> Result<&'a ObjRef, String> {
    let _ = who;
    v.as_obj().ok_or_else(|| "badTypeError".to_string())
}
fn as_name(v: &Value, who: &str) -> Result<String, String> {
    let _ = who;
    v.as_str().ok_or_else(|| "badTypeError".to_string())
}

fn arith(
    who: &str,
    a: &Value,
    b: &Value,
    fi: fn(i64, i64) -> Result<i64, String>,
    ff: fn(f64, f64) -> f64,
) -> Result<Value, String> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => fi(*x, *y).map(Value::Int),
        _ => Ok(Value::Float(ff(as_f(a, who)?, as_f(b, who)?))),
    }
}

fn order(who: &str, a: &Value, b: &Value) -> Result<std::cmp::Ordering, String> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
        _ => {
            let (x, y) = (as_f(a, who)?, as_f(b, who)?);
            x.partial_cmp(&y).ok_or_else(|| format!("{}: cannot order NaN", who))
        }
    }
}

fn ovf(who: &'static str) -> impl Fn(Option<i64>) -> Result<i64, String> {
    move |o| o.ok_or_else(|| format!("{}: integer overflow", who))
}

fn hash_of(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        Value::Float(f) => f.to_bits() as i64,
        Value::Obj(o) => {
            if let Payload::Bytes(b) = &o.borrow().payload {
                let mut h: u64 = 0xcbf29ce484222325;
                for c in b {
                    h = (h ^ *c as u64).wrapping_mul(0x100000001b3);
                }
                return (h >> 1) as i64;
            }
            (Rc::as_ptr(o) as usize >> 3) as i64
        }
    }
}

/// How many slots the scheduler's TWAINS result vector needs:
/// `2 * n_real_SelfSignals + 1`, as PendingSelfSignals::Self_result_size.
const TWAINS_RESULT_SIZE: i64 = 27;

/// Flags the C++ VM exposes to tune its compilers and diagnostics.
fn is_vm_flag(name: &str) -> bool {
    const FLAGS: &[&str] = &[
        "_CheckAssertions", "_WizardMode", "_NakedMethods", "_PrintCompiledCode",
        "_VMProfiling", "_VMCompilerProfiling", "_UseLRUInterrupts", "_Inline",
        "_CountSends", "_SaveOutgoingArgsOfPatchedFrames", "_Trace", "_Spy",
        "_LogVMMessages", "_PrintFlags", "_Verbose", "_TraceLookup",
        "_DebugPrims", "_SICDeferUncommonBranches", "_UseInlineCaching",
        "_CompileWithSICNames", "_BugHunt", "_ShowSICCode", "_PrintInlining",
    ];
    let base = name.trim_end_matches(':');
    FLAGS.contains(&base)
}

/// Follow a chain of names from the image's lobby, using the real lookup so
/// inherited slots resolve.
fn lookup_path(vm: &Vm, path: &[&str]) -> Option<Value> {
    let mut cur = vm.image_roots.as_ref()?[0].clone();
    for name in path {
        let hit = vm.lookup(&cur, name).ok()?;
        let next = hit.holder.borrow().slots[hit.idx].value.clone();
        cur = next;
    }
    Some(cur)
}

fn make_mirror(vm: &Vm, on: &Value) -> Result<Value, String> {
    let roots = vm.image_roots.as_ref().ok_or("_Mirror needs a loaded image")?;
    let proto = roots[mirror_proto_index(vm, on)].clone();
    let slots = proto.as_obj().ok_or("reflectTypeError")?.borrow().slots.clone();
    Ok(Value::obj(slots, Payload::Mirror(on.clone())))
}

fn reflectee(v: &Value) -> Result<Value, String> {
    match &v.as_obj().ok_or("reflectTypeError")?.borrow().payload {
        Payload::Mirror(r) => Ok(r.clone()),
        _ => Err("reflectTypeError".into()),
    }
}

/// A mirror's argument is a mirror on the contents, but primitives that take
/// plain objects are lenient about it -- as C++ is, since `contMirror` there
/// is typed.
fn unmirror(v: &Value) -> Value {
    reflectee(v).unwrap_or_else(|_| v.clone())
}

/// The slot a mirror primitive means by `name`, as `Map::find_nonVM_slot`
/// does: an assignment name (`x:`) names the data slot it writes. Answers the
/// slot's index and whether it was reached by its assignment name.
fn slot_desc(slots: &[Slot], n: &str) -> Result<(usize, bool), String> {
    let i = slots.iter().position(|s| &*s.name == n).ok_or("slotNameError")?;
    if slots[i].kind != SlotKind::Assign {
        return Ok((i, false));
    }
    let base = &n[..n.len() - 1];
    let j = slots
        .iter()
        .position(|s| &*s.name == base && s.kind != SlotKind::Assign)
        .ok_or("slotNameError")?;
    Ok((j, true))
}

fn clone_payload(p: &Payload) -> Payload {
    match p {
        Payload::None => Payload::None,
        Payload::Bytes(b) => Payload::Bytes(b.clone()),
        Payload::Vector(x) => Payload::Vector(x.clone()),
        Payload::Method(m) => Payload::Method(m.clone()),
        Payload::Block(m, s) => Payload::Block(m.clone(), s.clone()),
        Payload::Mirror(r) => Payload::Mirror(r.clone()),
        Payload::Proxy(p) => Payload::Proxy(*p),
    }
}

/// The pointer of a *live* proxy. A dead one, or a non-proxy, answers None.
fn proxy_ptr(v: &Value) -> Option<u64> {
    match &v.as_obj()?.borrow().payload {
        Payload::Proxy(p) => *p,
        _ => None,
    }
}

/// Self keeps an object's identity hash in its mark word, so it survives a
/// snapshot; an object loaded from one must keep the hash it had, or every
/// hashed collection in the image loses track of its own keys. Objects made
/// since then get one from their address, in the same 22 bits.
pub fn identity_hash(vm: &Vm, v: &Value) -> i64 {
    match v {
        Value::Obj(o) => match vm.id_hash.get(&(Rc::as_ptr(o) as usize)) {
            Some(h) => *h,
            None => (Rc::as_ptr(o) as usize >> 3) as i64 & 0x3f_ffff,
        },
        _ => hash_of(v),
    }
}

fn key_of(v: &Value) -> usize {
    match v {
        Value::Obj(o) => Rc::as_ptr(o) as usize,
        _ => 0,
    }
}

fn set_result(vm: &Vm, vec: &Value, items: &[Value]) -> Result<(), String> {
    let o = vec.as_obj().ok_or("TWAINS result must be a vector")?;
    let mut b = o.borrow_mut();
    match &mut b.payload {
        Payload::Vector(x) => {
            for (i, it) in items.iter().enumerate() {
                if i < x.len() {
                    x[i] = it.clone();
                }
            }
            let _ = vm;
            Ok(())
        }
        _ => Err("TWAINS result must be a vector".into()),
    }
}

fn indexable_len(v: &Value, who: &str) -> Result<usize, String> {
    match &as_obj(v, who)?.borrow().payload {
        Payload::Bytes(b) => Ok(b.len()),
        Payload::Vector(x) => Ok(x.len()),
        _ => Err("badTypeError".into()),
    }
}

/// Which of the image's mirror prototypes fits this object (Map::mirror_proto).
fn mirror_proto_index(vm: &Vm, v: &Value) -> usize {
    match v {
        Value::Int(_) => 28,   // smiMirrorObj
        Value::Float(_) => 25, // floatMirrorObj
        Value::Obj(o) => {
            let b = o.borrow();
            match &b.payload {
                Payload::Bytes(_) => {
                    if b.slots.iter().any(|s| s.kind == SlotKind::Parent && s.value.id_eq(&vm.t_string)) { 29 } else { 22 }
                }
                Payload::Vector(_) => 26,
                Payload::Method(m) => if m.is_block { 24 } else { 23 },
                Payload::Block(..) => 21,
                Payload::Mirror(_) => 36,
                Payload::Proxy(_) => 33, // proxyMirrorObj
                Payload::None => 27, // slotsMirrorObj
            }
        }
    }
}

pub fn call(vm: &mut Vm, name: &str, recv: &Value, args: &[Value]) -> Result<P, String> {
    let v = |x: Value| Ok(P::Val(x));
    match name {
        // ------------------------------------------------------ arithmetic
        "_IntAdd:" => v(arith(name, recv, &args[0], |a, b| ovf("_IntAdd:")(a.checked_add(b)), |a, b| a + b)?),
        "_IntSub:" => v(arith(name, recv, &args[0], |a, b| ovf("_IntSub:")(a.checked_sub(b)), |a, b| a - b)?),
        "_IntMul:" => v(arith(name, recv, &args[0], |a, b| ovf("_IntMul:")(a.checked_mul(b)), |a, b| a * b)?),
        "_IntDiv:" => v(arith(
            name,
            recv,
            &args[0],
            |a, b| {
                if b == 0 {
                    Err("divisionByZeroError".into())
                } else {
                    ovf("_IntDiv:")(a.checked_div(b))
                }
            },
            |a, b| a / b,
        )?),
        "_IntMod:" => v(arith(
            name,
            recv,
            &args[0],
            |a, b| {
                if b == 0 {
                    Err("divisionByZeroError".into())
                } else {
                    ovf("_IntMod:")(a.checked_rem(b))
                }
            },
            |a, b| a % b,
        )?),
        "_IntLT:" => v(vm.boolean(order(name, recv, &args[0])?.is_lt())),
        "_IntLE:" => v(vm.boolean(order(name, recv, &args[0])?.is_le())),
        "_IntGT:" => v(vm.boolean(order(name, recv, &args[0])?.is_gt())),
        "_IntGE:" => v(vm.boolean(order(name, recv, &args[0])?.is_ge())),
        "_IntEQ:" => v(vm.boolean(order(name, recv, &args[0])?.is_eq())),
        "_IntNE:" => v(vm.boolean(order(name, recv, &args[0])?.is_ne())),
        "_IntAnd:" => v(Value::Int(as_i(recv, name)? & as_i(&args[0], name)?)),
        "_IntOr:" => v(Value::Int(as_i(recv, name)? | as_i(&args[0], name)?)),
        "_IntXor:" => v(Value::Int(as_i(recv, name)? ^ as_i(&args[0], name)?)),
        "_IntArithmeticShiftLeft:" => {
            let (a, b) = (as_i(recv, name)?, as_i(&args[0], name)?);
            v(Value::Int(if b >= 64 { 0 } else { a << b }))
        }
        "_IntLogicalShiftLeft:" => {
            let (a, b) = (as_i(recv, name)? as u64, as_i(&args[0], name)?);
            v(Value::Int(if b >= 64 { 0 } else { (a << b) as i64 }))
        }
        "_IntLogicalShiftRight:" => {
            let (a, b) = (as_i(recv, name)? as u64, as_i(&args[0], name)?);
            v(Value::Int(if b >= 64 { 0 } else { (a >> b) as i64 }))
        }
        "_IntArithmeticShiftRight:" => {
            let (a, b) = (as_i(recv, name)?, as_i(&args[0], name)?);
            v(Value::Int(if b >= 64 { if a < 0 { -1 } else { 0 } } else { a >> b }))
        }
        // ---------------------------------------------------------- floats
        "_FloatAdd:" => v(Value::Float(as_f(recv, name)? + as_f(&args[0], name)?)),
        "_FloatSub:" => v(Value::Float(as_f(recv, name)? - as_f(&args[0], name)?)),
        "_FloatMul:" => v(Value::Float(as_f(recv, name)? * as_f(&args[0], name)?)),
        "_FloatDiv:" => {
            let d = as_f(&args[0], name)?;
            if d == 0.0 {
                return Err("divisionByZeroError".into());
            }
            v(Value::Float(as_f(recv, name)? / d))
        }
        "_FloatMod:" => {
            let d = as_f(&args[0], name)?;
            if d == 0.0 {
                return Err("divisionByZeroError".into());
            }
            v(Value::Float(as_f(recv, name)? % d))
        }
        "_FloatLT:" => v(vm.boolean(as_f(recv, name)? < as_f(&args[0], name)?)),
        "_FloatLE:" => v(vm.boolean(as_f(recv, name)? <= as_f(&args[0], name)?)),
        "_FloatGT:" => v(vm.boolean(as_f(recv, name)? > as_f(&args[0], name)?)),
        "_FloatGE:" => v(vm.boolean(as_f(recv, name)? >= as_f(&args[0], name)?)),
        "_FloatEQ:" => v(vm.boolean(as_f(recv, name)? == as_f(&args[0], name)?)),
        "_FloatNE:" => v(vm.boolean(as_f(recv, name)? != as_f(&args[0], name)?)),
        "_FloatAsInt" | "_FloatTruncate" => v(Value::Int(as_f(recv, name)?.trunc() as i64)),
        "_FloatFloor" => v(Value::Int(as_f(recv, name)?.floor() as i64)),
        "_FloatCeil" => v(Value::Int(as_f(recv, name)?.ceil() as i64)),
        "_FloatRound" => v(Value::Int(as_f(recv, name)?.round() as i64)),
        "_IntAsFloat" => v(Value::Float(as_i(recv, name)? as f64)),
        "_IntComplement" => v(Value::Int(!as_i(recv, name)?)),
        "_FloatFromInt32:" | "_FloatFromInt64:" => v(Value::Float(as_i(&args[0], name)? as f64)),
        "_FloatFromInt32" | "_FloatFromInt64" => v(Value::Float(as_i(recv, name)? as f64)),
        "_Int32FromFloat:" | "_Int64FromFloat:" => v(Value::Int(as_f(&args[0], name)?.trunc() as i64)),
        "_FloatPrintString" => {
            let t = fmt_float(as_f(recv, name)?);
            v(vm.string(&t))
        }
        "_FloatPrintStringPrecision:" => {
            let p = as_i(&args[0], name)?.clamp(0, 30) as usize;
            let t = format!("{:.*}", p, as_f(recv, name)?);
            v(vm.string(&t))
        }
        "_AsFloat" => v(Value::Float(as_f(recv, name)?)),
        "_AsInteger" => v(Value::Int(as_f(recv, name)? as i64)),
        "_Sqrt" => v(Value::Float(as_f(recv, name)?.sqrt())),
        "_Sin" => v(Value::Float(as_f(recv, name)?.sin())),
        "_Cos" => v(Value::Float(as_f(recv, name)?.cos())),
        "_Tan" => v(Value::Float(as_f(recv, name)?.tan())),
        "_Exp" => v(Value::Float(as_f(recv, name)?.exp())),
        "_Ln" => v(Value::Float(as_f(recv, name)?.ln())),
        "_Pow:" => v(Value::Float(as_f(recv, name)?.powf(as_f(&args[0], name)?))),
        "_Floor" => v(Value::Float(as_f(recv, name)?.floor())),
        "_Ceiling" => v(Value::Float(as_f(recv, name)?.ceil())),
        "_Truncate" => v(Value::Float(as_f(recv, name)?.trunc())),
        "_Hash" => v(Value::Int(hash_of(recv))),
        "_IdentityHash" | "_ObjectID" => v(Value::Int(identity_hash(vm, recv))),

        // -------------------------------------------------------- identity
        "_Eq:" => v(vm.boolean(recv.id_eq(&args[0]))),

        // --------------------------------------------------------- cloning
        "_Clone" => {
            let o = as_obj(recv, name)?.borrow();
            let payload = match &o.payload {
                Payload::None => Payload::None,
                Payload::Bytes(b) => Payload::Bytes(b.clone()),
                Payload::Vector(x) => Payload::Vector(x.clone()),
                Payload::Method(m) => Payload::Method(m.clone()),
                Payload::Block(m, s) => Payload::Block(m.clone(), s.clone()),
                Payload::Mirror(r) => Payload::Mirror(r.clone()),
                Payload::Proxy(p) => Payload::Proxy(*p),
            };
            v(Value::obj(o.slots.clone(), payload))
        }
        // Resize a clone. As objVectorMap::cloneSize does, the existing
        // elements are kept and only the new ones get the filler -- filling
        // the whole thing would quietly empty every copy in the system.
        "_Clone:Filler:" | "_CloneBytes:Filler:" => {
            let n = as_i(&args[0], name)?;
            if n < 0 {
                return Err("badSizeError".into());
            }
            let n = n as usize;
            let o = as_obj(recv, name)?.borrow();
            let payload = match &o.payload {
                Payload::Bytes(b) => {
                    let f = as_i(&args[1], name).unwrap_or(0) as u8;
                    let mut z = b.clone();
                    z.resize(n, f);
                    Payload::Bytes(z)
                }
                Payload::Vector(x) => {
                    let mut z = x.clone();
                    z.resize(n, args[1].clone());
                    Payload::Vector(z)
                }
                // a byte clone of a plain object starts empty
                _ if name == "_CloneBytes:Filler:" => {
                    Payload::Bytes(vec![as_i(&args[1], name).unwrap_or(0) as u8; n])
                }
                _ => return Err("badTypeError".into()),
            };
            v(Value::obj(o.slots.clone(), payload))
        }

        // ------------------------------------------------------ indexables
        "_ByteSize" => v(Value::Int(match &as_obj(recv, name)?.borrow().payload {
            Payload::Bytes(b) => b.len() as i64,
            Payload::Vector(x) => (x.len() * 4) as i64,
            _ => return Err("badTypeError".into()),
        })),
        "_ByteAt:" => call(vm, "_At:", recv, args),
        "_ByteAt:Put:" => call(vm, "_At:Put:", recv, args),
        "_ByteVectorConcatenate:Prototype:" => {
            let a = recv.bytes().ok_or("_ByteVectorConcatenate: receiver is not a byte vector")?;
            let b = args[0].bytes().ok_or("_ByteVectorConcatenate: argument is not a byte vector")?;
            let mut z = a;
            z.extend_from_slice(&b);
            let slots = as_obj(&args[1], name)?.borrow().slots.clone();
            v(Value::obj(slots, Payload::Bytes(z)))
        }
        // `string hash = canonicalize identityHash`, so this has to answer the
        // one instance the image's string table holds -- otherwise two equal
        // strings hash differently and no dictionary in the world can find its
        // own keys.
        "_StringCanonicalize" => {
            let b = recv.bytes().ok_or("badTypeError")?;
            match vm.canonical.get(&b) {
                Some(c) => v(c.clone()),
                None => {
                    vm.canonical.insert(b, recv.clone());
                    v(recv.clone())
                }
            }
        }
        // an empty proxy, so serf can hold a C pointer without an image
        "_ProxyNew" => v(Value::obj(vec![], Payload::Proxy(Some(0)))),

        // ------------------------------------------------- foreign objects
        "_ForeignIsLive" => v(vm.boolean(proxy_ptr(recv).is_some())),
        "_ForeignKill" => {
            if let Some(o) = recv.as_obj() {
                o.borrow_mut().payload = Payload::Proxy(None);
            }
            v(recv.clone())
        }
        "_ForeignHash" => v(Value::Int((proxy_ptr(recv).unwrap_or(0) >> 3) as i64)),
        "_SamePointerAs:" => v(vm.boolean(proxy_ptr(recv) == proxy_ptr(&args[0]))),
        // type seals label a proxy with the C type it points at; serf does not
        // check them, so stamping one is just handing the proxy back
        "_TypeSealResultProxy:" | "_TypeSeal:ResultProxy:" => {
            let p = proxy_ptr(recv);
            let t = args[args.len() - 1].clone();
            as_obj(&t, name)?.borrow_mut().payload = Payload::Proxy(p);
            v(t)
        }
        "_Sleep:" => {
            let ms = as_i(&args[0], name)?.max(0) as u64;
            std::thread::sleep(std::time::Duration::from_millis(ms));
            v(recv.clone())
        }
        "_ProxyIsNull" => match &as_obj(recv, name)?.borrow().payload {
            Payload::Proxy(p) => v(vm.boolean(p.unwrap_or(0) == 0)),
            _ => Err("badTypeError".into()),
        },

        // --------------------------------------------------------- mirrors
        "_Mirror" => v(make_mirror(vm, recv)?),
        "_MirrorReflectee" => v(reflectee(recv)?),
        "_MirrorReflecteeEq:" => {
            let a = reflectee(recv)?;
            let b = reflectee(&args[0]).unwrap_or_else(|_| args[0].clone());
            v(vm.boolean(a.id_eq(&b)))
        }
        "_MirrorNames" => {
            let r = reflectee(recv)?;
            let names: Vec<Value> = match r.as_obj() {
                Some(o) => o.borrow().slots.iter().map(|s| vm.string(&s.name)).collect(),
                None => vec![],
            };
            v(vm.vector(names))
        }
        // Slot attributes, as Map::mirror_is_*_at. mirrorProgramming.self's
        // copyAt:PutContents: asks for all three to carry a slot's attributes
        // over to the copy, so answering "false" by failing quietly turns
        // every parent slot it touches into a plain data slot.
        "_MirrorIsParentAt:" | "_MirrorIsAssignableAt:" | "_MirrorIsArgumentAt:" => {
            let r = reflectee(recv)?;
            let n = as_name(&args[0], name)?;
            let o = as_obj(&r, name)?;
            let b = o.borrow();
            let (i, by_assignment) = slot_desc(&b.slots, &n)?;
            let yes = !by_assignment
                && match name {
                    "_MirrorIsParentAt:" => b.slots[i].kind == SlotKind::Parent,
                    // assignable == there is an `x:` to write it with
                    "_MirrorIsAssignableAt:" => {
                        let a = format!("{}:", b.slots[i].name);
                        b.slots.iter().any(|s| s.kind == SlotKind::Assign && *s.name == *a)
                    }
                    // serf drops argument slots when it loads a method: they
                    // only mean anything inside an activation, which serf keeps
                    // in its own frames rather than as a reflectable object
                    _ => false,
                };
            v(vm.boolean(yes))
        }
        "_MirrorAnnotationAt:" => {
            let r = reflectee(recv)?;
            let n = as_name(&args[0], name)?;
            let slot = {
                let o = as_obj(&r, name)?;
                let b = o.borrow();
                let (i, _) = slot_desc(&b.slots, &n)?;
                b.slots[i].name.to_string()
            };
            // Map::get_annotation defaults to the world's empty annotation,
            // never nil: `annotationIfFail:` parses whatever comes back.
            let dflt = vm.image_roots.as_ref().map(|g| g[17].clone());
            v(vm
                .anno_slot
                .get(&(key_of(&r), slot))
                .cloned()
                .or(dflt)
                .unwrap_or_else(|| vm.nil_v()))
        }
        "_MirrorReflecteeIdentityHash" => v(Value::Int((key_of(&reflectee(recv)?) >> 3) as i64)),
        // A mirror on a *method* answers its source and its bytecodes
        // (methodMap::mirror_source and friends); a block method's source is
        // its enclosing method's, sliced by offset and length. This is what an
        // outliner shows when you expand a slot holding code. A mirror on a
        // block *object* fails, as blockMap inherits Map's refusal.
        "_MirrorSource" | "_MirrorSourceOffset" | "_MirrorSourceLength" | "_MirrorCodes"
        | "_MirrorLiterals" => {
            let r = reflectee(recv)?;
            let m = match &as_obj(&r, name)?.borrow().payload {
                Payload::Method(m) => m.clone(),
                _ => return Err("reflectTypeError".into()),
            };
            let src = m.source.as_ref();
            v(match name {
                // codes are a *string*, not a byteVector: methodMap::fix_up_method
                // ends with setCodes(new_string(...)), and the world counts on it
                // -- bytecodeFormat instructionSetForCodes: sends `firstByte`,
                // which only traits string has.
                "_MirrorCodes" => vm.bytes_with(vm.t_string.clone(), m.code.clone()),
                // the sub-blocks of a method live in its literals, which is
                // how anything walking a method reaches them
                "_MirrorLiterals" => vm.vector(m.lits.clone()),
                "_MirrorSource" => src.map_or_else(|| vm.string(""), |(s, _, _)| s.clone()),
                "_MirrorSourceOffset" => Value::Int(src.map_or(0, |&(_, o, _)| o)),
                _ => Value::Int(src.map_or(0, |&(_, _, l)| l)),
            })
        }
        "_MirrorCopyRemoveSlot:" => {
            let r = reflectee(recv)?;
            let n = as_name(&args[0], name)?;
            let (slots, payload) = {
                let o = as_obj(&r, name)?.borrow();
                (o.slots.clone(), clone_payload(&o.payload))
            };
            let (i, _) = slot_desc(&slots, &n)?;
            // an obj slot and the `x:` that writes it are one slot to Self
            let base = slots[i].name.to_string();
            let assign = format!("{}:", base);
            let kept = slots
                .into_iter()
                .filter(|s| *s.name != *base && *s.name != *assign)
                .collect();
            let copy = Value::obj(kept, payload);
            let mslots = as_obj(recv, name)?.borrow().slots.clone();
            v(Value::obj(mslots, Payload::Mirror(copy)))
        }
        "_MirrorContentsAt:" => {
            let r = reflectee(recv)?;
            let n = as_name(&args[0], name)?;
            let (i, by_assignment) = {
                let o = as_obj(&r, name)?.borrow();
                slot_desc(&o.slots, &n)?
            };
            // the contents of an `x:` is the assignment object, and the world
            // gets the one canonical mirror on it (Map::mirror_contents_at)
            if by_assignment {
                let roots = vm.image_roots.as_ref().ok_or("reflectTypeError")?;
                return v(roots[20].clone()); // assignmentMirrorObj
            }
            let got = as_obj(&r, name)?.borrow().slots[i].value.clone();
            // "Return a mirror on the contents of the specified slot"
            // (prim.cpp:1401), not the contents themselves
            v(make_mirror(vm, &got)?)
        }
        "_MirrorContentsAt:Put:" => {
            let r = reflectee(recv)?;
            let n = as_name(&args[0], name)?;
            let o = as_obj(&r, name)?;
            let i = o.borrow().find(&n);
            match i {
                Some(i) => o.borrow_mut().slots[i].value = args[1].clone(),
                None => return Err("slotNameError".into()),
            }
            v(recv.clone())
        }
        // "The receiver is a mirror on the object to be changed, the argument
        // is an object defining the new contents" (prim.cpp:1444). The
        // reflectee keeps its identity and takes on the new slots.
        "_MirrorDefine:" => {
            let r = reflectee(recv)?;
            let src = reflectee(&args[0]).unwrap_or_else(|_| args[0].clone());
            let (slots, payload) = {
                let o = as_obj(&src, name)?.borrow();
                (o.slots.clone(), clone_payload(&o.payload))
            };
            {
                let d = as_obj(&r, name)?;
                let mut b = d.borrow_mut();
                b.slots = slots;
                b.payload = payload;
            }
            // A successful define is the world's one programming change, so it
            // is the only thing that moves the timestamp -- `define_prim` holds
            // the C++ VM's only `increment_programming_timestamp` call
            // (objects/oop.cpp). Caches keyed on it refill when it moves: a
            // module is obsolete when `savedTimestamp /= freezingTimestamp`,
            // and the outliner's button cache when it differs from the fill
            // time, which reads back as 0 where it was never written.
            vm.timestamp += 1;
            v(recv.clone())
        }
        // "Evaluates the method in the context of the reflectee of the
        // receiver" (prim.cpp:1449): how the world runs a parsed expression.
        "_MirrorEvaluate:" => {
            let m = reflectee(&args[0])?.method().ok_or("badTypeError")?;
            if m.nargs != 0 {
                return Err("badTypeError".into());
            }
            Ok(P::Activate(m, reflectee(recv)?))
        }
        "_MirrorAnnotation" => {
            let r = reflectee(recv)?;
            let dflt = vm.image_roots.as_ref().map(|g| g[18].clone());
            v(vm.anno_obj.get(&key_of(&r)).cloned().or(dflt).unwrap_or_else(|| vm.nil_v()))
        }
        "_MirrorCopyAnnotation:" => {
            let r = reflectee(recv)?;
            vm.anno_obj.insert(key_of(&r), args[0].clone());
            v(recv.clone())
        }
        // add or replace a slot on a copy of the reflectee, answering a
        // mirror on that copy
        "_MirrorCopyAt:Put:IsParent:IsArgument:Annotation:" | "_MirrorCopyAddSlot:Contents:IsParent:IsArgument:" => {
            let r = reflectee(recv)?;
            let n = as_name(&args[0], name)?;
            let parent = args.get(2).map_or(false, |b| b.id_eq(&vm.boolean(true)));
            // the contents come as a mirror on them, not as themselves
            let contents = unmirror(&args[1]);
            let (slots, payload) = {
                let o = as_obj(&r, name)?.borrow();
                (o.slots.clone(), clone_payload(&o.payload))
            };
            let copy = Value::obj(slots, payload);
            // storing the assignment object is how Self says "make this slot
            // assignable" (slotsMap::copy_add_slot)
            let assignment = vm.image_roots.as_ref().map_or(false, |g| contents.id_eq(&g[5]));
            let kind = match (assignment, parent) {
                (true, _) => SlotKind::Assign,
                (_, true) => SlotKind::Parent,
                _ => SlotKind::Data,
            };
            copy.as_obj().unwrap().borrow_mut().put(Slot {
                name: n.as_str().into(),
                kind,
                value: if assignment { vm.nil_v() } else { contents },
            });
            if let Some(a) = args.get(4) {
                vm.anno_slot.insert((key_of(&copy), n.clone()), a.clone());
            }
            let mslots = as_obj(recv, name)?.borrow().slots.clone();
            v(Value::obj(mslots, Payload::Mirror(copy)))
        }

        // serf has no Self-level process stacks to print; let the world's
        // error reporter carry on so its message actually reaches stdout
        "_PrintProcessStack" | "_PrintProcessStacks" | "_Flush" | "_Breakpoint" => v(recv.clone()),
        // serf interprets everything, so "run this interpreted" is just a call
        "_Interpret" => v(vm.boolean(true)),

        // ------------------------------------------------------- processes
        // Self's coroutines: the scheduler runs a process with TWAINS, the
        // process transfers back with _Yield:. See objects/core/scheduler.self
        // and processOopClass::get_result in the C++ VM.
        "_Yield:" => Ok(P::Yield(recv.clone(), args[0].clone())),
        "_Yield" => Ok(P::Yield(recv.clone(), vm.nil_v())),
        "_TWAINSResultSize" => v(Value::Int(TWAINS_RESULT_SIZE)),

        // VM tuning flags. serf has no compiler to tune, so these are plain
        // settable values: the getter answers what was last set.
        _ if is_vm_flag(name) => {
            let key = name.trim_end_matches(':').to_string();
            if name.ends_with(':') {
                let old = vm.flags.get(&key).cloned().unwrap_or_else(|| vm.boolean(false));
                vm.flags.insert(key, args[0].clone());
                v(old)
            } else {
                let d = vm.boolean(false);
                v(vm.flags.get(&key).cloned().unwrap_or(d))
            }
        }
        "_AbortProcess" => {
            let k = key_of(recv);
            vm.procs.remove(&k);
            // aborting the process we are running unwinds it; the TWAINS
            // wrapper reports that to the scheduler as 'aborted'
            if vm.current_proc.as_ref().map_or(false, |p| key_of(p) == k) {
                return Err("processAborted".into());
            }
            v(recv.clone())
        }
        // serf delivers no Self-level signals, but the world blocks and
        // unblocks them around critical sections and wants the old state back
        "_BlockSignals" => {
            let old = vm.signals_blocked;
            vm.signals_blocked = !recv.id_eq(&vm.boolean(false));
            v(vm.boolean(old))
        }
        "_SetRealTimer" | "_SetCPUTimer" | "_StopRealTimer" | "_StopCPUTimer"
        | "_ActivateCPUTimer" | "_SetRealTimer:" | "_SetCPUTimer:" => v(recv.clone()),

        "_NewProcessSize:Receiver:Selector:Arguments:" => {
            let sel = as_name(&args[2], name)?;
            let a = match &as_obj(&args[3], name)?.borrow().payload {
                Payload::Vector(x) => x.clone(),
                _ => return Err("_NewProcessSize: Arguments: must be a vector".into()),
            };
            // shaped like the image's process prototype so the world's
            // process protocol finds its slots
            let proto = vm.image_roots.as_ref().map(|r| r[10].clone());
            let slots = match &proto {
                Some(p) => as_obj(p, name)?.borrow().slots.clone(),
                None => vec![],
            };
            let obj = Value::obj(slots, Payload::None);
            // A process starts newborn. Cloning the prototype carries over
            // whatever status it was saved with, and the scheduler then finds
            // a `ready` process on no queue.
            if let Some(nb) = lookup_path(vm, &["processStatus", "newborn"]) {
                if let Some(o) = obj.as_obj() {
                    let i = o.borrow().find("processStatus0");
                    if let Some(i) = i {
                        o.borrow_mut().slots[i].value = nb;
                    }
                }
            }
            let stack = crate::interp::stack_for_send(vm, args[1].clone(), &sel, a);
            vm.procs.insert(key_of(&obj), stack);
            v(obj)
        }

        "_TWAINSResultVector:SingleStep:StopAt:" => {
            vm.scheduler_running = true;
            let k = key_of(recv);
            let mut stack = match vm.procs.remove(&k) {
                Some(s) => s,
                // TWAINS on the process you are already in means "wait for the
                // next signal" (processOopClass::TWAINS_await_signal), which is
                // how `scheduler schedule` idles when its readyQ is empty. serf
                // has no signal layer, so it sleeps a tick and reports the two
                // the world drives itself with: `sigio` makes it select over
                // its async files, `sigrealtimer` makes it wake timerWaiters.
                // Both are safe to report when nothing happened -- the handlers
                // check. ponytail: a 10ms poll instead of real SIGIO/SIGALRM;
                // if idle CPU matters, wait on the async fds with a timeout.
                None if vm.current_proc.is_none() => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    let items = [
                        Value::Int(2),
                        vm.string("sigio"),
                        Value::Int(1),
                        vm.string("sigrealtimer"),
                        Value::Int(1),
                    ];
                    set_result(vm, &args[0], &items)?;
                    return v(vm.string("signal"));
                }
                None => return Err("noProcessError".into()),
            };
            let prev = vm.current_proc.replace(recv.clone());
            let outcome = crate::interp::run_stack(vm, &mut stack);
            vm.current_proc = prev;
            let cause = match outcome {
                Ok(crate::interp::Outcome::Yielded { rcvr, arg }) => {
                    set_result(vm, &args[0], &[rcvr, arg])?;
                    vm.procs.insert(k, stack);
                    "yielded"
                }
                Ok(crate::interp::Outcome::Done(r)) => {
                    if let Some(o) = recv.as_obj() {
                        let i = o.borrow().find("returnValue");
                        if let Some(i) = i {
                            o.borrow_mut().slots[i].value = r;
                        }
                    }
                    "terminated"
                }
                Err(u) => {
                    eprintln!("process aborted: {}", crate::interp::describe(vm, u));
                    "aborted"
                }
            };
            v(vm.string(cause))
        }
        // serf runs one Self process; hand back the image's process prototype
        // so the world's process protocol has something of the right shape
        "_ThisProcess" => {
            if let Some(p) = vm.current_proc.clone() {
                return v(p);
            }
            // Outside any Self process the C++ VM answers the process object
            // of its own main loop, which is Memory->processObj -- the very
            // prototype the world keeps in `globals process`. The world tests
            // for it (`isScheduled = process != process this`) to decide
            // whether it is running inside the scheduler, so a stand-in here
            // makes the scheduler think it is a scheduled process and yield.
            if let Some(p) = vm.image_roots.as_ref().map(|r| r[10].clone()) {
                return v(p);
            }
            if vm.vm_proc.is_none() {
                vm.vm_proc = Some(Value::obj(vec![], Payload::None));
            }
            v(vm.vm_proc.clone().unwrap())
        }
        // traits time currentTime reads this as (day. msecInDay. )
        "_TimeReal" | "_TimeUser" | "_TimeSystem" | "_TimeCPU" => {
            let t = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?;
            let secs = t.as_secs() as i64;
            let day = secs / 86400;
            let msec = (secs % 86400) * 1000 + t.subsec_millis() as i64;
            v(vm.vector(vec![Value::Int(day), Value::Int(msec)]))
        }
        // 18 integers: nine for local time, nine for UTC, in the order
        // OS::setDateTimeBuf writes them. libc does the timezone and DST work.
        "_DateTime:" => {
            let day = as_i(recv, name)?;
            let msec = as_i(&args[0], name)?;
            let t: i64 = day * 86400 + msec / 1000;
            let mut out = Vec::with_capacity(18);
            for utc in [false, true] {
                let tm = unsafe {
                    let p = if utc { libc_gmtime(&t) } else { libc_localtime(&t) };
                    p.as_ref().ok_or("primitiveFailedError")?
                };
                out.extend(
                    [
                        tm.year + 1900,
                        tm.mon + 1,
                        tm.mday,
                        tm.wday,
                        tm.hour,
                        tm.min,
                        tm.sec,
                        tm.yday,
                        tm.isdst,
                    ]
                    .map(|x| Value::Int(x as i64)),
                );
            }
            v(vm.vector(out))
        }
        // Only the program name: serf's own arguments are not Self's, and the
        // world warns about every one it does not recognise -- which drags
        // systemLog in before the world is ready to log anything.
        "_CommandLine" => {
            let a = std::env::args().next().unwrap_or_default();
            let n = vm.string(&a);
            v(vm.vector(vec![n]))
        }
        // Not 'macOSX': that selects quartzGlobals, and serf's graphics go
        // through libX11 (worldMorph.self:2665).
        "_OperatingSystem" => v(vm.string("unix")),
        // major, minor, snapshot version -- what about.self prints as '2023.1/13'
        "_VMversion" => v(vm.vector(vec![
            Value::Int(crate::image::VM_MAJOR as i64),
            Value::Int(crate::image::VM_MINOR as i64),
            Value::Int(crate::image::SNAPSHOT_VERSION as i64),
        ])),
        "_ProgrammingTimestamp" => v(Value::Int(vm.timestamp)),
        // ponytail: the receiver block is run, but the unwind handler is not
        // hooked up, so a non-local return through it skips the cleanup.
        // Proper support needs the unwind path in run_stack to run handlers.
        "_OnNonLocalReturn:" => Ok(P::Send(recv.clone(), "value".into(), vec![])),
        "_SetProgrammingTimestamp:" => {
            vm.timestamp = as_i(&args[0], name)?;
            v(recv.clone())
        }
        "_NodeName" | "_SysName" | "_Release" | "_Version" | "_Machine" => {
            let mut buf = vec![0u8; 256];
            let t = if name == "_NodeName" {
                if !vm.ffi.is_loaded() {
                    let _ = vm.ffi.load("libc.dylib");
                }
                match vm.ffi.sym("gethostname") {
                    Some(f) => {
                        let _ = crate::ffi::Ffi::call(f, &[buf.as_mut_ptr() as u64, 255]);
                        let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
                        String::from_utf8_lossy(&buf[..n]).into_owned()
                    }
                    None => "localhost".into(),
                }
            } else {
                match name {
                    "_SysName" => std::env::consts::OS.to_string(),
                    "_Machine" => std::env::consts::ARCH.to_string(),
                    _ => "serf".to_string(),
                }
            };
            v(vm.string(&t))
        }
        "_CurrentTimeString" => {
            let t = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?;
            v(vm.string(&format!("{}", t.as_secs())))
        }
        "_Size" => v(Value::Int(indexable_len(recv, name)? as i64)),
        "_At:" => {
            let i = as_i(&args[0], name)?;
            let o = as_obj(recv, name)?.borrow();
            let n = indexable_len(recv, name)?;
            if i < 0 || i as usize >= n {
                return Err("badIndexError".into());
            }
            match &o.payload {
                Payload::Bytes(b) => v(Value::Int(b[i as usize] as i64)),
                Payload::Vector(x) => v(x[i as usize].clone()),
                _ => unreachable!(),
            }
        }
        "_At:Put:" => {
            let i = as_i(&args[0], name)?;
            let n = indexable_len(recv, name)?;
            if i < 0 || i as usize >= n {
                return Err("badIndexError".into());
            }
            let mut o = as_obj(recv, name)?.borrow_mut();
            match &mut o.payload {
                Payload::Bytes(b) => b[i as usize] = as_i(&args[1], name)? as u8,
                Payload::Vector(x) => x[i as usize] = args[1].clone(),
                _ => unreachable!(),
            }
            drop(o);
            v(recv.clone())
        }
        // block copy, the workhorse behind copyFrom:UpTo: and friends
        "_CopyByteRangeDstPos:Src:SrcPos:Length:" | "_CopyRangeDstPos:Src:SrcPos:Length:" => {
            let dst_pos = as_i(&args[0], name)?;
            let src_pos = as_i(&args[2], name)?;
            let len = as_i(&args[3], name)?;
            if dst_pos < 0 || src_pos < 0 || len < 0 {
                return Err("badIndexError".into());
            }
            let (dp, sp, n) = (dst_pos as usize, src_pos as usize, len as usize);
            let src = args[1].clone();
            let d = as_obj(recv, name)?;
            let same = src.as_obj().map_or(false, |s| Rc::ptr_eq(s, d));
            let taken = if same { None } else { Some(as_obj(&src, name)?.borrow()) };
            let mut db = d.borrow_mut();
            let ok = match (&mut db.payload, taken.as_ref().map(|b| &b.payload)) {
                (Payload::Bytes(x), Some(Payload::Bytes(y))) => {
                    if dp + n > x.len() || sp + n > y.len() { false } else {
                        x[dp..dp + n].copy_from_slice(&y[sp..sp + n]); true
                    }
                }
                (Payload::Vector(x), Some(Payload::Vector(y))) => {
                    if dp + n > x.len() || sp + n > y.len() { false } else {
                        for k in 0..n { x[dp + k] = y[sp + k].clone(); }
                        true
                    }
                }
                (Payload::Bytes(x), None) => {
                    if dp + n > x.len() || sp + n > x.len() { false } else {
                        x.copy_within(sp..sp + n, dp); true
                    }
                }
                (Payload::Vector(x), None) => {
                    if dp + n > x.len() || sp + n > x.len() { false } else {
                        let t: Vec<Value> = x[sp..sp + n].to_vec();
                        x[dp..dp + n].clone_from_slice(&t); true
                    }
                }
                _ => return Err("badTypeError".into()),
            };
            drop(db);
            if !ok { return Err("badIndexError".into()) }
            v(recv.clone())
        }
        "_Concatenate:" => {
            let a = as_obj(recv, name)?.borrow();
            let b = as_obj(&args[0], name)?.borrow();
            let payload = match (&a.payload, &b.payload) {
                (Payload::Bytes(x), Payload::Bytes(y)) => {
                    let mut z = x.clone();
                    z.extend_from_slice(y);
                    Payload::Bytes(z)
                }
                (Payload::Vector(x), Payload::Vector(y)) => {
                    let mut z = x.clone();
                    z.extend(y.iter().cloned());
                    Payload::Vector(z)
                }
                _ => return Err("badTypeError".into()),
            };
            v(Value::obj(a.slots.clone(), payload))
        }
        "_ByteVectorCompare:" => {
            let a = recv.bytes().ok_or("_ByteVectorCompare: receiver is not a byte vector")?;
            let b = args[0].bytes().ok_or("_ByteVectorCompare: argument is not a byte vector")?;
            v(Value::Int(match a.cmp(&b) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        }

        // ------------------------------------------------------ reflection
        "_AddSlots:" => {
            let src = as_obj(&args[0], name)?.borrow().slots.clone();
            let dst = as_obj(recv, name)?;
            for s in src {
                dst.borrow_mut().put(s);
            }
            v(recv.clone())
        }
        "_RemoveSlot:" => {
            let n = as_name(&args[0], name)?;
            let with_colon = format!("{}:", n);
            let o = as_obj(recv, name)?;
            let before = o.borrow().slots.len();
            o.borrow_mut().slots.retain(|s| &*s.name != n && &*s.name != with_colon);
            if o.borrow().slots.len() == before {
                return Err("slotNameError".into());
            }
            v(recv.clone())
        }
        "_SlotNames" => {
            let names: Vec<Value> = match recv.as_obj() {
                Some(o) => o.borrow().slots.iter().map(|s| vm.string(&s.name)).collect(),
                None => vec![],
            };
            v(vm.vector(names))
        }
        "_HasSlot:" => {
            let n = as_name(&args[0], name)?;
            v(vm.boolean(recv.as_obj().map_or(false, |o| o.borrow().find(&n).is_some())))
        }
        "_SlotAt:" => {
            let n = as_name(&args[0], name)?;
            let o = as_obj(recv, name)?.borrow();
            match o.find(&n) {
                Some(i) => v(o.slots[i].value.clone()),
                None => Err("slotNameError".into()),
            }
        }
        "_SlotAt:Put:" => {
            let n = as_name(&args[0], name)?;
            let o = as_obj(recv, name)?;
            let i = o.borrow().find(&n);
            match i {
                Some(i) => o.borrow_mut().slots[i].value = args[1].clone(),
                None => return Err("slotNameError".into()),
            }
            v(recv.clone())
        }
        "_IsParentAt:" => {
            let n = as_name(&args[0], name)?;
            let o = as_obj(recv, name)?.borrow();
            match o.find(&n) {
                Some(i) => v(vm.boolean(o.slots[i].kind == SlotKind::Parent)),
                None => Err("slotNameError".into()),
            }
        }

        // ---------------------------------------------------------- control
        // traits sending goes up to thirteen arguments (sending.self)
        _ if name.starts_with("_Perform:")
            && name["_Perform:".len()..].split_inclusive(':').all(|k| k == "With:") =>
        {
            let sel = as_name(&args[0], name)?;
            Ok(P::Send(recv.clone(), sel, args[1..].to_vec()))
        }
        "_Error:" => Err(as_name(&args[0], name)?),

        // --------------------------------------------------------- parsing
        // The parser stays in the VM, as in C++: the world hands it source and
        // gets back a mirror on what came out -- an object when the body is
        // all slots, a method when it has code. Every evaluation the
        // programming environment does comes through here.
        "_ParseObjectFileName:ErrorObj:" => {
            let src = recv.bytes().ok_or("badTypeError")?.to_vec();
            let file = as_name(&args[0], name)?;
            match crate::parser::parse_body(&src)
                .and_then(|body| crate::compile::build_body(vm, &body, &file))
            {
                Ok(o) => v(make_mirror(vm, &o)?),
                Err(m) => {
                    // say what was wrong in the world's parseErrorPt, as
                    // Parser::fillErrorObj does; left alone it still holds the
                    // prototype's 'the prototypical syntax error'
                    let msg = vm.string(&m);
                    if let Some(o) = args[1].as_obj() {
                        let mut b = o.borrow_mut();
                        if let Some(i) = b.find("message") {
                            b.slots[i].value = msg;
                        }
                    }
                    Err("primitiveFailedError".into())
                }
            }
        }

        // ------------------------------------------------------------- i/o
        "_StringPrint" => {
            let b = recv.bytes().ok_or("_StringPrint: receiver is not a string")?;
            let out = std::io::stdout();
            let mut h = out.lock();
            h.write_all(&b).map_err(|e| e.to_string())?;
            h.flush().ok();
            v(recv.clone())
        }
        "_PrintString" => {
            let s = default_print_string(vm, recv);
            v(vm.string(&s))
        }
        "_ReadLine" => {
            let mut line = String::new();
            match std::io::stdin().lock().read_line(&mut line) {
                Ok(0) => v(vm.nil.clone()),
                Ok(_) => v(vm.string(line.trim_end_matches('\n'))),
                Err(e) => Err(e.to_string()),
            }
        }
        "_CurrentTimeMillis" => {
            let t = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?;
            v(Value::Int(t.as_millis() as i64))
        }
        "_Quit" => std::process::exit(0),
        "_Quit:" => std::process::exit(as_i(&args[0], name)? as i32),

        _ => glue_call(vm, name, recv, args),
    }
}

/// A generated glue primitive. `src/glue_table.rs` gives the C function, its
/// return type and its argument types, taken from the same
/// `objects/glue/*.primMaker.hh` tables the C++ VM builds its glue from, so
/// the calling convention is the declared one rather than a guess. Anything
/// not in the table falls back to resolving the longest prefix that dlsym
/// knows, which covers the Xlib macros.
fn glue_call(vm: &mut Vm, name: &str, recv: &Value, args: &[Value]) -> Result<P, String> {
    if !vm.ffi.is_loaded() {
        for lib in [
            "libX11.6.dylib",
            "/opt/X11/lib/libX11.dylib",
            "libX11.so.6",
            "/opt/X11/lib/libXtst.dylib",
            "libc.dylib",
        ] {
            let _ = vm.ffi.load(lib);
        }
    }
    let prim = name.trim_start_matches('_');

    // struct allocators and field accessors come first: they are generated
    // glue with no C function behind them, just malloc and a field offset
    // ponytail: `<Struct>_delete<selector>` only kills the proxy. The bytes
    // belong to vm.c_heap for the life of the VM, so there is nothing to free
    // and nothing that can dangle; add real freeing if a program churns them.
    // `get None` in the glue templates is X11's null constant, not a C call:
    // `void nullPixmap = proxy_null Pixmap Pixmap_seal {...} get None`
    // The 4.4 release images spell the same zero `NULL`, from the days before
    // the templates said None: `_NULLnullPixmapResultProxy`, and likewise for
    // the null cursor, region and window.
    if ["None", "NULL"]
        .iter()
        .any(|k| prim.strip_prefix(k).is_some_and(|sel| sel.starts_with(char::is_lowercase)))
    {
        let target = if prim.ends_with("ResultProxy:") { args.last() } else { Some(recv) };
        if let Some(t) = target.filter(|t| t.as_obj().is_some()) {
            t.as_obj().unwrap().borrow_mut().payload = Payload::Proxy(Some(0));
            return Ok(P::Val(t.clone()));
        }
        return Ok(P::Val(Value::obj(vec![], Payload::Proxy(Some(0)))));
    }
    if prim
        .split_once("_delete")
        .is_some_and(|(_, sel)| sel == "delete" || sel == "basicDelete")
    {
        as_obj(recv, name)?.borrow_mut().payload = Payload::Proxy(None);
        return Ok(P::Val(recv.clone()));
    }
    if let Some(e) = crate::struct_table::ALLOC.iter().find(|e| prim.starts_with(e.0)) {
        let mem = vec![0u8; e.2];
        let p = mem.as_ptr() as u64;
        vm.c_heap.push(mem);
        // as everywhere else: `...ResultProxy:` fills the last argument, the
        // unary `...ResultProxy` fills the receiver. A fresh object made here
        // instead would have no slots, so the world could not send it anything.
        let target = if prim.ends_with("ResultProxy:") {
            args.last().cloned().filter(|t| t.as_obj().is_some())
        } else {
            Some(recv.clone()).filter(|t| t.as_obj().is_some())
        };
        return match target {
            Some(t) => {
                t.as_obj().unwrap().borrow_mut().payload = Payload::Proxy(Some(p));
                Ok(P::Val(t))
            }
            None => Ok(P::Val(Value::obj(vec![], Payload::Proxy(Some(p))))),
        };
    }
    if let Some(e) = crate::struct_table::FIELD.iter().find(|e| prim.starts_with(e.0)) {
        let base = proxy_ptr(recv).ok_or("deadProxyError")?;
        if base == 0 {
            return Err("nullPointerError".into());
        }
        let at = (base + e.3 as u64) as *mut u8;
        let is_set = args.len() > usize::from(prim.ends_with("ResultProxy:"));
        unsafe {
            if is_set {
                let x = as_i(&args[0], name).unwrap_or(0) as u64;
                match e.4 {
                    1 => *at = x as u8,
                    2 => *(at as *mut u16) = x as u16,
                    4 => *(at as *mut u32) = x as u32,
                    _ => *(at as *mut u64) = x,
                }
                return Ok(P::Val(recv.clone()));
            }
            let raw: u64 = match e.4 {
                1 => *at as u64,
                2 => *(at as *const u16) as u64,
                4 => *(at as *const u32) as u64,
                _ => *(at as *const u64),
            };
            return match e.5 {
                "proxy" => {
                    let t = args.last().cloned().filter(|t| t.as_obj().is_some());
                    match t {
                        Some(t) => {
                            t.as_obj().unwrap().borrow_mut().payload = Payload::Proxy(Some(raw));
                            Ok(P::Val(t))
                        }
                        None => Ok(P::Val(Value::obj(vec![], Payload::Proxy(Some(raw))))),
                    }
                }
                "bool" => Ok(P::Val(vm.boolean(raw != 0))),
                _ => Ok(P::Val(Value::Int(match e.4 {
                    1 => raw as i8 as i64,
                    2 => raw as i16 as i64,
                    4 => raw as i32 as i64,
                    _ => raw as i64,
                }))),
            };
        }
    }

    let sig = crate::glue_table::GLUE.iter().find(|e| e.0 == prim);

    let (cname, ret, argtypes): (&str, &str, &[&str]) = match sig {
        Some(e) => (e.1, e.2, e.3),
        None => {
            let (f, cut) = vm
                .ffi
                .split_glue(prim)
                .ok_or_else(|| format!("unknown primitive '{}'", name))?;
            let sel = &prim[cut..];
            return untyped_glue(vm, name, f, sel, recv, args);
        }
    };
    // The _wrap functions live in the VM's own C sources, not in any library
    // (vm/src/unix/prims/unixPrims.cpp), so serf supplies them.
    if let Some(r) = native_wrap(vm, cname, recv, args)? {
        return Ok(r);
    }

    // MYSELF is the glue's identity function: it boxes the receiver, which is
    // how a plain int becomes a sealed proxy (a unix file descriptor)
    let handled_here = cname == "MYSELF" || is_x_wrap(cname);
    let f = if handled_here {
        std::ptr::null_mut()
    } else {
        vm.ffi
            .sym(cname)
            .or_else(|| vm.ffi.sym(&format!("X{}", cname)))
            .ok_or_else(|| format!("primitiveNotDefinedError: no C function '{}' for '{}'", cname, name))?
    };

    // receiver is the first declared argument
    let mut supplied: Vec<Value> = vec![recv.clone()];
    supplied.extend(args.iter().cloned());
    // `...ResultProxy:` fills the last argument; the unary `...ResultProxy`
    // fills the receiver, which is then not an argument at all -- that is how
    // primitiveMaker writes an allocator: `void new = XSizeHints {xlib
    // xSizeHints deadCopy} call XAllocSizeHints`.
    let result_proxy = if prim.ends_with("ResultProxy:") {
        supplied.pop()
    } else if prim.ends_with("ResultProxy") {
        Some(supplied.remove(0))
    } else {
        None
    };

    let mut keep: Vec<Vec<u8>> = vec![];
    let mut back: Vec<(Value, usize)> = vec![];
    let mut words: Vec<u64> = vec![];
    // how wide each argument is in C, for the ones the ABI puts on the stack
    let mut sizes: Vec<usize> = vec![];
    for (i, t) in argtypes.iter().enumerate() {
        let a = supplied
            .get(i)
            .ok_or_else(|| format!("{}: expects {} arguments", name, argtypes.len()))?;
        match *t {
            // a byte vector is passed as pointer plus length -- as are the
            // `_len` string flavours (bv_len_pass in glueDefs.hh), which is
            // what XDrawString needs to not send a malformed request
            "bv_len" | "cbv_len" | "bv_len_null" | "cbv_len_null" | "string_len"
            | "string_len_null" => {
                let b = a.bytes().ok_or_else(|| format!("{}: expected a byte vector", name))?;
                let n = b.len() as u64;
                let mut buf = b;
                buf.push(0);
                words.push(buf.as_ptr() as u64);
                words.push(n);
                sizes.extend([8, 4]); // char *, int
                keep.push(buf);
                back.push((a.clone(), keep.len() - 1));
            }
            "float" | "double" => {
                return Err(format!("{}: floating-point arguments are not supported", name))
            }
            // a plain `proxy` argument may not be null; `proxy_null` may
            "proxy" => match proxy_ptr(a) {
                None => return Err("deadProxyError".into()),
                Some(0) => return Err("nullPointerError".into()),
                Some(p) => {
                    words.push(p);
                    sizes.push(8);
                }
            },
            _ => {
                words.push(to_word(vm, a, &mut keep, &mut back)?);
                sizes.push(match *t {
                    "short" | "unsigned_short" => 2,
                    "bool" | "int" | "unsigned_int" => 4,
                    _ => 8,
                });
            }
        }
    }
    let r = if cname == "MYSELF" {
        words.first().copied().unwrap_or(0)
    } else if is_x_wrap(cname) {
        x_wrap(vm, cname, &words)?
    } else if let Some(fixed) = crate::ffi::variadic_fixed(cname) {
        // F_SETFL replaces the whole status word, and unix.self sets
        // O_NONBLOCK and O_ASYNC in two separate calls -- so the second wipes
        // the first. On the X connection that clears the non-blocking mode
        // XCB set for itself, and Xlib then sits in poll_for_response for
        // ever waiting for a reply. Keep the bit; the world wants both.
        const F_GETFL: u64 = 3;
        const F_SETFL: u64 = 4;
        const O_NONBLOCK: u64 = 0x4;
        if cname == "fcntl" && words.get(1) == Some(&F_SETFL) {
            let cur = crate::ffi::Ffi::call_variadic(f, fixed, &[words[0], F_GETFL])?;
            if let Some(fl) = words.get_mut(2) {
                *fl |= cur & O_NONBLOCK;
            }
        }
        crate::ffi::Ffi::call_variadic(f, fixed, &words)?
    } else {
        crate::ffi::Ffi::call_sized(f, &words, &sizes)?
    };
    if std::env::var_os("SERF_TRACE_FFI").is_some() {
        eprintln!("ffi {} {}({:x?}) -> {:#x}", ret, cname, words, r);
    }
    for (v, i) in back {
        if let Some(o) = v.as_obj() {
            if let Payload::Bytes(b) = &mut o.borrow_mut().payload {
                let n = b.len();
                b.copy_from_slice(&keep[i][..n]);
            }
        }
    }

    match ret {
        "void" => v_of(recv.clone()),
        "string" | "string_null" => {
            if r == 0 {
                return v_of(vm.nil_v());
            }
            let mut b = vec![];
            unsafe {
                let mut p = r as *const u8;
                while *p != 0 && b.len() < 1 << 20 {
                    b.push(*p);
                    p = p.add(1);
                }
            }
            let parent = vm.t_string.clone();
            v_of(vm.bytes_with(parent, b))
        }
        "bool" => v_of(vm.boolean(r != 0)),
        "proxy" | "proxy_null" | "proxy_or_errno" => {
            if ret == "proxy_or_errno" && r as i64 == -1 {
                return Err(os_error());
            }
            // proxy_ret fails on NULL and leaves the result proxy dead; only
            // proxy_null may answer one that points at nothing
            if r == 0 && ret == "proxy" {
                return Err("nullPointerError".into());
            }
            match result_proxy {
                Some(t) => {
                    as_obj(&t, name)?.borrow_mut().payload = Payload::Proxy(Some(r));
                    v_of(t)
                }
                None => v_of(Value::obj(vec![], Payload::Proxy(Some(r)))),
            }
        }
        "int_or_errno" => {
            let x = r as i32 as i64;
            if x == -1 {
                Err(os_error())
            } else {
                v_of(Value::Int(x))
            }
        }
        "oop" => Err(format!("{}: glue returns a Self object, which serf cannot build", name)),
        _ => v_of(Value::Int(r as i32 as i64)),
    }
}

/// The libm half of the glue table, answered in Rust. The second argument is
/// only read by the two-argument ones.
fn math_glue(cname: &str) -> Option<fn(f64, f64) -> f64> {
    let f: fn(f64, f64) -> f64 = match cname {
        "acos" => |x, _| x.acos(),
        "acosh" => |x, _| x.acosh(),
        "asin" => |x, _| x.asin(),
        "asinh" => |x, _| x.asinh(),
        "atan" => |x, _| x.atan(),
        "atan2" => |x, y| x.atan2(y),
        "atanh" => |x, _| x.atanh(),
        "cos" => |x, _| x.cos(),
        "cosh" => |x, _| x.cosh(),
        "exp" => |x, _| x.exp(),
        "log" => |x, _| x.ln(),
        "log10" => |x, _| x.log10(),
        "log2" => |x, _| x.log2(),
        "pow" => |x, y| x.powf(y),
        "sin" => |x, _| x.sin(),
        "sinh" => |x, _| x.sinh(),
        "sqrt" => |x, _| x.sqrt(),
        "tan" => |x, _| x.tan(),
        "tanh" => |x, _| x.tanh(),
        _ => return None,
    };
    Some(f)
}

/// Hand-written wrappers the C++ VM defines itself. `read_wrap`/`write_wrap`
/// take a buffer plus its length and an offset, and bounds-check before
/// touching the file descriptor.
fn native_wrap(vm: &mut Vm, cname: &str, recv: &Value, args: &[Value]) -> Result<Option<P>, String> {
    // libm takes and answers C doubles, which the integer-register call path
    // carries in neither direction. Rust's f64 has every one of them.
    if let Some(f) = math_glue(cname) {
        let y = args.first().map_or(Ok(0.0), |a| as_f(a, cname))?;
        return Ok(Some(P::Val(Value::Float(f(as_f(recv, cname)?, y)))));
    }
    if cname == "select_wrap" || cname == "select_read_wrap" {
        return select_wrap(recv, args).map(Some);
    }
    // XLookupString fills a byte vector and hands back the keysym in a
    // one-element Self vector.
    if cname == "XLookupString_wrap" {
        let evt = proxy_ptr(recv).ok_or("deadProxyError")?;
        let o = args[0].as_obj().ok_or("badTypeError")?;
        let f = vm.ffi.sym("XLookupString").ok_or("primitiveNotDefinedError")?;
        let mut ks: u64 = 0;
        let n = {
            let mut b = o.borrow_mut();
            let buf = match &mut b.payload {
                Payload::Bytes(x) => x,
                _ => return Err("badTypeError".into()),
            };
            let (p, len) = (buf.as_mut_ptr() as u64, buf.len() as u64);
            crate::ffi::Ffi::call(f, &[evt, p, len, &mut ks as *mut u64 as u64, 0])?
        };
        if let Some(o) = args[1].as_obj() {
            if let Payload::Vector(x) = &mut o.borrow_mut().payload {
                if let Some(first) = x.first_mut() {
                    *first = Value::Int(ks as i64);
                }
            }
        }
        return Ok(Some(P::Val(Value::Int(n as i32 as i64))));
    }
    // `rawTime` answers the event's time as (days. msecInDay.), which
    // timeAsVector builds in the C++ glue. The field sits at the same offset
    // in every core event struct.
    if cname.starts_with('x') && cname.ends_with("Event_time") {
        const TIME_OFFSET: usize = 56;
        const MS_PER_DAY: u64 = 1000 * 60 * 60 * 24;
        let e = proxy_ptr(recv).ok_or("deadProxyError")? as usize;
        let t = unsafe { *((e + TIME_OFFSET) as *const u64) };
        let v = vm.vector(vec![
            Value::Int((t / MS_PER_DAY) as i64),
            Value::Int((t % MS_PER_DAY) as i64),
        ]);
        return Ok(Some(P::Val(v)));
    }
    // These take the coordinates as two Self vectors and pass XPoints, so they
    // need the Self objects rather than converted words.
    if matches!(cname, "XFillPolygon_wrap" | "XDrawLines_wrap" | "XDrawString16_wrap") {
        let ints = |v: &Value| -> Result<Vec<i64>, String> {
            match &v.as_obj().ok_or("badTypeError")?.borrow().payload {
                Payload::Vector(x) => x.iter().map(|e| as_i(e, "wrap")).collect(),
                _ => Err("badTypeError".into()),
            }
        };
        let dpy = proxy_ptr(recv).ok_or("deadProxyError")?;
        let d = proxy_ptr(&args[0]).ok_or("deadProxyError")?;
        let gc = proxy_ptr(&args[1]).ok_or("deadProxyError")?;
        if cname == "XDrawString16_wrap" {
            // a vector of 16-bit chars becomes an XChar2b array
            let cs = ints(&args[4])?;
            let buf: Vec<u16> = cs.iter().map(|c| (*c as u16).to_be()).collect();
            let f = vm.ffi.sym("XDrawString16").ok_or("primitiveNotDefinedError")?;
            let (x, y) = (as_i(&args[2], cname)?, as_i(&args[3], cname)?);
            crate::ffi::Ffi::call(
                f,
                &[dpy, d, gc, x as u64, y as u64, buf.as_ptr() as u64, buf.len() as u64],
            )?;
            return Ok(Some(P::Val(recv.clone())));
        }
        let (xs, ys) = (ints(&args[2])?, ints(&args[3])?);
        if xs.len() != ys.len() {
            return Err("different number of x and y coordinates".into());
        }
        // XPoint is two shorts
        let pts: Vec<i16> =
            xs.iter().zip(&ys).flat_map(|(x, y)| [*x as i16, *y as i16]).collect();
        let n = xs.len() as u64;
        let p = pts.as_ptr() as u64;
        let fill = cname == "XFillPolygon_wrap";
        let f = vm
            .ffi
            .sym(if fill { "XFillPolygon" } else { "XDrawLines" })
            .ok_or("primitiveNotDefinedError")?;
        let mode = as_i(&args[if fill { 5 } else { 4 }], cname)? as u64;
        if fill {
            let shape = as_i(&args[4], cname)? as u64;
            crate::ffi::Ffi::call(f, &[dpy, d, gc, p, n, shape, mode])?;
        } else {
            crate::ffi::Ffi::call(f, &[dpy, d, gc, p, n, mode])?;
        }
        return Ok(Some(P::Val(recv.clone())));
    }
    // XImage's pixel accessors are function pointers inside the image struct
    // (XGetPixel/XPutPixel are macros), so these two walk it themselves.
    if cname == "XImagePutData_wrap" || cname == "XImageGetData_wrap" {
        const F_GET_PIXEL: usize = 104; // offsetof(XImage, f.get_pixel) here
        const F_PUT_PIXEL: usize = 112;
        let img = proxy_ptr(recv).ok_or("deadProxyError")? as usize;
        let (w, h) = unsafe { (*(img as *const i32), *((img + 4) as *const i32)) };
        let put = cname == "XImagePutData_wrap";
        let f = unsafe { *((img + if put { F_PUT_PIXEL } else { F_GET_PIXEL }) as *const u64) };
        let map: Vec<i64> = if put {
            match &args[1].as_obj().ok_or("badTypeError")?.borrow().payload {
                Payload::Vector(x) => x.iter().map(|v| as_i(v, cname)).collect::<Result<_, _>>()?,
                _ => return Err("badTypeError".into()),
            }
        } else {
            vec![]
        };
        let o = args[0].as_obj().ok_or("badTypeError")?;
        let mut b = o.borrow_mut();
        let px = match &mut b.payload {
            Payload::Bytes(x) => x,
            _ => return Err("badTypeError".into()),
        };
        if px.len() < (w * h) as usize {
            return Ok(Some(P::Val(recv.clone())));
        }
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as usize;
                if put {
                    let p = map.get(px[i] as usize).copied().unwrap_or(0);
                    crate::ffi::Ffi::call(f as *mut _, &[img as u64, x as u64, y as u64, p as u64])?;
                } else {
                    let p = crate::ffi::Ffi::call(f as *mut _, &[img as u64, x as u64, y as u64])?;
                    px[i] = p as u8;
                }
            }
        }
        return Ok(Some(P::Val(recv.clone())));
    }
    // XNextEvent_wrap answers a clone of the event prototype for the event's
    // type, pointing at a fresh XEvent -- it needs Self objects, so it lives
    // here rather than with the other X wrappers.
    if cname == "XNextEvent_wrap" {
        let dpy = proxy_ptr(recv).ok_or("deadProxyError")?;
        let peek = args[0].id_eq(&vm.boolean(true));
        let mem = vec![0u8; 192]; // sizeof(XEvent) here
        let p = mem.as_ptr() as u64;
        vm.c_heap.push(mem);
        let f = vm
            .ffi
            .sym(if peek { "XPeekEvent" } else { "XNextEvent" })
            .ok_or("primitiveNotDefinedError")?;
        crate::ffi::Ffi::call(f, &[dpy, p])?;
        let ty = unsafe { *(p as *const i32) };
        let proto = match &args[1].as_obj().ok_or("badTypeError")?.borrow().payload {
            Payload::Vector(x) => x.get(ty as usize).cloned().ok_or("badTypeError")?,
            _ => return Err("badTypeError".into()),
        };
        let slots = as_obj(&proto, cname)?.borrow().slots.clone();
        return Ok(Some(P::Val(Value::obj(slots, Payload::Proxy(Some(p))))));
    }
    // The font metrics are reads of the XFontStruct the world already holds:
    // max_bounds is the XCharStruct at offset 68 (min_bounds at 56, per_char
    // at 80 -- the offsets struct_table.rs already carries for this struct),
    // and an XCharStruct is six 16-bit fields, width third and ascent fourth.
    if let Some(field) = match cname {
        "maxCharWidth" => Some(68 + 4),
        "maxAscent" => Some(68 + 6),
        "maxDescent" => Some(68 + 8),
        _ => None,
    } {
        let p = proxy_ptr(recv).ok_or("deadProxyError")?;
        let x = unsafe { *((p + field) as *const i16) };
        return Ok(Some(P::Val(Value::Int(x as i64))));
    }
    if cname == "perCharWidth" {
        let p = proxy_ptr(recv).ok_or("deadProxyError")?;
        let per_char = unsafe { *((p + 80) as *const u64) };
        if per_char == 0 {
            // no per-character metrics: every character is max_bounds wide,
            // which is what a fixed font's XFontStruct means by a null per_char
            let w = unsafe { *((p + 68 + 4) as *const i16) };
            return Ok(Some(P::Val(Value::Int(w as i64))));
        }
        let i = as_i(&args[0], cname)?.max(0) as u64;
        let x = unsafe { *((per_char + 12 * i + 4) as *const i16) };
        return Ok(Some(P::Val(Value::Int(x as i64))));
    }
    // The X wrappers in objects/glue/xlib_glue.cpp are compiled into the C++
    // VM, so serf supplies the ones the world reaches. These two are XFree.
    if let Some(st) = cname.strip_prefix("XFree_").and_then(|c| c.strip_suffix("_wrap")) {
        let _ = st;
        let p = proxy_ptr(recv).ok_or("deadProxyError")?;
        if !vm.ffi.is_loaded() {
            for lib in ["libX11.6.dylib", "/opt/X11/lib/libX11.dylib"] {
                let _ = vm.ffi.load(lib);
            }
        }
        let f = vm.ffi.sym("XFree").ok_or("primitiveNotDefinedError")?;
        crate::ffi::Ffi::call(f, &[p])?;
        as_obj(recv, cname)?.borrow_mut().payload = Payload::Proxy(None);
        return Ok(Some(P::Val(recv.clone())));
    }
    let rw = match cname {
        "read_wrap" => false,
        "write_wrap" => true,
        _ => return Ok(None),
    };
    // A dead proxy must say so: the generated wrappers revive and retry only
    // when they see deadProxyError (see objects/glue/*_wrappers.self). Fd 0 is
    // stdin, not a dead proxy.
    let fd = proxy_ptr(recv).ok_or("deadProxyError")? as i32;
    let buf = args[0].clone();
    let offset = as_i(&args[1], cname)?;
    let n = as_i(&args[2], cname)?;
    if offset < 0 || n < 0 {
        return Err("badSignError".into());
    }
    let o = buf.as_obj().ok_or("badTypeError")?;
    let len = match &o.borrow().payload {
        Payload::Bytes(b) => b.len(),
        _ => return Err("badTypeError".into()),
    };
    if offset as usize + n as usize > len {
        return Err("badIndexError".into());
    }
    if !vm.ffi.is_loaded() {
        let _ = vm.ffi.load("libc.dylib");
    }
    let f = vm
        .ffi
        .sym(if rw { "write" } else { "read" })
        .ok_or("primitiveNotDefinedError")?;
    let got = {
        let mut b = o.borrow_mut();
        let bytes = match &mut b.payload {
            Payload::Bytes(x) => x,
            _ => unreachable!(),
        };
        let p = unsafe { bytes.as_mut_ptr().add(offset as usize) } as u64;
        crate::ffi::Ffi::call(f, &[fd as u64, p, n as u64])? as i64
    };
    if got as i32 == -1 {
        return Err(os_error());
    }
    Ok(Some(P::Val(Value::Int(got as i32 as i64))))
}

/// `select_wrap`: fill the receiving vector with the file descriptors that are
/// ready and answer how many. The scheduler calls this from `sigio` to find
/// which of its files woke it.
///
/// Readability *and* writability, as select_wrap does: a process suspended in
/// `suspendForIO` waiting to write is woken by the same signal, and if it is
/// never woken the world stalls with its window built but not yet mapped.
///
/// ponytail: the C++ VM keeps an `activeFDs` set, filled as the world marks
/// files async through fcntl. serf has no such bookkeeping, so it asks about
/// every open descriptor below the limit.
fn select_wrap(vec: &Value, args: &[Value]) -> Result<P, String> {
    const FD_SETSIZE: i64 = 1024;
    const F_GETFD: i32 = 1;
    #[repr(C)]
    struct Timeval {
        sec: i64,
        usec: i32,
    }
    extern "C" {
        fn select(n: i32, r: *mut u8, w: *mut u8, e: *mut u8, t: *const Timeval) -> i32;
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }

    let mut how_many = as_i(&args[0], "select_wrap")?;
    let cap = indexable_len(vec, "select_wrap")? as i64;
    if how_many > cap {
        return Err("badSizeError".into());
    }
    how_many = how_many.min(FD_SETSIZE);

    let mut rd = [0u8; FD_SETSIZE as usize / 8];
    for fd in 0..how_many as i32 {
        if unsafe { fcntl(fd, F_GETFD) } != -1 {
            rd[fd as usize / 8] |= 1 << (fd % 8);
        }
    }
    let mut wr = rd;
    let nowait = Timeval { sec: 0, usec: 0 };
    let n = unsafe {
        select(how_many as i32, rd.as_mut_ptr(), wr.as_mut_ptr(), std::ptr::null_mut(), &nowait)
    };
    if n < 0 {
        return Err(os_error());
    }
    let ready: Vec<i64> = (0..how_many)
        .filter(|fd| (rd[*fd as usize / 8] | wr[*fd as usize / 8]) & (1 << (fd % 8)) != 0)
        .collect();
    if let Some(o) = vec.as_obj() {
        if let Payload::Vector(x) = &mut o.borrow_mut().payload {
            for (i, fd) in ready.iter().enumerate() {
                if i < x.len() {
                    x[i] = Value::Int(*fd);
                }
            }
        }
    }
    Ok(P::Val(Value::Int(ready.len() as i64)))
}

/// The X wrappers `objects/glue/xlib_glue.cpp` compiles into the C++ VM. Most
/// are a single Xlib call with the arguments shuffled, so serf does the
/// shuffling here rather than growing a C file of its own; they run on the
/// converted machine words, after the usual argument conversion.
fn is_x_wrap(cname: &str) -> bool {
    X_RENAMES.iter().any(|(w, _)| *w == cname)
        || matches!(
            cname,
            "XStringToTextProperty_wrap"
                | "XCreateImage_wrap"
                | "XMatchVisualInfo_wrap"
                | "XSetWMProtocol_wrap"
                | "XMoveWindowBy_wrap"
                | "XSetClipRectangle_wrap"
                | "XGetWindowAttributes_wrap"
        )
}

/// wrappers that are the same call under a different name
const X_RENAMES: &[(&str, &str)] = &[
    ("XCreateRegion_wrap", "XCreateRegion"),
    ("XLoadQueryFont_wrap", "XLoadQueryFont"),
    ("XShapeQueryExtension_wrap", "XShapeQueryExtension"),
    ("XDestroyRegion_wrap", "XDestroyRegion"),
    ("XEmptyRegion_wrap", "XEmptyRegion"),
    ("XEqualRegion_wrap", "XEqualRegion"),
    ("XIntersectRegion_wrap", "XIntersectRegion"),
    ("XOffsetRegion_wrap", "XOffsetRegion"),
    ("XUnionRectWithRegion_wrap", "XUnionRectWithRegion"),
    ("XUnionRegion_wrap", "XUnionRegion"),
];

fn x_wrap(vm: &mut Vm, cname: &str, words: &[u64]) -> Result<u64, String> {
    let call = |vm: &mut Vm, sym: &str, a: &[u64]| -> Result<u64, String> {
        let f = vm.ffi.sym(sym).ok_or("primitiveNotDefinedError")?;
        crate::ffi::Ffi::call(f, a)
    };
    if let Some((_, real)) = X_RENAMES.iter().find(|(w, _)| *w == cname) {
        return call(vm, real, words);
    }
    let w = |i: usize| words.get(i).copied().unwrap_or(0);
    match cname {
        // XStringListToTextProperty takes the address of the string pointer
        "XStringToTextProperty_wrap" => {
            let s = w(1);
            let list = [s];
            call(vm, "XStringListToTextProperty", &[list.as_ptr() as u64, 1, w(0)])
        }
        // and XSetWMProtocols the address of a one-element atom array
        "XSetWMProtocol_wrap" => {
            let ps = [w(2)];
            call(vm, "XSetWMProtocols", &[w(0), w(1), ps.as_ptr() as u64, 1])
        }
        "XMoveWindowBy_wrap" => {
            let mut g = [0u64; 8]; // root, x, y, w, h, border, depth
            let ok = call(
                vm,
                "XGetGeometry",
                &[
                    w(0),
                    w(1),
                    g.as_mut_ptr() as u64,
                    unsafe { g.as_mut_ptr().add(1) } as u64,
                    unsafe { g.as_mut_ptr().add(2) } as u64,
                    unsafe { g.as_mut_ptr().add(3) } as u64,
                    unsafe { g.as_mut_ptr().add(4) } as u64,
                    unsafe { g.as_mut_ptr().add(5) } as u64,
                    unsafe { g.as_mut_ptr().add(6) } as u64,
                ],
            )?;
            if ok == 0 {
                return Ok(0);
            }
            let x = (g[1] as u32 as i32) as i64 + w(2) as i32 as i64;
            let y = (g[2] as u32 as i32) as i64 + w(3) as i32 as i64;
            call(vm, "XMoveWindow", &[w(0), w(1), x as u64, y as u64])
        }
        "XSetClipRectangle_wrap" => {
            // XRectangle is four shorts
            let r: [i16; 4] = [w(2) as i16, w(3) as i16, w(4) as i16, w(5) as i16];
            call(vm, "XSetClipRectangles", &[w(0), w(1), 0, 0, r.as_ptr() as u64, 1, 0])
        }
        // XMatchVisualInfo fills a caller-supplied XVisualInfo
        "XMatchVisualInfo_wrap" => {
            let mem = vec![0u8; 64]; // sizeof(XVisualInfo) here
            let p = mem.as_ptr() as u64;
            vm.c_heap.push(mem);
            if call(vm, "XMatchVisualInfo", &[w(0), w(1), w(2), w(3), p])? == 0 {
                return Err("no matching visual found".into());
            }
            Ok(p)
        }
        // XCreateImage_wrap allocates the pixel buffer the image will own
        "XCreateImage_wrap" => {
            let (depth, width, height, pad) = (w(2), w(4), w(5), w(6));
            let pixel_size = if depth == 24 { 32 } else { depth };
            let bits = width * pixel_size;
            let padded = (bits + pad - 1) & !(pad - 1);
            let data = vec![0u8; (padded * height / 8) as usize];
            let p = data.as_ptr() as u64;
            vm.c_heap.push(data);
            // dpy, visual, depth, format, offset, data, w, h, bitmap_pad,
            // bytes_per_line -- the last two are the ones on the stack
            const SIZES: [usize; 10] = [8, 8, 4, 4, 4, 8, 4, 4, 4, 4];
            let f = vm.ffi.sym("XCreateImage").ok_or("primitiveNotDefinedError")?;
            crate::ffi::Ffi::call_sized(
                f,
                &[w(0), w(1), depth, w(3), 0, p, width, height, pad, 0],
                &SIZES,
            )
        }
        "XGetWindowAttributes_wrap" => {
            let mem = vec![0u8; 136]; // sizeof(XWindowAttributes) here
            let p = mem.as_ptr() as u64;
            vm.c_heap.push(mem);
            if call(vm, "XGetWindowAttributes", &[w(0), w(1), p])? == 0 {
                return Err("primitiveFailedError".into());
            }
            Ok(p)
        }
        _ => Err(format!("primitiveNotDefinedError: no C function '{}'", cname)),
    }
}

fn v_of(x: Value) -> Result<P, String> {
    Ok(P::Val(x))
}

/// Fallback for primitives with no table entry (the Xlib macros).
fn untyped_glue(
    vm: &mut Vm,
    name: &str,
    f: *mut std::os::raw::c_void,
    sel: &str,
    recv: &Value,
    args: &[Value],
) -> Result<P, String> {
    // as in glue_call: `...ResultProxy:` fills the last argument, the unary
    // `...ResultProxy` fills the receiver and passes nothing
    let by_receiver = !sel.ends_with("ResultProxy:") && sel.ends_with("ResultProxy");
    let result_proxy = by_receiver || sel.ends_with("ResultProxy:");
    let n = if result_proxy && !by_receiver { args.len() - 1 } else { args.len() };
    let mut keep: Vec<Vec<u8>> = vec![];
    let mut back: Vec<(Value, usize)> = vec![];
    let mut words = Vec::with_capacity(n + 1);
    if !by_receiver {
        words.push(to_word(vm, recv, &mut keep, &mut back)?);
    }
    for a in &args[..n] {
        words.push(to_word(vm, a, &mut keep, &mut back)?);
    }
    let mut r = crate::ffi::Ffi::call(f, &words)?;
    // A snapshot remembers the display it was saved on, which is somebody
    // else's machine; rather than make the world ask, fall back to $DISPLAY.
    // Set SERF_DISPLAY_STRICT to keep the world's own answer.
    if r == 0
        && name.starts_with("_XOpenDisplay")
        && words.first().is_some_and(|w| *w != 0 && unsafe { *(*w as *const u8) } != 0)
        && std::env::var_os("SERF_DISPLAY_STRICT").is_none()
    {
        let want = recv.as_str().unwrap_or_default();
        r = crate::ffi::Ffi::call(f, &[0])?;
        if r != 0 {
            eprintln!("serf: no display {:?}; using $DISPLAY", want);
        }
    }
    if std::env::var_os("SERF_TRACE_FFI").is_some() {
        let strs: Vec<String> =
            keep.iter().map(|b| String::from_utf8_lossy(b).into_owned()).collect();
        eprintln!("ffi (untyped) {}({:x?}) {:?} -> {:#x}", name, words, strs, r);
    }
    for (v, i) in back {
        if let Some(o) = v.as_obj() {
            if let Payload::Bytes(b) = &mut o.borrow_mut().payload {
                let n = b.len();
                b.copy_from_slice(&keep[i][..n]);
            }
        }
    }
    if result_proxy {
        // as proxy_ret: a NULL answer is a failure and the result proxy is
        // left as it was -- dead. XOpenDisplay on a display that is not there
        // must not hand the world a live proxy pointing at nothing.
        if r == 0 {
            return Err("nullPointerError".into());
        }
        let target = if by_receiver { recv.clone() } else { args[args.len() - 1].clone() };
        as_obj(&target, name)?.borrow_mut().payload = Payload::Proxy(Some(r));
        return Ok(P::Val(target));
    }
    Ok(P::Val(Value::Int((r & 0xffff_ffff) as i64)))
}

/// Self value to machine word. Byte vectors go through a NUL-terminated
/// scratch buffer that is copied back afterwards.
fn to_word(
    vm: &Vm,
    v: &Value,
    keep: &mut Vec<Vec<u8>>,
    back: &mut Vec<(Value, usize)>,
) -> Result<u64, String> {
    Ok(match v {
        Value::Int(i) => *i as u64,
        Value::Float(f) => *f as i64 as u64,
        Value::Obj(o) => {
            if v.id_eq(&vm.nil) || vm.image_roots.as_ref().map_or(false, |r| v.id_eq(&r[1])) {
                return Ok(0);
            }
            if v.id_eq(&vm.boolean(true)) {
                return Ok(1);
            }
            if v.id_eq(&vm.boolean(false)) {
                return Ok(0);
            }
            match &o.borrow().payload {
                // as general_proxy_cnvt: a dead proxy is not passable, but a
                // live one pointing at 0 is (an fd, say)
                Payload::Proxy(p) => p.ok_or("deadProxyError")?,
                Payload::Bytes(b) => {
                    let mut buf = b.clone();
                    buf.push(0);
                    let p = buf.as_ptr() as u64;
                    keep.push(buf);
                    back.push((v.clone(), keep.len() - 1));
                    p
                }
                _ => return Err("badTypeError".into()),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    /// Every libm entry in the glue table has to be answered here, or it falls
    /// through to a call path that cannot pass a double. The lowercase C name
    /// is what tells those apart from the mac framework glue (CG..., ATSU...),
    /// which takes floats too and is still unsupported.
    #[test]
    fn every_libm_glue_entry_is_answered_natively() {
        for e in crate::glue_table::GLUE.iter().filter(|e| {
            !e.3.is_empty()
                && e.3.iter().all(|t| *t == "float")
                && e.1.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        }) {
            assert!(super::math_glue(e.1).is_some(), "no native {}", e.1);
        }
        assert_eq!(super::math_glue("sqrt").unwrap()(88804.0, 0.0), 298.0);
        assert_eq!(super::math_glue("pow").unwrap()(2.0, 10.0), 1024.0);
    }
}
