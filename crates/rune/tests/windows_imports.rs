//! The Windows binary loads the keyboard and socket libraries on first use.

#![cfg(windows)]
// Header offsets of the binary under test are small and fixed by the format;
// a debug build would panic on an overflow rather than misread.
#![allow(clippy::expect_used, clippy::panic, clippy::arithmetic_side_effects)]

/// Reads a little-endian `u16` at `at`.
fn u16_at(bytes: &[u8], at: usize) -> usize {
    let pair = bytes.get(at..at + 2).expect("u16 in range");
    usize::from(u16::from_le_bytes([pair[0], pair[1]]))
}

/// Reads a little-endian `u32` at `at`.
fn u32_at(bytes: &[u8], at: usize) -> usize {
    let quad = bytes.get(at..at + 4).expect("u32 in range");
    u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]) as usize
}

/// The library names one import table of a PE file lists.
///
/// `directory` is the data directory index: 1 for the imports loaded at start,
/// 13 for the delay-loaded ones. Each descriptor names its library at
/// `name_offset`; the table ends with an all-zero descriptor.
fn libraries(image: &[u8], directory: usize, size: usize, name_offset: usize) -> Vec<String> {
    let pe = u32_at(image, 0x3c);
    assert_eq!(image.get(pe..pe + 4), Some(&b"PE\0\0"[..]), "a PE image");
    let coff = pe + 4;
    let sections = u16_at(image, coff + 2);
    let optional = coff + 20;
    let optional_size = u16_at(image, coff + 16);
    assert_eq!(u16_at(image, optional), 0x20b, "a 64-bit image");
    let entry = optional + 112 + directory * 8;
    let table = u32_at(image, entry);
    if table == 0 {
        return Vec::new();
    }
    let headers = optional + optional_size;
    let offset = |rva: usize| -> usize {
        (0..sections)
            .map(|index| headers + index * 40)
            .find_map(|section| {
                let start = u32_at(image, section + 12);
                let length = u32_at(image, section + 8).max(u32_at(image, section + 16));
                (start..start + length)
                    .contains(&rva)
                    .then(|| rva - start + u32_at(image, section + 20))
            })
            .expect("an address inside a section")
    };
    let mut names = Vec::new();
    let mut descriptor = offset(table);
    while image
        .get(descriptor..descriptor + size)
        .is_some_and(|bytes| bytes.iter().any(|byte| *byte != 0))
    {
        let name = offset(u32_at(image, descriptor + name_offset));
        let end = image[name..]
            .iter()
            .position(|byte| *byte == 0)
            .expect("a terminated name");
        names.push(String::from_utf8_lossy(&image[name..name + end]).to_ascii_lowercase());
        descriptor += size;
    }
    names
}

#[test]
fn keyboard_and_socket_libraries_load_on_first_use() {
    let image = std::fs::read(env!("CARGO_BIN_EXE_rune")).expect("the built binary");
    let at_start = libraries(&image, 1, 20, 12);
    let delayed = libraries(&image, 13, 32, 4);
    assert!(
        at_start.contains(&"kernel32.dll".to_owned()),
        "{at_start:?}"
    );
    for library in ["user32.dll", "ws2_32.dll"] {
        assert!(
            !at_start.contains(&library.to_owned()),
            "{library} loads at start: {at_start:?}"
        );
        assert!(
            delayed.contains(&library.to_owned()),
            "{library} is not delayed: {delayed:?}"
        );
    }
}
