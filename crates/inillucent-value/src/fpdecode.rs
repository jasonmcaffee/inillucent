//! A double turned into decimal digits and then into text, as SQLite does it.
//!
//! Invariant: every digit this module produces is the digit SQLite 3.53.4
//! produces for the same double and the same conversion. That is a stronger
//! claim than "correctly rounded", and a different one. SQLite's
//! `sqlite3FpDecode` does not compute the exact decimal expansion of the
//! double. It multiplies the binary significand by an approximation of a power of
//! ten held in 96 bits, keeps the top 64 bits of the product, and reads at most twenty decimal
//! digits out of that. The last digit it prints can therefore differ from the
//! last digit of the exact expansion, in either direction.
//!
//! This engine used to render through Rust's correctly rounded `{:e}` and then
//! round again. On one random double in a hundred the two disagreed in the last
//! digit (`1.1304293785495057e+251` there, `...058` here), and on the `!`
//! flag they disagreed on how many digits there were at all, because SQLite
//! stops at the digits its decoder produced and this engine filled with zeros
//! (task-2080). Neither disagreement can be closed by a better formatter,
//! because the reference is not the best possible answer. It is SQLite's
//! answer. So this is a transcription of `sqlite3FpDecode`, of the three
//! arithmetic helpers under it, and of the `etFLOAT`, `etEXP` and `etGENERIC`
//! arm of `sqlite3_str_vappendf` that turns the digits into characters. The
//! functions are named after the C ones so that the two can be read side by
//! side.
//!
//! `real_to_text` in [`crate::numeric`] is `%!.17g` through [`render`], which
//! is what `vdbeMemRenderNum` does. `printf()` calls [`render`] with the
//! conversion the caller wrote.

/// The smallest power of ten [`power_of_ten`] has a table entry for.
const POWERS_OF_TEN_FIRST: i32 = -348;
/// The largest power of ten [`power_of_ten`] has a table entry for.
const POWERS_OF_TEN_LAST: i32 = 347;

/// `SQLITE_FP_PRECISION_LIMIT`, the largest precision a real conversion uses.
///
/// SQLite defines it as one hundred million when the build does not set
/// `SQLITE_PRINTF_PRECISION_LIMIT`, and the pinned build does not.
const PRECISION_LIMIT: usize = 100_000_000;

/// `pow(10, p)` for `p` from 0 to 26, shifted so the top bit is set.
///
/// Copied from `powerOfTen` in the pinned `sqlite3.c`, which generates it with
/// `tool/mkfptab.c --round`.
const BASE: [u64; 27] = [
    0x8000000000000000,
    0xa000000000000000,
    0xc800000000000000,
    0xfa00000000000000,
    0x9c40000000000000,
    0xc350000000000000,
    0xf424000000000000,
    0x9896800000000000,
    0xbebc200000000000,
    0xee6b280000000000,
    0x9502f90000000000,
    0xba43b74000000000,
    0xe8d4a51000000000,
    0x9184e72a00000000,
    0xb5e620f480000000,
    0xe35fa931a0000000,
    0x8e1bc9bf04000000,
    0xb1a2bc2ec5000000,
    0xde0b6b3a76400000,
    0x8ac7230489e80000,
    0xad78ebc5ac620000,
    0xd8d726b7177a8000,
    0x878678326eac9000,
    0xa968163f0a57b400,
    0xd3c21bcecceda100,
    0x84595161401484a0,
    0xa56fa5b99019a5c8,
];

/// The top 64 bits of `pow(10, 27 * (i - 13))`, with entry 13 standing for
/// `pow(10, -1)`. Copied from the same function.
const SCALE: [u64; 26] = [
    0x8049a4ac0c5811ae,
    0xcf42894a5dce35ea,
    0xa76c582338ed2621,
    0x873e4f75e2224e68,
    0xda7f5bf590966848,
    0xb080392cc4349dec,
    0x8e938662882af53e,
    0xe65829b3046b0afa,
    0xba121a4650e4ddeb,
    0x964e858c91ba2655,
    0xf2d56790ab41c2a2,
    0xc428d05aa4751e4c,
    0x9e74d1b791e07e48,
    0xcccccccccccccccc,
    0xcecb8f27f4200f3a,
    0xa70c3c40a64e6c51,
    0x86f0ac99b4e8dafd,
    0xda01ee641a708de9,
    0xb01ae745b101e9e4,
    0x8e41ade9fbebc27d,
    0xe5d3ef282a242e81,
    0xb9a74a0637ce2ee1,
    0x95f83d0a1fb69cd9,
    0xf24a01a73cf2dccf,
    0xc3b8358109e84f07,
    0x9e19db92b4e31ba9,
];

