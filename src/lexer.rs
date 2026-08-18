//! Self tokenizer -- see vm/src/any/parser/scanner.cpp.

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Name(String),
    /// `foo:` -- starts a keyword message (lowercase or `_` initial)
    Kw(String),
    /// `Foo:` -- continues a keyword message
    CapKw(String),
    /// `:foo` -- argument slot
    Arg(String),
    Op(String),
    SelfKw,
    Resend,
    LParen,
    RParen,
    LBrack,
    RBrack,
    /// `{` and `}`, which only ever bracket slot annotations
    LBrace,
    RBrace,
    Dot,
    /// the `.` of `parent.selector`
    Delegate,
    /// a newline outside every bracket, which ends a statement in a file
    Accept,
    Eof,
}

#[derive(Clone, Debug)]
pub struct Spanned {
    pub tok: Tok,
    pub line: u32,
}

fn is_id_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}
fn is_id(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}
/// Everything printable that is neither an identifier char nor structural.
fn is_punct(c: u8) -> bool {
    matches!(
        c,
        b'!' | b'@'
            | b'#'
            | b'$'
            | b'%'
            | b'^'
            | b'&'
            | b'*'
            | b'-'
            | b'+'
            | b'='
            | b'~'
            | b'/'
            | b'?'
            | b'<'
            | b'>'
            | b','
            | b';'
            | b'|'
            | b'\\'
            | b'`'
    )
}

pub struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
    line: u32,
    out: Vec<Spanned>,
    /// how deep in `( )` and `[ ]` the scanner is, and whether a newline out
    /// there ends a statement
    depth: i32,
    script: bool,
}

pub fn lex(src: &[u8]) -> Result<Vec<Spanned>, String> {
    lex_as(src, false)
}

/// Lex a file rather than a string. A file's top-level expressions are
/// separated by newlines outside any bracket, not by periods -- that is the
/// ACCEPT token `Scanner::get_token` hands the parser for anything but a
/// string scanner, and it is what lets a Self 4 fileout write one
/// `bootstrap addSlotsTo: ( | ... | )` per paragraph with no `.` in sight.
pub fn lex_script(src: &[u8]) -> Result<Vec<Spanned>, String> {
    lex_as(src, true)
}

fn lex_as(src: &[u8], script: bool) -> Result<Vec<Spanned>, String> {
    let mut l = Lexer { s: src, i: 0, line: 1, out: vec![], depth: 0, script };
    l.run()?;
    l.out.push(Spanned { tok: Tok::Eof, line: l.line });
    Ok(l.out)
}

impl<'a> Lexer<'a> {
    fn at(&self, k: usize) -> u8 {
        *self.s.get(self.i + k).unwrap_or(&0)
    }
    fn push(&mut self, t: Tok) {
        let line = self.line;
        self.out.push(Spanned { tok: t, line });
    }
    fn err(&self, m: &str) -> String {
        format!("line {}: {}", self.line, m)
    }

    /// Could the previous token end an expression? Decides whether `-` starts
    /// a negative literal or is a binary operator.
    fn prev_ends_expr(&self) -> bool {
        matches!(
            self.out.last().map(|s| &s.tok),
            Some(Tok::Int(_))
                | Some(Tok::Float(_))
                | Some(Tok::Str(_))
                | Some(Tok::Name(_))
                | Some(Tok::SelfKw)
                | Some(Tok::RParen)
                | Some(Tok::RBrack)
        )
    }

    fn run(&mut self) -> Result<(), String> {
        loop {
            // whitespace + comments
            let mut had_space = false;
            loop {
                let c = self.at(0);
                if self.i >= self.s.len() {
                    break;
                }
                if c == b'\n' {
                    self.line += 1;
                    self.i += 1;
                    had_space = true;
                    // one is as good as ten: blank lines say nothing more
                    let last = self.out.last().map(|s| &s.tok);
                    if self.script && self.depth <= 0 && !matches!(last, None | Some(Tok::Accept)) {
                        self.push(Tok::Accept);
                    }
                } else if c.is_ascii_whitespace() {
                    self.i += 1;
                    had_space = true;
                } else if c == b'"' {
                    self.i += 1;
                    while self.i < self.s.len() && self.at(0) != b'"' {
                        if self.at(0) == b'\n' {
                            self.line += 1;
                        }
                        self.i += 1;
                    }
                    if self.i >= self.s.len() {
                        return Err(self.err("unterminated comment"));
                    }
                    self.i += 1;
                    had_space = true;
                } else {
                    break;
                }
            }
            if self.i >= self.s.len() {
                return Ok(());
            }
            let c = self.at(0);
            match c {
                b'(' => {
                    self.i += 1;
                    self.depth += 1;
                    self.push(Tok::LParen);
                }
                b')' => {
                    self.i += 1;
                    self.depth -= 1;
                    self.push(Tok::RParen);
                }
                b'[' => {
                    self.i += 1;
                    self.depth += 1;
                    self.push(Tok::LBrack);
                }
                b']' => {
                    self.i += 1;
                    self.depth -= 1;
                    self.push(Tok::RBrack);
                }
                b'{' => {
                    self.i += 1;
                    self.push(Tok::LBrace);
                }
                b'}' => {
                    self.i += 1;
                    self.push(Tok::RBrace);
                }
                b'\'' => self.read_string()?,
                b'.' => {
                    // `p.sel` delegates; `p. sel` separates statements. The
                    // selector may be an operator -- `resend.,` and `p.+` are
                    // resends of a binary message -- so a punctuation char
                    // after the dot delegates too, as read_name has it.
                    let delegating = !had_space
                        && (is_id_start(self.at(1)) || is_punct(self.at(1)))
                        && matches!(
                            self.out.last().map(|s| &s.tok),
                            Some(Tok::Name(_)) | Some(Tok::Resend)
                        );
                    self.i += 1;
                    self.push(if delegating { Tok::Delegate } else { Tok::Dot });
                }
                b':' if is_id_start(self.at(1)) => {
                    self.i += 1;
                    let n = self.read_ident();
                    self.push(Tok::Arg(n));
                }
                b':' => return Err(self.err("stray ':'")),
                _ if c.is_ascii_digit() => self.read_number()?,
                b'-' if c == b'-' && self.at(1).is_ascii_digit() && !self.prev_ends_expr() => {
                    self.i += 1;
                    self.read_number()?;
                    match self.out.last_mut().map(|s| &mut s.tok) {
                        Some(Tok::Int(v)) => *v = -*v,
                        Some(Tok::Float(v)) => *v = -*v,
                        _ => unreachable!(),
                    }
                }
                _ if is_id_start(c) => {
                    let n = self.read_ident();
                    if self.at(0) == b':' && self.at(1) != b'=' {
                        self.i += 1;
                        let kw = format!("{}:", n);
                        let first = n.as_bytes()[0];
                        if first.is_ascii_uppercase() {
                            self.push(Tok::CapKw(kw));
                        } else {
                            self.push(Tok::Kw(kw));
                        }
                    } else if n == "self" {
                        self.push(Tok::SelfKw);
                    } else if n == "resend" {
                        self.push(Tok::Resend);
                    } else {
                        self.push(Tok::Name(n));
                    }
                }
                _ if is_punct(c) => {
                    let start = self.i;
                    while is_punct(self.at(0)) {
                        self.i += 1;
                    }
                    let op = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
                    self.push(Tok::Op(op));
                }
                _ => return Err(self.err(&format!("unexpected character {:?}", c as char))),
            }
        }
    }

