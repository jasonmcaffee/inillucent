//! The keyword table of the pinned SQLite release, and the fallback rule.
//!
//! Invariant: this table is transcribed from the keyword list the pinned
//! release documents, not generated from its parser source, and every entry
//! records whether SQLite lets that word be used as a bare identifier. A word
//! that is a keyword here but an identifier there is a parse divergence with no
//! other symptom, so the fallback flag is data rather than a special case
//! buried in the parser.
//!
//! SQLite's real rule is per grammar position, and it is written in its grammar
//! as **two** declarations rather than one. A `%fallback` declaration lets a
//! large set of keywords stand in for an identifier wherever an identifier is
//! expected; that set is reproduced here as [`Keyword::may_fall_back`]. Beside
//! it, a `%token_class` declaration names a second set that the *name*
//! production accepts and two narrower positions do not:
//!
//! ```text
//! %token_class idj  ID|INDEXED|JOIN_KW.
//! %token_class ids  ID|STRING.
//! nm(A)       ::= idj(A).      // every name: a column, a table, an index
//! nm(A)       ::= STRING(A).
//! as(X)       ::= AS nm(Y).    // an alias written with AS is a name
//! as(X)       ::= ids(X).      // one written without AS is not
//! typename(A) ::= ids(A).      // and a declared type is not either
//! typename(A) ::= typename ids.
//! ```
//!
//! `JOIN_KW` is `CROSS FULL INNER LEFT NATURAL OUTER RIGHT`, and neither those
//! seven nor `INDEXED` is in the fallback set - which is why transcribing
//! `%fallback` alone was not enough. `CREATE TABLE pairs (left TEXT)` is a
//! schema SQLite writes and accepts, and it was refused here as a syntax error;
//! the *reason* it was refused is that a single flat flag
//! cannot express a rule the grammar states twice. So the second set is data
//! too, as [`Keyword::JOIN_KEYWORDS`] and [`Keyword::may_be_name`], and the
//! parser asks whichever question the position calls for.
//!
//! The two questions are not interchangeable and widening one into the other is
//! a real regression rather than a harmless loosening. SQLite accepts `SELECT a
//! AS left FROM t` and refuses `SELECT a left FROM t`; it refuses `SELECT *
//! FROM t left` while accepting `SELECT * FROM t LEFT JOIN u ON ...`, and the
//! bare-alias position is exactly what keeps a join keyword from swallowing its
//! own join. And it accepts `CREATE TABLE t (left TEXT)` while refusing `CREATE
//! TABLE t (a left)`, because the column's *name* and the type beside it take
//! different classes - so asking the wide question in the type position trades
//! one divergence from the pinned release for another.

/// A SQL keyword recognised by the pinned release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Keyword(u8);

/// Declares the keyword table: a constant per word, the lookup, and the name.
macro_rules! keywords {
    ($($index:expr => $constant:ident, $text:literal, $fallback:literal;)*) => {
        impl Keyword {
            $(
                #[doc = concat!("The `", stringify!($constant), "` keyword.")]
                pub const $constant: Keyword = Keyword($index);
            )*

            /// Returns the canonical upper-case spelling of the keyword.
            pub fn text(self) -> &'static [u8] {
                match self.0 {
                    $($index => $text,)*
                    _ => b"",
                }
            }

            /// Returns whether the keyword may stand in for an identifier.
            ///
            /// SQLite declares a fallback set so that historical schemas using
            /// words like `KEY` or `MATCH` as column names keep working. A word
            /// outside the set is a hard keyword in every position.
            pub fn may_fall_back(self) -> bool {
                match self.0 {
                    $($index => $fallback,)*
                    _ => false,
                }
            }
        }

        /// Every keyword, in table order.
        pub const KEYWORDS: &[Keyword] = &[$(Keyword::$constant),*];

        /// Returns the keyword an ASCII-case-insensitive word names.
        pub fn lookup(word: &[u8]) -> Option<Keyword> {
            if word.len() > MAX_KEYWORD_LEN {
                return None;
            }
            let mut upper = [0u8; MAX_KEYWORD_LEN];
            let slot = upper.get_mut(..word.len())?;
            for (target, byte) in slot.iter_mut().zip(word.iter()) {
                *target = byte.to_ascii_uppercase();
            }
            let upper = upper.get(..word.len())?;
            match upper {
                $($text => Some(Keyword::$constant),)*
                _ => None,
            }
        }
    };
}

