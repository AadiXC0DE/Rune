//! Deterministic malformed variants of a valid input, for tests that check a
//! parser of model or hook output never panics.
//!
//! The generator is seeded from the test's name, so a failing variant is the
//! same on every run and on every machine.

/// Fragments a variant may have inserted: the markers the parsers look for,
/// quoting, a NUL, and the start of multi-byte characters left unfinished.
const INSERTS: &[&[u8]] = &[
    b"*** ",
    b"@@",
    b"{",
    b"}",
    b"\"",
    b"\\",
    b"\0",
    b"\r\n",
    b"\n",
    &[0xc3],
    &[0xe2, 0x82],
    &[0xf0, 0x9f, 0x98],
    "é".as_bytes(),
    "😀".as_bytes(),
];

/// A xorshift64 generator.
#[derive(Clone, Copy, Debug)]
pub struct Garble(u64);

impl Garble {
    /// Seeds the generator from `name`, usually the test's name.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self(fnv1a(name.as_bytes()).max(1))
    }

    /// The next number in the sequence.
    pub const fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A number below `bound`, or zero when `bound` is zero.
    pub fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        let bound = u64::try_from(bound).unwrap_or(u64::MAX);
        usize::try_from(self.next_u64().checked_rem(bound).unwrap_or(0)).unwrap_or(0)
    }

    /// `count` variants of `seed`, each changed by one to three edits:
    /// truncations, single-byte flips, duplicated and deleted lines, inserted
    /// markers and fragments, and swapped line endings. Bytes that are not
    /// UTF-8 become replacement characters.
    pub fn variants(&mut self, seed: &str, count: usize) -> Vec<String> {
        (0..count)
            .map(|_| {
                let mut bytes = seed.as_bytes().to_vec();
                for _ in 0..=self.below(3) {
                    self.edit(&mut bytes);
                }
                String::from_utf8_lossy(&bytes).into_owned()
            })
            .collect()
    }

    fn edit(&mut self, bytes: &mut Vec<u8>) {
        match self.below(6) {
            0 => {
                let at = self.below(bytes.len().saturating_add(1));
                bytes.truncate(at);
            }
            1 => {
                if !bytes.is_empty() {
                    let at = self.below(bytes.len());
                    let bit = self.below(8);
                    if let Some(byte) = bytes.get_mut(at) {
                        *byte ^= 1 << bit;
                    }
                }
            }
            2 => self.edit_lines(bytes, true),
            3 => self.edit_lines(bytes, false),
            4 => {
                let insert = INSERTS[self.below(INSERTS.len())];
                let at = self.below(bytes.len().saturating_add(1));
                bytes.splice(at..at, insert.iter().copied());
            }
            _ => {
                let text = String::from_utf8_lossy(bytes).into_owned();
                let swapped = if text.contains("\r\n") {
                    text.replace("\r\n", "\n")
                } else {
                    text.replace('\n', "\r\n")
                };
                *bytes = swapped.into_bytes();
            }
        }
    }

    /// Duplicates or deletes one line.
    fn edit_lines(&mut self, bytes: &mut Vec<u8>, duplicate: bool) {
        let mut lines: Vec<Vec<u8>> = bytes
            .split_inclusive(|byte| *byte == b'\n')
            .map(<[u8]>::to_vec)
            .collect();
        if lines.is_empty() {
            return;
        }
        let at = self.below(lines.len());
        if duplicate {
            let line = lines[at].clone();
            lines.insert(at, line);
        } else {
            lines.remove(at);
        }
        *bytes = lines.concat();
    }
}

/// The 64-bit FNV-1a hash.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_name_gives_the_same_variants() {
        let one = Garble::new("a test").variants("line one\nline two\n", 50);
        let two = Garble::new("a test").variants("line one\nline two\n", 50);
        assert_eq!(one, two);
        let other = Garble::new("another test").variants("line one\nline two\n", 50);
        assert_ne!(one, other);
        assert!(one.iter().any(|variant| variant != "line one\nline two\n"));
    }

    #[test]
    fn variants_cover_every_kind_of_edit() {
        let variants = Garble::new("coverage").variants("{\"a\": 1}\nsecond\n", 2_000);
        assert!(variants.iter().any(|variant| variant.contains('\u{fffd}')));
        assert!(variants.iter().any(|variant| variant.contains("\r\n")));
        assert!(variants.iter().any(|variant| variant.contains("*** ")));
        assert!(
            variants
                .iter()
                .any(|variant| variant.matches("second").count() > 1)
        );
        assert!(variants.iter().any(|variant| !variant.contains("second")));
        assert!(variants.iter().any(String::is_empty));
    }
}