/// The next 32 bits of each [`SCALE`] entry.
const SCALE_LOW: [u32; 26] = [
    0x205b896d, 0x52064cad, 0xaf2af2b8, 0x5a7744a7, 0xaf39a475, 0xbd8d794e, 0x547eb47b, 0x0cb4a5a3,
    0x92f34d62, 0x3a6a07f9, 0xfae27299, 0xaa97e14c, 0x775ea265, 0xcccccccc, 0x00000000, 0x999090b6,
    0x69a028bb, 0xe80e6f48, 0x5ec05dd0, 0x14588f14, 0x8f1668c9, 0x6d953e2c, 0x4abdaf10, 0xbc633b39,
    0x0a862f81, 0x6c07a2c2,
];

/// What kind of number a decoded double is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Special {
    /// A finite number, zero included.
    Finite,
    /// Positive or negative infinity.
    Infinity,
    /// Not a number.
    NaN,
}

/// A double as SQLite's `FpDecode` holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    /// Whether the value is below zero. Negative zero is not, because SQLite
    /// decides with `r < 0.0`.
    pub negative: bool,
    /// Whether the value is finite.
    pub special: Special,
    /// The significant digits, as ASCII, with no trailing zeros. Empty for an
    /// infinity or a NaN.
    pub digits: Vec<u8>,
    /// SQLite's `iDP`: how many of the digits come before the decimal point.
    /// Zero or negative when the value is below one.
    pub decimal_point: i32,
}

/// Which of the three real conversions is being rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conversion {
    /// `%f`.
    Fixed,
    /// `%e` and `%E`.
    Exponential,
    /// `%g` and `%G`.
    General,
}

/// A real conversion as the caller wrote it, less the field width.
///
/// The width is applied by whoever called [`render`], because it is applied
/// the same way to every conversion.
#[derive(Clone, Copy, Debug)]
pub struct Format {
    /// Which conversion.
    pub conversion: Conversion,
    /// The precision, or `None` for the default of six.
    pub precision: Option<usize>,
    /// `+` or a space, the character written before a value that is not
    /// negative.
    pub prefix: Option<u8>,
    /// The `#` flag.
    pub alternate: bool,
    /// The `!` flag.
    pub alternate2: bool,
    /// The `0` flag. It changes how an infinity or a NaN is written.
    pub zero_pad: bool,
    /// The `,` flag.
    pub thousands: bool,
    /// `%E` or `%G`, which write the exponent marker as `E`.
    pub upper: bool,
}

/// Returns the high 64 bits of `a * b`, and the low 64 bits.
///
/// `sqlite3Multiply128`.
///
/// @param a - one factor
/// @param b - the other factor
fn multiply128(a: u64, b: u64) -> (u64, u64) {
    let product = u128::from(a).wrapping_mul(u128::from(b));
    ((product >> 64) as u64, product as u64)
}

/// Returns the upper 96 bits of the 160 bit product `((a << 32) + a_low) * b`,
/// as the top 64 bits and the 32 bits under them.
///
/// `sqlite3Multiply160`. The lowest 64 bits of the product are dropped before
/// the addition that could carry out of them, as SQLite drops them. That is
/// part of why the result is an approximation, and it has to be the same
/// approximation.
///
/// @param a - the top 64 bits of the first factor
/// @param a_low - the next 32 bits of the first factor
/// @param b - the second factor
fn multiply160(a: u64, a_low: u32, b: u64) -> (u64, u32) {
    let low_product = (u128::from(a_low).wrapping_mul(u128::from(b))) >> 32;
    let product = u128::from(a)
        .wrapping_mul(u128::from(b))
        .wrapping_add(low_product);
    (
        (product >> 64) as u64,
        ((product >> 32) & 0xffff_ffff) as u32,
    )
}

