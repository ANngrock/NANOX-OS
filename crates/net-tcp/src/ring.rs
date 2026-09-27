//! Byte ring over a caller-provided buffer.

pub(crate) struct Ring<'a> {
    buf: &'a mut [u8],
    head: usize,
    len: usize,
}

impl<'a> Ring<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            head: 0,
            len: 0,
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn free(&self) -> usize {
        self.buf.len() - self.len
    }

    /// Appends as much of `data` as fits; returns the number of bytes taken.
    pub(crate) fn push(&mut self, data: &[u8]) -> usize {
        let count = data.len().min(self.free());
        for (i, byte) in data[..count].iter().enumerate() {
            let at = (self.head + self.len + i) % self.buf.len();
            self.buf[at] = *byte;
        }
        self.len += count;
        count
    }

    /// Copies bytes starting `offset` bytes after the head without removing
    /// them; returns the number copied.
    pub(crate) fn peek(&self, offset: usize, dst: &mut [u8]) -> usize {
        let count = dst.len().min(self.len.saturating_sub(offset));
        for (i, byte) in dst[..count].iter_mut().enumerate() {
            *byte = self.buf[(self.head + offset + i) % self.buf.len()];
        }
        count
    }

    pub(crate) fn consume(&mut self, count: usize) {
        let count = count.min(self.len);
        if !self.buf.is_empty() {
            self.head = (self.head + count) % self.buf.len();
        }
        self.len -= count;
    }

    pub(crate) fn pop(&mut self, dst: &mut [u8]) -> usize {
        let count = self.peek(0, dst);
        self.consume(count);
        count
    }
}
