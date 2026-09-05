//! The value model: five storage classes, borrowed or owned payloads, and the
//! flags and subtype that travel with a value through the machine.
//!
//! Invariant: a value never carries a storage class its bytes do not justify.
//! There is no constructor that turns bytes into an integer, no `as` cast
//! between classes, and no way to read a text payload as a number without
//! going through `numeric` or `cast`. Every conversion is a named, fallible
//! function, because SQLite's conversions are lossy in specific documented ways
//! and a Rust default would be lossy in different ones.
//!
//! Payloads borrow where they can. A value decoded out of a page borrows the
//! page's bytes for as long as the cursor holds the pin; `into_owned` copies
//! it when it has to outlive that. The owned form is reference counted, so
//! cloning a materialised row is a refcount bump rather than a copy, and the
//! only allocation is the one `Bytes::owned` performs and checks.

use std::sync::Arc;

use inillucent_base::buffer;
use inillucent_base::DbResult;

use crate::encoding::{self, TextEncoding};

/// The five storage classes SQLite gives a value.
///
/// The order is the one comparisons use: NULL sorts before every number, both
/// numeric classes sort together and before text, and text sorts before blob.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum StorageClass {
    /// SQL NULL, which is not a value so much as the absence of one.
    Null,
    /// A signed 64-bit integer.
    Integer,
    /// An IEEE-754 binary64.
    Real,
    /// Text in the database encoding.
    Text,
    /// An uninterpreted byte string.
    Blob,
}

impl StorageClass {
    /// Returns the rank comparisons use to order values of different classes.
    ///
    /// Integer and real share a rank because SQLite compares them numerically
    /// rather than by class.
    pub fn sort_rank(self) -> u8 {
        match self {
            StorageClass::Null => 0,
            StorageClass::Integer | StorageClass::Real => 1,
            StorageClass::Text => 2,
            StorageClass::Blob => 3,
        }
    }

    /// Returns the name `typeof()` reports.
    pub fn typeof_name(self) -> &'static str {
        match self {
            StorageClass::Null => "null",
            StorageClass::Integer => "integer",
            StorageClass::Real => "real",
            StorageClass::Text => "text",
            StorageClass::Blob => "blob",
        }
    }

    /// Reports whether this class holds a number.
    pub fn is_numeric(self) -> bool {
        matches!(self, StorageClass::Integer | StorageClass::Real)
    }

    /// Returns every class, for exhaustive matrices.
    pub fn all() -> [StorageClass; 5] {
        [
            StorageClass::Null,
            StorageClass::Integer,
            StorageClass::Real,
            StorageClass::Text,
            StorageClass::Blob,
        ]
    }
}

/// A payload that either borrows its bytes or owns a shared immutable buffer.
///
/// Cloning the owned form is a refcount bump, so a row that has been
/// materialised can be handed around without copying it again. Constructing
/// the owned form is the only allocation, and it goes through the fallible
/// allocator in `inillucent-base` so a hostile length cannot abort the process.
#[derive(Clone, Debug)]
pub enum Bytes<'a> {
    /// Bytes owned by something else that outlives this value.
    Borrowed(&'a [u8]),
    /// Bytes this value shares ownership of.
    Owned(Arc<[u8]>),
}

impl<'a> Bytes<'a> {
    /// Returns the bytes.
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Bytes::Borrowed(bytes) => bytes,
            Bytes::Owned(bytes) => bytes,
        }
    }

    /// Returns the payload length in bytes.
    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    /// Reports whether the payload is empty.
    ///
    /// An empty payload is a real value - `''` and `x''` are not NULL - so this
    /// is a question about length and never about presence.
    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    /// Copies `source` into a new shared buffer.
    pub fn owned(source: &[u8]) -> DbResult<Bytes<'static>> {
        let copy = buffer::try_copy_of(source)?;
        Ok(Bytes::Owned(Arc::from(copy)))
    }

    /// Returns a form that borrows nothing, copying when it has to.
    pub fn into_owned(self) -> DbResult<Bytes<'static>> {
        match self {
            Bytes::Borrowed(bytes) => Bytes::owned(bytes),
            Bytes::Owned(bytes) => Ok(Bytes::Owned(bytes)),
        }
    }

    /// Reports whether the payload borrows rather than owning.
    pub fn is_borrowed(&self) -> bool {
        matches!(self, Bytes::Borrowed(_))
    }
}