/// Returns the top 64 bits of `pow(10, p)` and the 32 bits under them.
///
/// `powerOfTen`. From 0 to 26 the answer is exact and read from [`BASE`].
/// Outside that range a [`SCALE`] entry, which is a multiple of 27, is refined
/// by one multiplication against a [`BASE`] entry.
///
/// @param p - the power, from -348 to 347
fn power_of_ten(p: i32) -> (u64, u32) {
    let at_base = |index: i32| BASE.get(index as usize).copied().unwrap_or(0);
    let at_scale = |index: i32| SCALE.get(index as usize).copied().unwrap_or(0);
    let at_scale_low = |index: i32| SCALE_LOW.get(index as usize).copied().unwrap_or(0);
    let (group, remainder) = if p < 0 {
        if p == -1 {
            return (at_scale(13), at_scale_low(13));
        }
        let mut group = p / 27;
        let mut remainder = p % 27;
        if remainder != 0 {
            group -= 1;
            remainder += 27;
        }
        (group, remainder)
    } else if p < 27 {
        return (at_base(p), 0);
    } else {
        (p / 27, p % 27)
    };
    let scale = at_scale(group + 13);
    let scale_low = at_scale_low(group + 13);
    if remainder == 0 {
        return (scale, scale_low);
    }
    let (mut high, mut low) = multiply160(scale, scale_low, at_base(remainder));
    if high & (1 << 63) == 0 {
        high = (high << 1) | u64::from((low >> 31) & 1);
        low = (low << 1) | 1;
    }
    (high, low)
}

/// `floor(log2(pow(10, p)))`, to five digits. `pwr10to2`.
///
/// @param p - a power of ten
fn pwr10to2(p: i32) -> i32 {
    p.wrapping_mul(108853) >> 15
}

/// `floor(log10(pow(2, p)))`, to five digits. `pwr2to10`.
///
/// @param p - a power of two
fn pwr2to10(p: i32) -> i32 {
    p.wrapping_mul(78913) >> 18
}

/// Shifts right, answering zero for a shift C would leave undefined.
///
/// @param value - the bits
/// @param shift - how far
fn shift_right(value: u64, shift: i32) -> u64 {
    u32::try_from(shift)
        .ok()
        .and_then(|shift| value.checked_shr(shift))
        .unwrap_or(0)
}

/// Returns `d` and `p` such that `m * 2^e` is about `d * 10^p`, with at least
/// `n` significant digits in `d`.
///
/// `sqlite3Fp2Convert10`. At eighteen digits the last bit is rounded half to
/// even; below that it is truncated.
///
/// @param m - the significand, with its top bit set
/// @param e - the binary exponent
/// @param n - how many decimal digits are wanted, from 1 to 18
fn fp2_convert10(m: u64, e: i32, n: i32) -> (u64, i32) {
    let p = n - 1 - pwr2to10(e + 63);
    let (power, _) = power_of_ten(p);
    let (high, _) = multiply128(m, power);
    let shift = -(e + pwr10to2(p) + 2);
    let digits = if n == 18 {
        let high = shift_right(high, shift);
        high.wrapping_add((high << 1) & 2) >> 1
    } else {
        shift_right(high, shift + 1)
    };
    (digits, -p)
}

