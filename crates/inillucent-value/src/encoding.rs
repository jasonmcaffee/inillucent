//! Database text encodings and the conversions between them.
//!
//! Invariant: text bytes always travel with the encoding they are in. A record
//! field decoded from a UTF-16 database is UTF-16 until something converts it,
//! and the conversion is a named function rather than a cast, because the two
//! encodings do not sort the same way and a silent reinterpretation would be a
//! wrong answer rather than a crash.
//!
//! SQLite stores the encoding in the database header (1 = UTF-8, 2 = UTF-16
//! little-endian, 3 = UTF-16 big-endian) and converts text to the collation's
//! preferred encoding before comparing it. Every built-in collation prefers
//! UTF-8, so `to_utf8` is on the comparison path for a UTF-16 database and is
//! a no-op for a UTF-8 one.

use std::borrow::Cow;

/// The three text encodings a SQLite database may use.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum TextEncoding {
    /// UTF-8. Header code 1, and the default for a database SQLite creates.
    Utf8,
    /// UTF-16, little-endian. Header code 2.
    Utf16Le,
    /// UTF-16, big-endian. Header code 3.
    Utf16Be,
}

impl TextEncoding {
    /// Returns the code this encoding has in the database header.
    pub fn header_code(self) -> u32 {
        match self {
            TextEncoding::Utf8 => 1,
            TextEncoding::Utf16Le => 2,
            TextEncoding::Utf16Be => 3,
        }
    }

    /// Returns the encoding a header code names.
    ///
    /// This is total, and deliberately so. SQLite does not validate the
    /// encoding field: `sqlite3InitOne` reads it as `meta & 3` and treats zero
    /// as UTF-8, so a header holding 4 is read as UTF-8 and a header holding 7
    /// is read as UTF-16be. Refusing those files - which is what a stricter
    /// reader would do, and what this function did until the differential run
    /// against 3.53.4 caught it - means refusing a database SQLite opens and
    /// reads without complaint, which is a parity difference rather than a
    /// safety win. The masking cannot make a value dangerous: every one of the
    /// four results is a real encoding.
    pub fn from_header_code(code: u32) -> TextEncoding {
        match code & 3 {
            2 => TextEncoding::Utf16Le,
            3 => TextEncoding::Utf16Be,
            // 0 means "no text has been stored yet", which is UTF-8.
            _ => TextEncoding::Utf8,
        }
    }

    /// Returns every encoding, for exhaustive matrices.
    pub fn all() -> [TextEncoding; 3] {
        [
            TextEncoding::Utf8,
            TextEncoding::Utf16Le,
            TextEncoding::Utf16Be,
        ]
    }
}

/// Converts text in `encoding` to UTF-8, returning the input untouched when it
/// already is UTF-8.
///
/// The UTF-16 path mirrors SQLite's own reader: a well-formed surrogate pair
/// becomes one supplementary code point, and an unpaired surrogate is written
/// out as its own code point rather than being rejected, because SQLite's
/// conversion does not validate pairing and a database may legitimately hold
/// text that a stricter reader would refuse. A trailing odd byte is dropped,
/// which is what SQLite's loop does when it runs out of input.
pub fn to_utf8(bytes: &[u8], encoding: TextEncoding) -> Cow<'_, [u8]> {
    match encoding {
        TextEncoding::Utf8 => Cow::Borrowed(bytes),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            Cow::Owned(utf16_to_utf8(bytes, encoding == TextEncoding::Utf16Be))
        }
    }
}

/// Converts UTF-8 bytes into `encoding`.
pub fn from_utf8(bytes: &[u8], encoding: TextEncoding) -> Cow<'_, [u8]> {
    match encoding {
        TextEncoding::Utf8 => Cow::Borrowed(bytes),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            Cow::Owned(utf8_to_utf16(bytes, encoding == TextEncoding::Utf16Be))
        }
    }
}

