//! The built-in function registry: names, arities, and identities.
//!
//! Invariant: a function is recognised here or it does not exist. The binder
//! resolves a name to one of these identities and refuses everything else with
//! "no such function", so an unknown name fails at prepare time rather than
//! part-way through a scan, and the VM never dispatches on a string.
//!
//! Arity is checked here too, because SQLite reports "wrong number of arguments
//! to function abs()" from prepare rather than from execution.

/// The names that exist in inillucent but need a component this build has not
/// got.
///
/// **`embed` is the whole list, and it is here rather than in the registry
/// because the registry is where it is absent** (task-1979, section 8.1, gap
/// 12). `inillucent-search` registers `embed` only when the `embed` feature is
/// compiled in, so on a build without it the name reaches the binder's
/// "no such function" path and answered exit 1 - which says the caller
/// misspelled something. The statement is spelled correctly and this build has
/// not got the function, which is exactly what exit 3 means.
///
/// A build that *does* have `embed` never reaches here, because the registry
/// resolves the name before the refusal is built. A machine that has the
/// function and not the model is a third thing again and keeps its own status:
/// `inillucent-search`'s `no_model` answers `invalid_state` and names
/// `inillucent setup-embeddings`, because the component is installable and
/// exit 3 would say the opposite.
const NEEDS_A_COMPONENT: &[(&[u8], &str)] = &[(
    b"embed",
    "embed(TEXT): this build has no embedding support compiled in",
)];