/// Returns the double nearest `d * 10^p`, as SQLite computes it.
///
/// `sqlite3Fp10Convert2`, which SQLite adapted from Russ Cox's fpfmt. The
/// decoder uses it only to ask whether a shorter digit string still reads back
/// as the same double.
///
/// @param d - the decimal significand, above zero
/// @param p - the power of ten
fn fp10_convert2(d: u64, p: i32) -> f64 {
    if p < POWERS_OF_TEN_FIRST {
        return 0.0;
    }
    if p > POWERS_OF_TEN_LAST {
        return f64::INFINITY;
    }
    let bits = 64 - d.leading_zeros() as i32;
    let lp = pwr10to2(p);
    let mut e = 53 - bits - lp;
    if e > 1074 {
        if e >= 1130 {
            return 0.0;
        }
        e = 1074;
    }
    let s = -(e - (64 - bits) + lp + 3);
    let (mut power_high, mut power_low) = power_of_ten(p);
    if power_low != 0 {
        power_high = power_high.wrapping_add(1);
        power_low = !power_low;
    }
    let x = u32::try_from(64 - bits)
        .ok()
        .and_then(|shift| d.checked_shl(shift))
        .unwrap_or(0);
    let (mut high, low) = multiply128(x, power_high);
    let middle1 = (low >> 32) as u32;
    let mut sticky = 1u64;
    let mask = u32::try_from(s)
        .ok()
        .and_then(|s| 1u64.checked_shl(s))
        .unwrap_or(0)
        .wrapping_sub(1);
    if high & mask == 0 {
        let (high2, _) = multiply128(x, u64::from(power_low) << 32);
        let middle2 = (high2 >> 32) as u32;
        sticky = u64::from(middle1.wrapping_sub(middle2) > 1);
        high = high.wrapping_sub(u64::from(middle1 < middle2));
    }
    let mut u = shift_right(high, s) | sticky;
    if u >= (1u64 << 55).wrapping_sub(2) {
        u = (u >> 1) | (u & 1);
        e -= 1;
    }
    let mut m = u.wrapping_add(1).wrapping_add((u >> 2) & 1) >> 2;
    if e <= -972 {
        return f64::INFINITY;
    }
    if m & (1 << 52) != 0 {
        m = (m & !(1u64 << 52)) | (((1075 - e) as u64) << 52);
    }
    f64::from_bits(m)
}

/// Reads the first `count` digits as an integer.
///
/// @param digits - ASCII digits
/// @param count - how many to read
fn leading_value(digits: &[u8], count: usize) -> u64 {
    digits.iter().take(count).fold(0u64, |value, digit| {
        value
            .wrapping_mul(10)
            .wrapping_add(u64::from(digit.wrapping_sub(b'0')))
    })
}

/// Decodes a double into significant digits and a decimal point.
///
/// `sqlite3FpDecode`. With `round` at zero or below, the value is rounded to
/// `-round` digits after the decimal point; above zero, to `round` significant
/// digits. Either way no more than `max_round` significant digits are kept.
///
/// @param value - the double
/// @param round - SQLite's `iRound`
/// @param max_round - SQLite's `mxRound`, above zero
pub fn decode(value: f64, round: i32, max_round: i32) -> Decoded {
    let negative = value < 0.0;
    if value == 0.0 {
        return Decoded {
            negative: false,
            special: Special::Finite,
            digits: b"0".to_vec(),
            decimal_point: 1,
        };
    }
    let magnitude = value.abs();
    let bits = magnitude.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i32;
    if biased == 0x7ff {
        let special = if bits == 0x7ff0_0000_0000_0000 {
            Special::Infinity
        } else {
            Special::NaN
        };
        return Decoded {
            negative,
            special,
            digits: Vec::new(),
            decimal_point: 0,
        };
    }
    let mut significand = bits & 0x000f_ffff_ffff_ffff;
    let exponent2 = if biased == 0 {
        let leading = significand.leading_zeros() as i32;
        significand <<= leading;
        -1074 - leading
    } else {
        significand = (significand << 11) | (1 << 63);
        biased - 1086
    };
    let wanted = if round <= 0 || round >= 18 {
        18
    } else {
        round + 1
    };
    let (decimal, exponent10) = fp2_convert10(significand, exponent2, wanted);
    if decimal == 0 {
        return Decoded {
            negative: false,
            special: Special::Finite,
            digits: b"0".to_vec(),
            decimal_point: 1,
        };
    }
    let digits = decimal.to_string().into_bytes();
    let decimal_point = digits.len() as i32 + exponent10;
    let mut decoded = Decoded {
        negative,
        special: Special::Finite,
        digits,
        decimal_point,
    };
    round_digits(&mut decoded, magnitude, exponent10, round, max_round);
    while decoded.digits.len() > 1 && decoded.digits.last() == Some(&b'0') {
        decoded.digits.pop();
    }
    decoded
}