/// Text, with the encoding its bytes are in.
#[derive(Clone, Debug)]
pub struct TextValue<'a> {
    bytes: Bytes<'a>,
    encoding: TextEncoding,
}

impl<'a> TextValue<'a> {
    /// Builds a text value from bytes already in `encoding`.
    pub fn new(bytes: Bytes<'a>, encoding: TextEncoding) -> TextValue<'a> {
        TextValue { bytes, encoding }
    }

    /// Builds a borrowed UTF-8 text value.
    pub fn utf8(bytes: &'a [u8]) -> TextValue<'a> {
        TextValue {
            bytes: Bytes::Borrowed(bytes),
            encoding: TextEncoding::Utf8,
        }
    }

    /// Returns the raw bytes, in this value's own encoding.
    pub fn raw(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Returns the encoding the raw bytes are in.
    pub fn encoding(&self) -> TextEncoding {
        self.encoding
    }

    /// Returns the payload length in bytes of its own encoding.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Reports whether the text is the empty string.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns the text as UTF-8, converting only when it is not already.
    pub fn utf8_bytes(&self) -> std::borrow::Cow<'_, [u8]> {
        encoding::to_utf8(self.bytes.as_slice(), self.encoding)
    }

    /// Returns the same text in `target`, converting when it has to.
    pub fn to_encoding(&self, target: TextEncoding) -> DbResult<TextValue<'static>> {
        let converted = encoding::convert(self.bytes.as_slice(), self.encoding, target);
        Ok(TextValue {
            bytes: Bytes::owned(converted.as_ref())?,
            encoding: target,
        })
    }

    /// Returns a form that borrows nothing.
    pub fn into_owned(self) -> DbResult<TextValue<'static>> {
        Ok(TextValue {
            bytes: self.bytes.into_owned()?,
            encoding: self.encoding,
        })
    }
}

/// An uninterpreted byte string.
#[derive(Clone, Debug)]
pub struct BlobValue<'a> {
    bytes: Bytes<'a>,
}