/// Returns what a name needs, when the name is one this build left out.
///
/// @param name - the folded function name that did not resolve
pub fn needs_a_component(name: &[u8]) -> Option<&'static str> {
    NEEDS_A_COMPONENT
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, said)| *said)
}

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
    /// `fts5_source_id()`
    Fts5SourceId,
    /// `sqlite_version()`
    Version,
    /// `vector_distance_cos(a, b)`, the cosine distance between two vectors.
    ///
    /// **Not a SQLite function, and the first one this engine adds.** pgvector
    /// spells it `a <=> b`; the whole point of Phase 2's Part 7 is that a
    /// vector is a value a `SELECT` can order by, and an operator that is sugar
    /// for a function needs the function to exist first. A vector is a blob of
    /// little-endian `f32`, which is what `inillucent_search` already stores and
    /// what `vector_distance_l2` and `vector_dot` read too.
    VectorDistanceCos,
    /// `vector_distance_l2(a, b)`, the Euclidean distance between two vectors.
    VectorDistanceL2,
    /// `vector_dot(a, b)`, the dot product of two vectors.
    ///
    /// Negated relative to pgvector's `<#>`, which answers the *negative* inner
    /// product so that a smaller number is a better match. This answers the dot
    /// product itself, because a function named `dot` that returned its negative
    /// would be a trap; the ordering sugar negates where it needs to.
    VectorDot,
    /// `l1_distance(a, b)`, the taxicab distance, spelled `a <+> b`.
    VectorDistanceL1,
    /// `hamming_distance(a, b)`, how many components differ.
    ///
    /// pgvector defines it over its `bit` type and spells it `a <~> b`. Here a
    /// bit vector is the blob `binary_quantize` produces, and the distance is
    /// the population count of the two blobs' exclusive-or - which is the same
    /// number, computed the same way, over the representation this engine has.
    VectorDistanceHamming,
    /// `jaccard_distance(a, b)`, one minus the overlap, spelled `a <%> b`.
    VectorDistanceJaccard,
    /// `vector_dims(a)`, how many components a vector has.
    VectorDims,
    /// `vector_norm(a)`, its Euclidean length.
    VectorNorm,
    /// `l2_normalize(a)`, the same direction with length one.
    VectorNormalize,
    /// `binary_quantize(a)`, one bit per component: set when it is positive.
    VectorQuantize,
    /// `subvector(a, start, count)`, a slice, counted from one.
    VectorSlice,
    /// `vector_add(a, b)`, component by component.
    ///
    /// **A function rather than `+`, and that is a compatibility choice rather
    /// than a shortcut.** pgvector can overload `+` because a `vector` is a
    /// distinct type in PostgreSQL; here a vector is a blob, and SQLite says
    /// that a blob in arithmetic is zero. Overloading the operator for every
    /// blob would change the answer to `x'00' + x'00'` from `0` to a blob,
    /// which is a difference every application that adds two blobs would see.
    ///
    /// **The operators were given back, on the one condition that keeps
    /// both answers.** `a + b` binds to this function when a side reads a
    /// column *declared* `VECTOR(n)` - which is the same thing PostgreSQL is
    /// using, a declared type - and stays SQLite's arithmetic otherwise. So
    /// `x'00' + x'00'` is still `0` and `v + v` over a vector column is a
    /// vector.
    VectorAdd,
    /// `vector_sub(a, b)`, component by component.
    VectorSubtract,
    /// `vector_mul(a, b)`, component by component.
    VectorMultiply,
    /// `vector_concat(a, b)`, one vector after the other.
    VectorConcat,
    /// `geopoly_area(P)`, the signed area a polygon encloses.
    ///
    /// **The `geopoly` surface is thirteen functions and one aggregate**, and
    /// they are listed here individually rather than folded into one
    /// `Geopoly(kind)` variant because arity checking reads this enum: they
    /// take one, two, three, four, seven and any number of arguments, and a
    /// single variant could not say so.
    GeopolyArea,
    /// `geopoly_blob(P)`, the stored form of a polygon.
    GeopolyBlob,
    /// `geopoly_json(P)`, the GeoJSON form.
    GeopolyJson,
    /// `geopoly_svg(P, ...)`, an SVG `<polyline>` with the extra arguments
    /// written into the tag.
    GeopolySvg,
    /// `geopoly_within(P1, P2)`, whether the second is inside the first.
    GeopolyWithin,
    /// `geopoly_contains_point(P, X, Y)`, where a point sits.
    GeopolyContainsPoint,
    /// `geopoly_overlap(P1, P2)`, how two polygons meet.
    GeopolyOverlap,
    /// `geopoly_debug(X)`, which answers nothing.
    ///
    /// It switches on the reference's own tracing, which only exists in a build
    /// made with `GEOPOLY_ENABLE_DEBUG`; in every other build it reads its
    /// argument and returns nothing at all. That is what this does, and it is
    /// registered because a name the reference resolves and this engine does
    /// not is a difference an application can see.
    GeopolyDebug,
    /// `geopoly_bbox(P)`, the bounding box as a four-sided polygon.
    GeopolyBbox,
    /// `geopoly_xform(P, A, B, C, D, E, F)`, an affine transform.
    GeopolyXform,
    /// `geopoly_regular(X, Y, R, N)`, a regular polygon.
    GeopolyRegular,
    /// `geopoly_ccw(P)`, the same ring wound counter-clockwise.
    GeopolyCcw,
    /// `unknown(...)`, which answers NULL to anything.
    ///
    /// SQLite registers it, lists it in `function_list`, and returns NULL from
    /// it whatever it is given. It is here because a name the reference resolves
    /// and this engine does not is a difference an application can see.
    Unknown,
    /// `subtype(x)`, the tag a function attached to its answer.
    Subtype,
    /// `unistr(x)`, which expands `\uXXXX` and `\UXXXXXXXX` escapes.
    Unistr,
    /// `unistr_quote(x)`, `quote()` with the control characters escaped.
    UnistrQuote,
    /// `sqlite_compileoption_used(name)`
    CompileOptionUsed,
    /// `sqlite_compileoption_get(n)`
    CompileOptionGet,
    /// `sqlite_log(code, message)`, which writes to the log and answers NULL.
    Log,
    /// `load_extension(path[, entry])`
    LoadExtension,
    /// `regexp(pattern, subject)`, which is what `X REGEXP Y` calls.
    Regexp,
    /// `sqlar_compress(X)`, a blob compressed if that makes it smaller.
    ///
    /// **The archive format's own rule, and it is why this is not just a
    /// compressor.** A row of a `.sqlar` table holds either a zlib stream or
    /// the raw bytes, and which one is decided by whichever is shorter; the
    /// stored `sz` column is what tells the two apart on the way back. So a
    /// value that does not compress is stored as it stands, and a value that is
    /// not a blob at all is returned unchanged, type and all.
    SqlarCompress,
    /// `sqlar_uncompress(Z, SZ)`, the inverse.
    ///
    /// `SZ` is the size the row claims the content is. When it equals the
    /// blob's own length the blob *is* the content and is returned unchanged,
    /// which is how the format says "this one was stored raw".
    SqlarUncompress,
    /// `sqlite_offset(X)`, where in the file the row holding X is.
    ///
    /// **The page, not the record, and that is the whole of the difference.**
    /// SQLite reports the byte offset of the *record* a value would be read
    /// from, because a row there is one contiguous run of bytes. A leaf here is
    /// PAX: each column is its own run, so one row occupies several places on
    /// its page and there is no single offset for it. What is reported is the
    /// offset of the page, which is where the value is genuinely read from.
    ///
    /// Folded to its answer by the physical pass, like `rtreecheck`, because it
    /// is a question about a *tree* rather than about a value.
    Offset,
    /// `rtreedepth(X)`, the depth stored at the front of an R-Tree node.
    RTreeDepth,
    /// `rtreenode(D, X)`, an R-Tree node rendered as a readable list.
    RTreeNode,
    /// `rtreecheck(T)`, an integrity check over one R-Tree table.
    ///
    /// **Answered where the table is reachable, which is not here.** A scalar
    /// is handed values and nothing else; this one is about a *table*, so the
    /// physical pass folds it to its answer while it still has the catalog,
    /// and what reaches the evaluator is already the text. Running once per
    /// preparation rather than once per row is also what it means: the
    /// argument is a table name, so the answer cannot vary down a column.
    RTreeCheck,
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
    /// `median(x)`, which is `percentile_cont(x, 0.5)` under a shorter name.
    Median,
    /// `geopoly_group_bbox(P)`, the box that holds every polygon in the group.
    GeopolyGroupBbox,
    /// `sum(v)` and `total(v)` over a vector column, component by component.
    ///
    /// Not a name a caller writes: the binder picks it when `sum`'s argument
    /// reads a vector, because that is where the argument's type is known.
    VectorSum,
    /// `avg(v)` over a vector column, component by component.
    VectorAvg,
    /// `percentile(x, p)`, where `p` runs 0 to 100.
    Percentile,
    /// `percentile_cont(x, f)`, where `f` runs 0 to 1 and the answer is
    /// interpolated between the two rows it falls between.
    PercentileCont,
    /// `percentile_disc(x, f)`, which answers one of the rows rather than a
    /// value between two of them.
    PercentileDisc,
    /// An aggregate an application registered, named beside the call.
    ///
    /// The name is not in here because this enum is `Copy` and travels through
    /// the program's operands; it rides in `AggregateCall` instead.
    External,
}

