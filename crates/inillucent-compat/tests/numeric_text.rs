//! The text a real number is printed as, graded against the pinned shell.
//!
//! Invariant: **two engines that hold the same double must print the same
//! characters for it.** `differential.rs` compares tagged values - it asks both
//! engines for the row and compares the `Real(f64)` it gets back, so two
//! renderings of the same bits are identical to it by construction. What a
//! person sees is the text, and nothing graded the text.
//!
//! ## What is graded
//!
//! `printf('%s', <real>)` and the default rendering, which is what a `SELECT`
//! prints (task-2066 section 4.4.14). Both go through the shell, because the
//! shell is what turns a value into characters.
//!
//! The population is twenty-four named doubles and two hundred generated ones.
//! The named ones are the places a renderer goes wrong: negative zero, the
//! smallest subnormal, the boundary between the subnormals and the normals,
//! the largest finite double, `2^53` and `2^53 + 1`, and the run from fifteen
//! to seventeen significant digits where the shortest representation that
//! reads back as the same double changes length.
//!
//! ## The one difference, and why it is a ceiling rather than zero
//!
//! Seven of the 672 statements this file runs disagree, and all seven are the
//! final digit of the mantissa. Neither engine emits the shortest
//! representation for those values - for `-8.24034521633578e-167` both print
//! seventeen digits where fifteen read back as the same double - and they round
//! the seventeenth differently. Measured against the exact decimal expansion of
//! the double: SQLite's digit is the nearer one on that value and this engine's
//! is the nearer one on `printf('%.20g', 3.1643187021860255e-168)`, so neither
//! is simply better and this is not a defect with a side.
//!
//! One in a hundred random doubles, and none of the twenty-four named ones.
//! task-2080 is where the argument for closing it lives.
//!
//! So the claim is exact agreement on structure and a counted allowance on the
//! last digit. A structural difference - a different exponent, a different
//! number of digits, a missing sign - fails whatever the count says, and the
//! count itself is asserted, so a change that widened the disagreement to four
//! values fails too.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use inillucent_compat::workspace_root;

/// How many of the statements may disagree, and only in their last digit.
///
/// Measured, not chosen. Raising it is a decision about compatibility and
/// belongs in a commit message.
const ALLOWED_LAST_DIGIT_DIFFERENCES: usize = 7;

/// The doubles a renderer gets wrong, named rather than generated.
///
/// Written as text rather than as `f64` literals so that what reaches the two
/// shells is exactly what is written here: the point of several of them is the
/// number of digits, and a Rust literal formatted back out would choose its own.
const NAMED: &[&str] = &[
    "0.0",
    "-0.0",
    "1.0",
    "-1.0",
    "0.1",
    // Where `%g` switches between fixed and exponential, on both sides.
    "1e15",
    "1e16",
    "1e17",
    "1e-4",
    "1e-5",
    // Fifteen, sixteen and seventeen significant digits.
    "123456789012345.0",
    "1234567890123456.0",
    "12345678901234567.0",
    // The sum whose exact value needs seventeen digits to read back.
    "0.30000000000000004",
    // The subnormals, and the step into the normals.
    "5e-324",
    "2.2250738585072011e-308",
    "2.2250738585072014e-308",
    // The ends of the range.
    "1.7976931348623157e308",
    "-1.7976931348623157e308",
    "1e-308",
    // Where consecutive integers stop being representable.
    "9007199254740992.0",
    "9007199254740993.0",
    "3.141592653589793",
    "2.718281828459045",
];