impl<'a> BlobValue<'a> {
    /// Builds a blob from a payload.
    pub fn new(bytes: Bytes<'a>) -> BlobValue<'a> {
        BlobValue { bytes }
    }

    /// Builds a borrowed blob.
    pub fn borrowed(bytes: &'a [u8]) -> BlobValue<'a> {
        BlobValue {
            bytes: Bytes::Borrowed(bytes),
        }
    }

    /// Returns the bytes.
    pub fn raw(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Returns the payload length.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Reports whether the blob has no bytes.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns a form that borrows nothing.
    pub fn into_owned(self) -> DbResult<BlobValue<'static>> {
        Ok(BlobValue {
            bytes: self.bytes.into_owned()?,
        })
    }
}

/// A SQL value.
#[derive(Clone, Debug)]
pub enum Value<'a> {
    /// SQL NULL.
    Null,
    /// A signed 64-bit integer.
    Integer(i64),
    /// An IEEE-754 binary64, kept as its bits including a signed zero.
    Real(f64),
    /// Text in some database encoding.
    Text(TextValue<'a>),
    /// An uninterpreted byte string.
    Blob(BlobValue<'a>),
}

impl<'a> Value<'a> {
    /// Returns the value's storage class.
    pub fn storage_class(&self) -> StorageClass {
        match self {
            Value::Null => StorageClass::Null,
            Value::Integer(_) => StorageClass::Integer,
            Value::Real(_) => StorageClass::Real,
            Value::Text(_) => StorageClass::Text,
            Value::Blob(_) => StorageClass::Blob,
        }
    }

    /// Reports whether the value is NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Builds a borrowed UTF-8 text value.
    pub fn text_utf8(bytes: &'a [u8]) -> Value<'a> {
        Value::Text(TextValue::utf8(bytes))
    }

    /// Builds a borrowed blob value.
    pub fn blob(bytes: &'a [u8]) -> Value<'a> {
        Value::Blob(BlobValue::borrowed(bytes))
    }

    /// Builds an owned UTF-8 text value.
    pub fn owned_text(source: &[u8]) -> DbResult<Value<'static>> {
        Ok(Value::Text(TextValue::new(
            Bytes::owned(source)?,
            TextEncoding::Utf8,
        )))
    }

    /// Builds an owned blob value.
    pub fn owned_blob(source: &[u8]) -> DbResult<Value<'static>> {
        Ok(Value::Blob(BlobValue::new(Bytes::owned(source)?)))
    }

    /// Returns the integer payload, or `None` for every other class.
    ///
    /// This is an accessor, not a conversion: text that looks like a number
    /// answers `None`, because turning it into one is `cast` or `affinity`.
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(value) => Some(*value),
            _ => None,
        }
    }

    /// Returns the real payload, or `None` for every other class.
    pub fn as_real(&self) -> Option<f64> {
        match self {
            Value::Real(value) => Some(*value),
            _ => None,
        }
    }

    /// Returns the text payload, or `None` for every other class.
    pub fn as_text(&self) -> Option<&TextValue<'a>> {
        match self {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }

    /// Returns the blob payload, or `None` for every other class.
    pub fn as_blob(&self) -> Option<&BlobValue<'a>> {
        match self {
            Value::Blob(blob) => Some(blob),
            _ => None,
        }
    }

    /// Returns the payload length in bytes for text and blobs, and zero for
    /// the classes that do not have one.
    pub fn byte_len(&self) -> usize {
        match self {
            Value::Text(text) => text.len(),
            Value::Blob(blob) => blob.len(),
            _ => 0,
        }
    }

    /// Returns a form that borrows nothing, copying payloads when it has to.
    pub fn into_owned(self) -> DbResult<Value<'static>> {
        Ok(match self {
            Value::Null => Value::Null,
            Value::Integer(value) => Value::Integer(value),
            Value::Real(value) => Value::Real(value),
            Value::Text(text) => Value::Text(text.into_owned()?),
            Value::Blob(blob) => Value::Blob(blob.into_owned()?),
        })
    }

    /// Compares two values for exact identity, including the bit pattern of a
    /// real and the encoding of text.
    ///
    /// This is not SQL comparison - `1` and `1.0` are not identical here even
    /// though SQL says they are equal - and exists so tests can assert that a
    /// value survived a round trip unchanged.
    pub fn identical(&self, other: &Value<'_>) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Integer(left), Value::Integer(right)) => left == right,
            (Value::Real(left), Value::Real(right)) => left.to_bits() == right.to_bits(),
            (Value::Text(left), Value::Text(right)) => {
                left.encoding == right.encoding && left.raw() == right.raw()
            }
            (Value::Blob(left), Value::Blob(right)) => left.raw() == right.raw(),
            _ => false,
        }
    }
}

/// Properties a value carries that its class does not describe.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct ValueFlags(u32);

impl ValueFlags {
    /// No flags.
    pub const NONE: ValueFlags = ValueFlags(0);
    /// A subtype has been set on this value.
    pub const HAS_SUBTYPE: ValueFlags = ValueFlags(1 << 0);
    /// The value was produced by applying an affinity rather than by the user,
    /// which is what decides whether a comparison may convert it again.
    pub const FROM_AFFINITY: ValueFlags = ValueFlags(1 << 1);
    /// The payload borrows bytes that outlive the statement, so it does not
    /// need copying when the row is held.
    pub const STATIC: ValueFlags = ValueFlags(1 << 2);

    /// Returns the union of two flag sets.
    pub fn with(self, other: ValueFlags) -> ValueFlags {
        ValueFlags(self.0 | other.0)
    }