/// What a registered function promises about itself.
///
/// It lives here, below `inillucent-ext`, because two different layers have to
/// read the same promise: `inillucent_ext::registry::Registry` records it when
/// an application registers a function, and the binder enforces it when a
/// schema names one. `inillucent-ext` re-exports this type, so a registrant
/// writes `inillucent_ext::registry::FunctionFlags` exactly as before.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FunctionFlags {
    /// The function may only be called from top-level SQL, never from a
    /// schema: not from a `DEFAULT`, a `CHECK`, a generated column, an index
    /// expression, a partial-index predicate, a view or a trigger.
    ///
    /// [`FunctionFlags::external`] sets this, because the safe assumption about
    /// code somebody else wrote is that it does something. **It is not what the
    /// `Default` derive gives**, which is every flag false: a registrant who
    /// writes `..FunctionFlags::default()` gets a function a schema may name.
    /// That is the hole `embed` was registered through (task-1969, 7.4), and
    /// `inillucent_ext::registry::UserFunction::external` is the constructor to
    /// reach for instead.
    pub direct_only: bool,
    /// The function does nothing an ordinary expression could not: no side
    /// effects, no file access, no dependence on anything but its arguments.
    pub innocuous: bool,
    /// The function returns the same answer for the same arguments within one
    /// statement, so the planner may call it once.
    pub deterministic: bool,
}

impl FunctionFlags {
    /// Returns the flags a built-in carries: safe for a schema to call.
    pub fn builtin() -> FunctionFlags {
        FunctionFlags {
            direct_only: false,
            innocuous: true,
            deterministic: true,
        }
    }

    /// Returns the flags anything registered from outside carries by default.
    pub fn external() -> FunctionFlags {
        FunctionFlags {
            direct_only: true,
            innocuous: false,
            deterministic: false,
        }
    }
}

/// Which context a name is being resolved from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallSite {
    /// The statement an application submitted.
    Statement,
    /// A `DEFAULT`, `CHECK`, generated column, index expression, partial-index
    /// predicate, view or trigger stored in the schema.
    Schema,
}

/// Returns why a schema may not call this function, or nothing when it may.
///
/// **One rule, read by two layers (task-1972).** `Registry::authorize_function`
/// wraps the answer in a `DbError` for an application that asks the registry
/// directly, and the binder wraps it in a `ParseError` for the statement it is
/// compiling. Writing the rule twice is how the two would eventually disagree,
/// and the half nobody exercised would be the permissive one.
///
/// The rule reads the same way SQLite's does: a direct-only function is never
/// callable from a schema; anything else is callable from a schema only when
/// the connection trusts the schema or the function is innocuous.
///
/// @param flags - what the function promises about itself
/// @param site - where the call was written
/// @param trusted_schema - whether the connection trusts the schema it read
pub fn schema_refusal(
    flags: FunctionFlags,
    site: CallSite,
    trusted_schema: bool,
) -> Option<&'static str> {
    if site == CallSite::Statement {
        return None;
    }
    if flags.direct_only {
        return Some("may only be used from top-level SQL");
    }
    if trusted_schema || flags.innocuous {
        return None;
    }
    Some("is not allowed in a schema")
}

