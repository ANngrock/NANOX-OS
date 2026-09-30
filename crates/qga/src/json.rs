//! A small, allocation-free JSON reader (RFC 8259) for the guest agent:
//! validates a whole message, reports errors with the wording of QEMU's
//! JSON parser (captured from `qemu-ga` 9.2.4, tests/fixtures), and lets
//! the caller walk objects and read strings and integers in place.

pub const MAX_DEPTH: usize = 16;
pub const MAX_KEYS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Object,
    Array,
    String,
    /// A number without fraction or exponent.
    Int,
    Number,
    True,
    False,
    Null,
}

/// A value: its kind and byte range in the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Val {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    ExpectingValue,
    MissingColon,
    ExpectedSeparatorDict,
    ExpectedSeparatorList,
    DuplicateKey,
    BadString,
    TooDeep,
    TooManyKeys,
    Trailing(u8),
}

impl Error {
    /// The text after "JSON parse error, " in QEMU's reply.
    pub fn text(self) -> &'static str {
        match self {
            Error::ExpectingValue => "expecting value",
            Error::MissingColon => "missing : in object pair",
            Error::ExpectedSeparatorDict => "expected separator in dict",
            Error::ExpectedSeparatorList => "expected separator in list",
            Error::DuplicateKey => "duplicate key",
            Error::BadString => "invalid string",
            Error::TooDeep => "nesting too deep",
            Error::TooManyKeys => "too many keys",
            Error::Trailing(_) => "stray character",
        }
    }
}

fn skip_ws(s: &[u8], mut p: usize) -> usize {
    while p < s.len() && matches!(s[p], b' ' | b'\t' | b'\r' | b'\n') {
        p += 1;
    }
    p
}

/// Parses `s` as exactly one JSON value (surrounded by whitespace).
pub fn parse(s: &[u8]) -> Result<Val, Error> {
    let p = skip_ws(s, 0);
    let (v, end) = value(s, p, 0)?;
    let end = skip_ws(s, end);
    if end != s.len() {
        return Err(Error::Trailing(s[end]));
    }
    Ok(v)
}

fn string_end(s: &[u8], start: usize) -> Result<usize, Error> {
    // s[start] == '"'
    let mut p = start + 1;
    while p < s.len() {
        match s[p] {
            b'"' => return Ok(p + 1),
            b'\\' => match s.get(p + 1) {
                Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => p += 2,
                Some(b'u') => {
                    let ok = s
                        .get(p + 2..p + 6)
                        .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit));
                    if !ok {
                        return Err(Error::BadString);
                    }
                    p += 6;
                }
                _ => return Err(Error::BadString),
            },
            c if c < 0x20 => return Err(Error::BadString),
            _ => p += 1,
        }
    }
    Err(Error::BadString)
}

fn number(s: &[u8], start: usize) -> Result<(Val, usize), Error> {
    let mut p = start;
    if s.get(p) == Some(&b'-') {
        p += 1;
    }
    match s.get(p) {
        Some(b'0') => p += 1,
        Some(b'1'..=b'9') => {
            while s.get(p).is_some_and(u8::is_ascii_digit) {
                p += 1;
            }
        }
        _ => return Err(Error::ExpectingValue),
    }
    let mut kind = Kind::Int;
    if s.get(p) == Some(&b'.') {
        kind = Kind::Number;
        p += 1;
        if !s.get(p).is_some_and(u8::is_ascii_digit) {
            return Err(Error::ExpectingValue);
        }
        while s.get(p).is_some_and(u8::is_ascii_digit) {
            p += 1;
        }
    }
    if matches!(s.get(p), Some(b'e' | b'E')) {
        kind = Kind::Number;
        p += 1;
        if matches!(s.get(p), Some(b'+' | b'-')) {
            p += 1;
        }
        if !s.get(p).is_some_and(u8::is_ascii_digit) {
            return Err(Error::ExpectingValue);
        }
        while s.get(p).is_some_and(u8::is_ascii_digit) {
            p += 1;
        }
    }
    Ok((
        Val {
            kind,
            start,
            end: p,
        },
        p,
    ))
}

fn literal(s: &[u8], p: usize, word: &[u8], kind: Kind) -> Result<(Val, usize), Error> {
    if s.get(p..p + word.len()) == Some(word) {
        Ok((
            Val {
                kind,
                start: p,
                end: p + word.len(),
            },
            p + word.len(),
        ))
    } else {
        Err(Error::ExpectingValue)
    }
}

fn value(s: &[u8], p: usize, depth: usize) -> Result<(Val, usize), Error> {
    if depth >= MAX_DEPTH {
        return Err(Error::TooDeep);
    }
    match s.get(p) {
        Some(b'{') => object(s, p, depth),
        Some(b'[') => array(s, p, depth),
        Some(b'"') => {
            let end = string_end(s, p)?;
            Ok((
                Val {
                    kind: Kind::String,
                    start: p,
                    end,
                },
                end,
            ))
        }
        Some(b'-' | b'0'..=b'9') => number(s, p),
        Some(b't') => literal(s, p, b"true", Kind::True),
        Some(b'f') => literal(s, p, b"false", Kind::False),
        Some(b'n') => literal(s, p, b"null", Kind::Null),
        _ => Err(Error::ExpectingValue),
    }
}