    /// Returns this set without the flags in `other`.
    pub fn without(self, other: ValueFlags) -> ValueFlags {
        ValueFlags(self.0 & !other.0)
    }

    /// Reports whether every flag in `other` is set.
    pub fn contains(self, other: ValueFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns the raw bits, for tests and diagnostics.
    pub fn bits(self) -> u32 {
        self.0
    }
}

/// A value as the machine holds it: the value itself, its subtype, and flags.
///
/// The subtype is the application-defined tag SQLite lets a function attach to
/// a value - JSON1 uses it to say "this text is already JSON" - and it is
/// preserved only across the operations SQLite preserves it across, which is
/// why setting one also sets `HAS_SUBTYPE` rather than being inferred from a
/// non-zero tag.
#[derive(Clone, Debug)]
pub struct MemValue<'a> {
    /// The value.
    pub value: Value<'a>,
    /// The application-defined subtype, meaningful only with `HAS_SUBTYPE`.
    pub subtype: u32,
    /// Properties the class does not describe.
    pub flags: ValueFlags,
}

impl<'a> MemValue<'a> {
    /// Wraps a value with no subtype and no flags.
    pub fn new(value: Value<'a>) -> MemValue<'a> {
        MemValue {
            value,
            subtype: 0,
            flags: ValueFlags::NONE,
        }
    }

    /// Returns the same value carrying `subtype`.
    pub fn with_subtype(mut self, subtype: u32) -> MemValue<'a> {
        self.subtype = subtype;
        self.flags = self.flags.with(ValueFlags::HAS_SUBTYPE);
        self
    }

    /// Returns the subtype, or `None` when none was set.
    pub fn subtype(&self) -> Option<u32> {
        self.flags
            .contains(ValueFlags::HAS_SUBTYPE)
            .then_some(self.subtype)
    }