    fn read_ident(&mut self) -> String {
        let start = self.i;
        while is_id(self.at(0)) {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }

    fn read_number(&mut self) -> Result<(), String> {
        let start = self.i;
        while self.at(0).is_ascii_digit() {
            self.i += 1;
        }
        // radix: 16r1F
        if (self.at(0) == b'r' || self.at(0) == b'R') && self.at(1).is_ascii_alphanumeric() {
            let radix: u32 = String::from_utf8_lossy(&self.s[start..self.i])
                .parse()
                .map_err(|_| self.err("bad radix"))?;
            if !(2..=36).contains(&radix) {
                return Err(self.err("radix must be 2..36"));
            }
            self.i += 1;
            let ds = self.i;
            while self.at(0).is_ascii_alphanumeric() {
                self.i += 1;
            }
            let text = String::from_utf8_lossy(&self.s[ds..self.i]).into_owned();
            let v = i64::from_str_radix(&text, radix).map_err(|_| self.err("bad number"))?;
            self.push(Tok::Int(v));
            return Ok(());
        }
        let mut is_float = false;
        if self.at(0) == b'.' && self.at(1).is_ascii_digit() {
            is_float = true;
            self.i += 1;
            while self.at(0).is_ascii_digit() {
                self.i += 1;
            }
        }
        if (self.at(0) == b'e' || self.at(0) == b'E')
            && (self.at(1).is_ascii_digit()
                || ((self.at(1) == b'-' || self.at(1) == b'+') && self.at(2).is_ascii_digit()))
        {
            is_float = true;
            self.i += 2;
            while self.at(0).is_ascii_digit() {
                self.i += 1;
            }
        }
        let text = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
        if is_float {
            self.push(Tok::Float(text.parse().map_err(|_| self.err("bad float"))?));
        } else {
            self.push(Tok::Int(text.parse().map_err(|_| self.err("integer too large"))?));
        }
        Ok(())
    }

    fn read_string(&mut self) -> Result<(), String> {
        self.i += 1;
        let mut out: Vec<u8> = vec![];
        loop {
            if self.i >= self.s.len() {
                return Err(self.err("unterminated string"));
            }
            let c = self.at(0);
            self.i += 1;
            match c {
                b'\'' => break,
                b'\n' => {
                    self.line += 1;
                    out.push(b'\n');
                }
                b'\\' => {
                    let e = self.at(0);
                    self.i += 1;
                    match e {
                        b't' => out.push(b'\t'),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b'f' => out.push(12),
                        b'b' => out.push(8),
                        b'v' => out.push(11),
                        b'a' => out.push(7),
                        b'0' => out.push(0),
                        b'\\' => out.push(b'\\'),
                        b'\'' => out.push(b'\''),
                        b'"' => out.push(b'"'),
                        b'\n' => self.line += 1, // line continuation
                        b'x' | b'd' | b'o' => {
                            let (base, n) = match e {
                                b'x' => (16, 2),
                                b'd' => (10, 3),
                                _ => (8, 3),
                            };
                            let ds = self.i;
                            let mut k = 0;
                            while k < n && (self.at(0) as char).is_digit(base) {
                                self.i += 1;
                                k += 1;
                            }
                            let text = String::from_utf8_lossy(&self.s[ds..self.i]).into_owned();
                            let v = u32::from_str_radix(&text, base)
                                .map_err(|_| self.err("bad numeric escape"))?;
                            out.push(v as u8);
                        }
                        _ => return Err(self.err(&format!("bad escape \\{}", e as char))),
                    }
                }
                _ => out.push(c),
            }
        }
        self.push(Tok::Str(out));
        Ok(())
    }
}
