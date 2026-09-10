//! `REGEXP`, in the dialect the reference shell registers.
//!
//! Invariant: this is a port of `ext/misc/regexp.c` rather than a regular
//! expression engine chosen on its merits, and the difference matters. The
//! dialect is deliberately small - no capture groups, no back-references, no
//! lazy quantifiers, no POSIX classes - and a library that supported more would
//! answer `1` where the reference answers an error, which is a difference an
//! application can see. So the opcode set, the error strings and the character
//! classes below are the reference's, and the tests at the foot of the file are
//! the cases where a more capable engine would disagree.
//!
//! The machine is an NFA simulated breadth-first over the input: every active
//! state is stepped against one character at a time, so a pattern cannot
//! backtrack exponentially. That is the reference's design too, and it is why a
//! hostile pattern in a `WHERE` clause is a bounded cost rather than an outage.

/// What one opcode does.
///
/// The numbering is not the reference's - these are names rather than integers
/// - but the set is exactly its, and `Fork` and `Goto` carry the same relative
/// jumps, because `{m,n}` is compiled by copying opcodes and the offsets have to
/// survive the copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    /// Match the one character in the argument.
    Match,
    /// Match any one character, which is `.`.
    Any,
    /// The optimised `.*`.
    AnyStar,
    /// Continue at both the next opcode and the one the argument offsets to.
    Fork,
    /// Jump to the opcode the argument offsets to.
    Goto,
    /// Halt, having matched.
    Accept,
    /// The head of a `[...]` class; the argument is its length.
    ClassInclude,
    /// The head of a `[^...]` class.
    ClassExclude,
    /// One value inside a class.
    ClassValue,
    /// One end of a range inside a class; they come in pairs.
    ClassRange,
    /// `\w`, which is `[A-Za-z0-9_]`.
    Word,
    /// `\W`.
    NotWord,
    /// `\d`.
    Digit,
    /// `\D`.
    NotDigit,
    /// `\s`.
    Space,
    /// `\S`.
    NotSpace,
    /// `\b`, a boundary between a word character and anything else.
    Boundary,
    /// `^`, which asserts rather than consuming.
    AtStart,
}

/// The character code that stands for end of input.
const EOF: u32 = 0;

/// The character code that stands for the position before the first character.
///
/// Larger than any code point, so it can never equal a `Match` argument.
const START: u32 = 0x0fff_ffff;

/// How many opcodes a pattern may compile to.
///
/// The reference derives its limit from `SQLITE_LIMIT_LENGTH`; this is the same
/// order of magnitude and exists for the same reason: `a{1000}{1000}` compiles
/// by copying, and a pattern is a string somebody else may have written.
const MAX_STATES: usize = 100_000;

/// A compiled pattern.
pub struct Regexp {
    /// One entry per state.
    ops: Vec<Op>,
    /// The argument of each state, parallel to `ops`.
    args: Vec<i64>,
    /// A literal prefix the matcher may search for before running the machine.
    ///
    /// The reference's one optimisation, kept because without it a pattern with
    /// no anchor scans the whole input once per starting position through the
    /// state machine rather than through `memchr`.
    init: Vec<u8>,
    /// Whether comparisons fold ASCII case.
    fold_case: bool,
}

/// A pattern being compiled.
struct Compiler<'p> {
    /// The pattern's bytes.
    text: &'p [u8],
    /// How far into them the compiler has read.
    at: usize,
    /// The opcodes so far.
    ops: Vec<Op>,
    /// Their arguments.
    args: Vec<i64>,
    /// Whether comparisons fold ASCII case.
    fold_case: bool,
}

/// Returns whether a character is a Perl word character.
fn is_word(character: u32) -> bool {
    matches!(character, 0x30..=0x39 | 0x41..=0x5a | 0x61..=0x7a | 0x5f)
}

/// Returns whether a character is a decimal digit.
fn is_digit(character: u32) -> bool {
    (0x30..=0x39).contains(&character)
}

