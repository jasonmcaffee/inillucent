//! The built-in function registry: names, arities, and identities.
//!
//! Invariant: a function is recognised here or it does not exist. The binder
//! resolves a name to one of these identities and refuses everything else with
//! "no such function", so an unknown name fails at prepare time rather than
//! part-way through a scan, and the VM never dispatches on a string.
//!
//! Arity is checked here too, because SQLite reports "wrong number of arguments
//! to function abs()" from prepare rather than from execution.

/// A scalar built-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarFunc {
    /// `abs(x)`
    Abs,
    /// `char(...)`
    Char,
    /// `coalesce(...)`
    Coalesce,
    /// `concat(...)`
    Concat,
    /// `concat_ws(sep, ...)`
    ConcatWs,
    /// `glob(pattern, text)`
    Glob,
    /// `hex(x)`
    Hex,
    /// `ifnull(a, b)`
    IfNull,
    /// `iif(a, b, c)`
    Iif,
    /// `instr(haystack, needle)`
    Instr,
    /// `length(x)`
    Length,
    /// `like(pattern, text[, escape])`
    Like,
    /// `likelihood(x, y)`, `likely(x)` and `unlikely(x)`, which are no-ops.
    Likelihood,
    /// `lower(x)`
    Lower,
    /// `ltrim(x[, chars])`
    LTrim,
    /// `max(a, b, ...)`, the scalar form.
    Max,
    /// `min(a, b, ...)`, the scalar form.
    Min,
    /// `nullif(a, b)`
    NullIf,
    /// `quote(x)`
    Quote,
    /// `replace(text, from, to)`
    Replace,
    /// `round(x[, digits])`
    Round,
    /// `rtrim(x[, chars])`
    RTrim,
    /// `sign(x)`
    Sign,
    /// `substr(x, start[, length])`
    Substr,
    /// `trim(x[, chars])`
    Trim,
    /// `typeof(x)`
    TypeOf,
    /// `unhex(x[, chars])`
    Unhex,
    /// `unicode(x)`
    Unicode,
    /// `upper(x)`
    Upper,
    /// `zeroblob(n)`
    ZeroBlob,
    /// `sqlite_version()`
    Version,
}

/// An aggregate built-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateFunc {
    /// `count(x)` and `count(*)`
    Count,
    /// `sum(x)`
    Sum,
    /// `total(x)`
    Total,
    /// `avg(x)`
    Avg,
    /// `min(x)`
    Min,
    /// `max(x)`
    Max,
    /// `group_concat(x[, sep])` and `string_agg(x, sep)`
    GroupConcat,
}

/// Returns the scalar function a folded name spells.
pub fn lookup_scalar(folded: &[u8]) -> Option<ScalarFunc> {
    let func = match folded {
        b"abs" => ScalarFunc::Abs,
        b"char" => ScalarFunc::Char,
        b"coalesce" => ScalarFunc::Coalesce,
        b"concat" => ScalarFunc::Concat,
        b"concat_ws" => ScalarFunc::ConcatWs,
        b"glob" => ScalarFunc::Glob,
        b"hex" => ScalarFunc::Hex,
        b"ifnull" => ScalarFunc::IfNull,
        b"iif" => ScalarFunc::Iif,
        b"instr" => ScalarFunc::Instr,
        b"length" => ScalarFunc::Length,
        b"like" => ScalarFunc::Like,
        b"likelihood" | b"likely" | b"unlikely" => ScalarFunc::Likelihood,
        b"lower" => ScalarFunc::Lower,
        b"ltrim" => ScalarFunc::LTrim,
        b"max" => ScalarFunc::Max,
        b"min" => ScalarFunc::Min,
        b"nullif" => ScalarFunc::NullIf,
        b"quote" => ScalarFunc::Quote,
        b"replace" => ScalarFunc::Replace,
        b"round" => ScalarFunc::Round,
        b"rtrim" => ScalarFunc::RTrim,
        b"sign" => ScalarFunc::Sign,
        b"substr" | b"substring" => ScalarFunc::Substr,
        b"trim" => ScalarFunc::Trim,
        b"typeof" => ScalarFunc::TypeOf,
        b"unhex" => ScalarFunc::Unhex,
        b"unicode" => ScalarFunc::Unicode,
        b"upper" => ScalarFunc::Upper,
        b"zeroblob" => ScalarFunc::ZeroBlob,
        b"sqlite_version" => ScalarFunc::Version,
        _ => return None,
    };
    Some(func)
}

/// Returns the aggregate a folded name spells.
///
/// `min` and `max` are both: one argument makes them aggregates and two or more
/// make them scalars, which is why the binder asks about the argument count
/// before it decides.
pub fn lookup_aggregate(folded: &[u8]) -> Option<AggregateFunc> {
    let func = match folded {
        b"count" => AggregateFunc::Count,
        b"sum" => AggregateFunc::Sum,
        b"total" => AggregateFunc::Total,
        b"avg" => AggregateFunc::Avg,
        b"group_concat" | b"string_agg" => AggregateFunc::GroupConcat,
        _ => return None,
    };
    Some(func)
}