/// Rounds decoded digits to the precision asked for.
///
/// The second half of `sqlite3FpDecode`, including its one special case: at
/// exactly seventeen significant digits, which only the `!` flag reaches, a
/// shorter string is kept when it reads back as the same double. That is why
/// `49.47` renders as `49.47` rather than `49.469999999999999`.
///
/// @param decoded - the digits and decimal point, rounded in place
/// @param magnitude - the absolute value that was decoded
/// @param exponent10 - the power of ten the unrounded digits are scaled by
/// @param round - SQLite's `iRound`
/// @param max_round - SQLite's `mxRound`
fn round_digits(
    decoded: &mut Decoded,
    magnitude: f64,
    exponent10: i32,
    round: i32,
    max_round: i32,
) {
    let digit = |digits: &[u8], index: i32| {
        usize::try_from(index)
            .ok()
            .and_then(|index| digits.get(index).copied())
            .unwrap_or(b'0')
    };
    let mut round = round;
    if round <= 0 {
        round = decoded.decimal_point - round;
        if round == 0 && digit(&decoded.digits, 0) >= b'5' {
            round = 1;
            decoded.digits.insert(0, b'0');
            decoded.decimal_point += 1;
        }
    }
    let count = decoded.digits.len() as i32;
    if round <= 0 || (round >= count && count <= max_round) {
        return;
    }
    round = round.min(max_round);
    if round == 17 {
        round = shortened(decoded, magnitude, exponent10).unwrap_or(round);
    }
    let rounds_up = digit(&decoded.digits, round) >= b'5';
    decoded.digits.truncate(round as usize);
    if !rounds_up {
        return;
    }
    let mut index = decoded.digits.len();
    loop {
        if index == 0 {
            decoded.digits.insert(0, b'1');
            decoded.decimal_point += 1;
            return;
        }
        index -= 1;
        let Some(place) = decoded.digits.get_mut(index) else {
            return;
        };
        if *place < b'9' {
            *place += 1;
            return;
        }
        *place = b'0';
    }
}

/// Returns a shorter significant digit count that reads back as the same
/// double, if SQLite would find one.
///
/// SQLite looks at two shapes and no others. A run of nines at the sixteenth
/// and fifteenth digits means the value sits just below a shorter decimal, so
/// the prefix before the nines is incremented and tried. A run of zeros there,
/// or a value with no fraction at all, means it sits just above one, so the
/// prefix before the zeros is tried. The count returned is one more than the
/// prefix, because the caller rounds at it.
///
/// @param decoded - the unrounded digits, at least eighteen of them
/// @param magnitude - the absolute value that was decoded
/// @param exponent10 - the power of ten the unrounded digits are scaled by
fn shortened(decoded: &Decoded, magnitude: f64, exponent10: i32) -> Option<i32> {
    let digits = &decoded.digits;
    let count = digits.len() as i32;
    let at = |index: usize| digits.get(index).copied().unwrap_or(b'0');
    if at(15) == b'9' && at(14) == b'9' {
        let mut kept = 14usize;
        while kept > 0 && at(kept - 1) == b'9' {
            kept -= 1;
        }
        let candidate = if kept == 0 {
            1
        } else {
            leading_value(digits, kept).wrapping_add(1)
        };
        let power = exponent10 + count - kept as i32;
        return (fp10_convert2(candidate, power) == magnitude).then_some(kept as i32 + 1);
    }
    if decoded.decimal_point >= count || (at(15) == b'0' && at(14) == b'0' && at(13) == b'0') {
        let mut kept = 13usize;
        while kept > 0 && at(kept - 1) == b'0' {
            kept -= 1;
        }
        let candidate = leading_value(digits, kept);
        let power = exponent10 + count - kept as i32;
        return (fp10_convert2(candidate, power) == magnitude).then_some(kept as i32 + 1);
    }
    None
}

