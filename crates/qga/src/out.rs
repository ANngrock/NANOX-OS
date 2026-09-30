//! Bounded JSON output in QEMU's format: `", "` and `": "` separators, all
//! non-ASCII characters escaped as `\uXXXX` in upper-case hex (as in
//! captured `qemu-ga` replies).

use crate::json::{self, Kind, Val};

pub struct Out<'a> {
    buf: &'a mut [u8],
    len: usize,
    overflow: bool,
}

impl<'a> Out<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            len: 0,
            overflow: false,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    pub fn raw(&mut self, b: &[u8]) {
        match self.buf.get_mut(self.len..self.len + b.len()) {
            Some(d) => {
                d.copy_from_slice(b);
                self.len += b.len();
            }
            None => self.overflow = true,
        }
    }

    pub fn str(&mut self, s: &str) {
        self.raw(s.as_bytes());
    }

    pub fn int(&mut self, v: i64) {
        self.decimal(v.unsigned_abs(), v < 0);
    }

    pub fn uint(&mut self, v: u64) {
        self.decimal(v, false);
    }

    fn decimal(&mut self, mut v: u64, neg: bool) {
        let mut t = [0u8; 21];
        let mut i = t.len();
        loop {
            i -= 1;
            t[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        if neg {
            i -= 1;
            t[i] = b'-';
        }
        self.raw(&t[i..]);
    }

    pub fn bool(&mut self, v: bool) {
        self.str(if v { "true" } else { "false" });
    }

    /// `"` + the escaped pieces + `"`.
    pub fn string(&mut self, pieces: &[&[u8]]) {
        self.raw(b"\"");
        for p in pieces {
            self.escaped(p);
        }
        self.raw(b"\"");
    }

    fn escaped(&mut self, mut b: &[u8]) {
        while !b.is_empty() {
            let (valid, rest_start) = match core::str::from_utf8(b) {
                Ok(s) => (s, b.len()),
                Err(e) => {
                    let n = e.valid_up_to();
                    // SAFETY-free: the prefix is valid by from_utf8's contract.
                    (core::str::from_utf8(&b[..n]).unwrap_or(""), n)
                }
            };
            for c in valid.chars() {
                self.escape_char(c);
            }
            if rest_start == b.len() {
                return;
            }
            // Invalid byte(s): one replacement character, skip them.
            self.escape_char('\u{FFFD}');
            let skip = match core::str::from_utf8(&b[rest_start..]) {
                Err(e) => e.error_len().unwrap_or(b.len() - rest_start),
                Ok(_) => 0,
            };
            b = &b[rest_start + skip.max(1)..];
        }
    }

    fn escape_char(&mut self, c: char) {
        match c {
            '"' => self.raw(b"\\\""),
            '\\' => self.raw(b"\\\\"),
            '\u{8}' => self.raw(b"\\b"),
            '\u{c}' => self.raw(b"\\f"),
            '\n' => self.raw(b"\\n"),
            '\r' => self.raw(b"\\r"),
            '\t' => self.raw(b"\\t"),
            c if (c as u32) < 0x20 || (c as u32) >= 0x7F => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    self.raw(b"\\u");
                    for shift in [12, 8, 4, 0] {
                        self.raw(&[b"0123456789ABCDEF"[usize::from(*u >> shift & 15)]]);
                    }
                }
            }
            c => self.raw(&[c as u8]),
        }
    }

    /// Re-serializes a validated JSON value in QEMU's normal form. Strings
    /// longer than the scratch buffer are cut (the caller limits the size).
    pub fn value(&mut self, s: &[u8], v: Val) {
        match v.kind {
            Kind::Object => {
                self.raw(b"{");
                let mut first = true;
                for (k, x) in json::members(s, v) {
                    if !first {
                        self.raw(b", ");
                    }
                    first = false;
                    self.json_string_val(s, k);
                    self.raw(b": ");
                    self.value(s, x);
                }
                self.raw(b"}");
            }
            Kind::Array => {
                self.raw(b"[");
                let mut p = v.start + 1;
                let mut first = true;
                loop {
                    while p < v.end && matches!(s[p], b' ' | b'\t' | b'\r' | b'\n' | b',') {
                        p += 1;
                    }
                    if p >= v.end || s[p] == b']' {
                        break;
                    }
                    let Some(x) = value_at(s, p) else { break };
                    if !first {
                        self.raw(b", ");
                    }
                    first = false;
                    self.value(s, x);
                    p = x.end;
                }
                self.raw(b"]");
            }
            Kind::String => self.json_string_val(s, v),
            _ => self.raw(&s[v.start..v.end]),
        }
    }

    fn json_string_val(&mut self, s: &[u8], v: Val) {
        let mut tmp = [0u8; 256];
        match json::unescape(s, v, &mut tmp) {
            Some(n) => self.string(&[&tmp[..n]]),
            None => self.raw(b"\"\""),
        }
    }
}

/// The value starting at `p` in an already validated message.
fn value_at(s: &[u8], p: usize) -> Option<Val> {
    // Wrap the value in a one-element array to reuse the validator.
    let end = json::value_end(s, p)?;
    let kind = match s[p] {
        b'{' => Kind::Object,
        b'[' => Kind::Array,
        b'"' => Kind::String,
        b't' => Kind::True,
        b'f' => Kind::False,
        b'n' => Kind::Null,
        _ => {
            if s[p..end].iter().any(|&c| matches!(c, b'.' | b'e' | b'E')) {
                Kind::Number
            } else {
                Kind::Int
            }
        }
    };
    Some(Val {
        kind,
        start: p,
        end,
    })
}