/// Returns whether an argument count is legal for a scalar function.
pub fn scalar_arity_ok(func: ScalarFunc, count: usize) -> bool {
    match func {
        ScalarFunc::Abs
        | ScalarFunc::Hex
        | ScalarFunc::Length
        | ScalarFunc::Lower
        | ScalarFunc::Quote
        | ScalarFunc::Sign
        | ScalarFunc::TypeOf
        | ScalarFunc::Unicode
        | ScalarFunc::Upper
        | ScalarFunc::ZeroBlob => count == 1,
        ScalarFunc::IfNull | ScalarFunc::NullIf | ScalarFunc::Glob => count == 2,
        ScalarFunc::Iif | ScalarFunc::Replace => count == 3,
        ScalarFunc::Instr => count == 2,
        ScalarFunc::Like => count == 2 || count == 3,
        ScalarFunc::Likelihood => count == 1 || count == 2,
        ScalarFunc::LTrim | ScalarFunc::RTrim | ScalarFunc::Trim | ScalarFunc::Unhex => {
            count == 1 || count == 2
        }
        ScalarFunc::Round => count == 1 || count == 2,
        ScalarFunc::Substr => count == 2 || count == 3,
        ScalarFunc::Coalesce | ScalarFunc::Max | ScalarFunc::Min => count >= 2,
        ScalarFunc::Char | ScalarFunc::Concat => count >= 1,
        ScalarFunc::ConcatWs => count >= 2,
        ScalarFunc::Version => count == 0,
    }
}

/// Returns whether an argument count is legal for an aggregate.
pub fn aggregate_arity_ok(func: AggregateFunc, count: usize, star: bool) -> bool {
    match func {
        AggregateFunc::Count => star || count == 1,
        AggregateFunc::Sum | AggregateFunc::Total | AggregateFunc::Avg => !star && count == 1,
        AggregateFunc::Min | AggregateFunc::Max => !star && count == 1,
        AggregateFunc::GroupConcat => !star && (count == 1 || count == 2),
    }
}

/// Returns whether a folded name may be an aggregate at this argument count.
///
/// `min(x)` is the aggregate and `min(x, y)` is the scalar; asking the question
/// this way keeps the rule in one place instead of in both lookups.
pub fn is_aggregate_call(folded: &[u8], count: usize, star: bool) -> bool {
    if folded == b"min" || folded == b"max" {
        return !star && count == 1;
    }
    lookup_aggregate(folded).is_some()
}

/// Returns the aggregate a `min`/`max` call resolves to at one argument.
pub fn minmax_aggregate(folded: &[u8]) -> Option<AggregateFunc> {
    match folded {
        b"min" => Some(AggregateFunc::Min),
        b"max" => Some(AggregateFunc::Max),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names are matched folded, and an unknown name is not a function.
    #[test]
    fn lookup_matches_folded_names() {
        assert_eq!(lookup_scalar(b"abs"), Some(ScalarFunc::Abs));
        assert_eq!(lookup_scalar(b"substring"), Some(ScalarFunc::Substr));
        assert_eq!(lookup_scalar(b"nope"), None);
        assert_eq!(lookup_aggregate(b"count"), Some(AggregateFunc::Count));
        assert_eq!(
            lookup_aggregate(b"string_agg"),
            Some(AggregateFunc::GroupConcat)
        );
    }

    /// `min` and `max` change identity with their argument count, which is the
    /// one place SQLite overloads a name across the scalar/aggregate boundary.
    #[test]
    fn min_and_max_are_aggregates_only_at_one_argument() {
        assert!(is_aggregate_call(b"min", 1, false));
        assert!(!is_aggregate_call(b"min", 2, false));
        assert!(!is_aggregate_call(b"min", 0, true));
        assert_eq!(minmax_aggregate(b"max"), Some(AggregateFunc::Max));
    }

    /// Arity is checked at bind time, so a wrong count is a prepare failure.
    #[test]
    fn arity_is_checked_per_function() {
        assert!(scalar_arity_ok(ScalarFunc::Abs, 1));
        assert!(!scalar_arity_ok(ScalarFunc::Abs, 2));
        assert!(scalar_arity_ok(ScalarFunc::Substr, 2));
        assert!(scalar_arity_ok(ScalarFunc::Substr, 3));
        assert!(!scalar_arity_ok(ScalarFunc::Substr, 4));
        assert!(scalar_arity_ok(ScalarFunc::Coalesce, 5));
        assert!(!scalar_arity_ok(ScalarFunc::Coalesce, 1));
        assert!(aggregate_arity_ok(AggregateFunc::Count, 0, true));
        assert!(!aggregate_arity_ok(AggregateFunc::Sum, 0, true));
    }
}
