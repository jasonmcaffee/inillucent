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
    /// `printf(format, ...)` and `format(format, ...)`
    Printf,
    /// `octet_length(x)`
    OctetLength,
    /// `random()`
    Random,
    /// `randomblob(n)`
    RandomBlob,
    /// `changes()`
    Changes,
    /// `total_changes()`
    TotalChanges,
    /// `last_insert_rowid()`
    LastInsertRowid,
    /// `sqlite_source_id()`
    SourceId,
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
    /// `json_group_array(x)`
    JsonGroupArray,
    /// `jsonb_group_array(x)`
    JsonbGroupArray,
    /// `json_group_object(label, x)`
    JsonGroupObject,
    /// `jsonb_group_object(label, x)`
    JsonbGroupObject,
}

/// A date or time built-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeFunc {
    /// `date(...)`
    Date,
    /// `time(...)`
    Time,
    /// `datetime(...)`
    DateTime,
    /// `julianday(...)`
    JulianDay,
    /// `unixepoch(...)`
    UnixEpoch,
    /// `strftime(format, ...)`
    StrfTime,
    /// `timediff(a, b)`
    TimeDiff,
}

/// Returns the date or time function a folded name spells.
pub fn lookup_time(folded: &[u8]) -> Option<TimeFunc> {
    let func = match folded {
        b"date" => TimeFunc::Date,
        b"time" => TimeFunc::Time,
        b"datetime" => TimeFunc::DateTime,
        b"julianday" => TimeFunc::JulianDay,
        b"unixepoch" => TimeFunc::UnixEpoch,
        b"strftime" => TimeFunc::StrfTime,
        b"timediff" => TimeFunc::TimeDiff,
        _ => return None,
    };
    Some(func)
}

/// A math built-in.
///
/// They are their own enum rather than more `ScalarFunc` variants because they
/// are a compile-time option in SQLite (`SQLITE_ENABLE_MATH_FUNCTIONS`) and
/// share one rule the others do not: an argument outside the domain is NULL
/// rather than an error or a NaN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MathFunc {
    /// `acos(x)`
    Acos,
    /// `acosh(x)`
    Acosh,
    /// `asin(x)`
    Asin,
    /// `asinh(x)`
    Asinh,
    /// `atan(x)`
    Atan,
    /// `atan2(y, x)`
    Atan2,
    /// `atanh(x)`
    Atanh,
    /// `ceil(x)` and `ceiling(x)`
    Ceil,
    /// `cos(x)`
    Cos,
    /// `cosh(x)`
    Cosh,
    /// `degrees(x)`
    Degrees,
    /// `exp(x)`
    Exp,
    /// `floor(x)`
    Floor,
    /// `ln(x)`
    Ln,
    /// `log(x)` base 10, or `log(b, x)` base b.
    Log,
    /// `log10(x)`
    Log10,
    /// `log2(x)`
    Log2,
    /// `mod(x, y)`
    Mod,
    /// `pi()`
    Pi,
    /// `pow(x, y)` and `power(x, y)`
    Pow,
    /// `radians(x)`
    Radians,
    /// `sin(x)`
    Sin,
    /// `sinh(x)`
    Sinh,
    /// `sqrt(x)`
    Sqrt,
    /// `tan(x)`
    Tan,
    /// `tanh(x)`
    Tanh,
    /// `trunc(x)`
    Trunc,
}

impl MathFunc {
    /// Returns how many arguments the function takes, as `(least, most)`.
    pub fn arity(self) -> (usize, usize) {
        match self {
            MathFunc::Pi => (0, 0),
            MathFunc::Atan2 | MathFunc::Mod | MathFunc::Pow => (2, 2),
            MathFunc::Log => (1, 2),
            _ => (1, 1),
        }
    }
}