/// Renders a double through one of the real conversions, without the width.
///
/// The `etFLOAT`, `etEXP` and `etGENERIC` arm of `sqlite3_str_vappendf`. Three
/// of its rules are easy to get wrong and each was wrong here before:
///
/// - the digits after the ones the decoder produced are zeros only when no
///   trailing zeros are being removed. With the `!` flag they are removed, so
///   `printf('%!.25f', 0.1)` is `0.1000000000000000056`, which is the
///   nineteen digits the decoder produced and nothing after them;
/// - without a precision the `!` flag still removes trailing zeros, so
///   `printf('%!e', 0.1)` is `1.0e-01` and `printf('%!f', 0.1)` is `0.1`;
/// - the `,` flag groups whatever is rendered in fixed form, and `%g` that
///   chose the fixed form is grouped too: `printf('%,.10g', 1234567.0)` is
///   `1,234,567`.
///
/// @param value - the number
/// @param format - the conversion and its flags
pub fn render(value: f64, format: &Format) -> Vec<u8> {
    let mut precision = format.precision.unwrap_or(6).min(PRECISION_LIMIT) as i32;
    let round = match format.conversion {
        Conversion::Fixed => -precision,
        Conversion::General => {
            if precision == 0 {
                precision = 1;
            }
            precision
        }
        Conversion::Exponential => precision + 1,
    };
    let max_round = if format.alternate2 { 20 } else { 16 };
    let mut decoded = decode(value, round, max_round);
    match decoded.special {
        Special::NaN => {
            return match format.zero_pad {
                true => b"null".to_vec(),
                false => b"NaN".to_vec(),
            };
        }
        Special::Infinity if !format.zero_pad => {
            let mut out = Vec::with_capacity(4);
            if decoded.negative {
                out.push(b'-');
            } else if let Some(prefix) = format.prefix {
                out.push(prefix);
            }
            out.extend_from_slice(b"Inf");
            return out;
        }
        Special::Infinity => {
            decoded.digits = b"9".to_vec();
            decoded.decimal_point = 1000;
        }
        Special::Finite => {}
    }
    let prefix = if decoded.negative {
        let displays_zero = format.alternate
            && format.prefix.is_none()
            && format.conversion == Conversion::Fixed
            && decoded.decimal_point <= round;
        (!displays_zero).then_some(b'-')
    } else {
        format.prefix
    };
    let exponent = decoded.decimal_point - 1;
    let mut conversion = format.conversion;
    let remove_trailing_zeros;
    if conversion == Conversion::General {
        precision -= 1;
        remove_trailing_zeros = !format.alternate;
        if exponent < -4 || exponent > precision {
            conversion = Conversion::Exponential;
        } else {
            precision -= exponent;
            conversion = Conversion::Fixed;
        }
    } else {
        remove_trailing_zeros = format.alternate2;
    }
    let layout = Layout {
        conversion,
        precision,
        prefix,
        remove_trailing_zeros,
        decimal_point: precision > 0 || format.alternate || format.alternate2,
        alternate2: format.alternate2,
        thousands: format.thousands,
        marker: if format.upper { b'E' } else { b'e' },
    };
    write_digits(&decoded, &layout)
}

/// The decisions [`render`] made, which [`write_digits`] carries out.
struct Layout {
    /// Fixed or exponential, after `%g` has chosen.
    conversion: Conversion,
    /// How many digits go after the decimal point.
    precision: i32,
    /// The sign or the `+` or space written first, if any.
    prefix: Option<u8>,
    /// Whether trailing zeros after the point are removed.
    remove_trailing_zeros: bool,
    /// Whether a decimal point is written.
    decimal_point: bool,
    /// The `!` flag, which keeps one zero after a point that would be bare.
    alternate2: bool,
    /// The `,` flag.
    thousands: bool,
    /// `e` or `E`.
    marker: u8,
}