/// The longest keyword in the table, which bounds the case-folding buffer.
pub const MAX_KEYWORD_LEN: usize = 17;

keywords! {
    0 => ABORT, b"ABORT", true;
    1 => ACTION, b"ACTION", true;
    2 => ADD, b"ADD", false;
    3 => AFTER, b"AFTER", true;
    4 => ALL, b"ALL", false;
    5 => ALTER, b"ALTER", false;
    6 => ALWAYS, b"ALWAYS", true;
    7 => ANALYZE, b"ANALYZE", true;
    8 => AND, b"AND", false;
    9 => AS, b"AS", false;
    10 => ASC, b"ASC", true;
    11 => ATTACH, b"ATTACH", true;
    12 => AUTOINCREMENT, b"AUTOINCREMENT", false;
    13 => BEFORE, b"BEFORE", true;
    14 => BEGIN, b"BEGIN", true;
    15 => BETWEEN, b"BETWEEN", false;
    16 => BY, b"BY", true;
    17 => CASCADE, b"CASCADE", true;
    18 => CASE, b"CASE", false;
    19 => CAST, b"CAST", true;
    20 => CHECK, b"CHECK", false;
    21 => COLLATE, b"COLLATE", false;
    22 => COLUMN, b"COLUMN", true;
    23 => COMMIT, b"COMMIT", false;
    24 => CONFLICT, b"CONFLICT", true;
    25 => CONSTRAINT, b"CONSTRAINT", false;
    26 => CREATE, b"CREATE", false;
    27 => CROSS, b"CROSS", false;
    28 => CURRENT, b"CURRENT", true;
    29 => CURRENT_DATE, b"CURRENT_DATE", false;
    30 => CURRENT_TIME, b"CURRENT_TIME", false;
    31 => CURRENT_TIMESTAMP, b"CURRENT_TIMESTAMP", false;
    32 => DATABASE, b"DATABASE", true;
    33 => DEFAULT, b"DEFAULT", false;
    34 => DEFERRABLE, b"DEFERRABLE", false;
    35 => DEFERRED, b"DEFERRED", true;
    36 => DELETE, b"DELETE", false;
    37 => DESC, b"DESC", true;
    38 => DETACH, b"DETACH", true;
    39 => DISTINCT, b"DISTINCT", false;
    40 => DO, b"DO", true;
    41 => DROP, b"DROP", false;
    42 => EACH, b"EACH", true;
    43 => ELSE, b"ELSE", false;
    44 => END, b"END", true;
    45 => ESCAPE, b"ESCAPE", false;
    46 => EXCEPT, b"EXCEPT", false;
    47 => EXCLUDE, b"EXCLUDE", true;
    48 => EXCLUSIVE, b"EXCLUSIVE", true;
    49 => EXISTS, b"EXISTS", false;
    50 => EXPLAIN, b"EXPLAIN", true;
    51 => FAIL, b"FAIL", true;
    52 => FILTER, b"FILTER", true;
    53 => FIRST, b"FIRST", true;
    54 => FOLLOWING, b"FOLLOWING", true;
    55 => FOR, b"FOR", true;
    56 => FOREIGN, b"FOREIGN", false;
    57 => FROM, b"FROM", false;
    58 => FULL, b"FULL", false;
    59 => GENERATED, b"GENERATED", true;
    60 => GLOB, b"GLOB", false;
    61 => GROUP, b"GROUP", false;
    62 => GROUPS, b"GROUPS", true;
    63 => HAVING, b"HAVING", false;
    64 => IF, b"IF", true;
    65 => IGNORE, b"IGNORE", true;
    66 => IMMEDIATE, b"IMMEDIATE", true;
    67 => IN, b"IN", false;
    68 => INDEX, b"INDEX", false;
    69 => INDEXED, b"INDEXED", false;
    70 => INITIALLY, b"INITIALLY", true;
    71 => INNER, b"INNER", false;
    72 => INSERT, b"INSERT", false;
    73 => INSTEAD, b"INSTEAD", true;
    74 => INTERSECT, b"INTERSECT", false;
    75 => INTO, b"INTO", false;
    76 => IS, b"IS", false;
    77 => ISNULL, b"ISNULL", false;
    78 => JOIN, b"JOIN", false;
    79 => KEY, b"KEY", true;
    80 => LAST, b"LAST", true;
    81 => LEFT, b"LEFT", false;
    82 => LIKE, b"LIKE", false;
    83 => LIMIT, b"LIMIT", false;
    84 => MATCH, b"MATCH", true;
    85 => MATERIALIZED, b"MATERIALIZED", true;
    86 => NATURAL, b"NATURAL", false;
    87 => NO, b"NO", true;
    88 => NOT, b"NOT", false;
    89 => NOTHING, b"NOTHING", false;
    90 => NOTNULL, b"NOTNULL", false;
    91 => NULL, b"NULL", false;
    92 => NULLS, b"NULLS", true;
    93 => OF, b"OF", true;
    94 => OFFSET, b"OFFSET", true;
    95 => ON, b"ON", false;
    96 => OR, b"OR", false;
    97 => ORDER, b"ORDER", false;
    98 => OTHERS, b"OTHERS", true;
    99 => OUTER, b"OUTER", false;
    100 => OVER, b"OVER", true;
    101 => PARTITION, b"PARTITION", true;
    102 => PLAN, b"PLAN", true;
    103 => PRAGMA, b"PRAGMA", true;
    104 => PRECEDING, b"PRECEDING", true;
    105 => PRIMARY, b"PRIMARY", false;
    106 => QUERY, b"QUERY", true;
    107 => RAISE, b"RAISE", true;
    108 => RANGE, b"RANGE", true;
    109 => RECURSIVE, b"RECURSIVE", true;
    110 => REFERENCES, b"REFERENCES", false;
    111 => REGEXP, b"REGEXP", false;
    112 => REINDEX, b"REINDEX", true;
    113 => RELEASE, b"RELEASE", true;
    114 => RENAME, b"RENAME", true;
    115 => REPLACE, b"REPLACE", true;
    116 => RESTRICT, b"RESTRICT", true;
    117 => RETURNING, b"RETURNING", false;
    118 => RIGHT, b"RIGHT", false;
    119 => ROLLBACK, b"ROLLBACK", true;
    120 => ROW, b"ROW", true;
    121 => ROWS, b"ROWS", true;
    122 => SAVEPOINT, b"SAVEPOINT", true;
    123 => SELECT, b"SELECT", false;
    124 => SET, b"SET", false;
    125 => TABLE, b"TABLE", false;
    126 => TEMP, b"TEMP", true;
    127 => TEMPORARY, b"TEMPORARY", true;
    128 => THEN, b"THEN", false;
    129 => TIES, b"TIES", true;
    130 => TO, b"TO", false;
    131 => TRANSACTION, b"TRANSACTION", false;
    132 => TRIGGER, b"TRIGGER", true;
    133 => UNBOUNDED, b"UNBOUNDED", true;
    134 => UNION, b"UNION", false;
    135 => UNIQUE, b"UNIQUE", false;
    136 => UPDATE, b"UPDATE", false;
    137 => USING, b"USING", false;
    138 => VACUUM, b"VACUUM", true;
    139 => VALUES, b"VALUES", false;
    140 => VIEW, b"VIEW", true;
    141 => VIRTUAL, b"VIRTUAL", true;
    142 => WHEN, b"WHEN", false;
    143 => WHERE, b"WHERE", false;
    144 => WINDOW, b"WINDOW", true;
    145 => WITH, b"WITH", true;
    146 => WITHOUT, b"WITHOUT", true;
}