/// Returns the math function a folded name spells.
pub fn lookup_math(folded: &[u8]) -> Option<MathFunc> {
    let func = match folded {
        b"acos" => MathFunc::Acos,
        b"acosh" => MathFunc::Acosh,
        b"asin" => MathFunc::Asin,
        b"asinh" => MathFunc::Asinh,
        b"atan" => MathFunc::Atan,
        b"atan2" => MathFunc::Atan2,
        b"atanh" => MathFunc::Atanh,
        b"ceil" | b"ceiling" => MathFunc::Ceil,
        b"cos" => MathFunc::Cos,
        b"cosh" => MathFunc::Cosh,
        b"degrees" => MathFunc::Degrees,
        b"exp" => MathFunc::Exp,
        b"floor" => MathFunc::Floor,
        b"ln" => MathFunc::Ln,
        b"log" => MathFunc::Log,
        b"log10" => MathFunc::Log10,
        b"log2" => MathFunc::Log2,
        b"mod" => MathFunc::Mod,
        b"pi" => MathFunc::Pi,
        b"pow" | b"power" => MathFunc::Pow,
        b"radians" => MathFunc::Radians,
        b"sin" => MathFunc::Sin,
        b"sinh" => MathFunc::Sinh,
        b"sqrt" => MathFunc::Sqrt,
        b"tan" => MathFunc::Tan,
        b"tanh" => MathFunc::Tanh,
        b"trunc" => MathFunc::Trunc,
        _ => return None,
    };
    Some(func)
}

/// A window function that is not an aggregate.
///
/// The aggregates are the same functions in a different frame, so they are not
/// listed again here: `sum(x) OVER (...)` is `AggregateFunc::Sum` with a frame,
/// and giving it a second spelling would mean two implementations of `sum`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowFunc {
    /// `row_number()`
    RowNumber,
    /// `rank()`
    Rank,
    /// `dense_rank()`
    DenseRank,
    /// `percent_rank()`
    PercentRank,
    /// `cume_dist()`
    CumeDist,
    /// `ntile(n)`
    Ntile,
    /// `lag(x[, offset[, default]])`
    Lag,
    /// `lead(x[, offset[, default]])`
    Lead,
    /// `first_value(x)`
    FirstValue,
    /// `last_value(x)`
    LastValue,
    /// `nth_value(x, n)`
    NthValue,
}

impl WindowFunc {
    /// Returns how many arguments the function takes, as `(least, most)`.
    pub fn arity(self) -> (usize, usize) {
        match self {
            WindowFunc::RowNumber
            | WindowFunc::Rank
            | WindowFunc::DenseRank
            | WindowFunc::PercentRank
            | WindowFunc::CumeDist => (0, 0),
            WindowFunc::Ntile | WindowFunc::FirstValue | WindowFunc::LastValue => (1, 1),
            WindowFunc::NthValue => (2, 2),
            WindowFunc::Lag | WindowFunc::Lead => (1, 3),
        }
    }

    /// Returns whether the function reads the frame or the whole partition.
    ///
    /// `lag` and `lead` are defined on the partition and ignore the frame
    /// entirely; the ranking functions are defined on the peer groups. Only
    /// `first_value`, `last_value` and `nth_value` read the frame, and treating
    /// them alike is a wrong answer for every query with a narrow frame.
    pub fn reads_frame(self) -> bool {
        matches!(
            self,
            WindowFunc::FirstValue | WindowFunc::LastValue | WindowFunc::NthValue
        )
    }
}

/// Returns the window function a folded name spells.
pub fn lookup_window(folded: &[u8]) -> Option<WindowFunc> {
    let func = match folded {
        b"row_number" => WindowFunc::RowNumber,
        b"rank" => WindowFunc::Rank,
        b"dense_rank" => WindowFunc::DenseRank,
        b"percent_rank" => WindowFunc::PercentRank,
        b"cume_dist" => WindowFunc::CumeDist,
        b"ntile" => WindowFunc::Ntile,
        b"lag" => WindowFunc::Lag,
        b"lead" => WindowFunc::Lead,
        b"first_value" => WindowFunc::FirstValue,
        b"last_value" => WindowFunc::LastValue,
        b"nth_value" => WindowFunc::NthValue,
        _ => return None,
    };
    Some(func)
}