/// Returns whether a character is one of Perl's six space characters.
fn is_space(character: u32) -> bool {
    matches!(character, 0x20 | 0x09 | 0x0a | 0x0d | 0x0b | 0x0c)
}

/// Reads one UTF-8 character out of a byte string, replacing anything malformed.
///
/// A port of the reference's `re_next_char`, including its answer for a bad
/// sequence: U+FFFD rather than a refusal, so a `REGEXP` over a column holding
/// arbitrary bytes returns an answer instead of failing the statement.
///
/// @param text - the bytes
/// @param at - where to read, advanced past the character
fn next_char(text: &[u8], at: &mut usize) -> u32 {
    let Some(first) = text.get(*at).copied() else {
        return EOF;
    };
    *at += 1;
    if first < 0x80 {
        return u32::from(first);
    }
    let continuation = |offset: usize| -> Option<u32> {
        text.get(*at + offset)
            .copied()
            .filter(|byte| byte & 0xc0 == 0x80)
            .map(|byte| u32::from(byte & 0x3f))
    };
    if first & 0xe0 == 0xc0 {
        if let Some(second) = continuation(0) {
            *at += 1;
            let value = (u32::from(first & 0x1f) << 6) | second;
            return if value < 0x80 { 0xfffd } else { value };
        }
    } else if first & 0xf0 == 0xe0 {
        if let (Some(second), Some(third)) = (continuation(0), continuation(1)) {
            *at += 2;
            let value = (u32::from(first & 0x0f) << 12) | (second << 6) | third;
            return if value <= 0x7ff || (0xd800..=0xdfff).contains(&value) {
                0xfffd
            } else {
                value
            };
        }
    } else if first & 0xf8 == 0xf0 {
        if let (Some(second), Some(third), Some(fourth)) =
            (continuation(0), continuation(1), continuation(2))
        {
            *at += 3;
            let value = (u32::from(first & 0x07) << 18) | (second << 12) | (third << 6) | fourth;
            return if value <= 0xffff || value > 0x10ffff {
                0xfffd
            } else {
                value
            };
        }
    }
    0xfffd
}