    /// Returns the same value with the subtype dropped.
    ///
    /// Any operation that produces a new value drops the subtype; only the
    /// operations SQLite documents as preserving it call `with_subtype` again.
    pub fn without_subtype(mut self) -> MemValue<'a> {
        self.subtype = 0;
        self.flags = self.flags.without(ValueFlags::HAS_SUBTYPE);
        self
    }

    /// Returns a form that borrows nothing.
    pub fn into_owned(self) -> DbResult<MemValue<'static>> {
        Ok(MemValue {
            value: self.value.into_owned()?,
            subtype: self.subtype,
            flags: self.flags.without(ValueFlags::STATIC),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The class order is what every comparison of unlike classes uses, so it
    /// is asserted directly rather than being left to the comparison tests.
    #[test]
    fn storage_classes_rank_the_way_comparisons_need() {
        assert!(StorageClass::Null.sort_rank() < StorageClass::Integer.sort_rank());
        assert_eq!(
            StorageClass::Integer.sort_rank(),
            StorageClass::Real.sort_rank()
        );
        assert!(StorageClass::Real.sort_rank() < StorageClass::Text.sort_rank());
        assert!(StorageClass::Text.sort_rank() < StorageClass::Blob.sort_rank());
    }

    /// `typeof()` names are part of the SQL surface and are spelled exactly.
    #[test]
    fn typeof_names_match_sqlite() {
        let names: Vec<&str> = StorageClass::all()
            .iter()
            .map(|class| class.typeof_name())
            .collect();
        assert_eq!(names, vec!["null", "integer", "real", "text", "blob"]);
    }

    /// An accessor is not a conversion: text that looks like a number is still
    /// text, and asking for its integer answers nothing.
    #[test]
    fn accessors_never_convert_between_classes() {
        let text = Value::text_utf8(b"42");
        assert_eq!(text.as_integer(), None);
        assert_eq!(text.as_real(), None);
        assert!(text.as_blob().is_none());
        assert_eq!(Value::Integer(42).as_real(), None);
        assert!(Value::Integer(42).as_text().is_none());
    }

    /// Borrowed payloads must become owned without changing what they hold.
    #[test]
    fn owning_a_borrowed_value_preserves_its_bytes() {
        let source = vec![0u8, 1, 2, 0xff];
        let borrowed = Value::blob(&source);
        assert!(matches!(&borrowed, Value::Blob(blob) if blob.bytes.is_borrowed()));
        let owned = borrowed.clone().into_owned().unwrap();
        assert!(owned.identical(&borrowed));
        assert!(matches!(&owned, Value::Blob(blob) if !blob.bytes.is_borrowed()));
    }

    /// Identity is by bits, so the two zeroes and the two NaNs are told apart.
    /// A value that round-trips through a record has to come back with the
    /// same bits, not merely with an equal number.
    #[test]
    fn identity_compares_the_bits_of_a_real() {
        assert!(Value::Real(0.0).identical(&Value::Real(0.0)));
        assert!(!Value::Real(0.0).identical(&Value::Real(-0.0)));
        assert!(Value::Real(f64::NAN).identical(&Value::Real(f64::NAN)));
        assert!(!Value::Integer(1).identical(&Value::Real(1.0)));
    }

    /// Text identity includes the encoding: the same string in UTF-8 and in
    /// UTF-16 is the same text but not the same bytes.
    #[test]
    fn identity_includes_the_text_encoding() {
        let utf8 = TextValue::utf8(b"abc");
        let wide = utf8.to_encoding(TextEncoding::Utf16Le).unwrap();
        assert!(!Value::Text(utf8.clone()).identical(&Value::Text(wide.clone())));
        assert_eq!(wide.utf8_bytes().as_ref(), b"abc");
        assert_eq!(wide.len(), 6);
    }

    /// Empty is a length, not an absence: the empty string and the empty blob
    /// are values and are not NULL.
    #[test]
    fn empty_payloads_are_values_rather_than_nulls() {
        assert!(!Value::text_utf8(b"").is_null());
        assert!(!Value::blob(b"").is_null());
        assert_eq!(Value::text_utf8(b"").byte_len(), 0);
        assert!(Value::Null.is_null());
    }

    /// Flags are a set, and a subtype is only present when it was set - a
    /// subtype of zero that someone set is different from no subtype at all.
    #[test]
    fn a_subtype_is_present_only_when_it_was_set() {
        let plain = MemValue::new(Value::Integer(1));
        assert_eq!(plain.subtype(), None);
        let tagged = plain.clone().with_subtype(0);
        assert_eq!(tagged.subtype(), Some(0));
        assert!(tagged.flags.contains(ValueFlags::HAS_SUBTYPE));
        assert_eq!(tagged.without_subtype().subtype(), None);
    }

    /// Flag arithmetic must be a set union and difference rather than a
    /// replacement, or a later flag would silently clear an earlier one.
    #[test]
    fn flags_union_and_difference_behave_as_a_set() {
        let both = ValueFlags::NONE
            .with(ValueFlags::STATIC)
            .with(ValueFlags::FROM_AFFINITY);
        assert!(both.contains(ValueFlags::STATIC));
        assert!(both.contains(ValueFlags::FROM_AFFINITY));
        assert!(!both.contains(ValueFlags::HAS_SUBTYPE));
        let one = both.without(ValueFlags::STATIC);
        assert!(!one.contains(ValueFlags::STATIC));
        assert!(one.contains(ValueFlags::FROM_AFFINITY));
    }

    /// Owning a value drops the static flag, because the copy is owned by this
    /// value and no longer by something that outlives the statement.
    #[test]
    fn owning_a_value_drops_the_static_flag() {
        let source = b"borrowed".to_vec();
        let held = MemValue {
            value: Value::text_utf8(&source),
            subtype: 7,
            flags: ValueFlags::STATIC.with(ValueFlags::HAS_SUBTYPE),
        };
        let owned = held.into_owned().unwrap();
        assert!(!owned.flags.contains(ValueFlags::STATIC));
        assert_eq!(owned.subtype(), Some(7));
    }
}
