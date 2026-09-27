//! Fixed-width set of logical CPU indices.

#![forbid(unsafe_code)]

use crate::MAX_CPUS;

const WORDS: usize = MAX_CPUS / 64;

/// A set of logical CPU indices `0..MAX_CPUS`, stored inline (no allocation).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct CpuMask {
    words: [u64; WORDS],
}

/// A CPU index outside the range accepted by the receiver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InvalidCpu(pub usize);

impl CpuMask {
    /// The empty set.
    pub const fn empty() -> Self {
        Self { words: [0; WORDS] }
    }

    /// The set `{0, 1, .., n-1}`.
    pub fn first_n(n: usize) -> Result<Self, InvalidCpu> {
        if n > MAX_CPUS {
            return Err(InvalidCpu(n));
        }
        let mut mask = Self::empty();
        for cpu in 0..n {
            mask.words[cpu / 64] |= 1 << (cpu % 64);
        }
        Ok(mask)
    }

    /// The set containing only `cpu`.
    pub fn single(cpu: usize) -> Result<Self, InvalidCpu> {
        let mut mask = Self::empty();
        mask.insert(cpu)?;
        Ok(mask)
    }

    /// Adds `cpu`; indices `>= MAX_CPUS` are rejected rather than dropped.
    pub fn insert(&mut self, cpu: usize) -> Result<(), InvalidCpu> {
        if cpu >= MAX_CPUS {
            return Err(InvalidCpu(cpu));
        }
        self.words[cpu / 64] |= 1 << (cpu % 64);
        Ok(())
    }

    /// Removes `cpu` (no-op if absent or out of range).
    pub fn remove(&mut self, cpu: usize) {
        if cpu < MAX_CPUS {
            self.words[cpu / 64] &= !(1 << (cpu % 64));
        }
    }

    /// Whether `cpu` is in the set.
    pub fn contains(&self, cpu: usize) -> bool {
        cpu < MAX_CPUS && self.words[cpu / 64] & (1 << (cpu % 64)) != 0
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    /// Number of CPUs in the set.
    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Highest CPU index in the set.
    pub fn highest(&self) -> Option<usize> {
        (0..WORDS)
            .rev()
            .find(|&i| self.words[i] != 0)
            .map(|i| i * 64 + 63 - self.words[i].leading_zeros() as usize)
    }

    /// Set intersection.
    pub fn and(self, other: Self) -> Self {
        let mut out = self;
        for (o, w) in out.words.iter_mut().zip(other.words) {
            *o &= w;
        }
        out
    }

    /// Set union.
    pub fn or(self, other: Self) -> Self {
        let mut out = self;
        for (o, w) in out.words.iter_mut().zip(other.words) {
            *o |= w;
        }
        out
    }

    /// Set difference `self \ other`.
    pub fn and_not(self, other: Self) -> Self {
        let mut out = self;
        for (o, w) in out.words.iter_mut().zip(other.words) {
            *o &= !w;
        }
        out
    }

    /// Iterates the CPU indices in ascending order.
    pub fn iter(&self) -> CpuMaskIter {
        CpuMaskIter {
            words: self.words,
            word: 0,
        }
    }
}

impl IntoIterator for CpuMask {
    type Item = usize;
    type IntoIter = CpuMaskIter;
    fn into_iter(self) -> CpuMaskIter {
        self.iter()
    }
}

/// Ascending iterator over a [`CpuMask`] snapshot.
#[derive(Clone, Debug)]
pub struct CpuMaskIter {
    words: [u64; WORDS],
    word: usize,
}

impl Iterator for CpuMaskIter {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        while self.word < WORDS {
            let w = self.words[self.word];
            if w != 0 {
                self.words[self.word] = w & (w - 1);
                return Some(self.word * 64 + w.trailing_zeros() as usize);
            }
            self.word += 1;
        }
        None
    }
}
