//! A fixed-capacity set of small integers stored one bit each.

pub mod iter;

pub use iter::Iter;

const WORD: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitSet {
    size: usize,
    words: Vec<u64>,
}

impl BitSet {
    /// An empty set that can hold the values `0..size`.
    pub fn new(size: usize) -> Self {
        BitSet { size, words: vec![0; size.div_ceil(WORD)] }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Adds `value`. Panics when `value >= size`.
    pub fn insert(&mut self, value: usize) {
        assert!(value < self.size, "value {value} out of range for size {}", self.size);
        self.words[value / WORD] |= 1 << (value % WORD);
    }

    pub fn contains(&self, value: usize) -> bool {
        value < self.size && self.words[value / WORD] & (1 << (value % WORD)) != 0
    }

    pub fn len(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every value in either set. The result can hold the larger of the two sizes.
    pub fn union(&self, other: &BitSet) -> BitSet {
        let mut out = BitSet::new(self.size.max(other.size));
        for i in 0..self.words.len().min(other.words.len()) {
            out.words[i] = self.words[i] | other.words[i];
        }
        out
    }

    /// Every value in both sets. The result can hold the smaller of the two sizes.
    pub fn intersection(&self, other: &BitSet) -> BitSet {
        let mut out = BitSet::new(self.size.min(other.size));
        for i in 0..out.words.len() {
            out.words[i] = self.words[i] & other.words[i];
        }
        out
    }

    /// Every value in `self` that is not in `other`. The result keeps the size of `self`.
    pub fn difference(&self, other: &BitSet) -> BitSet {
        let mut out = BitSet::new(self.size);
        for (i, (a, b)) in self.words.iter().zip(other.words.iter()).enumerate() {
            out.words[i] = a & !b;
        }
        out
    }

    /// The values in ascending order.
    pub fn iter(&self) -> Iter<'_> {
        Iter::new(&self.words)
    }
}