/// Converts text between any two encodings.
pub fn convert(bytes: &[u8], from: TextEncoding, to: TextEncoding) -> Cow<'_, [u8]> {
    if from == to {
        return Cow::Borrowed(bytes);
    }
    match to_utf8(bytes, from) {
        Cow::Borrowed(utf8) => from_utf8(utf8, to),
        Cow::Owned(utf8) => Cow::Owned(from_utf8(&utf8, to).into_owned()),
    }
}

/// Reads one code point out of UTF-8 bytes at `offset`, SQLite-style.
///
/// Returns the code point and the number of bytes it consumed. A byte that
/// cannot begin a sequence, or a truncated sequence, consumes one byte and
/// yields U+FFFD, which is what SQLite's `READ_UTF8` does; refusing the string
/// instead would make a legal-but-odd blob-as-text value unreadable.
pub fn next_utf8(bytes: &[u8], offset: usize) -> Option<(u32, usize)> {
    let first = u32::from(*bytes.get(offset)?);
    if first < 0x80 {
        return Some((first, 1));
    }
    let extra: usize = match first {
        0xc0..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf7 => 3,
        _ => return Some((0xfffd, 1)),
    };
    let mut value = first & (0x7f >> extra);
    for index in 1..=extra {
        let Some(byte) = bytes.get(offset.saturating_add(index)) else {
            return Some((0xfffd, 1));
        };
        if byte & 0xc0 != 0x80 {
            return Some((0xfffd, 1));
        }
        value = (value << 6) | u32::from(byte & 0x3f);
    }
    // Overlong encodings and code points past the Unicode range decode to the
    // replacement character, matching SQLite's own guard.
    let overlong = match extra {
        1 => value < 0x80,
        2 => value < 0x800,
        _ => value < 0x1_0000,
    };
    if overlong || value > 0x10_ffff {
        return Some((0xfffd, extra.saturating_add(1)));
    }
    Some((value, extra.saturating_add(1)))
}

/// Counts the code points in UTF-8 bytes, the way `length()` does.
pub fn utf8_character_count(bytes: &[u8]) -> usize {
    let mut offset = 0usize;
    let mut count = 0usize;
    while let Some((_, width)) = next_utf8(bytes, offset) {
        offset = offset.saturating_add(width);
        count = count.saturating_add(1);
    }
    count
}

/// Appends one code point to `output` as UTF-8.
pub fn push_utf8(output: &mut Vec<u8>, code_point: u32) {
    if code_point < 0x80 {
        output.push(code_point as u8);
    } else if code_point < 0x800 {
        output.push(0xc0 | (code_point >> 6) as u8);
        output.push(0x80 | (code_point & 0x3f) as u8);
    } else if code_point < 0x1_0000 {
        output.push(0xe0 | (code_point >> 12) as u8);
        output.push(0x80 | ((code_point >> 6) & 0x3f) as u8);
        output.push(0x80 | (code_point & 0x3f) as u8);
    } else {
        output.push(0xf0 | (code_point >> 18) as u8);
        output.push(0x80 | ((code_point >> 12) & 0x3f) as u8);
        output.push(0x80 | ((code_point >> 6) & 0x3f) as u8);
        output.push(0x80 | (code_point & 0x3f) as u8);
    }
}

/// Converts UTF-16 bytes to UTF-8.
fn utf16_to_utf8(bytes: &[u8], big_endian: bool) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index.saturating_add(1) < bytes.len() {
        let unit = read_unit(bytes, index, big_endian);
        index = index.saturating_add(2);
        let code_point =
            if (0xd800..0xdc00).contains(&unit) && index.saturating_add(1) < bytes.len() {
                let low = read_unit(bytes, index, big_endian);
                if (0xdc00..0xe000).contains(&low) {
                    index = index.saturating_add(2);
                    0x1_0000u32
                        .wrapping_add((u32::from(unit).wrapping_sub(0xd800)) << 10)
                        .wrapping_add(u32::from(low).wrapping_sub(0xdc00))
                } else {
                    u32::from(unit)
                }
            } else {
                u32::from(unit)
            };
        push_utf8(&mut output, code_point);
    }
    output
}

