//! Foreign calls, for the glue primitives an image uses to reach C libraries
//! (`objects/glue/*`). A generated glue primitive is named
//! `<CFunctionName><SelfSelector>` -- `XFlushxFlush`,
//! `XCreateGCxCreateGCDrawable:ValueMask:Values:ResultProxy:` -- so the C
//! symbol is recovered by trying successively shorter prefixes against
//! `dlsym`, which is exact and needs no table.
//!
//! ponytail: no libffi. Arguments are passed as machine words through a
//! prototype of the right arity (up to 12), so the ABI puts the ones past the
//! eighth on the stack. Floats and structs by value are not supported -- Xlib
//! does not need them. Add libffi if a library does.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

extern "C" {
    fn dlopen(file: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

const RTLD_LAZY: c_int = 1;
const RTLD_GLOBAL: c_int = 8;

macro_rules! call_arity {
    ($f:expr, $a:expr, $($n:literal => ($($i:literal),*)),+ $(,)?) => {
        match $a.len() {
            $($n => {
                let g: extern "C" fn($(cast!($i)),*) -> u64 = unsafe { std::mem::transmute($f) };
                g($($a[$i]),*)
            })+
            k => return Err(format!("foreign call with {} arguments; at most 12 are supported", k)),
        }
    };
}
macro_rules! cast {
    ($i:literal) => {
        u64
    };
}

/// C functions whose arguments past the given count are variadic, and how many
/// come before the `...`. AArch64 passes variadic arguments on the stack, not
/// in registers, so calling one through a plain prototype hands it whatever
/// happened to be there: `fcntl(fd, F_SETOWN, pid)` answered ESRCH, and the
/// world fell back to busy-polling stdin instead of waiting for SIGIO.
const VARIADIC: &[(&str, usize)] =
    &[("fcntl", 2), ("ioctl", 2), ("open", 2), ("openat", 3), ("syscall", 1)];

pub fn variadic_fixed(name: &str) -> Option<usize> {
    VARIADIC.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
}

/// Apple's arm64 ABI packs stack arguments at their natural size where the
/// generic AAPCS64 gives each one an eight-byte slot, so a prototype of all
/// `u64`s misplaces every argument past the eighth. XCopyArea read `dest_y`
/// out of the top half of `dest_x` and blitted every repaint to y = 0, which
/// is why Morphic's picture sat above the morphs you were clicking on.
/// Lay the tail out the way the callee will read it.
#[cfg(all(target_arch = "aarch64", target_vendor = "apple"))]
fn pack_stack_args(a: &[u64], sizes: &[usize]) -> Vec<u64> {
    if a.len() <= 8 {
        return a.to_vec();
    }
    let mut words = a[..8].to_vec();
    let mut tail: Vec<u8> = vec![];
    for (i, x) in a[8..].iter().enumerate() {
        let n = match sizes.get(i + 8) {
            Some(&(1 | 2 | 4)) => sizes[i + 8],
            _ => 8,
        };
        while tail.len() % n != 0 {
            tail.push(0);
        }
        tail.extend_from_slice(&x.to_le_bytes()[..n]);
    }
    tail.resize(tail.len().next_multiple_of(8), 0);
    words.extend(tail.chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())));
    words
}

/// Everywhere else a stack argument gets its own eight-byte slot, which is
/// what passing every argument as a `u64` already does.
#[cfg(not(all(target_arch = "aarch64", target_vendor = "apple")))]
fn pack_stack_args(a: &[u64], _sizes: &[usize]) -> Vec<u64> {
    a.to_vec()
}

#[derive(Default)]
pub struct Ffi {
    libs: Vec<*mut c_void>,
    cache: HashMap<String, Option<*mut c_void>>,
}

impl Ffi {
    /// Load a shared library; later lookups search it.
    pub fn load(&mut self, path: &str) -> Result<(), String> {
        let c = CString::new(path).map_err(|e| e.to_string())?;
        let h = unsafe { dlopen(c.as_ptr(), RTLD_LAZY | RTLD_GLOBAL) };
        if h.is_null() {
            return Err(format!("cannot load {}", path));
        }
        self.libs.push(h);
        self.cache.clear();
        Ok(())
    }

    pub fn is_loaded(&self) -> bool {
        !self.libs.is_empty()
    }

    fn raw_sym(&self, name: &str) -> Option<*mut c_void> {
        let c = CString::new(name).ok()?;
        for &h in &self.libs {
            let p = unsafe { dlsym(h, c.as_ptr()) };
            if !p.is_null() {
                return Some(p);
            }
        }
        // also try the process's own symbols
        let p = unsafe { dlsym(std::ptr::null_mut(), c.as_ptr()) };
        if p.is_null() {
            None
        } else {
            Some(p)
        }
    }

    pub fn sym(&mut self, name: &str) -> Option<*mut c_void> {
        if let Some(&v) = self.cache.get(name) {
            return v;
        }
        let v = self.raw_sym(name);
        self.cache.insert(name.to_string(), v);
        v
    }