/// A JSON built-in.
///
/// They are their own enum for the same reason the math functions are: they
/// share a rule none of the others has. Every one of them can fail - a document
/// that will not parse is an error and not a NULL - and every one of them cares
/// whether its arguments are already JSON, which is a property of the value
/// rather than of the expression. Folding them into `ScalarFunc` would push
/// both facts onto eighty functions that have neither.
///
/// The `b` spellings return the binary format rather than text. They are
/// separate identities rather than a flag because `json_extract` and
/// `jsonb_extract` differ in more than their output: the text form answers a
/// SQL value for a leaf and the binary form answers a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonFunc {
    /// `json(X)`
    Json,
    /// `jsonb(X)`
    Jsonb,
    /// `json_array(...)`
    Array,
    /// `jsonb_array(...)`
    ArrayB,
    /// `json_array_length(X[, P])`
    ArrayLength,
    /// `json_error_position(X)`
    ErrorPosition,
    /// `json_extract(X, P, ...)`
    Extract,
    /// `jsonb_extract(X, P, ...)`
    ExtractB,
    /// The `->` operator.
    Arrow,
    /// The `->>` operator.
    ArrowShift,
    /// `json_insert(X, P, V, ...)`
    Insert,
    /// `jsonb_insert(X, P, V, ...)`
    InsertB,
    /// `json_object(...)`
    Object,
    /// `jsonb_object(...)`
    ObjectB,
    /// `json_patch(T, P)`
    Patch,
    /// `jsonb_patch(T, P)`
    PatchB,
    /// `json_pretty(X[, indent])`
    Pretty,
    /// `json_remove(X, P, ...)`
    Remove,
    /// `jsonb_remove(X, P, ...)`
    RemoveB,
    /// `json_replace(X, P, V, ...)`
    Replace,
    /// `jsonb_replace(X, P, V, ...)`
    ReplaceB,
    /// `json_set(X, P, V, ...)`
    Set,
    /// `jsonb_set(X, P, V, ...)`
    SetB,
    /// `json_type(X[, P])`
    Type,
    /// `json_valid(X[, flags])`
    Valid,
    /// `json_quote(X)`
    Quote,
}

impl JsonFunc {
    /// Returns how many arguments the function takes, as `(least, most)`.
    ///
    /// `usize::MAX` as the upper bound means "any number", which the editing
    /// functions further restrict to an odd count in
    /// [`JsonFunc::arity_ok`] - a rule a pair of bounds cannot express.
    pub fn arity(self) -> (usize, usize) {
        match self {
            JsonFunc::Json | JsonFunc::Jsonb | JsonFunc::ErrorPosition | JsonFunc::Quote => (1, 1),
            JsonFunc::Array | JsonFunc::ArrayB | JsonFunc::Object | JsonFunc::ObjectB => {
                (0, usize::MAX)
            }
            JsonFunc::ArrayLength | JsonFunc::Type | JsonFunc::Valid | JsonFunc::Pretty => (1, 2),
            JsonFunc::Patch | JsonFunc::PatchB | JsonFunc::Arrow | JsonFunc::ArrowShift => (2, 2),
            JsonFunc::Extract | JsonFunc::ExtractB | JsonFunc::Remove | JsonFunc::RemoveB => {
                (2, usize::MAX)
            }
            JsonFunc::Insert
            | JsonFunc::InsertB
            | JsonFunc::Replace
            | JsonFunc::ReplaceB
            | JsonFunc::Set
            | JsonFunc::SetB => (3, usize::MAX),
        }
    }

    /// Returns whether an argument count is legal for this function.
    pub fn arity_ok(self, count: usize) -> bool {
        let (least, most) = self.arity();
        if count < least || count > most {
            return false;
        }
        match self {
            // A path and a value go together, so the count past the document
            // has to be even and the whole count therefore odd.
            JsonFunc::Insert
            | JsonFunc::InsertB
            | JsonFunc::Replace
            | JsonFunc::ReplaceB
            | JsonFunc::Set
            | JsonFunc::SetB => count % 2 == 1,
            JsonFunc::Object | JsonFunc::ObjectB => count % 2 == 0,
            _ => true,
        }
    }

