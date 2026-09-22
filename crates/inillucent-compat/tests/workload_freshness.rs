//! Two checked-in files that go stale, and the checks that notice.
//!
//! Invariant: **a copy goes stale, so something has to notice.** Two copies
//! live in this repository and both are graded here.
//! `tests/workloads/nikaya/statements.sql` is a copy of a consumer's
//! statements: Nikaya adds one, nobody re-runs the extractor, and the corpus
//! this repository grades itself against is the corpus Nikaya had in September
//! - which reads as coverage of a consumer and is coverage of a consumer's
//! past. `tests/nightly-history.tsv` is a copy of when the nightly tier last
//! ran: the scheduled task is removed, or the machine it ran on is rebuilt, and
//! the tier nobody runs by hand stops running with nothing saying so.
//!
//! ## Why it runs the extractor rather than re-implementing it
//!
//! There is one extractor, `tools/extract-nikaya-workload.py`, and this asks it
//! whether the checked-in file is what it would write. Writing a second
//! extractor in Rust would be two implementations of "what counts as a
//! statement literal" that agree on the day they are written.
//!
//! ## Loud here, silent on a clone
//!
//! Nikaya is not part of this repository and most machines that build it do not
//! have it. So the extractor answers three ways - the file matches, the file is
//! stale, the checkout is not here - and only the middle one is a failure. The
//! third prints `; skipping`, which is what `--strict` counts, so a machine
//! that cannot check this says so rather than reporting a pass.

use std::path::PathBuf;
use std::process::Command;

use inillucent_compat::workspace_root;

/// Where Nikaya is, unless `NIKAYA_ROOT` says otherwise.
///
/// The extractor holds the same default, and reads the same variable; this is
/// here so the skip message can name the path it looked at.
fn nikaya_root() -> PathBuf {
    match std::env::var("NIKAYA_ROOT") {
        Ok(path) => PathBuf::from(path),
        Err(_) => PathBuf::from("C:/jason/dev/nikaya/server"),
    }
}

/// The interpreter to run the extractor with, if one is on the path.
fn python() -> Option<&'static str> {
    for name in ["python", "python3"] {
        if Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
        {
            return Some(name);
        }
    }
    None
}

/// The workload file holds what Nikaya's source holds.
///
/// **Remove a statement from `statements.sql` and this fails**, which is rule
/// 1.5: the guard is checked by taking away the thing it guards. The check was
/// run both ways on the machine this was written on.
#[test]
fn the_workload_matches_the_consumers_source() {
    let Some(python) = python() else {
        inillucent_compat::differential::skipping(
            "no python on the path, so the Nikaya workload cannot be re-extracted",
        );
        return;
    };
    let root = nikaya_root();
    if !root.join("src").is_dir() {
        inillucent_compat::differential::skipping(&format!(
            "the Nikaya checkout is not at {}, so the workload cannot be compared to it",
            root.display()
        ));
        return;
    }

    let script = workspace_root().join("tools/extract-nikaya-workload.py");
    let ran = Command::new(python)
        .arg(&script)
        .arg("--check")
        .arg("--nikaya")
        .arg(&root)
        .output()
        .unwrap_or_else(|why| panic!("{} did not run: {why}", script.display()));
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );

    match ran.status.code() {
        Some(0) => {}
        // The extractor decided the checkout was not usable after all - a
        // partial clone, a directory that is there and empty. Its own word for
        // that is 2, and it is a skip rather than a failure for the same reason
        // the directory check above is.
        Some(2) => inillucent_compat::differential::skipping(&format!(
            "the extractor could not read {}: {said}",
            root.display()
        )),
        _ => panic!(
            "tests/workloads/nikaya/statements.sql is not what Nikaya's source says it should \
             be. The corpus this repository grades a consumer's statements against is a copy \
             of an older Nikaya, which reads as coverage of the consumer and is coverage of \
             the consumer's past:\n{said}"
        ),
    }
}