/// A function an application registered, as the binder needs to see it.
///
/// Only what resolution needs: a name, how many arguments it takes, whether it
/// reduces a group, and what it promises about itself. What it *does* is the
/// machine's business.
///
/// **The flags are here because the binder is where the promise is kept
/// (task-1972).** `Registry::authorize_function` had no caller, so
/// `direct_only`, `innocuous` and `PRAGMA trusted_schema` were a policy with a
/// passing unit test and no effect on the engine: a `CHECK`, an index
/// expression or a generated column could name any registered function whatever
/// its flags. `inillucent-sql` sits below `inillucent-ext` and cannot reach the
/// registry, so what the registry knows travels down here with the name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalFunction {
    /// The folded name.
    pub name: Vec<u8>,
    /// How many arguments it takes, or -1 for any number.
    pub arity: i32,
    /// Whether it reduces a group rather than a row.
    pub aggregate: bool,
    /// What it promises about itself, which decides whether a schema may name
    /// it.
    pub flags: FunctionFlags,
}

impl ExternalFunction {
    /// Returns whether this registration answers a call with this many
    /// arguments.
    pub fn accepts(&self, argc: usize) -> bool {
        self.arity < 0 || self.arity as usize == argc
    }
}

/// Returns the registration that answers a call, preferring an exact arity.
///
/// SQLite resolves the same way: a function registered for exactly this many
/// arguments wins over one registered for any number, so an application can
/// define both a fast two-argument form and a general one.
pub fn lookup_external<'a>(
    functions: &'a [ExternalFunction],
    name: &[u8],
    argc: usize,
) -> Option<&'a ExternalFunction> {
    let folded = name.to_ascii_lowercase();
    functions
        .iter()
        .find(|function| function.name == folded && function.arity as usize == argc)
        .or_else(|| {
            functions
                .iter()
                .find(|function| function.name == folded && function.arity < 0)
        })
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
    /// `json_array_insert(X, P, V, ...)`
    ArrayInsert,
    /// `jsonb_array_insert(X, P, V, ...)`
    ArrayInsertB,
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
            | JsonFunc::SetB
            | JsonFunc::ArrayInsert
            | JsonFunc::ArrayInsertB => (3, usize::MAX),
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
            | JsonFunc::SetB
            | JsonFunc::ArrayInsert
            | JsonFunc::ArrayInsertB => count % 2 == 1,
            JsonFunc::Object | JsonFunc::ObjectB => count.is_multiple_of(2),
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

    /// Returns whether this function's first argument names a document to be
    /// read, rather than a value to be embedded or quoted.
    ///
    /// The distinction an executor's document-cache optimisation needs: it
    /// may only substitute a pre-parsed JSONB blob for the first argument
    /// when that argument *is* the document a call reads, such as `X` in
    /// `json_extract(X, P)`. `json_array`, `json_object` and `json_quote`
    /// take that same position as a **value** - one that merely happens to
    /// look like JSON is still meant to be embedded or quoted as a string,
    /// per the subtype rule this module's own doc comment states. Handing
    /// them a blob instead answered "JSON cannot hold BLOB values" for a
    /// perfectly ordinary unmarked string, which is what
    /// `json_array('[1]')` did before this existed. `Valid` reads its
    /// argument as a document too, but is excluded by its caller for the
    /// unrelated reason that substituting a re-encoded blob changes what its
    /// flags answer about the original text.
    pub fn first_argument_is_a_document(self) -> bool {
        !matches!(
            self,
            JsonFunc::Array
                | JsonFunc::ArrayB
                | JsonFunc::Object
                | JsonFunc::ObjectB
                | JsonFunc::Quote
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
        // **The operators are function names too.** SQLite registers `->` and
        // `->>` as ordinary two-argument functions, so `"->"(a, b)` binds and
        // `pragma_function_list` reports them. The parser lowered the operators
        // here already; only the spellings were missing, which made this engine
        // report two fewer functions than it has and refuse a call SQLite
        // answers.
        b"->" => JsonFunc::Arrow,
        b"->>" => JsonFunc::ArrowShift,
        b"jsonb_extract" => JsonFunc::ExtractB,
        b"json_array_insert" => JsonFunc::ArrayInsert,
        b"jsonb_array_insert" => JsonFunc::ArrayInsertB,
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
        b"iif" | b"if" => ScalarFunc::Iif,
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
        b"fts5_source_id" => ScalarFunc::Fts5SourceId,
        b"trim" => ScalarFunc::Trim,
        b"typeof" => ScalarFunc::TypeOf,
        b"unhex" => ScalarFunc::Unhex,
        b"unicode" => ScalarFunc::Unicode,
        b"upper" => ScalarFunc::Upper,
        b"zeroblob" => ScalarFunc::ZeroBlob,
        b"sqlite_version" => ScalarFunc::Version,
        b"vector_distance_cos" | b"cosine_distance" => ScalarFunc::VectorDistanceCos,
        b"vector_distance_l2" | b"l2_distance" => ScalarFunc::VectorDistanceL2,
        b"vector_dot" | b"inner_product" => ScalarFunc::VectorDot,
        // **Both spellings of each distance.** `l1_distance` is pgvector's name
        // and `vector_distance_l1` is this engine's own, and the family reads
        // as a family only if every member answers to both - `cos` and `l2`
        // already did, and `l1` answered to one of the two.
        b"l1_distance" | b"vector_distance_l1" => ScalarFunc::VectorDistanceL1,
        b"hamming_distance" | b"vector_distance_hamming" => ScalarFunc::VectorDistanceHamming,
        b"jaccard_distance" | b"vector_distance_jaccard" => ScalarFunc::VectorDistanceJaccard,
        b"vector_dims" => ScalarFunc::VectorDims,
        b"vector_norm" => ScalarFunc::VectorNorm,
        b"l2_normalize" => ScalarFunc::VectorNormalize,
        b"binary_quantize" => ScalarFunc::VectorQuantize,
        b"subvector" => ScalarFunc::VectorSlice,
        b"vector_add" => ScalarFunc::VectorAdd,
        b"vector_sub" => ScalarFunc::VectorSubtract,
        b"vector_mul" => ScalarFunc::VectorMultiply,
        b"vector_concat" => ScalarFunc::VectorConcat,
        b"geopoly_area" => ScalarFunc::GeopolyArea,
        b"geopoly_blob" => ScalarFunc::GeopolyBlob,
        b"geopoly_json" => ScalarFunc::GeopolyJson,
        b"geopoly_svg" => ScalarFunc::GeopolySvg,
        b"geopoly_within" => ScalarFunc::GeopolyWithin,
        b"geopoly_contains_point" => ScalarFunc::GeopolyContainsPoint,
        b"geopoly_overlap" => ScalarFunc::GeopolyOverlap,
        b"geopoly_debug" => ScalarFunc::GeopolyDebug,
        b"geopoly_bbox" => ScalarFunc::GeopolyBbox,
        b"geopoly_xform" => ScalarFunc::GeopolyXform,
        b"geopoly_regular" => ScalarFunc::GeopolyRegular,
        b"geopoly_ccw" => ScalarFunc::GeopolyCcw,
        b"unknown" => ScalarFunc::Unknown,
        b"subtype" => ScalarFunc::Subtype,
        b"unistr" => ScalarFunc::Unistr,
        b"unistr_quote" => ScalarFunc::UnistrQuote,
        b"sqlite_compileoption_used" => ScalarFunc::CompileOptionUsed,
        b"sqlite_compileoption_get" => ScalarFunc::CompileOptionGet,
        b"sqlite_log" => ScalarFunc::Log,
        b"load_extension" => ScalarFunc::LoadExtension,
        b"regexp" => ScalarFunc::Regexp,
        b"sqlite_offset" => ScalarFunc::Offset,
        b"sqlar_compress" => ScalarFunc::SqlarCompress,
        b"sqlar_uncompress" => ScalarFunc::SqlarUncompress,
        b"rtreedepth" => ScalarFunc::RTreeDepth,
        b"rtreenode" => ScalarFunc::RTreeNode,
        b"rtreecheck" => ScalarFunc::RTreeCheck,
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
        b"geopoly_group_bbox" => AggregateFunc::GeopolyGroupBbox,
        b"median" => AggregateFunc::Median,
        b"percentile" => AggregateFunc::Percentile,
        b"percentile_cont" => AggregateFunc::PercentileCont,
        b"percentile_disc" => AggregateFunc::PercentileDisc,
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
        ScalarFunc::VectorDistanceCos
        | ScalarFunc::VectorDistanceL2
        | ScalarFunc::VectorDot
        | ScalarFunc::VectorDistanceL1
        | ScalarFunc::VectorDistanceHamming
        | ScalarFunc::VectorDistanceJaccard
        | ScalarFunc::VectorAdd
        | ScalarFunc::VectorSubtract
        | ScalarFunc::VectorMultiply
        | ScalarFunc::VectorConcat => count == 2,
        ScalarFunc::VectorDims
        | ScalarFunc::VectorNorm
        | ScalarFunc::VectorNormalize
        | ScalarFunc::VectorQuantize => count == 1,
        ScalarFunc::VectorSlice => count == 3,
        ScalarFunc::RTreeDepth | ScalarFunc::Offset | ScalarFunc::SqlarCompress => count == 1,
        ScalarFunc::SqlarUncompress => count == 2,
        ScalarFunc::RTreeNode => count == 2,
        // One argument is the table and two is a schema and a table, which is
        // the same pair `rtreecheck` takes in the reference.
        ScalarFunc::RTreeCheck => count == 1 || count == 2,
        ScalarFunc::GeopolyArea
        | ScalarFunc::GeopolyBlob
        | ScalarFunc::GeopolyJson
        | ScalarFunc::GeopolyDebug
        | ScalarFunc::GeopolyBbox
        | ScalarFunc::GeopolyCcw => count == 1,
        ScalarFunc::GeopolyWithin | ScalarFunc::GeopolyOverlap => count == 2,
        ScalarFunc::GeopolyContainsPoint => count == 3,
        ScalarFunc::GeopolyRegular => count == 4,
        ScalarFunc::GeopolyXform => count == 7,
        ScalarFunc::GeopolySvg => count >= 1,
        ScalarFunc::Replace => count == 3,
        // `iif` is `CASE` written as a call: pairs of a test and a value, with
        // an optional final answer. Two arguments is the shortest legal form
        // and there is no upper bound, which is why it is not `count == 3`.
        ScalarFunc::Iif => count >= 2,
        ScalarFunc::Unknown => true,
        ScalarFunc::Subtype
        | ScalarFunc::Unistr
        | ScalarFunc::UnistrQuote
        | ScalarFunc::CompileOptionUsed
        | ScalarFunc::CompileOptionGet => count == 1,
        ScalarFunc::Log | ScalarFunc::Regexp => count == 2,
        ScalarFunc::LoadExtension => count == 1 || count == 2,
        ScalarFunc::Instr => count == 2,
        ScalarFunc::Like => count == 2 || count == 3,
        ScalarFunc::Likelihood => count == 1 || count == 2,
        ScalarFunc::LTrim | ScalarFunc::RTrim | ScalarFunc::Trim | ScalarFunc::Unhex => {
            count == 1 || count == 2
        }
        ScalarFunc::Round => count == 1 || count == 2,
        ScalarFunc::Substr => count == 2 || count == 3,
        ScalarFunc::Coalesce | ScalarFunc::Max | ScalarFunc::Min => count >= 2,
        // `char()` with no arguments is the empty string in SQLite, not a
        // parse error (task-1979, F16). `concat()` keeps its floor of one,
        // which is the reference's own rule for that name.
        ScalarFunc::Char => true,
        ScalarFunc::Concat => count >= 1,
        ScalarFunc::ConcatWs => count >= 2,
        ScalarFunc::Version => count == 0,
        ScalarFunc::Printf => count >= 1,
        ScalarFunc::OctetLength | ScalarFunc::RandomBlob => count == 1,
        ScalarFunc::Random
        | ScalarFunc::Changes
        | ScalarFunc::TotalChanges
        | ScalarFunc::LastInsertRowid
        | ScalarFunc::SourceId
        | ScalarFunc::Fts5SourceId => count == 0,
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
        AggregateFunc::Median
        | AggregateFunc::GeopolyGroupBbox
        | AggregateFunc::VectorSum
        | AggregateFunc::VectorAvg => !star && count == 1,
        AggregateFunc::Percentile
        | AggregateFunc::PercentileCont
        | AggregateFunc::PercentileDisc => !star && count == 2,
        // An application's aggregate declared its own arity, and the binder
        // checked it against the registration before getting here.
        AggregateFunc::External => !star,
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

/// One row of `PRAGMA function_list`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionEntry {
    /// The name as it is written.
    pub name: &'static str,
    /// `s` for a scalar, `w` for a window function, `a` for an aggregate.
    pub kind: &'static str,
    /// How many arguments, or -1 for any number.
    pub arity: i64,
    /// The flag word the C surface reports.
    ///
    /// 2048 is `SQLITE_INNOCUOUS` and 524288 is `SQLITE_DETERMINISTIC`, which
    /// is what a built-in carries: it does nothing an expression could not, and
    /// it answers the same thing twice.
    pub flags: i64,
}

/// The bit `function_list` sets for a function a schema may safely call.
///
/// Named rather than written twice because `inillucent-engine`'s
/// `function_list` reports the connection's registered functions beside these
/// built-ins, and it has to describe them in the same column with the same
/// meaning. A registered function that promised `innocuous` and was reported
/// with a bit nothing else uses would be a register that under-describes, which
/// is the defect this whole list was extended to fix.
pub const INNOCUOUS_FLAG: i64 = 2048;

/// The bit `function_list` sets for a function that answers the same twice.
pub const DETERMINISTIC_FLAG: i64 = 524288;

/// The flags every built-in carries: innocuous and deterministic.
const BUILTIN_FLAGS: i64 = INNOCUOUS_FLAG | DETERMINISTIC_FLAG;

/// The flags a built-in that is not deterministic carries.
const VOLATILE_FLAGS: i64 = INNOCUOUS_FLAG;

/// Returns every built-in this build has, in the order `function_list` reports.
///
/// The list is written out rather than derived from the lookup tables because
/// the arity is per *overload*: `substr` is here twice, at two and at three
/// arguments, which is what SQLite reports and what an application checking
/// whether a call will bind needs to see.
///
/// **It must name everything the binder will resolve, and a completeness check
/// found that it did not.** The register answered 161 names where SQLite answers 218, and
/// the functionality behind most of the difference was present and
/// byte-identical - `current_date`, `regexp`, `unistr`, `median`, `bm25`,
/// `matchinfo` and the rest all answered when called. A caller that
/// introspects the register to decide what it may use was told less than the
/// truth, with no error, which is the one *silent* difference this project has
/// had. The additions below were each verified against the engine before being
/// listed: a name here that the binder refuses would be the same defect
/// pointing the other way.
pub fn every_function() -> Vec<FunctionEntry> {
    let mut out = Vec::new();
    let mut scalar = |name: &'static str, arity: i64| {
        out.push(FunctionEntry {
            name,
            kind: "s",
            arity,
            flags: BUILTIN_FLAGS,
        });
    };
    for (name, arity) in SCALARS {
        scalar(name, *arity);
    }
    for (name, arity) in VOLATILE {
        out.push(FunctionEntry {
            name,
            kind: "s",
            arity: *arity,
            flags: VOLATILE_FLAGS,
        });
    }
    for (name, arity) in AGGREGATES {
        out.push(FunctionEntry {
            name,
            kind: "a",
            arity: *arity,
            flags: BUILTIN_FLAGS,
        });
    }
    for (name, arity) in WINDOWS {
        out.push(FunctionEntry {
            name,
            kind: "w",
            arity: *arity,
            flags: BUILTIN_FLAGS,
        });
    }
    out.sort_by(|left, right| left.name.cmp(right.name).then(left.arity.cmp(&right.arity)));
    out
}

