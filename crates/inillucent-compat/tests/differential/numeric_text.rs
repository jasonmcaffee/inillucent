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
//! `printf('%s', <real>)`, the default rendering, which is what a `SELECT`
//! prints, and `printf('%.20g', <real>)` (task-2066 section 4.4.14). A second
//! test grades the `!`, `#` and `,` flags on the real conversions (task-2080).
//! Everything goes through the shell, because the shell is what turns a value
//! into characters.
//!
//! The population is twenty-four named doubles and a thousand generated ones.
//! The named ones are the places a renderer goes wrong: negative zero, the
//! smallest subnormal, the boundary between the subnormals and the normals,
//! the largest finite double, `2^53` and `2^53 + 1`, and the run from fifteen
//! to seventeen significant digits where the shortest representation that
//! reads back as the same double changes length.
//!
//! ## Why the answer is exact agreement, with no allowance
//!
//! Until task-2080 this file allowed seven statements to differ in their last
//! digit. They were random doubles with large exponents, such as
//! `1.1304293785495057e251`, which SQLite prints with a final `7` and this
//! engine printed with a final `8`. Neither engine prints the shortest
//! representation for those values, and SQLite does not print the correctly
//! rounded seventeenth digit either: `sqlite3FpDecode` multiplies the binary
//! significand by an approximation of a power of ten, so its last digit is
//! sometimes the nearer one and sometimes not. This engine used Rust's
//! correctly rounded conversion and then rounded a second time.
//!
//! That was closed by rendering through a transcription of `sqlite3FpDecode`,
//! `inillucent_value::fpdecode`, so the digits are SQLite's by construction. A
//! last digit that differs now is a defect in that transcription, and the test
//! fails on it rather than counting it.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use inillucent_compat::workspace_root;

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

/// The conversions the ordinary test runs on every value, with `{}` for it.
///
/// `%s`, the default rendering, and `%.20g`, which is the widest ordinary way
/// to ask for a number and the one that exposes a difference in how many
/// digits an engine believes it has.
const ORDINARY: &[&str] = &["printf('%s', {})", "{}", "printf('%.20g', {})"];

/// The conversions the flag test runs on every value, with `{}` for it.
///
/// The `!` flag with a precision stops at the digits SQLite's decoder
/// produced, which is eighteen for pi and nineteen for `0.1`, and removes the
/// trailing zeros. Without a precision it still removes them, so `%!e` of
/// `0.1` is `1.0e-01`. `%!.17g` is the one precision at which SQLite tries a
/// shorter string. `#` keeps the point and drops the sign of a negative value
/// that displays as zero, and `,` groups a `%g` that chose the fixed form.
const FLAGGED: &[&str] = &[
    "printf('%!.20g', {})",
    "printf('%!.25e', {})",
    "printf('%!.25f', {})",
    "printf('%!e', {})",
    "printf('%!f', {})",
    "printf('%!g', {})",
    "printf('%!.17g', {})",
    "printf('%!.0e', {})",
    "printf('%#g', {})",
    "printf('%#.0f', {})",
    "printf('%,.10g', {})",
    "printf('%.20e', {})",
];

/// A fixed seed, so a failure names a population somebody else can rebuild.
const SEED: u64 = 20_662_014;