/// Seconds since the epoch for an ISO stamp of the form `2026-09-21T06:20:51Z`.
///
/// **Written out rather than taken from a crate.** A date library is not
/// infrastructure under `docs/dependency-policy.md` and would be a new
/// `[[external]]` row for eleven lines of arithmetic. The algorithm is the
/// civil-days one: shift March to the front of the year so the leap day is the
/// last day of it, then count eras of four hundred years, which have a fixed
/// 146,097 days each.
///
/// @param stamp - the timestamp text from a ledger row
fn epoch_seconds(stamp: &str) -> Option<i64> {
    let digits = |from: usize, to: usize| stamp.get(from..to)?.parse::<i64>().ok();
    let (year, month, day) = (digits(0, 4)?, digits(5, 7)?, digits(8, 10)?);
    let (hour, minute, second) = (digits(11, 13)?, digits(14, 16)?, digits(17, 19)?);

    let shifted = if month <= 2 { year - 1 } else { year };
    let era = if shifted >= 0 { shifted } else { shifted - 399 } / 400;
    let year_of_era = shifted - era * 400;
    let month_of_year = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_of_year + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;

    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// The newest stamp in `tests/nightly-history.tsv`, and the row it came from.
///
/// Comment lines and the header are skipped; a row whose stamp will not parse
/// is skipped too, because a ledger with one unreadable row is still evidence
/// about the rows that are readable.
///
/// @param ledger - the whole file, as it is on disk
fn newest_run(ledger: &str) -> Option<(i64, String)> {
    ledger
        .lines()
        .filter(|line| !line.starts_with('#') && !line.starts_with("stamp"))
        .filter_map(|line| {
            let stamp = line.split('\t').next()?;
            Some((epoch_seconds(stamp)?, line.to_string()))
        })
        .max_by_key(|(seconds, _)| *seconds)
}

/// **The nightly tier has run in the last seven days.**
///
/// The nightly tier is the one nobody runs by hand: it holds
/// `inillucent::story_ledger_day_nightly`, which takes half an hour, and the
/// large-table story, and it is reached by a scheduled task rather than by the
/// gate anybody runs before a commit. So it is the tier that can stop running
/// without anybody finding out, and "stopped running" and "has nothing to
/// report" look identical from outside.
///
/// `tests/nightly-history.tsv` is what tells them apart, and it only tells them
/// apart while it is being appended to. This reads its newest row.
///
/// **Seven days rather than one, because the scheduled task can miss a night
/// for reasons that are not a defect** - the machine was off, a release was
/// building, the box was given to another agent. Seven consecutive misses is
/// not one of those.
///
/// **This case is a time bomb on purpose**, which is the one thing about it to
/// understand before changing it. It goes red seven days after the last
/// recorded run whether or not anybody touched the code, and that is what it is
/// for: a freshness check that only fails when somebody edits something is a
/// freshness check that never fires. The failure names the one command that
/// clears it.
///
/// Rule 1.5: the guard was checked by taking away the thing it guards. The
/// newest stamp in the ledger was moved back a month on the machine this was
/// written on, the case failed with `the nightly tier last ran 32 days ago`,
/// and the ledger was put back.
#[test]
fn the_nightly_tier_has_run_in_the_last_seven_days() {
    let path = workspace_root().join("tests/nightly-history.tsv");
    let ledger = std::fs::read_to_string(&path)
        .unwrap_or_else(|why| panic!("{} could not be read: {why}", path.display()));

    let Some((newest, row)) = newest_run(&ledger) else {
        panic!(
            "{} holds no run at all. It is checked in with the rows from the commit that \
             created `tools/run-nightly.ps1`, so an empty ledger means those rows were \
             deleted rather than that the nightly has never run.",
            path.display()
        );
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0);
    let days = (now - newest) / 86_400;
    assert!(
        days <= 7,
        "the nightly tier last ran {days} days ago. The tier that holds the half-hour \
         story tests is reached by a scheduled task and by nothing else, so a ledger \
         that has stopped growing is a tier that has stopped running - and a tier that \
         has stopped running reports nothing, which reads the same as a tier with \
         nothing to report.\n\
         \n\
         Run `pwsh tools/run-nightly.ps1` to run it now and append a row, or \
         `pwsh tools/run-nightly.ps1 -Register` to put the scheduled task back.\n\
         \n\
         The newest row is:\n{row}"
    );
}

/// The stamp parser agrees with dates whose answers are known.
///
/// Rule 1.5: the guard above is only as good as the arithmetic under it, and
/// that arithmetic is eleven lines written here rather than taken from a
/// library. Three of these four are the cases a hand-written civil-days
/// calculation gets wrong - the leap day, the day after it, and a century year
/// that is a leap year because it divides by four hundred.
#[test]
fn the_stamp_parser_answers_the_dates_whose_answers_are_known() {
    let known = [
        ("1970-01-01T00:00:00Z", 0_i64),
        ("2000-02-29T00:00:00Z", 951_782_400),
        ("2000-03-01T00:00:00Z", 951_868_800),
        ("2026-09-21T06:20:51Z", 1_789_971_651),
    ];
    for (stamp, expected) in known {
        assert_eq!(
            epoch_seconds(stamp),
            Some(expected),
            "{stamp} did not come back as {expected} seconds since the epoch"
        );
    }

    assert_eq!(
        epoch_seconds("not a date"),
        None,
        "text that is not a stamp was read as one"
    );

    // A day apart is 86,400 seconds apart, which is what the seven-day
    // comparison above is counting in.
    let first = epoch_seconds("2026-09-21T06:20:51Z").expect("a stamp parses");
    let second = epoch_seconds("2026-09-22T06:20:51Z").expect("a stamp parses");
    assert_eq!(
        second - first,
        86_400,
        "two consecutive days are not a day apart"
    );
}

/// The newest row is found wherever it sits in the file.
///
/// The ledger is appended to, so the newest row is the last one - but it is
/// read by stamp rather than by position, because two runs finishing out of
/// order would put an older stamp last and a positional reader would then
/// report the run before it. This checks the stamp wins.
#[test]
fn the_newest_row_is_found_by_its_stamp_and_not_by_its_position() {
    let ledger = "# a comment\n\
                  stamp\tcommit\tmachine\ttarget\tverdict\tseconds\n\
                  2026-09-21T06:20:51Z\tabc\tm\tone\tpass\t1.0\n\
                  2026-09-20T06:20:51Z\tabc\tm\ttwo\tpass\t1.0\n";
    let (_, row) = newest_run(ledger).expect("the ledger holds two rows");
    assert!(
        row.contains("2026-09-21"),
        "the row before the newest one was reported as the newest: {row}"
    );

    assert!(
        newest_run("# nothing but a comment\n").is_none(),
        "a ledger with no rows reported a newest run"
    );
}