/// Writes decoded digits out in fixed or exponential form.
///
/// The second half of the real conversion in `sqlite3_str_vappendf`: the digits
/// before the point, the point, the zeros between it and the first significant
/// digit, the significant digits after it, the trailing zeros, and the
/// exponent.
///
/// @param decoded - the digits and decimal point
/// @param layout - what [`render`] decided
fn write_digits(decoded: &Decoded, layout: &Layout) -> Vec<u8> {
    let digits = &decoded.digits;
    let count = digits.len() as i32;
    let digit = |index: i32| digits.get(index as usize).copied().unwrap_or(b'0');
    let mut precision = layout.precision;
    let mut e2 = match layout.conversion {
        Conversion::Exponential => 0,
        _ => decoded.decimal_point - 1,
    };
    let mut out: Vec<u8> = Vec::with_capacity(e2.max(0) as usize + precision.max(0) as usize + 10);
    if let Some(prefix) = layout.prefix {
        out.push(prefix);
    }
    let mut used = 0i32;
    if e2 < 0 {
        out.push(b'0');
    } else if layout.thousands {
        while e2 >= 0 {
            out.push(if used < count { digit(used) } else { b'0' });
            if used < count {
                used += 1;
            }
            if e2 % 3 == 0 && e2 > 1 {
                out.push(b',');
            }
            e2 -= 1;
        }
    } else {
        used = (e2 + 1).min(count);
        out.extend_from_slice(digits.get(..used as usize).unwrap_or(&[]));
        e2 -= used;
        if e2 >= 0 {
            out.extend(core::iter::repeat_n(b'0', (e2 + 1) as usize));
            e2 = -1;
        }
    }
    if layout.decimal_point {
        out.push(b'.');
    }
    if e2 < -1 && precision > 0 {
        let zeros = (-1 - e2).min(precision);
        out.extend(core::iter::repeat_n(b'0', zeros as usize));
        precision -= zeros;
    }
    if precision > 0 {
        let significant = (count - used).min(precision);
        if significant > 0 {
            let end = used + significant;
            out.extend_from_slice(digits.get(used as usize..end as usize).unwrap_or(&[]));
            precision -= significant;
        }
        if precision > 0 && !layout.remove_trailing_zeros {
            out.extend(core::iter::repeat_n(b'0', precision as usize));
        }
    }
    if layout.remove_trailing_zeros && layout.decimal_point {
        while out.last() == Some(&b'0') {
            out.pop();
        }
        if out.last() == Some(&b'.') {
            if layout.alternate2 {
                out.push(b'0');
            } else {
                out.pop();
            }
        }
    }
    if layout.conversion == Conversion::Exponential {
        write_exponent(&mut out, decoded.decimal_point - 1, layout.marker);
    }
    out
}