/// Converts UTF-8 bytes to UTF-16.
fn utf8_to_utf16(bytes: &[u8], big_endian: bool) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len().saturating_mul(2));
    let mut offset = 0usize;
    while let Some((code_point, width)) = next_utf8(bytes, offset) {
        offset = offset.saturating_add(width);
        if code_point < 0x1_0000 {
            push_unit(&mut output, code_point as u16, big_endian);
        } else {
            let adjusted = code_point.saturating_sub(0x1_0000);
            push_unit(
                &mut output,
                (0xd800u32.saturating_add(adjusted >> 10)) as u16,
                big_endian,
            );
            push_unit(
                &mut output,
                (0xdc00u32.saturating_add(adjusted & 0x3ff)) as u16,
                big_endian,
            );
        }
    }
    output
}

/// Reads one UTF-16 code unit.
fn read_unit(bytes: &[u8], index: usize, big_endian: bool) -> u16 {
    let first = bytes.get(index).copied().unwrap_or(0);
    let second = bytes.get(index.saturating_add(1)).copied().unwrap_or(0);
    if big_endian {
        (u16::from(first) << 8) | u16::from(second)
    } else {
        (u16::from(second) << 8) | u16::from(first)
    }
}

/// Appends one UTF-16 code unit.
fn push_unit(output: &mut Vec<u8>, unit: u16, big_endian: bool) {
    if big_endian {
        output.push((unit >> 8) as u8);
        output.push((unit & 0xff) as u8);
    } else {
        output.push((unit & 0xff) as u8);
        output.push((unit >> 8) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header codes are part of the file format and cannot drift.
    #[test]
    fn header_codes_match_the_file_format() {
        assert_eq!(TextEncoding::Utf8.header_code(), 1);
        assert_eq!(TextEncoding::Utf16Le.header_code(), 2);
        assert_eq!(TextEncoding::Utf16Be.header_code(), 3);
        for encoding in TextEncoding::all() {
            assert_eq!(
                TextEncoding::from_header_code(encoding.header_code()),
                encoding
            );
        }
    }

    /// A zero encoding means "no text stored yet" and is UTF-8, and a code
    /// past three is masked rather than refused - which is what SQLite does,
    /// verified against 3.53.4 on a file whose encoding field holds 4.
    #[test]
    fn an_unset_encoding_is_utf8_and_an_unknown_one_is_masked() {
        assert_eq!(TextEncoding::from_header_code(0), TextEncoding::Utf8);
        assert_eq!(TextEncoding::from_header_code(4), TextEncoding::Utf8);
        assert_eq!(TextEncoding::from_header_code(5), TextEncoding::Utf8);
        assert_eq!(TextEncoding::from_header_code(6), TextEncoding::Utf16Le);
        assert_eq!(TextEncoding::from_header_code(7), TextEncoding::Utf16Be);
        assert_eq!(
            TextEncoding::from_header_code(u32::MAX),
            TextEncoding::Utf16Be
        );
    }

    /// Every code point must survive a round trip through both UTF-16 forms.
    #[test]
    fn text_round_trips_through_both_utf16_forms() {
        let samples: [&str; 6] = [
            "",
            "hello",
            "h\u{e9}llo",
            "\u{2603} snowman",
            "\u{1F600}\u{1F601}",
            "mixed \u{10FFFF} end",
        ];
        for sample in samples {
            for encoding in [TextEncoding::Utf16Le, TextEncoding::Utf16Be] {
                let wide = from_utf8(sample.as_bytes(), encoding);
                let back = to_utf8(&wide, encoding);
                assert_eq!(
                    back.as_ref(),
                    sample.as_bytes(),
                    "{sample:?} through {encoding:?}"
                );
            }
        }
    }

    /// A conversion between the two UTF-16 forms must go through the code
    /// points rather than swapping bytes blindly.
    #[test]
    fn the_two_utf16_forms_convert_into_each_other() {
        let source = "\u{1F600}a\u{2603}";
        let little = from_utf8(source.as_bytes(), TextEncoding::Utf16Le).into_owned();
        let big = convert(&little, TextEncoding::Utf16Le, TextEncoding::Utf16Be).into_owned();
        assert_eq!(
            to_utf8(&big, TextEncoding::Utf16Be).as_ref(),
            source.as_bytes()
        );
        assert_ne!(little, big);
    }

    /// UTF-8 is never copied on the way in or out; the comparison path runs on
    /// every row and a copy per operand would be a real cost.
    #[test]
    fn utf8_conversion_borrows_rather_than_copying() {
        let bytes = b"no copy please";
        assert!(matches!(
            to_utf8(bytes, TextEncoding::Utf8),
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            from_utf8(bytes, TextEncoding::Utf8),
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            convert(bytes, TextEncoding::Utf16Le, TextEncoding::Utf16Le),
            Cow::Borrowed(_)
        ));
    }

    /// A lone surrogate is text SQLite will store and read back, so the
    /// conversion must carry it rather than refusing the string.
    #[test]
    fn an_unpaired_surrogate_survives_conversion() {
        let unpaired = [0x00u8, 0xd8, 0x41, 0x00];
        let utf8 = to_utf8(&unpaired, TextEncoding::Utf16Le);
        assert_eq!(utf8.as_ref(), &[0xed, 0xa0, 0x80, 0x41]);
    }

    /// An odd trailing byte cannot form a code unit and is dropped, which is
    /// what SQLite's conversion loop does when the input runs out.
    #[test]
    fn an_odd_trailing_byte_is_dropped() {
        let odd = [0x41u8, 0x00, 0x42];
        assert_eq!(to_utf8(&odd, TextEncoding::Utf16Le).as_ref(), b"A");
    }

    /// Malformed UTF-8 must decode to the replacement character instead of
    /// panicking or being read past its end.
    #[test]
    fn malformed_utf8_decodes_to_the_replacement_character() {
        assert_eq!(next_utf8(&[0x80], 0), Some((0xfffd, 1)));
        assert_eq!(next_utf8(&[0xc2], 0), Some((0xfffd, 1)));
        assert_eq!(next_utf8(&[0xe0, 0x80], 0), Some((0xfffd, 1)));
        // An overlong two-byte encoding of U+0041.
        assert_eq!(next_utf8(&[0xc1, 0x81], 0), Some((0xfffd, 2)));
        assert_eq!(next_utf8(&[], 0), None);
    }

    /// Character counting is by code point, not by byte or by code unit.
    #[test]
    fn characters_are_counted_by_code_point() {
        assert_eq!(utf8_character_count(b""), 0);
        assert_eq!(utf8_character_count(b"abc"), 3);
        assert_eq!(utf8_character_count("\u{1F600}".as_bytes()), 1);
        assert_eq!(utf8_character_count("a\u{2603}b".as_bytes()), 3);
    }

    /// Reading never indexes past the slice, whatever the bytes say.
    #[test]
    fn decoding_arbitrary_bytes_never_reads_past_the_end() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..inillucent_base::probe::sample_rounds(20_000) {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let len = (state % 9) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|index| (state >> (index % 8 * 8)) as u8)
                .collect();
            let mut offset = 0usize;
            while let Some((_, width)) = next_utf8(&bytes, offset) {
                assert!(width > 0);
                offset = offset.saturating_add(width);
            }
            let _ = to_utf8(&bytes, TextEncoding::Utf16Le);
            let _ = to_utf8(&bytes, TextEncoding::Utf16Be);
        }
    }
}