/// Returns the pinned reference shell, when it has been built.
fn reference() -> Option<PathBuf> {
    let directory = workspace_root().join(".sqlite-ref/3.53.4/shell");
    let path = directory.join(format!("sqlite3{}", std::env::consts::EXE_SUFFIX));
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

/// Returns the named doubles followed by `count` generated ones.
///
/// @param count - how many generated doubles to add
fn population(count: usize) -> Vec<String> {
    let mut values: Vec<String> = NAMED.iter().map(|text| (*text).to_string()).collect();
    values.extend(generated(SEED, count));
    values
}

/// The script both shells are fed.
///
/// @param values - the doubles, as the text that produces them
/// @param conversions - the expressions to run on each, with `{}` for the value
fn script(values: &[String], conversions: &[&str]) -> String {
    let mut lines = vec![".mode list".to_string(), ".headers off".to_string()];
    for value in values {
        for conversion in conversions {
            lines.push(format!("SELECT {};", conversion.replace("{}", value)));
        }
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
    // **The script is written from its own thread.** Writing all of it before
    // reading any output deadlocks once the output is larger than the pipe
    // buffer: the shell blocks writing its stdout while this blocks writing its
    // stdin, neither uses any CPU, and nothing times out. At 672 statements the
    // output fit and it never showed; at 7,000 it hung the run for 33 minutes
    // (task-2080).
    let writer = child.stdin.take().map(|mut stdin| {
        let script = text.to_string();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(script.as_bytes());
        })
    });
    let Ok(output) = child.wait_with_output() else {
        return Vec::new();
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let mut printed = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    printed.push_str(&String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"));
    printed.lines().map(str::to_string).collect()
}

/// Runs one script through both shells and returns every statement whose
/// output differs, with both outputs.
///
/// Checks first that each shell printed one line per statement, because a
/// script that stopped early would otherwise compare as a short list of
/// agreements.
///
/// @param shells - the pinned shell and this repository's shell
/// @param area - the name of a scratch directory for this comparison
/// @param text - the script
fn disagreements(shells: &(PathBuf, PathBuf), area: &str, text: &str) -> Vec<String> {
    let area = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(area);
    let theirs = run(&shells.0, &area.join("sqlite"), text);
    let mine = run(&shells.1, &area.join("inillucent"), text);
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
    let mut wrong = Vec::new();
    for (at, statement) in statements.iter().enumerate() {
        let left = theirs.get(at).map(String::as_str).unwrap_or("");
        let right = mine.get(at).map(String::as_str).unwrap_or("");
        if left != right {
            wrong.push(format!(
                "{statement}\n    sqlite     : {left}\n    inillucent : {right}"
            ));
        }
    }
    wrong
}

/// Returns both shells, or `None` after reporting that the pinned SQLite shell
/// is missing.
///
/// Only the reference can be missing. This repository's shell is built from
/// the workspace, and a build that fails panics with cargo's output rather
/// than being reported here as a missing shell (task-2106).
fn shells() -> Option<(PathBuf, PathBuf)> {
    let Some(reference) = reference() else {
        inillucent_compat::differential::skipping("the pinned SQLite shell is not built");
        return None;
    };
    Some((
        reference,
        inillucent_compat::cliproc::program("inillucent-shell"),
    ))
}

/// **`printf('%s', <real>)`, the default rendering and `%.20g` agree with the
/// shell on every value, to the last digit.**
///
/// This allowed seven last digit differences until task-2080. The module
/// comment says why the allowance is gone rather than lowered.
#[test]
fn the_text_a_real_is_printed_as_agrees_with_the_pinned_shell() {
    let Some(shells) = shells() else {
        return;
    };
    let text = script(&population(1000), ORDINARY);
    let statements = text
        .lines()
        .filter(|line| line.starts_with("SELECT"))
        .count();
    let wrong = disagreements(&shells, "numeric-text", &text);
    assert!(
        wrong.is_empty(),
        "seed {SEED}: {} of {statements} statements print differently:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// **The `!`, `#` and `,` flags on the real conversions agree with the shell.**
///
/// Before task-2080 `printf('%!.25f', 0.1)` was `0.1000000000000000055500000`
/// here and `0.1000000000000000056` in SQLite, and `printf('%!e', 0.1)` was
/// `1.000000e-01` against `1.0e-01`: the flag was read for the digit count and
/// ignored for the trailing zeros. `printf('%,.10g', 1234567.0)` was
/// `1234567` against `1,234,567`.
#[test]
fn the_real_conversion_flags_agree_with_the_pinned_shell() {
    let Some(shells) = shells() else {
        return;
    };
    let mut values = population(300);
    // Values whose digits `%!.17g` shortens, values that `%#.0f` shows as a
    // zero with no sign, and one that `%,.10g` groups.
    values.extend(
        [
            "49.47",
            "0.3",
            "1e23",
            "-0.1",
            "-0.0004",
            "1234567.0",
            "0.5",
            "2.5",
        ]
        .iter()
        .map(|text| (*text).to_string()),
    );
    let text = script(&values, FLAGGED);
    let statements = text
        .lines()
        .filter(|line| line.starts_with("SELECT"))
        .count();
    let wrong = disagreements(&shells, "numeric-text-flags", &text);
    assert!(
        wrong.is_empty(),
        "seed {SEED}: {} of {statements} statements print differently:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// **`run` does not deadlock on output larger than the pipe buffer.**
///
/// `run` used to write the whole script to the shell's stdin before it read
/// any stdout. Once the output outgrew the pipe buffer, the shell blocked
/// writing and the test blocked writing too, and neither used any CPU, so the
/// run hung instead of failing (task-2080). This feeds the pinned shell a
/// script whose output is a few hundred kilobytes, far past any pipe buffer,
/// and fails if `run` has not come back within a minute. Without the thread
/// that writes stdin, this test fails.
#[test]
fn a_script_larger_than_the_pipe_buffer_does_not_hang() {
    let Some(reference) = reference() else {
        inillucent_compat::differential::skipping("the reference shell is missing");
        return;
    };
    const STATEMENTS: usize = 50_000;
    let mut lines = vec![".mode list".to_string()];
    lines.extend((0..STATEMENTS).map(|at| format!("SELECT {at};")));
    lines.push(".quit".to_string());
    let text = lines.join("\n") + "\n";
    let area = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("numeric-text-pipe");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(run(&reference, &area, &text));
    });
    let printed = receiver
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("run did not return within a minute, so it deadlocked on a full pipe");
    assert_eq!(printed.len(), STATEMENTS);
    assert_eq!(printed.last().map(String::as_str), Some("49999"));
}