/// The deterministic scalars, with one row per overload.
///
/// **`narg` is SQLite's own encoding, not "how many arguments".** A negative
/// number means variadic *and carries a minimum*: `coalesce` reads -4 and
/// `concat` -3 in the reference's register, not -1. A
/// register-completeness check compares this column because it is the one an
/// application reads to decide whether a call will bind, and it found seven
/// entries here that disagreed with the reference while answering identically.
const SCALARS: &[(&str, i64)] = &[
    ("abs", 1),
    ("acos", 1),
    ("acosh", 1),
    ("asin", 1),
    ("asinh", 1),
    ("atan", 1),
    ("atan2", 2),
    ("atanh", 1),
    ("ceil", 1),
    ("ceiling", 1),
    ("char", -1),
    ("coalesce", -4),
    ("concat", -3),
    ("concat_ws", -4),
    ("cos", 1),
    ("cosh", 1),
    ("date", -1),
    ("datetime", -1),
    ("degrees", 1),
    ("exp", 1),
    ("floor", 1),
    ("format", -1),
    ("glob", 2),
    ("hex", 1),
    ("ifnull", 2),
    ("iif", -4),
    ("instr", 2),
    ("json", 1),
    ("json_array", -1),
    ("json_array_length", 1),
    ("json_array_length", 2),
    ("json_error_position", 1),
    ("json_extract", -1),
    ("json_insert", -1),
    ("json_object", -1),
    ("json_patch", 2),
    ("json_pretty", 1),
    ("json_pretty", 2),
    ("json_quote", 1),
    ("json_remove", -1),
    ("json_replace", -1),
    ("json_set", -1),
    ("json_type", 1),
    ("json_type", 2),
    ("json_valid", 1),
    ("json_valid", 2),
    ("jsonb", 1),
    ("jsonb_array", -1),
    ("jsonb_extract", -1),
    ("jsonb_insert", -1),
    ("jsonb_object", -1),
    ("jsonb_patch", 2),
    ("jsonb_remove", -1),
    ("jsonb_replace", -1),
    ("jsonb_set", -1),
    ("julianday", -1),
    ("length", 1),
    ("like", 2),
    ("like", 3),
    ("likelihood", 2),
    ("likely", 1),
    ("ln", 1),
    ("log", 1),
    ("log", 2),
    ("log10", 1),
    ("log2", 1),
    ("lower", 1),
    ("ltrim", 1),
    ("ltrim", 2),
    ("max", -3),
    ("min", -3),
    ("mod", 2),
    ("nullif", 2),
    ("octet_length", 1),
    ("pi", 0),
    ("pow", 2),
    ("power", 2),
    ("printf", -1),
    ("quote", 1),
    ("radians", 1),
    ("replace", 3),
    ("round", 1),
    ("round", 2),
    ("rtrim", 1),
    ("rtrim", 2),
    ("sign", 1),
    ("sin", 1),
    ("sinh", 1),
    ("fts5_source_id", 0),
    ("optimize", 1),
    ("sqlite_source_id", 0),
    ("sqlite_version", 0),
    ("sqrt", 1),
    ("strftime", -1),
    ("substr", 2),
    ("substr", 3),
    ("substring", 2),
    ("substring", 3),
    ("tan", 1),
    ("tanh", 1),
    ("time", -1),
    ("timediff", 2),
    ("trim", 1),
    ("trim", 2),
    ("trunc", 1),
    ("typeof", 1),
    ("unhex", 1),
    ("unhex", 2),
    ("unicode", 1),
    ("unixepoch", -1),
    ("unlikely", 1),
    ("upper", 1),
    ("binary_quantize", 1),
    ("rtreecheck", -1),
    ("sqlar_compress", 1),
    ("sqlar_uncompress", 2),
    ("sqlite_offset", 1),
    ("rtreedepth", 1),
    ("rtreenode", 2),
    ("geopoly_area", 1),
    ("geopoly_bbox", 1),
    ("geopoly_blob", 1),
    ("geopoly_ccw", 1),
    ("geopoly_contains_point", 3),
    ("geopoly_debug", 1),
    ("geopoly_group_bbox", 1),
    ("geopoly_json", 1),
    ("geopoly_overlap", 2),
    ("geopoly_regular", 4),
    ("geopoly_svg", -1),
    ("geopoly_within", 2),
    ("geopoly_xform", 7),
    ("cosine_distance", 2),
    ("hamming_distance", 2),
    ("inner_product", 2),
    ("jaccard_distance", 2),
    ("l1_distance", 2),
    ("l2_distance", 2),
    ("l2_normalize", 1),
    ("subvector", 3),
    ("vector_add", 2),
    ("vector_concat", 2),
    ("vector_dims", 1),
    ("vector_distance_cos", 2),
    ("vector_distance_l2", 2),
    ("vector_dot", 2),
    ("vector_mul", 2),
    ("vector_norm", 1),
    ("vector_sub", 2),
    ("zeroblob", 1),
    // Present and answering, and missing from this list until now.
    // Each was checked against the shell before it was added.
    ("->", 2),
    ("->>", 2),
    ("bm25", -1),
    ("highlight", -1),
    ("if", -4),
    ("json_array_insert", -1),
    ("jsonb_array_insert", -1),
    ("match", 2),
    ("matchinfo", 1),
    ("matchinfo", 2),
    ("offsets", 1),
    ("regexp", 2),
    ("snippet", -1),
    ("sqlite_compileoption_get", 1),
    ("sqlite_compileoption_used", 1),
    ("subtype", 1),
    ("unistr", 1),
    ("unistr_quote", 1),
    ("unknown", -1),
];

