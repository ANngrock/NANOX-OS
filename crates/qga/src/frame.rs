//! Message framing on the agent's byte channel (virtio-serial
//! `org.qemu.guest_agent.0`): a message is one balanced top-level JSON
//! object (or array), as QEMU's streaming parser sees it. A 0xFF byte
//! resets the parser (clients send it to discard stale input); characters
//! outside a message are reported once as "stray".

pub enum Feed {
    /// More bytes needed.
    Need,
    /// A complete message is in [`Channel::message`]; call [`Channel::reset`].
    Message,
    /// The message did not fit the buffer; it was skipped to its end.
    TooLarge,
    /// A character outside any message (reported once per run).
    Stray(u8),
}

pub struct Channel<const N: usize> {
    buf: [u8; N],
    len: usize,
    depth: usize,
    in_string: bool,
    escape: bool,
    too_big: bool,
    stray_reported: bool,
}

impl<const N: usize> Default for Channel<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Channel<N> {
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
            depth: 0,
            in_string: false,
            escape: false,
            too_big: false,
            stray_reported: false,
        }
    }

    pub fn message(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    pub fn reset(&mut self) {
        self.len = 0;
        self.depth = 0;
        self.in_string = false;
        self.escape = false;
        self.too_big = false;
        self.stray_reported = false;
    }

    pub fn feed(&mut self, b: u8) -> Feed {
        if b == 0xFF {
            self.reset();
            return Feed::Need;
        }
        if self.depth == 0 {
            match b {
                b'{' | b'[' => {
                    self.reset();
                    self.depth = 1;
                    self.store(b);
                    return Feed::Need;
                }
                b' ' | b'\t' | b'\r' => return Feed::Need,
                b'\n' => {
                    self.stray_reported = false;
                    return Feed::Need;
                }
                _ if self.stray_reported => return Feed::Need,
                _ => {
                    self.stray_reported = true;
                    return Feed::Stray(b);
                }
            }
        }
        self.store(b);
        if self.in_string {
            if self.escape {
                self.escape = false;
            } else if b == b'\\' {
                self.escape = true;
            } else if b == b'"' {
                self.in_string = false;
            }
            return Feed::Need;
        }
        match b {
            b'"' => self.in_string = true,
            b'{' | b'[' => self.depth += 1,
            b'}' | b']' => {
                self.depth -= 1;
                if self.depth == 0 {
                    return if self.too_big {
                        self.reset();
                        Feed::TooLarge
                    } else {
                        Feed::Message
                    };
                }
            }
            _ => {}
        }
        Feed::Need
    }

    fn store(&mut self, b: u8) {
        if self.len < N {
            self.buf[self.len] = b;
            self.len += 1;
        } else {
            self.too_big = true;
        }
    }
}