impl Compiler<'_> {
    /// Reads the next character of the pattern, folding case when asked to.
    fn take(&mut self) -> u32 {
        let character = next_char(self.text, &mut self.at);
        if self.fold_case && (0x41..=0x5a).contains(&character) {
            character + 0x20
        } else {
            character
        }
    }

    /// Returns the next byte of the pattern without consuming it.
    fn peek(&self) -> u8 {
        self.text.get(self.at).copied().unwrap_or(0)
    }

    /// Appends one opcode, returning where it landed.
    fn append(&mut self, op: Op, arg: i64) -> Result<usize, &'static str> {
        self.insert(self.ops.len(), op, arg)
    }

    /// Inserts one opcode before an existing one, sliding the rest along.
    ///
    /// The jumps are relative, so the opcodes after the insertion point keep
    /// their meaning without being rewritten - which is what makes `X*` and
    /// `X?` expressible as an insertion in front of an already-compiled `X`.
    fn insert(&mut self, before: usize, op: Op, arg: i64) -> Result<usize, &'static str> {
        if self.ops.len() >= MAX_STATES {
            return Err("REGEXP pattern too big");
        }
        self.ops.insert(before, op);
        self.args.insert(before, arg);
        Ok(before)
    }

    /// Copies a run of opcodes onto the end, which is how `{m,n}` repeats.
    fn copy(&mut self, start: usize, count: usize) -> Result<(), &'static str> {
        if self.ops.len().saturating_add(count) >= MAX_STATES {
            return Err("REGEXP pattern too big");
        }
        let ops: Vec<Op> = self.ops.get(start..start + count).unwrap_or(&[]).to_vec();
        let args: Vec<i64> = self.args.get(start..start + count).unwrap_or(&[]).to_vec();
        self.ops.extend(ops);
        self.args.extend(args);
        Ok(())
    }

    /// Reads what a backslash introduces.
    ///
    /// `\uXXXX` and `\xXX` are code points; the punctuation escapes are
    /// themselves; the six control letters are their control characters; and
    /// anything else is the reference's `unknown \\ escape`.
    fn escape(&mut self) -> Result<u32, &'static str> {
        const ESCAPES: &[u8] = b"afnrtv\\()*.+?[$^{|}]-";
        const TRANSLATED: &[u8] = b"\x07\x0c\x0a\x0d\x09\x0b";
        let Some(letter) = self.text.get(self.at).copied() else {
            return Ok(0);
        };
        if letter == b'u' {
            if let Some(value) = self.hex(4) {
                self.at += 5;
                return Ok(value);
            }
        }
        if letter == b'x' {
            if let Some(value) = self.hex(2) {
                self.at += 3;
                return Ok(value);
            }
        }
        let Some(index) = ESCAPES.iter().position(|byte| *byte == letter) else {
            return Err("unknown \\ escape");
        };
        self.at += 1;
        Ok(u32::from(TRANSLATED.get(index).copied().unwrap_or(letter)))
    }

    /// Reads `count` hexadecimal digits after the escape letter.
    fn hex(&self, count: usize) -> Option<u32> {
        let mut value = 0u32;
        for offset in 1..=count {
            let digit = self.text.get(self.at + offset).copied()?;
            value = value * 16 + char::from(digit).to_digit(16)?;
        }
        Some(value)
    }

    /// Compiles alternatives, up to the first unmatched `)`.
    fn alternatives(&mut self) -> Result<(), &'static str> {
        let start = self.ops.len();
        self.sequence()?;
        while self.peek() == b'|' {
            let end = self.ops.len();
            let span = i64::try_from(end + 2 - start).unwrap_or(i64::MAX);
            self.insert(start, Op::Fork, span)?;
            let goto = self.append(Op::Goto, 0)?;
            self.at += 1;
            self.sequence()?;
            let jump = i64::try_from(self.ops.len() - goto).unwrap_or(i64::MAX);
            if let Some(slot) = self.args.get_mut(goto) {
                *slot = jump;
            }
        }
        Ok(())
    }

    /// Compiles one alternative: everything `|` binds looser than.
    fn sequence(&mut self) -> Result<(), &'static str> {
        let mut previous: Option<usize> = None;
        loop {
            let character = self.take();
            if character == EOF {
                return Ok(());
            }
            let start = self.ops.len();
            match character {
                c if c == u32::from(b'|') || c == u32::from(b')') => {
                    self.at -= 1;
                    return Ok(());
                }
                c if c == u32::from(b'(') => {
                    self.alternatives()?;
                    if self.peek() != b')' {
                        return Err("unmatched '('");
                    }
                    self.at += 1;
                }
                c if c == u32::from(b'.') => {
                    if self.peek() == b'*' {
                        self.append(Op::AnyStar, 0)?;
                        self.at += 1;
                    } else {
                        self.append(Op::Any, 0)?;
                    }
                }
                c if c == u32::from(b'*') => {
                    let Some(prev) = previous else {
                        return Err("'*' without operand");
                    };
                    let forward = i64::try_from(self.ops.len() - prev + 1).unwrap_or(i64::MAX);
                    self.insert(prev, Op::Goto, forward)?;
                    let back = i64::try_from(self.ops.len()).unwrap_or(i64::MAX);
                    let target = i64::try_from(prev).unwrap_or(0) - back + 1;
                    self.append(Op::Fork, target)?;
                }
                c if c == u32::from(b'+') => {
                    let Some(prev) = previous else {
                        return Err("'+' without operand");
                    };
                    let back = i64::try_from(self.ops.len()).unwrap_or(i64::MAX);
                    let target = i64::try_from(prev).unwrap_or(0) - back;
                    self.append(Op::Fork, target)?;
                }
                c if c == u32::from(b'?') => {
                    let Some(prev) = previous else {
                        return Err("'?' without operand");
                    };
                    let forward = i64::try_from(self.ops.len() - prev + 1).unwrap_or(i64::MAX);
                    self.insert(prev, Op::Fork, forward)?;
                }
                c if c == u32::from(b'$') => {
                    self.append(Op::Match, i64::from(EOF))?;
                }
                c if c == u32::from(b'^') => {
                    self.append(Op::AtStart, 0)?;
                }
                c if c == u32::from(b'{') => {
                    self.repeat(previous)?;
                }
                c if c == u32::from(b'[') => {
                    self.class()?;
                }
                c if c == u32::from(b'\\') => {
                    let special = match self.peek() {
                        b'b' => Some(Op::Boundary),
                        b'd' => Some(Op::Digit),
                        b'D' => Some(Op::NotDigit),
                        b's' => Some(Op::Space),
                        b'S' => Some(Op::NotSpace),
                        b'w' => Some(Op::Word),
                        b'W' => Some(Op::NotWord),
                        _ => None,
                    };
                    match special {
                        Some(op) => {
                            self.at += 1;
                            self.append(op, 0)?;
                        }
                        None => {
                            let value = self.escape()?;
                            self.append(Op::Match, i64::from(value))?;
                        }
                    }
                }
                other => {
                    self.append(Op::Match, i64::from(other))?;
                }
            }
            previous = Some(start);
        }
    }

    /// Compiles `{m,n}` by copying the operand the required number of times.
    fn repeat(&mut self, previous: Option<usize>) -> Result<(), &'static str> {
        let Some(mut prev) = previous else {
            return Err("'{m,n}' without operand");
        };
        let mut low = 0usize;
        while self.peek().is_ascii_digit() {
            low = low
                .saturating_mul(10)
                .saturating_add(usize::from(self.peek() - b'0'));
            if low * 2 > MAX_STATES {
                return Err("REGEXP pattern too big");
            }
            self.at += 1;
        }
        let mut high = low;
        if self.peek() == b',' {
            self.at += 1;
            high = 0;
            while self.peek().is_ascii_digit() {
                high = high
                    .saturating_mul(10)
                    .saturating_add(usize::from(self.peek() - b'0'));
                if high * 2 > MAX_STATES {
                    return Err("REGEXP pattern too big");
                }
                self.at += 1;
            }
        }
        if self.peek() != b'}' {
            return Err("unmatched '{'");
        }
        if high < low {
            return Err("n less than m in '{m,n}'");
        }
        self.at += 1;
        let size = self.ops.len() - prev;
        if low == 0 {
            if high == 0 {
                return Err("both m and n are zero in '{m,n}'");
            }
            let span = i64::try_from(size + 1).unwrap_or(i64::MAX);
            self.insert(prev, Op::Fork, span)?;
            prev += 1;
            high -= 1;
        } else {
            for _ in 1..low {
                self.copy(prev, size)?;
            }
        }
        for _ in low..high {
            let span = i64::try_from(size + 1).unwrap_or(i64::MAX);
            self.append(Op::Fork, span)?;
            self.copy(prev, size)?;
        }
        if high == 0 && low > 0 {
            let span = -i64::try_from(size).unwrap_or(0);
            self.append(Op::Fork, span)?;
        }
        Ok(())
    }

    /// Compiles a `[...]` character class.
    fn class(&mut self) -> Result<(), &'static str> {
        let first = self.ops.len();
        if self.peek() == b'^' {
            self.append(Op::ClassExclude, 0)?;
            self.at += 1;
        } else {
            self.append(Op::ClassInclude, 0)?;
        }
        loop {
            let mut character = self.take();
            if character == EOF {
                return Err("unclosed '['");
            }
            if character == u32::from(b'[') && self.peek() == b':' {
                return Err("POSIX character classes not supported");
            }
            if character == u32::from(b'\\') {
                character = self.escape()?;
            }
            if self.peek() == b'-' {
                self.append(Op::ClassRange, i64::from(character))?;
                self.at += 1;
                let mut upper = self.take();
                if upper == u32::from(b'\\') {
                    upper = self.escape()?;
                }
                self.append(Op::ClassRange, i64::from(upper))?;
            } else {
                self.append(Op::ClassValue, i64::from(character))?;
            }
            if self.peek() == b']' {
                self.at += 1;
                break;
            }
        }
        let length = i64::try_from(self.ops.len() - first).unwrap_or(i64::MAX);
        if let Some(slot) = self.args.get_mut(first) {
            *slot = length;
        }
        Ok(())
    }
}