fn object(s: &[u8], start: usize, depth: usize) -> Result<(Val, usize), Error> {
    let mut keys: [(usize, usize); MAX_KEYS] = [(0, 0); MAX_KEYS];
    let mut n = 0;
    let mut p = skip_ws(s, start + 1);
    if s.get(p) == Some(&b'}') {
        return Ok((
            Val {
                kind: Kind::Object,
                start,
                end: p + 1,
            },
            p + 1,
        ));
    }
    loop {
        if s.get(p) != Some(&b'"') {
            return Err(Error::ExpectingValue);
        }
        let kend = string_end(s, p)?;
        if keys[..n].iter().any(|&(a, b)| s[a..b] == s[p..kend]) {
            return Err(Error::DuplicateKey);
        }
        if n == MAX_KEYS {
            return Err(Error::TooManyKeys);
        }
        keys[n] = (p, kend);
        n += 1;
        p = skip_ws(s, kend);
        if s.get(p) != Some(&b':') {
            return Err(Error::MissingColon);
        }
        p = skip_ws(s, p + 1);
        let (_, vend) = value(s, p, depth + 1)?;
        p = skip_ws(s, vend);
        match s.get(p) {
            Some(b',') => p = skip_ws(s, p + 1),
            Some(b'}') => {
                return Ok((
                    Val {
                        kind: Kind::Object,
                        start,
                        end: p + 1,
                    },
                    p + 1,
                ))
            }
            _ => return Err(Error::ExpectedSeparatorDict),
        }
    }
}

fn array(s: &[u8], start: usize, depth: usize) -> Result<(Val, usize), Error> {
    let mut p = skip_ws(s, start + 1);
    if s.get(p) == Some(&b']') {
        return Ok((
            Val {
                kind: Kind::Array,
                start,
                end: p + 1,
            },
            p + 1,
        ));
    }
    loop {
        let (_, vend) = value(s, p, depth + 1)?;
        p = skip_ws(s, vend);
        match s.get(p) {
            Some(b',') => p = skip_ws(s, p + 1),
            Some(b']') => {
                return Ok((
                    Val {
                        kind: Kind::Array,
                        start,
                        end: p + 1,
                    },
                    p + 1,
                ))
            }
            _ => return Err(Error::ExpectedSeparatorList),
        }
    }
}

/// End (exclusive) of the value starting at `p` in a validated message.
pub fn value_end(s: &[u8], p: usize) -> Option<usize> {
    value(s, p, 0).ok().map(|(_, e)| e)
}

/// Members of an already validated object, in order: (key, value).
pub struct Members<'a> {
    s: &'a [u8],
    p: usize,
    end: usize,
}

pub fn members(s: &[u8], obj: Val) -> Members<'_> {
    debug_assert_eq!(obj.kind, Kind::Object);
    Members {
        s,
        p: skip_ws(s, obj.start + 1),
        end: obj.end,
    }
}

impl Iterator for Members<'_> {
    type Item = (Val, Val);

    fn next(&mut self) -> Option<(Val, Val)> {
        if self.p >= self.end || self.s[self.p] == b'}' {
            return None;
        }
        let kend = string_end(self.s, self.p).ok()?;
        let key = Val {
            kind: Kind::String,
            start: self.p,
            end: kend,
        };
        let p = skip_ws(self.s, kend);
        let p = skip_ws(self.s, p + 1); // the colon
        let (val, vend) = value(self.s, p, 0).ok()?;
        let mut p = skip_ws(self.s, vend);
        if self.s.get(p) == Some(&b',') {
            p = skip_ws(self.s, p + 1);
        }
        self.p = p;
        Some((key, val))
    }
}

/// Decodes the string `v` into `out` (UTF-8); None if it does not fit.
pub fn unescape(s: &[u8], v: Val, out: &mut [u8]) -> Option<usize> {
    debug_assert_eq!(v.kind, Kind::String);
    let raw = &s[v.start + 1..v.end - 1];
    let mut n = 0;
    let mut i = 0;
    let mut put = |b: &[u8], n: &mut usize| -> Option<()> {
        out.get_mut(*n..*n + b.len())?.copy_from_slice(b);
        *n += b.len();
        Some(())
    };
    while i < raw.len() {
        if raw[i] != b'\\' {
            put(&raw[i..=i], &mut n)?;
            i += 1;
            continue;
        }
        let c = match raw[i + 1] {
            b'"' => b'"',
            b'\\' => b'\\',
            b'/' => b'/',
            b'b' => 8,
            b'f' => 12,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            _ => {
                let mut cp = hex4(&raw[i + 2..i + 6]);
                i += 6;
                if (0xD800..0xDC00).contains(&cp)
                    && raw.get(i) == Some(&b'\\')
                    && raw.get(i + 1) == Some(&b'u')
                {
                    let lo = hex4(&raw[i + 2..i + 6]);
                    if (0xDC00..0xE000).contains(&lo) {
                        cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                        i += 6;
                    }
                }
                let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                let mut buf = [0u8; 4];
                put(ch.encode_utf8(&mut buf).as_bytes(), &mut n)?;
                continue;
            }
        };
        put(&[c], &mut n)?;
        i += 2;
    }
    Some(n)
}

fn hex4(h: &[u8]) -> u32 {
    h.iter()
        .fold(0, |a, &c| a << 4 | (c as char).to_digit(16).unwrap_or(0))
}

/// The value of an integer token, or None when it does not fit an i64
/// (QEMU then treats it as a float and rejects it as an integer).
pub fn as_i64(s: &[u8], v: Val) -> Option<i64> {
    if v.kind != Kind::Int {
        return None;
    }
    core::str::from_utf8(&s[v.start..v.end]).ok()?.parse().ok()
}