impl Keyword {
    /// Returns the canonical spelling as text, for diagnostics.
    pub fn as_str(self) -> &'static str {
        core::str::from_utf8(self.text()).unwrap_or("")
    }

    /// SQLite's `JOIN_KW` token class: the words that introduce a join.
    ///
    /// They are hard keywords in the sense that matters to `may_fall_back` -
    /// none appears in the `%fallback` declaration - and they are still legal
    /// names, because the name production accepts the token class directly.
    /// Both facts are true at once and the grammar states them separately.
    pub const JOIN_KEYWORDS: &'static [Keyword] = &[
        Keyword::CROSS,
        Keyword::FULL,
        Keyword::INNER,
        Keyword::LEFT,
        Keyword::NATURAL,
        Keyword::OUTER,
        Keyword::RIGHT,
    ];

    /// Returns whether the keyword introduces a join.
    pub fn is_join_keyword(self) -> bool {
        Keyword::JOIN_KEYWORDS.contains(&self)
    }

    /// Returns whether the keyword may be written where a **name** is expected.
    ///
    /// This is SQLite's `nm ::= idj | STRING` with `idj ::= ID|INDEXED|JOIN_KW`,
    /// so it is the fallback set plus the seven join keywords plus `INDEXED`.
    /// It is the right question for a column declaration, a table or index
    /// name, a qualified reference, a window name, and an alias written with
    /// `AS`.
    ///
    /// It is the **wrong** question for the two positions that take `ids` - a
    /// bare alias and a declared type name - which take only
    /// [`Keyword::may_fall_back`]; see this module's header for why the two
    /// cannot be merged.
    pub fn may_be_name(self) -> bool {
        self.may_fall_back() || self.is_join_keyword() || self == Keyword::INDEXED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned release documents 147 keywords; a table that has drifted from
    /// that count is either missing a word or has invented one.
    #[test]
    fn the_table_holds_every_documented_keyword() {
        assert_eq!(KEYWORDS.len(), 147);
    }

    /// Lookup is ASCII-case-insensitive, which is the whole reason it exists.
    #[test]
    fn lookup_ignores_case() {
        assert_eq!(lookup(b"select"), Some(Keyword::SELECT));
        assert_eq!(lookup(b"SeLeCt"), Some(Keyword::SELECT));
        assert_eq!(lookup(b"SELECT"), Some(Keyword::SELECT));
        assert_eq!(lookup(b"selectx"), None);
    }

    /// A word longer than the longest keyword cannot be one, and must not
    /// overrun the fold buffer while proving it.
    #[test]
    fn an_overlong_word_is_not_a_keyword() {
        assert_eq!(lookup(&[b'A'; 512]), None);
    }

    /// Every constant must round-trip through its own text.
    #[test]
    fn every_keyword_round_trips() {
        for keyword in KEYWORDS {
            let text = keyword.text();
            assert_eq!(lookup(text), Some(*keyword), "{}", keyword.as_str());
        }
    }

    /// The fallback set is the compatibility rule that lets old schemas keep
    /// working. `KEY` must be usable as a name and `SELECT` must not.
    #[test]
    fn the_fallback_set_matches_the_grammar() {
        assert!(Keyword::KEY.may_fall_back());
        assert!(Keyword::MATCH.may_fall_back());
        assert!(Keyword::ROWS.may_fall_back());
        assert!(!Keyword::SELECT.may_fall_back());
        assert!(!Keyword::FROM.may_fall_back());
        assert!(!Keyword::WHERE.may_fall_back());
    }

    /// A join keyword is outside the fallback set and is still a name, which is
    /// the whole reason the two questions are separate.
    #[test]
    fn a_join_keyword_is_a_name_without_being_a_fallback() {
        for keyword in Keyword::JOIN_KEYWORDS {
            assert!(!keyword.may_fall_back(), "{}", keyword.as_str());
            assert!(keyword.may_be_name(), "{}", keyword.as_str());
        }
        assert!(!Keyword::INDEXED.may_fall_back());
        assert!(Keyword::INDEXED.may_be_name());
    }

    /// The name set is the fallback set plus exactly eight words. Pinned as a
    /// count so that widening either set is a visible edit rather than a
    /// silent one - `may_fall_back` is the transcription of `%fallback` and
    /// must not drift to mean "may be a name".
    #[test]
    fn the_name_set_is_the_fallback_set_plus_the_token_class() {
        let extra: Vec<&str> = KEYWORDS
            .iter()
            .filter(|keyword| keyword.may_be_name() && !keyword.may_fall_back())
            .map(|keyword| keyword.as_str())
            .collect();
        assert_eq!(
            extra,
            vec!["CROSS", "FULL", "INDEXED", "INNER", "LEFT", "NATURAL", "OUTER", "RIGHT"]
        );
        for keyword in KEYWORDS {
            if keyword.may_fall_back() {
                assert!(keyword.may_be_name(), "{}", keyword.as_str());
            }
        }
    }

    /// A hard keyword is a name in neither position.
    #[test]
    fn a_hard_keyword_is_never_a_name() {
        assert!(!Keyword::SELECT.may_be_name());
        assert!(!Keyword::FROM.may_be_name());
        assert!(!Keyword::JOIN.may_be_name());
        assert!(!Keyword::ON.may_be_name());
    }
}
