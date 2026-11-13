//! Ascending iteration over the values of a `BitSet`.

pub struct Iter<'a> {
    words: &'a [u64],
    word: usize,
    rest: u64,
}

impl<'a> Iter<'a> {
    pub(crate) fn new(words: &'a [u64]) -> Self {
        Iter { words, word: 0, rest: words.first().copied().unwrap_or(0) }
    }
}

impl Iterator for Iter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        loop {
            if self.rest != 0 {
                let bit = self.rest.trailing_zeros() as usize;
                self.rest &= self.rest - 1;
                return Some(self.word * 64 + bit);
            }
            self.word += 1;
            if self.word >= self.words.len() {
                return None;
            }
            self.rest = self.words[self.word];
        }
    }
}