/// Writes `e+NN`, with at least two digits and three when needed.
///
/// @param out - where to write
/// @param exponent - the power of ten
/// @param marker - `e` or `E`
fn write_exponent(out: &mut Vec<u8>, exponent: i32, marker: u8) {
    out.push(marker);
    out.push(if exponent < 0 { b'-' } else { b'+' });
    let mut magnitude = exponent.unsigned_abs();
    if magnitude >= 100 {
        out.push(b'0'.wrapping_add((magnitude / 100) as u8));
        magnitude %= 100;
    }
    out.push(b'0'.wrapping_add((magnitude / 10) as u8));
    out.push(b'0'.wrapping_add((magnitude % 10) as u8));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conversion with no flags.
    fn plain(conversion: Conversion, precision: Option<usize>) -> Format {
        Format {
            conversion,
            precision,
            prefix: None,
            alternate: false,
            alternate2: false,
            zero_pad: false,
            thousands: false,
            upper: false,
        }
    }

    /// The same conversion with the `!` flag.
    fn bang(conversion: Conversion, precision: Option<usize>) -> Format {
        Format {
            alternate2: true,
            ..plain(conversion, precision)
        }
    }

    /// Renders to a string.
    fn text(value: f64, format: Format) -> String {
        String::from_utf8(render(value, &format)).expect("ASCII")
    }

    /// Every expected string here was printed by the pinned 3.53.4 shell.
    ///
    /// These are the values task-2080 names: two where the last digit used to
    /// differ, and the `!` cases where the digit count used to differ.
    #[test]
    fn the_values_task_2080_names_render_as_the_pinned_shell_does() {
        assert_eq!(
            text(1.1304293785495057e251, bang(Conversion::General, Some(17))),
            "1.1304293785495057e+251"
        );
        assert_eq!(
            text(-1.2466384898631833e-6, bang(Conversion::General, Some(17))),
            "-1.2466384898631833e-06"
        );
        assert_eq!(
            text(-8.24034521633578e-167, bang(Conversion::General, Some(17))),
            "-8.2403452163357795e-167"
        );
        assert_eq!(
            text(
                3.1643187021860255e-168,
                plain(Conversion::General, Some(20))
            ),
            "3.164318702186026e-168"
        );
        assert_eq!(
            text(3.14159265358979, bang(Conversion::General, Some(20))),
            "3.14159265358979001"
        );
        assert_eq!(
            text(0.1, bang(Conversion::General, Some(20))),
            "0.1000000000000000056"
        );
        assert_eq!(
            text(0.1, bang(Conversion::Exponential, Some(25))),
            "1.000000000000000056e-01"
        );
        assert_eq!(
            text(0.1, bang(Conversion::Fixed, Some(25))),
            "0.1000000000000000056"
        );
        assert_eq!(text(0.1, bang(Conversion::Exponential, None)), "1.0e-01");
        assert_eq!(text(0.1, bang(Conversion::Fixed, None)), "0.1");
        assert_eq!(text(49.47, bang(Conversion::General, Some(17))), "49.47");
    }

    /// Without the `!` flag the decoder stops at sixteen digits and the rest
    /// are zeros, which is the rule task-2066 section 4.2 item 27 found.
    #[test]
    fn sixteen_digits_and_then_zeros() {
        assert_eq!(
            text(1.0 / 3.0, plain(Conversion::Fixed, Some(20))),
            "0.33333333333333330000"
        );
        assert_eq!(
            text(1.0 / 3.0, plain(Conversion::Exponential, Some(20))),
            "3.33333333333333300000e-01"
        );
        assert_eq!(
            text(1e20, plain(Conversion::Fixed, None)),
            "100000000000000000000.000000"
        );
    }

    /// The sign of a value that displays as zero is dropped only under `#`.
    #[test]
    fn a_negative_that_displays_as_zero_keeps_its_sign_without_the_hash() {
        let hash = Format {
            alternate: true,
            ..plain(Conversion::Fixed, Some(0))
        };
        assert_eq!(text(-0.1, hash), "0.");
        assert_eq!(text(-0.0004, plain(Conversion::Fixed, Some(3))), "-0.000");
    }

    /// `%,g` that chooses the fixed form is grouped.
    #[test]
    fn the_thousands_flag_groups_a_general_conversion_in_fixed_form() {
        let grouped = Format {
            thousands: true,
            ..plain(Conversion::General, Some(10))
        };
        assert_eq!(text(1234567.0, grouped), "1,234,567");
        let exponential = Format {
            thousands: true,
            ..plain(Conversion::General, None)
        };
        assert_eq!(text(1234567.0, exponential), "1.23457e+06");
    }

    /// Infinity is `Inf`, and under the `0` flag it is a nine followed by a
    /// thousand zeros, which is what the reference prints.
    #[test]
    fn infinity_is_written_as_the_reference_writes_it() {
        let plus = Format {
            prefix: Some(b'+'),
            ..plain(Conversion::Fixed, Some(3))
        };
        assert_eq!(text(f64::INFINITY, plus), "+Inf");
        assert_eq!(
            text(f64::NEG_INFINITY, plain(Conversion::Fixed, None)),
            "-Inf"
        );
        let zero = Format {
            zero_pad: true,
            ..plain(Conversion::Fixed, Some(2))
        };
        let rendered = text(f64::NEG_INFINITY, zero);
        assert!(rendered.starts_with("-9000"), "{rendered}");
        assert!(rendered.ends_with("000.00"), "{rendered}");
        assert_eq!(rendered.len(), 1 + 1000 + 3);
    }
}