    /// Returns whether the function answers the binary format.
    pub fn is_binary(self) -> bool {
        matches!(
            self,
            JsonFunc::Jsonb
                | JsonFunc::ArrayB
                | JsonFunc::ExtractB
                | JsonFunc::InsertB
                | JsonFunc::ObjectB
                | JsonFunc::PatchB
                | JsonFunc::RemoveB
                | JsonFunc::ReplaceB
                | JsonFunc::SetB
        )
    }
}

/// Returns the JSON function a folded name spells.
pub fn lookup_json(folded: &[u8]) -> Option<JsonFunc> {
    let func = match folded {
        b"json" => JsonFunc::Json,
        b"jsonb" => JsonFunc::Jsonb,
        b"json_array" => JsonFunc::Array,
        b"jsonb_array" => JsonFunc::ArrayB,
        b"json_array_length" => JsonFunc::ArrayLength,
        b"json_error_position" => JsonFunc::ErrorPosition,
        b"json_extract" => JsonFunc::Extract,
        b"jsonb_extract" => JsonFunc::ExtractB,
        b"json_insert" => JsonFunc::Insert,
        b"jsonb_insert" => JsonFunc::InsertB,
        b"json_object" => JsonFunc::Object,
        b"jsonb_object" => JsonFunc::ObjectB,
        b"json_patch" => JsonFunc::Patch,
        b"jsonb_patch" => JsonFunc::PatchB,
        b"json_pretty" => JsonFunc::Pretty,
        b"json_remove" => JsonFunc::Remove,
        b"jsonb_remove" => JsonFunc::RemoveB,
        b"json_replace" => JsonFunc::Replace,
        b"jsonb_replace" => JsonFunc::ReplaceB,
        b"json_set" => JsonFunc::Set,
        b"jsonb_set" => JsonFunc::SetB,
        b"json_type" => JsonFunc::Type,
        b"json_valid" => JsonFunc::Valid,
        b"json_quote" => JsonFunc::Quote,
        _ => return None,
    };
    Some(func)
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
        b"printf" | b"format" => ScalarFunc::Printf,
        b"octet_length" => ScalarFunc::OctetLength,
        b"random" => ScalarFunc::Random,
        b"randomblob" => ScalarFunc::RandomBlob,
        b"changes" => ScalarFunc::Changes,
        b"total_changes" => ScalarFunc::TotalChanges,
        b"last_insert_rowid" => ScalarFunc::LastInsertRowid,
        b"sqlite_source_id" => ScalarFunc::SourceId,
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
        b"json_group_array" => AggregateFunc::JsonGroupArray,
        b"jsonb_group_array" => AggregateFunc::JsonbGroupArray,
        b"json_group_object" => AggregateFunc::JsonGroupObject,
        b"jsonb_group_object" => AggregateFunc::JsonbGroupObject,
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
        ScalarFunc::Printf => count >= 1,
        ScalarFunc::OctetLength | ScalarFunc::RandomBlob => count == 1,
        ScalarFunc::Random
        | ScalarFunc::Changes
        | ScalarFunc::TotalChanges
        | ScalarFunc::LastInsertRowid
        | ScalarFunc::SourceId => count == 0,
    }
}

/// Returns whether an argument count is legal for an aggregate.
pub fn aggregate_arity_ok(func: AggregateFunc, count: usize, star: bool) -> bool {
    match func {
        AggregateFunc::Count => star || count == 1,
        AggregateFunc::Sum | AggregateFunc::Total | AggregateFunc::Avg => !star && count == 1,
        AggregateFunc::Min | AggregateFunc::Max => !star && count == 1,
        AggregateFunc::GroupConcat => !star && (count == 1 || count == 2),
        AggregateFunc::JsonGroupArray | AggregateFunc::JsonbGroupArray => !star && count == 1,
        AggregateFunc::JsonGroupObject | AggregateFunc::JsonbGroupObject => !star && count == 2,
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