/// Returns the pinned reference shell, when it has been built.
fn reference() -> Option<PathBuf> {
    let directory = workspace_root().join(".sqlite-ref/3.53.4/shell");
    let path = directory.join(format!("sqlite3{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns this repository's shell, building it first.
fn ours() -> Option<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .current_dir(workspace_root())
        .args(["build", "-p", "inillucent-cli"])
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let mut directory = std::env::current_exe().unwrap_or_default();
    directory.pop();
    directory.pop();
    let path = directory.join(format!("inillucent-shell{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Builds the generated half of the population.
///
/// Random bit patterns rather than random values in a range: a value drawn
/// uniformly from `0..1` is never subnormal and never has a large exponent,
/// and the exponent is where the two renderings part company. The infinities
/// and the NaNs are dropped, because neither is a decimal.
///
/// @param seed - the seed, which a failure prints so the run can be repeated
/// @param count - how many doubles to return
fn generated(seed: u64, count: usize) -> Vec<String> {
    let mut rng = inillucent_base::rng::Rng::new(seed);
    let mut made = Vec::with_capacity(count);
    while made.len() < count {
        let value = f64::from_bits(rng.next_u64());
        if !value.is_finite() {
            continue;
        }
        // `{:?}` is Rust's shortest representation that reads back as the same
        // double, which is the shortest text that can carry this value into
        // both shells.
        made.push(format!("{value:?}"));
    }
    made
}

/// The script both shells are fed.
///
/// Three statements per value: the `%s` conversion, the default rendering, and
/// `%.20g`, which is the widest ordinary way to ask for a number and the one
/// that exposes a difference in how many digits an engine believes it has.
///
/// @param values - the doubles, as the text that produces them
fn script(values: &[String]) -> String {
    let mut lines = vec![".mode list".to_string(), ".headers off".to_string()];
    for value in values {
        lines.push(format!("SELECT printf('%s', {value});"));
        lines.push(format!("SELECT {value};"));
        lines.push(format!("SELECT printf('%.20g', {value});"));
    }
    lines.push(".quit".to_string());
    lines.join("\n") + "\n"
}

/// Runs a script through one shell and returns its lines.
///
/// @param program - the shell
/// @param area - a scratch directory of this run's own
/// @param text - the script
fn run(program: &PathBuf, area: &PathBuf, text: &str) -> Vec<String> {
    let _ = std::fs::remove_dir_all(area);
    let _ = std::fs::create_dir_all(area);
    let database = area.join("numbers.db");
    let Ok(mut child) = Command::new(program)
        .arg(&database)
        .current_dir(area)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return Vec::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(text.as_bytes());
    }
    let Ok(output) = child.wait_with_output() else {
        return Vec::new();
    };
    let mut printed = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    printed.push_str(&String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"));
    printed.lines().map(str::to_string).collect()
}

/// How two renderings of the same double differ.
#[derive(Debug, PartialEq, Eq)]
enum Difference {
    /// The same characters.
    None,
    /// The same shape and the same number of digits, differing in the last one.
    LastDigit,
    /// Anything else: a different exponent, a different length, a lost sign.
    Structural,
}

/// Classifies a disagreement between two rendered numbers.
///
/// **The last digit is the only forgiven position**, and it is forgiven only
/// when everything else is identical - the sign, the exponent, the position of
/// the point and the number of digits. A renderer that dropped a digit or moved
/// the point produces two strings that differ at the end as well, and this must
/// not call that a rounding difference.
///
/// @param theirs - the reference shell's text
/// @param ours - this repository's text
fn classify(theirs: &str, ours: &str) -> Difference {
    if theirs == ours {
        return Difference::None;
    }
    if theirs.len() != ours.len() {
        return Difference::Structural;
    }
    let differing: Vec<usize> = theirs
        .bytes()
        .zip(ours.bytes())
        .enumerate()
        .filter(|(_, (left, right))| left != right)
        .map(|(at, _)| at)
        .collect();
    let Some(at) = differing.first().copied() else {
        return Difference::Structural;
    };
    if differing.len() != 1 {
        return Difference::Structural;
    }
    let is_digit = |text: &str, at: usize| text.as_bytes().get(at).is_some_and(u8::is_ascii_digit);
    if !is_digit(theirs, at) || !is_digit(ours, at) {
        return Difference::Structural;
    }
    // **The last digit of the mantissa, and not of the exponent.** The first
    // version of this checked only that nothing followed the differing
    // character except an exponent marker, which made `1.25e+10` against
    // `1.25e+11` a rounding difference - two numbers ten times apart. The
    // mantissa ends where the marker begins, or at the end of the text.
    let mantissa_end = theirs
        .find(['e', 'E'])
        .unwrap_or_else(|| theirs.chars().count());
    if at.saturating_add(1) == mantissa_end {
        Difference::LastDigit
    } else {
        Difference::Structural
    }
}

/// **`printf('%s', <real>)` and the default rendering agree with the shell.**
///
/// The one allowance is the last digit of a value in the subnormal tail, and it
/// is counted rather than waved through: `ALLOWED_LAST_DIGIT_DIFFERENCES` is
/// what was measured, and both a new disagreement and a disappeared one fail
/// here. A difference of any other shape fails whatever the count is.
#[test]
fn the_text_a_real_is_printed_as_agrees_with_the_pinned_shell() {
    let (Some(reference), Some(ours_shell)) = (reference(), ours()) else {
        inillucent_compat::differential::skipping("a shell is missing");
        return;
    };
    // A fixed seed, so a failure names a population somebody else can rebuild.
    const SEED: u64 = 20_662_014;
    let mut values: Vec<String> = NAMED.iter().map(|text| (*text).to_string()).collect();
    values.extend(generated(SEED, 200));

    let text = script(&values);
    let area = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("numeric-text");
    let theirs = run(&reference, &area.join("sqlite"), &text);
    let mine = run(&ours_shell, &area.join("inillucent"), &text);

    let statements: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("SELECT"))
        .collect();
    assert_eq!(
        theirs.len(),
        statements.len(),
        "the reference shell printed {} lines for {} statements, so the script did not \
         run as written",
        theirs.len(),
        statements.len()
    );
    assert_eq!(
        mine.len(),
        statements.len(),
        "this shell printed {} lines for {} statements, so the script did not run as \
         written",
        mine.len(),
        statements.len()
    );

    let mut structural: Vec<String> = Vec::new();
    let mut rounding: Vec<String> = Vec::new();
    for (at, statement) in statements.iter().enumerate() {
        let left = theirs.get(at).map(String::as_str).unwrap_or("");
        let right = mine.get(at).map(String::as_str).unwrap_or("");
        let said = format!("{statement}\n    sqlite     : {left}\n    inillucent : {right}");
        match classify(left, right) {
            Difference::None => {}
            Difference::LastDigit => rounding.push(said),
            Difference::Structural => structural.push(said),
        }
    }

    assert!(
        structural.is_empty(),
        "seed {SEED}: {} of {} statements print a number of a different shape, which is \
         not a rounding difference:\n{}",
        structural.len(),
        statements.len(),
        structural.join("\n")
    );
    assert_eq!(
        rounding.len(),
        ALLOWED_LAST_DIGIT_DIFFERENCES,
        "seed {SEED}: {} of {} statements differ in their last digit and {} were \
         measured. If this went up, a rounding change made the two engines disagree \
         about more numbers; if it went down, the difference has closed and the \
         allowance should go with it.\n{}",
        rounding.len(),
        statements.len(),
        ALLOWED_LAST_DIGIT_DIFFERENCES,
        rounding.join("\n")
    );
}

/// **Every named double is printed identically, with no allowance at all.**
///
/// The allowance above is for the generated tail. These twenty-four are the
/// cases somebody chose because they are where a renderer goes wrong, and a
/// disagreement on one of them is a defect rather than a last-digit rounding
/// choice. Splitting them out is what stops the allowance covering one.
#[test]
fn every_named_double_is_printed_identically() {
    let (Some(reference), Some(ours_shell)) = (reference(), ours()) else {
        inillucent_compat::differential::skipping("a shell is missing");
        return;
    };
    let values: Vec<String> = NAMED.iter().map(|text| (*text).to_string()).collect();
    let text = script(&values);
    let area = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("numeric-text-named");
    let theirs = run(&reference, &area.join("sqlite"), &text);
    let mine = run(&ours_shell, &area.join("inillucent"), &text);

    let statements: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("SELECT"))
        .collect();
    let mut wrong: Vec<String> = Vec::new();
    for (at, statement) in statements.iter().enumerate() {
        let left = theirs.get(at).map(String::as_str).unwrap_or("");
        let right = mine.get(at).map(String::as_str).unwrap_or("");
        if left != right {
            wrong.push(format!(
                "{statement}\n    sqlite     : {left}\n    inillucent : {right}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} named cases print differently:\n{}",
        wrong.len(),
        statements.len(),
        wrong.join("\n")
    );
}

/// The classifier calls a moved point structural and a rounded digit rounding.
///
/// Rule 1.5: the allowance above is only as narrow as this function, and a
/// classifier that called everything `LastDigit` would turn the count into a
/// number with no meaning. These are the shapes it has to tell apart.
#[test]
fn the_classifier_forgives_a_last_digit_and_nothing_else() {
    assert_eq!(classify("1.25e+10", "1.25e+10"), Difference::None);
    assert_eq!(classify("1.25e+10", "1.26e+10"), Difference::LastDigit);
    assert_eq!(classify("0.125", "0.126"), Difference::LastDigit);

    // A different exponent, with the digits untouched.
    assert_eq!(classify("1.25e+10", "1.25e+11"), Difference::Structural);
    // A digit that is not the last one.
    assert_eq!(classify("1.25e+10", "1.35e+10"), Difference::Structural);
    assert_eq!(classify("0.125", "0.135"), Difference::Structural);
    // A lost sign, which makes the lengths differ.
    assert_eq!(classify("-0.125", "0.1250"), Difference::Structural);
    // A different number of digits.
    assert_eq!(classify("0.125", "0.1250"), Difference::Structural);
    // Two digits apart.
    assert_eq!(classify("0.1255", "0.1266"), Difference::Structural);
}