/// The scalars whose answer depends on something other than their arguments.
const VOLATILE: &[(&str, i64)] = &[
    ("changes", 0),
    // The three date keywords are functions in SQLite's register and answer
    // like functions here; they read the clock, so they are not deterministic.
    ("current_date", 0),
    ("current_time", 0),
    ("current_timestamp", 0),
    ("last_insert_rowid", 0),
    ("load_extension", 1),
    ("load_extension", 2),
    ("random", 0),
    ("randomblob", 1),
    ("sqlite_log", 2),
    ("total_changes", 0),
];

/// The aggregates, with one row per overload.
const AGGREGATES: &[(&str, i64)] = &[
    ("avg", 1),
    ("count", 0),
    ("count", 1),
    ("group_concat", 1),
    ("group_concat", 2),
    ("json_group_array", 1),
    ("json_group_object", 2),
    ("jsonb_group_array", 1),
    ("jsonb_group_object", 2),
    ("max", 1),
    ("min", 1),
    ("string_agg", 2),
    ("sum", 1),
    ("total", 1),
];

/// The window functions that are not aggregates.
const WINDOWS: &[(&str, i64)] = &[
    ("cume_dist", 0),
    ("dense_rank", 0),
    ("first_value", 1),
    ("lag", 1),
    ("lag", 2),
    ("lag", 3),
    ("last_value", 1),
    ("lead", 1),
    ("lead", 2),
    ("lead", 3),
    ("nth_value", 2),
    ("ntile", 1),
    ("percent_rank", 0),
    ("rank", 0),
    ("row_number", 0),
    // The percentile family, which SQLite reports as window functions and which
    // this engine answers as both aggregates and window functions.
    ("median", 1),
    ("percentile", 2),
    ("percentile_cont", 2),
    ("percentile_disc", 2),
];