    /// Split a glue primitive name into its C function and the Self selector
    /// that follows it, by finding the longest prefix that actually resolves.
    pub fn split_glue(&mut self, prim: &str) -> Option<(*mut c_void, usize)> {
        // Try every cut, longest first, and let dlsym decide. Generated glue
        // puts a lowercase selector right after the C name
        // (XFlushxFlush), but hand-written VM primitives do not:
        // _XOpenDisplayResultProxy: needs the cut before an uppercase R.
        let mut cuts: Vec<usize> = (1..=prim.len()).filter(|&i| prim.is_char_boundary(i)).collect();
        cuts.sort_by(|a, c| c.cmp(a)); // longest first
        for i in cuts {
            if let Some(f) = self.sym(&prim[..i]) {
                return Some((f, i));
            }
            // Much of Xlib's interface is macros, which have no symbol; Xlib
            // ships an X-prefixed function for each (DefaultScreen ->
            // XDefaultScreen), so try that too.
            if let Some(f) = self.sym(&format!("X{}", &prim[..i])) {
                return Some((f, i));
            }
        }
        None
    }

    /// Call a variadic C function: the arguments past `fixed` go through the
    /// `...` so the ABI puts them where the callee's va_list looks.
    pub fn call_variadic(f: *mut c_void, fixed: usize, a: &[u64]) -> Result<u64, String> {
        Ok(match (fixed, a.len()) {
            (1, 2) => {
                let g: extern "C" fn(u64, ...) -> u64 = unsafe { std::mem::transmute(f) };
                g(a[0], a[1])
            }
            (1, 3) => {
                let g: extern "C" fn(u64, ...) -> u64 = unsafe { std::mem::transmute(f) };
                g(a[0], a[1], a[2])
            }
            (1, 4) => {
                let g: extern "C" fn(u64, ...) -> u64 = unsafe { std::mem::transmute(f) };
                g(a[0], a[1], a[2], a[3])
            }
            (1, 5) => {
                let g: extern "C" fn(u64, ...) -> u64 = unsafe { std::mem::transmute(f) };
                g(a[0], a[1], a[2], a[3], a[4])
            }
            (2, 3) => {
                let g: extern "C" fn(u64, u64, ...) -> u64 = unsafe { std::mem::transmute(f) };
                g(a[0], a[1], a[2])
            }
            (3, 4) => {
                let g: extern "C" fn(u64, u64, u64, ...) -> u64 = unsafe { std::mem::transmute(f) };
                g(a[0], a[1], a[2], a[3])
            }
            // nothing variadic to pass: the plain prototype is right
            (n, k) if k <= n => return Self::call(f, a),
            (n, k) => return Err(format!("variadic call with {} fixed and {} arguments", n, k)),
        })
    }

    /// Call `f` with the arguments' declared sizes known, so the ones past the
    /// eighth land where the callee looks for them. `sizes` runs parallel to
    /// `a`; a missing or zero entry means a full word.
    pub fn call_sized(f: *mut c_void, a: &[u64], sizes: &[usize]) -> Result<u64, String> {
        Self::call(f, &pack_stack_args(a, sizes))
    }

    /// Call `f` with the exact number of machine-word arguments given, so the
    /// ABI places the ones past the eighth on the stack itself.
    pub fn call(f: *mut c_void, a: &[u64]) -> Result<u64, String> {
        Ok(call_arity!(f, a,
            0 => (),
            1 => (0),
            2 => (0, 1),
            3 => (0, 1, 2),
            4 => (0, 1, 2, 3),
            5 => (0, 1, 2, 3, 4),
            6 => (0, 1, 2, 3, 4, 5),
            7 => (0, 1, 2, 3, 4, 5, 6),
            8 => (0, 1, 2, 3, 4, 5, 6, 7),
            9 => (0, 1, 2, 3, 4, 5, 6, 7, 8),
            10 => (0, 1, 2, 3, 4, 5, 6, 7, 8, 9),
            11 => (0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10),
            12 => (0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::Ffi;

    /// XCopyArea's tail: two `int`s, the arguments the ABI puts on the stack.
    #[allow(clippy::too_many_arguments)]
    extern "C" fn tail_of_two_ints(
        _1: u64,
        _2: u64,
        _3: u64,
        _4: u64,
        _5: u64,
        _6: u64,
        _7: u64,
        _8: u64,
        dest_x: i32,
        dest_y: i32,
    ) -> u64 {
        (dest_x as u64) << 32 | dest_y as u32 as u64
    }

    #[test]
    fn stack_arguments_land_where_the_callee_reads_them() {
        let f = tail_of_two_ints as *mut std::os::raw::c_void;
        let sizes = [8, 8, 8, 8, 8, 8, 8, 8, 4, 4];
        let args = [0, 0, 0, 0, 0, 0, 0, 0, 86, 15];
        assert_eq!(Ffi::call_sized(f, &args, &sizes).unwrap(), 86 << 32 | 15);
    }
}