impl Regexp {
    /// Compiles a pattern, or returns the reference's message for what is wrong.
    ///
    /// @param pattern - the pattern's bytes
    /// @param fold_case - whether comparisons ignore ASCII case
    pub fn compile(pattern: &[u8], fold_case: bool) -> Result<Regexp, &'static str> {
        let anchored = pattern.first() == Some(&b'^');
        let body = if anchored {
            pattern.get(1..).unwrap_or(&[])
        } else {
            pattern
        };
        let mut compiler = Compiler {
            text: body,
            at: 0,
            ops: Vec::new(),
            args: Vec::new(),
            fold_case,
        };
        if !anchored {
            compiler.append(Op::AnyStar, 0)?;
        }
        compiler.alternatives()?;
        if compiler.at < compiler.text.len() {
            return Err("unrecognized character");
        }
        compiler.append(Op::Accept, 0)?;
        let mut compiled = Regexp {
            ops: compiler.ops,
            args: compiler.args,
            init: Vec::new(),
            fold_case,
        };
        compiled.take_literal_prefix();
        Ok(compiled)
    }

    /// Records the literal bytes an unanchored pattern must start with.
    ///
    /// The reference's only optimisation, and it is worth keeping: without it
    /// `'...' REGEXP 'needle'` steps the machine once per starting position,
    /// and with it the search for the first byte is a memory scan.
    fn take_literal_prefix(&mut self) {
        if self.fold_case || self.ops.first() != Some(&Op::AnyStar) {
            return;
        }
        let mut prefix = Vec::new();
        for at in 1.. {
            if self.ops.get(at) != Some(&Op::Match) || prefix.len() >= 10 {
                break;
            }
            let Some(value) = self.args.get(at).and_then(|arg| u32::try_from(*arg).ok()) else {
                break;
            };
            // **`$` is a `Match` too, and its argument is zero.** It asserts the
            // end of the input rather than naming a byte, so taking it into the
            // literal prefix searches the subject for a NUL that is not there -
            // which made `'abc' REGEXP 'c$'` answer false.
            if value == EOF {
                break;
            }
            let Some(character) = char::from_u32(value) else {
                break;
            };
            let mut buffer = [0u8; 4];
            prefix.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        }
        self.init = prefix;
    }

    /// Returns whether the pattern matches anywhere in the subject.
    ///
    /// @param subject - the bytes to search
    pub fn matches(&self, subject: &[u8]) -> bool {
        let mut at = 0usize;
        let mut previous;
        let mut character = START;
        if !self.init.is_empty() {
            let width = self.init.len();
            while at + width <= subject.len()
                && subject.get(at..at + width) != Some(self.init.as_slice())
            {
                at += 1;
            }
            if at + width > subject.len() {
                return false;
            }
            character = START - 1;
        }
        let mut current: Vec<usize> = Vec::new();
        let mut next: Vec<usize> = vec![0];
        while character != EOF && !next.is_empty() {
            previous = character;
            character = self.read(subject, &mut at);
            std::mem::swap(&mut current, &mut next);
            next.clear();
            // `current` grows while it is walked: `Fork`, `Goto`, `AtStart` and
            // `Boundary` all add states that must be stepped against the *same*
            // character, which is the reference's `re_add_state(pThis, ...)`.
            let mut index = 0;
            while index < current.len() {
                let Some(state) = current.get(index).copied() else {
                    break;
                };
                index += 1;
                if self.step(state, character, previous, &mut current, &mut next) {
                    return true;
                }
            }
        }
        next.iter().any(|state| {
            let mut at = *state;
            while self.ops.get(at) == Some(&Op::Goto) {
                let Some(jump) = self.args.get(at).copied() else {
                    return false;
                };
                let Ok(moved) = usize::try_from(i64::try_from(at).unwrap_or(0) + jump) else {
                    return false;
                };
                at = moved;
            }
            self.ops.get(at) == Some(&Op::Accept)
        })
    }

    /// Reads one character of the subject, folding case when the pattern does.
    fn read(&self, subject: &[u8], at: &mut usize) -> u32 {
        let character = next_char(subject, at);
        if self.fold_case && (0x41..=0x5a).contains(&character) {
            character + 0x20
        } else {
            character
        }
    }

    /// Steps one state against one character, reporting whether it accepted.
    ///
    /// @param state - which opcode
    /// @param character - the character being consumed
    /// @param previous - the one before it, for `\b` and `^`
    /// @param current - states still to be stepped against this character
    /// @param next - states to step against the following one
    fn step(
        &self,
        state: usize,
        character: u32,
        previous: u32,
        current: &mut Vec<usize>,
        next: &mut Vec<usize>,
    ) -> bool {
        let Some(op) = self.ops.get(state).copied() else {
            return false;
        };
        let arg = self.args.get(state).copied().unwrap_or(0);
        let advance = |set: &mut Vec<usize>, to: usize| {
            if !set.contains(&to) {
                set.push(to);
            }
        };
        let jump = |offset: i64| -> Option<usize> {
            usize::try_from(i64::try_from(state).unwrap_or(0) + offset).ok()
        };
        match op {
            Op::Match => {
                if arg == i64::from(character) {
                    advance(next, state + 1);
                }
            }
            Op::AtStart => {
                if previous == START {
                    advance(current, state + 1);
                }
            }
            Op::Any => {
                if character != EOF {
                    advance(next, state + 1);
                }
            }
            Op::Word => {
                if is_word(character) {
                    advance(next, state + 1);
                }
            }
            Op::NotWord => {
                if !is_word(character) && character != EOF {
                    advance(next, state + 1);
                }
            }
            Op::Digit => {
                if is_digit(character) {
                    advance(next, state + 1);
                }
            }
            Op::NotDigit => {
                if !is_digit(character) && character != EOF {
                    advance(next, state + 1);
                }
            }
            Op::Space => {
                if is_space(character) {
                    advance(next, state + 1);
                }
            }
            Op::NotSpace => {
                if !is_space(character) && character != EOF {
                    advance(next, state + 1);
                }
            }
            Op::Boundary => {
                if is_word(character) != is_word(previous) {
                    advance(current, state + 1);
                }
            }
            Op::AnyStar => {
                advance(next, state);
                advance(current, state + 1);
            }
            Op::Fork => {
                if let Some(to) = jump(arg) {
                    advance(current, to);
                }
                advance(current, state + 1);
            }
            Op::Goto => {
                if let Some(to) = jump(arg) {
                    advance(current, to);
                }
            }
            Op::Accept => return true,
            Op::ClassInclude | Op::ClassExclude => {
                if op == Op::ClassExclude && character == EOF {
                    return false;
                }
                let length = usize::try_from(arg).unwrap_or(0);
                let mut hit = false;
                let mut offset = 1usize;
                while offset < length {
                    match self.ops.get(state + offset) {
                        Some(Op::ClassValue) => {
                            if self.args.get(state + offset) == Some(&i64::from(character)) {
                                hit = true;
                                break;
                            }
                            offset += 1;
                        }
                        _ => {
                            let low = self.args.get(state + offset).copied().unwrap_or(0);
                            let high = self.args.get(state + offset + 1).copied().unwrap_or(0);
                            if low <= i64::from(character) && high >= i64::from(character) {
                                hit = true;
                                break;
                            }
                            offset += 2;
                        }
                    }
                }
                if op == Op::ClassExclude {
                    hit = !hit;
                }
                if hit {
                    advance(next, state + length);
                }
            }
            Op::ClassValue | Op::ClassRange => {}
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a pattern against a subject the way `X REGEXP Y` does.
    fn matches(pattern: &str, subject: &str) -> bool {
        Regexp::compile(pattern.as_bytes(), false)
            .expect("the pattern compiles")
            .matches(subject.as_bytes())
    }

    /// An unanchored pattern searches, which is the whole shape of `REGEXP`.
    #[test]
    fn a_pattern_matches_anywhere_unless_it_is_anchored() {
        assert!(matches("b", "abc"));
        assert!(matches("^a", "abc"));
        assert!(!matches("^b", "abc"));
        assert!(matches("c$", "abc"));
        assert!(!matches("b$", "abc"));
    }

    /// The quantifiers, including the counted form that compiles by copying.
    #[test]
    fn the_quantifiers_count_what_they_say() {
        assert!(matches("ab*c", "ac"));
        assert!(matches("ab*c", "abbbc"));
        assert!(!matches("^ab+c$", "ac"));
        assert!(matches("^ab?c$", "ac"));
        assert!(matches("^a{3}$", "aaa"));
        assert!(!matches("^a{3}$", "aa"));
        assert!(matches("^a{2,4}$", "aaa"));
        assert!(!matches("^a{2,4}$", "aaaaa"));
    }

    /// Classes, ranges and negation.
    #[test]
    fn a_character_class_reads_its_ranges() {
        assert!(matches("^[a-c]+$", "abc"));
        assert!(!matches("^[a-c]+$", "abd"));
        assert!(matches("^[^0-9]+$", "abc"));
        assert!(!matches("^[^0-9]+$", "ab1"));
    }

    /// The Perl shorthands, and the boundary assertion that consumes nothing.
    #[test]
    fn the_perl_shorthands_answer_for_their_sets() {
        assert!(matches(r"^\d+$", "123"));
        assert!(matches(r"^\w+$", "a_1"));
        assert!(matches(r"\bcat\b", "a cat sat"));
        assert!(!matches(r"\bcat\b", "concatenate"));
    }

    /// Alternation, including inside a group.
    #[test]
    fn alternation_binds_looser_than_a_sequence() {
        assert!(matches("^(cat|dog)$", "dog"));
        assert!(!matches("^(cat|dog)$", "cow"));
        assert!(matches("^a(b|c)d$", "acd"));
    }

    /// The dialect is small on purpose, and refuses what it does not have in
    /// the reference's own words rather than quietly accepting it.
    #[test]
    fn the_refusals_are_the_references_own_words() {
        assert_eq!(
            Regexp::compile(b"[[:alpha:]]", false).err(),
            Some("POSIX character classes not supported")
        );
        assert_eq!(
            Regexp::compile(b"*a", false).err(),
            Some("'*' without operand")
        );
        assert_eq!(Regexp::compile(b"(a", false).err(), Some("unmatched '('"));
        assert_eq!(Regexp::compile(b"[a", false).err(), Some("unclosed '['"));
        assert_eq!(
            Regexp::compile(br"\q", false).err(),
            Some("unknown \\ escape")
        );
        assert_eq!(
            Regexp::compile(b"a{3,2}", false).err(),
            Some("n less than m in '{m,n}'")
        );
    }

    /// A malformed UTF-8 subject answers rather than failing the statement.
    #[test]
    fn invalid_utf8_in_the_subject_is_a_replacement_character() {
        let compiled = Regexp::compile(b"a", false).expect("compiles");
        assert!(compiled.matches(&[0xff, b'a']));
    }
}
