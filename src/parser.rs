//! Self grammar -- see vm/src/any/parser/parser.cpp.
//!
//! Object literals double as grouping parens: `(expr)` is an object with a
//! body, which the compiler inlines; `(| slots |)` has no body and becomes a
//! constant object; in slot position a body makes it a method.

use crate::lexer::{lex, lex_script, Spanned, Tok};

#[derive(Debug, Clone)]
pub enum SendKind {
    Normal,
    ImplicitSelf,
    UndirectedResend,
    Directed(String),
}

#[derive(Debug, Clone)]
pub enum Expr {
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    SelfE,
    Send { recv: Option<Box<Expr>>, sel: String, args: Vec<Expr>, kind: SendKind, line: u32 },
    ObjLit(Box<ObjLit>),
    Block(Box<ObjLit>),
    Return(Box<Expr>),
}

#[derive(Debug, Clone)]
pub struct SlotDecl {
    pub name: String,
    pub parent: bool,
    pub mutable: bool,
    pub init: Option<Expr>,
    /// the `{ '...' }` group this slot stands in, if any
    pub anno: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct ObjLit {
    pub args: Vec<String>,
    pub slots: Vec<SlotDecl>,
    pub body: Vec<Expr>,
    pub line: u32,
    /// what `{} = '...'` in the slot list said
    pub anno: Option<Vec<u8>>,
}

/// Source line a statement starts on, for error traces.
pub fn stmt_line(e: &Expr) -> u32 {
    match e {
        Expr::Send { recv: Some(r), line, .. } => match stmt_line(r) {
            0 => *line,
            l => l,
        },
        Expr::Send { line, .. } => *line,
        Expr::ObjLit(o) | Expr::Block(o) => o.line,
        Expr::Return(x) => stmt_line(x),
        _ => 0,
    }
}

pub struct Parser {
    t: Vec<Spanned>,
    i: usize,
}

/// Nested `{ }` groups join their annotations with DEL, as
/// `extend_annotation` does in vm/src/any/parser/expr.cpp -- which is why a
/// world's annotations read as `ModuleInfo: ...\x7fVisibility: public`.
fn extend_anno(outer: &[u8], s: &[u8]) -> Vec<u8> {
    if outer.is_empty() {
        return s.to_vec();
    }
    let mut v = outer.to_vec();
    v.push(0x7f);
    v.extend_from_slice(s);
    v
}

pub fn parse_program(src: &[u8]) -> Result<Vec<Expr>, String> {
    let mut p = Parser { t: lex(src)?, i: 0 };
    let body = p.statements(&Tok::Eof)?;
    Ok(body)
}

/// The same, for a file: its statements are separated by newlines as well as
/// by periods. See `lex_script`.
pub fn parse_script(src: &[u8]) -> Result<Vec<Expr>, String> {
    let mut p = Parser { t: lex_script(src)?, i: 0 };
    let body = p.statements(&Tok::Eof)?;
    Ok(body)
}

/// Parse a string as what stands inside a `( )`: a slot list, then statements.
/// Parser::readBody in the C++ VM, which `_ParseObjectFileName:ErrorObj:` uses.
pub fn parse_body(src: &[u8]) -> Result<ObjLit, String> {
    let mut p = Parser { t: lex(src)?, i: 0 };
    p.object(&Tok::Eof)
}

impl Parser {
    fn tok(&self) -> &Tok {
        &self.t[self.i].tok
    }
    fn at(&self, k: usize) -> &Tok {
        self.t.get(self.i + k).map(|s| &s.tok).unwrap_or(&Tok::Eof)
    }
    fn line(&self) -> u32 {
        self.t[self.i].line
    }
    fn bump(&mut self) -> Tok {
        let t = self.t[self.i].tok.clone();
        self.i += 1;
        t
    }
    fn err<T>(&self, m: &str) -> Result<T, String> {
        Err(format!("line {}: {} (near {:?})", self.line(), m, self.tok()))
    }
    fn expect(&mut self, t: &Tok, what: &str) -> Result<(), String> {
        if self.tok() == t {
            self.i += 1;
            Ok(())
        } else {
            self.err(&format!("expected {}", what))
        }
    }
    /// Ops lex greedily, so `*=` must be split back into `*` and `=`.
    fn eat_op_prefix(&mut self, ch: char) -> bool {
        if let Tok::Op(s) = self.tok() {
            if s.starts_with(ch) {
                let rest = s[ch.len_utf8()..].to_string();
                if rest.is_empty() {
                    self.i += 1;
                } else {
                    self.t[self.i].tok = Tok::Op(rest);
                }
                return true;
            }
        }
        false
    }
    fn eat_exact_op(&mut self, s: &str) -> bool {
        if self.tok() == &Tok::Op(s.to_string()) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    // ------------------------------------------------------------ statements

    fn statements(&mut self, close: &Tok) -> Result<Vec<Expr>, String> {
        let mut out = vec![];
        loop {
            while self.tok() == &Tok::Dot || self.tok() == &Tok::Accept {
                self.i += 1;
            }
            if self.tok() == close {
                break;
            }
            if self.tok() == &Tok::Eof {
                return self.err("unexpected end of input");
            }
            let e = if self.eat_op_prefix('^') {
                Expr::Return(Box::new(self.expr()?))
            } else {
                self.expr()?
            };
            out.push(e);
            // A script is a sequence of expressions, and the `.` between them
            // is optional: evalExpressions in shell.cpp reads one expression
            // at a time and readExpr eats a `.` only if there is one. That is
            // how a fileout's header -- two string literals, no period --
            // reads. Inside a `( )` a period is still required, as parseBody
            // has it.
            if close != &Tok::Eof && self.tok() != &Tok::Dot && self.tok() != close {
                return self.err("expected '.' between statements");
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------- expression tree

    /// `Some(kind)` if the tokens at `i` are a `p.` / `resend.` prefix.
    fn delegate_prefix(&self) -> Option<SendKind> {
        if self.at(1) != &Tok::Delegate {
            return None;
        }
        match self.at(0) {
            Tok::Name(n) => Some(SendKind::Directed(n.clone())),
            Tok::Resend => Some(SendKind::UndirectedResend),
            _ => None,
        }
    }

    fn expr(&mut self) -> Result<Expr, String> {
        if let Some(kind) = self.delegate_prefix() {
            if matches!(self.at(2), Tok::Kw(_)) {
                self.i += 2;
                return self.kw_tail(None, kind);
            }
        }
        if matches!(self.tok(), Tok::Kw(_)) {
            return self.kw_tail(None, SendKind::ImplicitSelf); // `at: 1 Put: 2` -- receiver is self
        }
        let recv = self.binary()?;
        self.kw_tail(Some(recv), SendKind::Normal)
    }

    fn kw_tail(&mut self, recv: Option<Expr>, kind: SendKind) -> Result<Expr, String> {
        let line = self.line();
        let mut sel = match self.tok() {
            Tok::Kw(k) => k.clone(),
            _ => {
                return match recv {
                    Some(r) => Ok(r),
                    None => self.err("expected a keyword message"),
                }
            }
        };
        self.i += 1;
        // an argument is a whole expression, not just a binary one: `a foo: b
        // bar: c` is `a foo: (b bar: c)`, which is how a fileout writes
        // `bootstrap define: ... ToBe: bootstrap addSlotsTo: ... From: ...`.
        // A `Bar:` part is not a keyword, so it still binds to the innermost
        // keyword message being built -- parseExpr in parser.cpp.
        let mut args = vec![self.expr()?];
        while let Tok::CapKw(k) = self.tok().clone() {
            self.i += 1;
            sel.push_str(&k);
            args.push(self.expr()?);
        }
        let kind = match recv {
            Some(_) => SendKind::Normal,
            None => kind,
        };
        Ok(Expr::Send { recv: recv.map(Box::new), sel, args, kind, line })
    }

    fn binary(&mut self) -> Result<Expr, String> {
        let line = self.line();
        let mut e = if let Some(kind) = self.delegate_prefix() {
            if let Tok::Op(_) = self.at(2) {
                self.i += 2;
                let op = match self.bump() {
                    Tok::Op(o) => o,
                    _ => unreachable!(),
                };
                let arg = self.binary_arg()?;
                Expr::Send { recv: None, sel: op, args: vec![arg], kind, line }
            } else {
                self.unary()?
            }
        } else if matches!(self.tok(), Tok::Op(o) if o != "|") {
            // nothing to the left of the operator: `+ 1` is `self + 1`, the
            // implicit-self binary send parseBinary makes. A bare `|` is not
            // an operator here -- it closes a slot list.
            let op = match self.bump() {
                Tok::Op(o) => o,
                _ => unreachable!(),
            };
            let arg = self.binary_arg()?;
            Expr::Send { recv: None, sel: op, args: vec![arg], kind: SendKind::ImplicitSelf, line }
        } else {
            self.unary()?
        };
        while let Tok::Op(op) = self.tok().clone() {
            // a lone `|` always delimits a slot list, never a binary message
            if op == "|" {
                break;
            }
            let line = self.line();
            self.i += 1;
            let arg = self.binary_arg()?;
            e = Expr::Send {
                recv: Some(Box::new(e)),
                sel: op,
                args: vec![arg],
                kind: SendKind::Normal,
                line,
            };
        }
        Ok(e)
    }

    /// The right of a binary send: a unary expression, or a whole keyword
    /// message where one starts -- `a + foo: b` is `a + (self foo: b)`, which
    /// is how `textBox origin + offsetOfItem: i` reads. parseBinary again.
    fn binary_arg(&mut self) -> Result<Expr, String> {
        if matches!(self.tok(), Tok::Kw(_)) {
            self.expr()
        } else {
            self.unary()
        }
    }

    fn unary(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        while let Tok::Name(n) = self.tok().clone() {
            let line = self.line();
            self.i += 1;
            e = Expr::Send {
                recv: Some(Box::new(e)),
                sel: n,
                args: vec![],
                kind: SendKind::Normal,
                line,
            };
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let line = self.line();
        if let Some(kind) = self.delegate_prefix() {
            if let Tok::Name(n) = self.at(2).clone() {
                self.i += 3;
                return Ok(Expr::Send { recv: None, sel: n, args: vec![], kind, line });
            }
            return self.err("expected a message after '.'");
        }
        match self.tok().clone() {
            Tok::Int(v) => {
                self.i += 1;
                Ok(Expr::Int(v))
            }
            Tok::Float(v) => {
                self.i += 1;
                Ok(Expr::Float(v))
            }
            Tok::Str(v) => {
                self.i += 1;
                Ok(Expr::Str(v))
            }
            Tok::SelfKw => {
                self.i += 1;
                Ok(Expr::SelfE)
            }
            Tok::Name(n) => {
                self.i += 1;
                Ok(Expr::Send {
                    recv: None,
                    sel: n,
                    args: vec![],
                    kind: SendKind::ImplicitSelf,
                    line,
                })
            }
            Tok::LParen => {
                self.i += 1;
                Ok(Expr::ObjLit(Box::new(self.object(&Tok::RParen)?)))
            }
            Tok::LBrack => {
                self.i += 1;
                Ok(Expr::Block(Box::new(self.object(&Tok::RBrack)?)))
            }
            _ => self.err("expected an expression"),
        }
    }

    // ------------------------------------------------------------- objects

    /// Opening delimiter already consumed.
    fn object(&mut self, close: &Tok) -> Result<ObjLit, String> {
        let line = self.line();
        let mut o = ObjLit { args: vec![], slots: vec![], body: vec![], line, anno: None };
        if self.eat_op_prefix('|') {
            self.slots(&mut o, &[], &Tok::Op("|".to_string()))?;
        }
        o.body = self.statements(close)?;
        self.expect(close, "a closing delimiter")?;
        Ok(o)
    }

    /// A slot list, or the inside of a `{ '...' ... }` group: same grammar,
    /// different terminator. `close` is the bare `|` that ends a slot list --
    /// `||` and friends are binary selectors -- or the `}` that ends a group.
    /// Every slot in a group carries its annotation, groups nest, and `{} =
    /// '...'` anywhere in them is the object's own annotation.
    fn slots(&mut self, o: &mut ObjLit, anno: &[u8], close: &Tok) -> Result<(), String> {
        loop {
            while self.tok() == &Tok::Dot {
                self.i += 1;
            }
            if self.tok() == close {
                self.i += 1;
                return Ok(());
            }
            if self.tok() == &Tok::Eof {
                return self.err("unterminated slot list");
            }
            if self.tok() == &Tok::LBrace {
                self.i += 1;
                if self.tok() == &Tok::RBrace {
                    self.i += 1;
                    if !self.eat_exact_op("=") {
                        return self.err("expected '=' after '{}'");
                    }
                    o.anno = Some(self.anno_string()?);
                } else {
                    let inner = extend_anno(anno, &self.anno_string()?);
                    self.slots(o, &inner, &Tok::RBrace)?;
                }
                continue; // an annotation is not a slot: no '.' need follow it
            }
            self.one_slot(o, anno)?;
            if self.tok() != &Tok::Dot && self.tok() != close {
                return self.err("expected '.' or '|' after a slot");
            }
        }
    }

    fn anno_string(&mut self) -> Result<Vec<u8>, String> {
        match self.tok().clone() {
            Tok::Str(s) => {
                self.i += 1;
                Ok(s)
            }
            _ => self.err("expected a string holding an annotation"),
        }
    }

    fn one_slot(&mut self, o: &mut ObjLit, anno: &[u8]) -> Result<(), String> {
        let anno = (!anno.is_empty()).then(|| anno.to_vec());
        match self.tok().clone() {
            Tok::Arg(n) => {
                self.i += 1;
                o.args.push(n);
                Ok(())
            }
            Tok::Name(n) => {
                self.i += 1;
                let parent = self.eat_op_prefix('*');
                if self.eat_exact_op("=") {
                    let init = self.expr()?;
                    o.slots.push(SlotDecl {
                        name: n,
                        parent,
                        mutable: false,
                        init: Some(init),
                        anno,
                    });
                } else if self.eat_exact_op("<-") {
                    let init = self.expr()?;
                    o.slots.push(SlotDecl {
                        name: n,
                        parent,
                        mutable: true,
                        init: Some(init),
                        anno,
                    });
                } else {
                    o.slots.push(SlotDecl { name: n, parent, mutable: true, init: None, anno });
                }
                Ok(())
            }
            Tok::Op(op) => {
                // binary method slot: `+ x = ( ... )`
                self.i += 1;
                let mut args = vec![];
                if let Tok::Name(a) = self.tok().clone() {
                    self.i += 1;
                    args.push(a);
                }
                let body = self.method_body(args)?;
                o.slots.push(SlotDecl {
                    name: op,
                    parent: false,
                    mutable: false,
                    init: Some(body),
                    anno,
                });
                Ok(())
            }
            Tok::Kw(k) => {
                // keyword method slot: `at: k Put: v = ( ... )`
                self.i += 1;
                let mut sel = k;
                let mut args = vec![self.slot_arg_name()?];
                while let Tok::CapKw(c) = self.tok().clone() {
                    self.i += 1;
                    sel.push_str(&c);
                    args.push(self.slot_arg_name()?);
                }
                let body = self.method_body(args)?;
                o.slots.push(SlotDecl {
                    name: sel,
                    parent: false,
                    mutable: false,
                    init: Some(body),
                    anno,
                });
                Ok(())
            }
            _ => self.err("expected a slot"),
        }
    }

    fn slot_arg_name(&mut self) -> Result<String, String> {
        match self.tok().clone() {
            Tok::Name(n) => {
                self.i += 1;
                Ok(n)
            }
            _ => self.err("expected an argument name"),
        }
    }

    fn method_body(&mut self, args: Vec<String>) -> Result<Expr, String> {
        if !self.eat_exact_op("=") {
            return self.err("expected '=' in a method slot");
        }
        let close = match self.tok() {
            Tok::LParen => Tok::RParen,
            Tok::LBrack => Tok::RBrack,
            _ => return self.err("expected '(' after '='"),
        };
        self.i += 1;
        let mut o = self.object(&close)?;
        let mut all = args;
        all.extend(o.args.drain(..));
        o.args = all;
        Ok(Expr::ObjLit(Box::new(o)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anno(a: &Option<Vec<u8>>) -> &str {
        a.as_ref().map_or("", |a| std::str::from_utf8(a).unwrap())
    }

    #[test]
    fn annotations_reach_the_object_and_its_slots() {
        let o =
            parse_body(b"| {} = 'mine'. { 'outer' { 'inner' a = 1. } b = 2. } c = 3. |").unwrap();
        assert_eq!(anno(&o.anno), "mine");
        let got: Vec<(&str, &str)> =
            o.slots.iter().map(|d| (d.name.as_str(), anno(&d.anno))).collect();
        // nested groups join with DEL, as extend_annotation does
        assert_eq!(got, vec![("a", "outer\u{7f}inner"), ("b", "outer"), ("c", "")]);
    }

    /// Every `bootstrap read: 'module' From: 'directory'` in a fileout.
    fn reads(src: &[u8]) -> Vec<(String, String)> {
        let src = String::from_utf8_lossy(src).into_owned();
        src.split("bootstrap read: '")
            .skip(1)
            .filter_map(|part| {
                let (module, rest) = part.split_once('\'')?;
                let (dir, _) = rest.trim_start().strip_prefix("From: '")?.split_once('\'')?;
                Some((module.to_string(), dir.to_string()))
            })
            .collect()
    }

    /// What the syntax is for: the fileouts a Self 4 world is built from
    /// parse. That set is what worldBuilder.self reads, and what those read in
    /// turn -- 354 files, the rest of `objects/` being tutorial prose and Self
    /// 3 leftovers the reference VM does not parse either. Skipped when the
    /// reference submodule is not checked out.
    #[test]
    fn the_fileouts_a_world_is_built_from_parse() {
        let root = std::path::Path::new("reference/self/objects");
        let Ok(top) = std::fs::read(root.join("worldBuilder.self")) else { return };
        let mut todo = reads(&top);
        let mut files: Vec<std::path::PathBuf> = vec![];
        while let Some((module, dir)) = todo.pop() {
            let p = root.join(dir).join(module + ".self");
            if files.contains(&p) {
                continue;
            }
            let src = std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {}", p.display(), e));
            files.push(p);
            todo.extend(reads(&src));
        }
        assert!(files.len() > 300, "only {} modules: the walk broke", files.len());
        let bad: Vec<String> = files
            .iter()
            .filter_map(|f| {
                let e = parse_script(&std::fs::read(f).unwrap()).err()?;
                Some(format!("{}: {}", f.display(), e))
            })
            .collect();
        assert!(
            bad.is_empty(),
            "{} of {} fileouts failed:\n{}",
            bad.len(),
            files.len(),
            bad.join("\n")
        );
    }
}
