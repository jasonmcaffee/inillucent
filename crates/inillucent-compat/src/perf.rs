//! The performance scorecard: the plan both engines read, and the statistics
//! that turn paired timings into a verdict.
//!
//! Invariant: **there is one copy of the workload and both engines are driven
//! from it.** The plan is rendered to a text file; the inillucent arm reads it and
//! so does the SQLite arm, which is a separate C program compiled from the
//! pinned amalgamation. The SQL, the parameter generator, the transaction
//! grouping, the prepared-statement policy, the page size, the journal mode and
//! the durability level are therefore the same bytes for both, rather than two
//! implementations of one intention that are meant to agree.
//!
//! The second invariant is that a timing is only a measurement if both engines
//! produced the same answer. Every workload accumulates a digest over the values
//! it returned, computed the same way on both sides; a workload whose digests
//! differ is not a slow result, it is a wrong one, and the analyser refuses to
//! time it rather than reporting a ratio nobody should read.
//!
//! What the statistics do, and why:
//!
//! - The samples are **paired**: one round runs both engines over the same
//!   freshly cloned database, so a machine that was busy for a second penalises
//!   both arms of that pair. The quantity analysed is the log of the ratio
//!   within each pair, which is what makes "a 2x win and a 2x loss average to
//!   no change" true rather than a 1.25x win.
//! - The interval is a **bootstrap** over the paired log ratios, because a ratio
//!   of medians has no closed-form interval and a normal approximation on
//!   timings is wrong in the direction that flatters the winner: latency
//!   distributions have a long right tail.
//! - No sample is removed for being slow. The only exclusion is a digest
//!   mismatch, which is declared before the run and is a correctness gate rather
//!   than an outlier policy.

use std::fmt::Write as _;

use inillucent_base::rng::Rng;

/// The plan format version, written into the file both engines read.
pub const PLAN_VERSION: u32 = 1;

/// How a parameter is generated, identically on both sides.
///
/// The formulas are here and in `compat/oracle/sqlite_bench.c`, and they have to
/// agree exactly: a benchmark whose two arms read different rows is not a
/// comparison. They are deliberately trivial - a modulus, a multiply, a fixed
/// sentence - so that "the same" is checkable by reading them side by side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bind {
    /// `1 + (iteration % rows)`: a sequential walk of existing keys.
    Rowid,
    /// `1 + ((iteration * 2654435761) % rows)`: a scattered walk of the same.
    Scatter,
    /// `rows + 1 + iteration`: a key that is not there yet, for inserts.
    Counter,
    /// `(iteration * 1103515245 + 12345) & 0x7fffffff`: an unclustered integer.
    Int,
    /// `"row <iteration> lorem ipsum dolor sit amet consectetur"`.
    Text,
    /// Sixty-four bytes, byte `j` being `(iteration + j) & 0xff`.
    Blob,
}

impl Bind {
    /// Returns the name the plan file writes.
    pub fn name(self) -> &'static str {
        match self {
            Bind::Rowid => "rowid",
            Bind::Scatter => "scatter",
            Bind::Counter => "counter",
            Bind::Int => "int",
            Bind::Text => "text",
            Bind::Blob => "blob",
        }
    }
}

/// The number of bytes a `Bind::Blob` produces.
pub const BLOB_BYTES: usize = 64;

/// How a workload groups its statements into transactions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grouping {
    /// Every statement commits on its own.
    Autocommit,
    /// One transaction around the whole repeat.
    Single,
    /// A commit every so many statements.
    Every(u32),
}

impl Grouping {
    /// Returns the value the plan file writes.
    pub fn name(self) -> String {
        match self {
            Grouping::Autocommit => "none".to_string(),
            Grouping::Single => "all".to_string(),
            Grouping::Every(count) => count.to_string(),
        }
    }
}

/// One measured workload.
#[derive(Clone, Debug)]
pub struct Workload {
    /// The workload's own name, unique in the plan.
    pub name: String,
    /// The family it is weighted under.
    pub family: String,
    /// The statement that is measured.
    pub sql: String,
    /// A statement run untimed before the measurement, or nothing.
    pub pre: Option<String>,
    /// A statement run untimed after the measurement, or nothing.
    pub post: Option<String>,
    /// How many times the statement runs.
    pub repeat: u32,
    /// How the statements are grouped into transactions.
    pub grouping: Grouping,
    /// Whether the statement is prepared once or per iteration.
    ///
    /// Once is the fair default and matches how an application uses a database.
    /// Per iteration is a family of its own - it is what "prepare" costs - and
    /// mixing the two into one number would hide whichever is worse.
    pub prepare_each: bool,
    /// The parameters, in order.
    pub binds: Vec<Bind>,
    /// Whether the workload changes the database.
    pub mutates: bool,
}

/// One scale of one plan.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The scale's name: `small`, `medium` or `large`.
    pub scale: String,
    /// How many rows the base table holds.
    pub rows: u32,
    /// The journal mode both engines run in.
    pub journal: String,
    /// The locking mode SQLite's arm runs in.
    ///
    /// **Only SQLite has one.** The new engine takes no operating-system lock
    /// on its file at all and answers `exclusive` when asked, so this is not a
    /// setting both arms share - it is the question of whether SQLite is asked
    /// to behave the way the engine it is being compared against behaves.
    /// Measured on Windows: a rowid point lookup in `normal` mode was 12.4
    /// microseconds against Linux's 2.0 for the same C, because the mode
    /// takes and drops a `LockFileEx` per statement.
    pub locking: String,
    /// The durability level both engines run at.
    pub synchronous: String,
    /// The page size both engines use.
    pub page_size: u32,
    /// The page cache budget both engines use, in SQLite's own units.
    pub cache_size: i32,
    /// The statements that build the pristine database.
    pub setup: Vec<String>,
    /// The workloads, in the order both engines run them.
    pub workloads: Vec<Workload>,
}

impl Plan {
    /// Renders the plan as the file both engines read.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("# inillucent performance plan. Both engines read this file.\n");
        let _ = writeln!(out, "version\t{PLAN_VERSION}");
        let _ = writeln!(out, "scale\t{}", self.scale);
        let _ = writeln!(out, "rows\t{}", self.rows);
        let _ = writeln!(out, "journal\t{}", self.journal);
        let _ = writeln!(out, "synchronous\t{}", self.synchronous);
        let _ = writeln!(out, "page_size\t{}", self.page_size);
        let _ = writeln!(out, "cache_size\t{}", self.cache_size);
        for statement in &self.setup {
            let _ = writeln!(out, "setup\t{}", one_line(statement));
        }
        for workload in &self.workloads {
            let _ = writeln!(out, "workload\t{}", workload.name);
            let _ = writeln!(out, "family\t{}", workload.family);
            let _ = writeln!(out, "repeat\t{}", workload.repeat);
            let _ = writeln!(out, "txn\t{}", workload.grouping.name());
            let _ = writeln!(
                out,
                "prepare\t{}",
                if workload.prepare_each {
                    "each"
                } else {
                    "once"
                }
            );
            if !workload.binds.is_empty() {
                let names: Vec<&str> = workload.binds.iter().map(|bind| bind.name()).collect();
                let _ = writeln!(out, "bind\t{}", names.join(","));
            }
            if let Some(pre) = &workload.pre {
                let _ = writeln!(out, "pre\t{}", one_line(pre));
            }
            let _ = writeln!(out, "sql\t{}", one_line(&workload.sql));
            if let Some(post) = &workload.post {
                let _ = writeln!(out, "post\t{}", one_line(post));
            }
        }
        out
    }
}

/// Returns a statement with its line breaks flattened.
///
/// The plan is line oriented, so a statement that spanned lines would be read
/// as several keys. Flattening rather than escaping keeps the format one a
/// person can read, and SQL does not care.
fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// One engine's result for one workload in one round.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// The workload's name.
    pub workload: String,
    /// How long it took, in nanoseconds.
    pub nanos: f64,
    /// How many rows it produced.
    pub rows: u64,
    /// The digest of everything it produced.
    pub digest: u64,
}

impl Sample {
    /// Reads a sample from one line of a driver's output.
    pub fn parse(line: &str) -> Option<Sample> {
        let mut fields = line.split('\t');
        let workload = fields.next()?.to_string();
        let nanos = fields.next()?.trim().parse::<f64>().ok()?;
        let rows = fields.next()?.trim().parse::<u64>().ok()?;
        let digest = u64::from_str_radix(fields.next()?.trim(), 16).ok()?;
        Some(Sample {
            workload,
            nanos,
            rows,
            digest,
        })
    }

    /// Renders the sample the way a driver prints it.
    pub fn render(&self) -> String {
        format!(
            "{}\t{:.0}\t{}\t{:016x}",
            self.workload, self.nanos, self.rows, self.digest
        )
    }
}

/// A digest over the values a workload produced.
///
/// FNV-1a, tagged by storage class, matching `sqlite_bench.c` byte for byte. It
/// is not a security property: what it has to do is notice that two engines
/// disagreed about an answer, and it is checked against the C implementation by
/// a test rather than assumed.
#[derive(Clone, Copy, Debug)]
pub struct Digest {
    hash: u64,
}

impl Default for Digest {
    /// Returns an empty digest.
    fn default() -> Digest {
        Digest::new()
    }
}

impl Digest {
    /// Returns a digest over nothing.
    pub fn new() -> Digest {
        Digest {
            hash: 0xcbf2_9ce4_8422_2325,
        }
    }

    /// Adds bytes.
    pub fn bytes(&mut self, data: &[u8]) {
        for byte in data {
            self.hash ^= u64::from(*byte);
            self.hash = self.hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    /// Adds a tag byte.
    pub fn tag(&mut self, tag: u8) {
        self.bytes(&[tag]);
    }

    /// Adds a 64-bit value, little endian.
    pub fn word(&mut self, value: u64) {
        self.bytes(&value.to_le_bytes());
    }

    /// Returns the digest.
    pub fn finish(self) -> u64 {
        self.hash
    }
}

/// One workload's paired timings across every round.
#[derive(Clone, Debug, Default)]
pub struct Paired {
    /// The workload's name.
    pub workload: String,
    /// The family it is weighted under.
    pub family: String,
    /// One `(inillucent, sqlite)` pair per round, in nanoseconds.
    pub pairs: Vec<(f64, f64)>,
    /// Whether every round agreed on the answer.
    pub agreed: bool,
    /// What disagreed, when something did.
    pub disagreement: String,
}

impl Paired {
    /// Returns the median speed ratio, SQLite over inillucent.
    ///
    /// Above one means inillucent is faster, which is the direction a reader
    /// expects of a number called a speedup.
    pub fn ratio(&self) -> f64 {
        median(&self.log_ratios()).exp()
    }

    /// Returns the paired log speedups.
    pub fn log_ratios(&self) -> Vec<f64> {
        self.pairs
            .iter()
            .filter(|(ours, theirs)| *ours > 0.0 && *theirs > 0.0)
            .map(|(ours, theirs)| (theirs / ours).ln())
            .collect()
    }

    /// Returns the bootstrap 95% interval of the speed ratio.
    pub fn interval(&self, seed: u64) -> (f64, f64) {
        let (low, high) = bootstrap(&self.log_ratios(), seed);
        (low.exp(), high.exp())
    }

    /// Returns the median nanoseconds each engine took.
    pub fn medians(&self) -> (f64, f64) {
        let ours: Vec<f64> = self.pairs.iter().map(|(ours, _)| *ours).collect();
        let theirs: Vec<f64> = self.pairs.iter().map(|(_, theirs)| *theirs).collect();
        (median(&ours), median(&theirs))
    }
}

/// What a family's interval says about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The lower bound is at least 1.20x.
    Win,
    /// The whole interval lies inside 0.95x to 1.05x.
    Equivalent,
    /// The upper bound is below 1.00x.
    Loss,
    /// None of the above.
    Inconclusive,
}

impl Verdict {
    /// Returns the verdict an interval earns.
    pub fn of(lower: f64, upper: f64) -> Verdict {
        if lower >= 1.20 {
            return Verdict::Win;
        }
        if lower >= 0.95 && upper <= 1.05 {
            return Verdict::Equivalent;
        }
        if upper < 1.00 {
            return Verdict::Loss;
        }
        Verdict::Inconclusive
    }

    /// Returns the verdict's name.
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Win => "win",
            Verdict::Equivalent => "equivalent",
            Verdict::Loss => "loss",
            Verdict::Inconclusive => "inconclusive",
        }
    }
}

/// How many bootstrap resamples the interval is built from.
///
/// Two thousand: enough that the 2.5th and 97.5th percentiles are stable to
/// three digits across seeds, cheap enough to run for every workload at every
/// scale.
pub const RESAMPLES: usize = 2_000;

/// Returns the 95% bootstrap interval of the mean of a sample.
///
/// The mean of the *log* ratios, which is the geometric mean of the ratios -
/// the right centre for a quantity where halving and doubling are the same size
/// of change.
pub fn bootstrap(values: &[f64], seed: u64) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    if values.len() == 1 {
        let only = values.first().copied().unwrap_or(0.0);
        return (only, only);
    }
    let mut rng = Rng::new(seed);
    let mut means: Vec<f64> = Vec::with_capacity(RESAMPLES);
    for _ in 0..RESAMPLES {
        let mut total = 0.0;
        for _ in 0..values.len() {
            let index = rng.below(values.len() as u64) as usize;
            total += values.get(index).copied().unwrap_or(0.0);
        }
        means.push(total / values.len() as f64);
    }
    means.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let low = percentile(&means, 0.025);
    let high = percentile(&means, 0.975);
    (low, high)
}

/// Returns one percentile of a sorted sample.
fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let position = (fraction * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted
        .get(position.min(sorted.len().saturating_sub(1)))
        .copied()
        .unwrap_or(0.0)
}

/// Returns the median of a sample.
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        return sorted.get(middle).copied().unwrap_or(0.0);
    }
    let lower = sorted.get(middle.saturating_sub(1)).copied().unwrap_or(0.0);
    let upper = sorted.get(middle).copied().unwrap_or(0.0);
    (lower + upper) / 2.0
}

/// Returns the median absolute deviation of a sample.
pub fn deviation(values: &[f64]) -> f64 {
    let centre = median(values);
    let spread: Vec<f64> = values.iter().map(|value| (value - centre).abs()).collect();
    median(&spread)
}

/// One family's weight in the headline number, and whether it has a floor.
#[derive(Clone, Debug, PartialEq)]
pub struct FamilyWeight {
    /// The family's identifier.
    pub id: String,
    /// Its share of the weighted geometric mean.
    pub weight: f64,
    /// Whether it may not fall below the floor.
    pub required: bool,
    /// What the family is, in words.
    pub description: String,
}

/// The weights, floors and thresholds a release is judged against.
#[derive(Clone, Debug, Default)]
pub struct Contract {
    /// The weight of every family.
    pub families: Vec<FamilyWeight>,
    /// The lower bound the weighted geometric mean must reach.
    pub headline: f64,
    /// The lower bound below which a required family fails.
    pub floor: f64,
    /// The most of SQLite's peak resident set this engine may hold, as a ratio.
    ///
    /// **Under one, because the goal is to hold less.** Absent from a contract
    /// written before the memory and CPU bars existed, in which case there is
    /// no memory bar and the gate reports the ratio without judging it - a
    /// missing bar must read as "nobody set one", never as "met".
    pub memory: Option<f64>,
    /// The most of SQLite's processor time this engine may spend, as a ratio.
    pub cpu: Option<f64>,
}

impl Contract {
    /// Reads the contract from its checked-in file.
    pub fn parse(text: &str) -> Result<Contract, String> {
        let document = crate::toml_lite::parse(text)?;
        let headline = document
            .top
            .get("headline")
            .and_then(crate::toml_lite::Value::as_str)
            .and_then(|value| value.parse::<f64>().ok())
            .ok_or_else(|| "the contract needs a `headline` bound".to_string())?;
        let floor = document
            .top
            .get("floor")
            .and_then(crate::toml_lite::Value::as_str)
            .and_then(|value| value.parse::<f64>().ok())
            .ok_or_else(|| "the contract needs a `floor` bound".to_string())?;
        let mut families = Vec::new();
        for table in document.array("family") {
            let field = |name: &str| {
                table
                    .get(name)
                    .and_then(crate::toml_lite::Value::as_str)
                    .map(str::to_string)
            };
            let id = field("id").ok_or_else(|| "a family needs an `id`".to_string())?;
            let weight = field("weight")
                .and_then(|value| value.parse::<f64>().ok())
                .ok_or_else(|| format!("{id} needs a `weight`"))?;
            let required = table
                .get("required")
                .and_then(crate::toml_lite::Value::as_bool)
                .unwrap_or(true);
            families.push(FamilyWeight {
                id,
                weight,
                required,
                description: field("description").unwrap_or_default(),
            });
        }
        if families.is_empty() {
            return Err("the contract names no families".to_string());
        }
        let total: f64 = families.iter().map(|family| family.weight).sum();
        if (total - 1.0).abs() > 1.0e-6 {
            return Err(format!("the family weights sum to {total}, not 1"));
        }
        // **A bar that is present but unreadable is an error, not an absence.**
        // Returning `None` for `bar = "nought point nine"` would silently drop
        // the judgement the file was edited to add.
        let ratio = |section: &str| -> Result<Option<f64>, String> {
            let table = document.table(section);
            match table.get("bar").and_then(crate::toml_lite::Value::as_str) {
                None if table.is_empty() => Ok(None),
                None => Err(format!("the contract's [{section}] needs a `bar`")),
                Some(text) => text.parse::<f64>().map(Some).map_err(|_| {
                    format!("the contract's [{section}] bar `{text}` is not a number")
                }),
            }
        };
        Ok(Contract {
            families,
            headline,
            floor,
            memory: ratio("memory")?,
            cpu: ratio("cpu")?,
        })
    }

    /// Returns one family's weight, or zero for a family nobody declared.
    pub fn weight_of(&self, family: &str) -> f64 {
        self.families
            .iter()
            .find(|declared| declared.id == family)
            .map(|declared| declared.weight)
            .unwrap_or(0.0)
    }

    /// Returns whether a family has a floor under it.
    pub fn is_required(&self, family: &str) -> bool {
        self.families
            .iter()
            .find(|declared| declared.id == family)
            .map(|declared| declared.required)
            .unwrap_or(false)
    }
}

/// Returns the log ratios shaped one vector per round, over the workloads whose
/// two engines agreed.
///
/// This is what "correctness-qualified measurement" means, and it is here
/// rather than in the reporting binary because it is the rule the whole
/// exercise rests on: a timing is evidence only if both engines produced the
/// same answer. A workload that disagreed carries no pairs at all - the
/// measurement loop records the disagreement instead of a duration - and it is
/// reported as a correctness failure rather than as a fast or slow result.
///
/// The depth is the shortest agreeing workload's, so every round the headline
/// is built from has a value for every workload in it. Taking the minimum over
/// *all* workloads instead once made a single correctness failure report a
/// headline of exactly 1.000x: depth zero, no rounds, an empty bootstrap. That
/// is the most misleading number this report could produce, which is why the
/// filter is applied before the minimum and not after.
/// @param measured - every workload of one scale
pub fn qualified_rounds(measured: &[Paired]) -> Vec<Vec<(String, f64)>> {
    let depth = measured
        .iter()
        .filter(|paired| paired.agreed)
        .map(|paired| paired.pairs.len())
        .min()
        .unwrap_or(0);
    (0..depth)
        .map(|round| {
            measured
                .iter()
                .filter(|paired| paired.agreed)
                .filter_map(|paired| {
                    let (ours, theirs) = paired.pairs.get(round).copied()?;
                    if ours <= 0.0 || theirs <= 0.0 {
                        return None;
                    }
                    Some((paired.family.clone(), (theirs / ours).ln()))
                })
                .collect()
        })
        .collect()
}

/// Returns the weighted geometric mean's bootstrap interval.
///
/// The headline number. Each round contributes one weighted mean of that
/// round's log ratios, so the bootstrap resamples *rounds* rather than
/// workloads - which is what keeps the pairing intact: a round in which the
/// machine was busy is one draw, not one draw per workload.
/// @param families - the log ratios of every workload, by family, per round
/// @param contract - the weights
/// @param seed - the bootstrap seed
pub fn weighted_headline(
    rounds: &[Vec<(String, f64)>],
    contract: &Contract,
    seed: u64,
) -> (f64, f64, f64) {
    let per_round: Vec<f64> = rounds
        .iter()
        .map(|round| weighted_mean(round, contract))
        .collect();
    let centre = median(&per_round).exp();
    let (low, high) = bootstrap(&per_round, seed);
    (centre, low.exp(), high.exp())
}

/// Returns a family's centre and 95% bootstrap interval, resampling rounds.
///
/// Each round contributes one value: the mean of that round's log ratios over
/// the family's workloads. The bootstrap then resamples those per-round means.
/// That is the statistic [`weighted_headline`] already uses for every family
/// inside the headline, so a family is now graded by the same number it
/// contributes to the headline.
///
/// **Why not one list of every workload's every round (task-2086).** That is
/// what every gate did until task-2086, and it makes the interval measure how
/// far apart the family's workloads are rather than how precisely they were
/// measured. A resample of the pooled list draws the workloads in random
/// proportions, and when the workloads differ that proportion moves the mean
/// more than any timing noise does. `read.join` is the case that exposed it:
/// on the nine pinned passes task-2082 took, `join.selective` read about 21x
/// with its own interval 20.3x to 21.2x and `join.range` read 0.87x with its
/// own interval 0.86x to 0.89x, and the pooled family interval printed beside
/// them was 2.8x to 6.5x. The family's lower bound missed a 3.00x bar on every
/// build because the two workloads are a factor of 24 apart. The family's
/// composition is fixed by the plan, so the proportion of each workload is not
/// a random quantity and the interval should not treat it as one.
///
/// A family whose workloads ran a different number of rounds is cut to the
/// shortest, so every per-round mean has every workload in it. For the same
/// reason a round in which any workload has a zero or negative time is left
/// out whole: dropping only that workload would give that one round a
/// different mix of workloads, which is the defect above in a smaller form.
/// @param members - the workloads in the family, each with its paired timings
/// @param seed - the seed the resampling uses
pub fn family_interval(members: &[&Paired], seed: u64) -> (f64, f64, f64) {
    let depth = members
        .iter()
        .map(|paired| paired.pairs.len())
        .min()
        .unwrap_or(0);
    let per_round: Vec<f64> = (0..depth)
        .filter_map(|round| {
            let logs: Option<Vec<f64>> = members
                .iter()
                .map(|paired| {
                    let (ours, theirs) = paired.pairs.get(round).copied()?;
                    (ours > 0.0 && theirs > 0.0).then(|| (theirs / ours).ln())
                })
                .collect();
            let logs = logs.filter(|logs| !logs.is_empty())?;
            Some(logs.iter().sum::<f64>() / logs.len() as f64)
        })
        .collect();
    if per_round.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let centre = per_round.iter().sum::<f64>() / per_round.len() as f64;
    let (low, high) = bootstrap(&per_round, seed);
    (centre.exp(), low.exp(), high.exp())
}

/// Returns one round's weighted mean log ratio.
///
/// A family with several workloads contributes the mean of its workloads, so a
/// family is not weighted by how many cases somebody happened to write for it.
fn weighted_mean(round: &[(String, f64)], contract: &Contract) -> f64 {
    let mut total = 0.0;
    let mut weight_used = 0.0;
    for family in &contract.families {
        let values: Vec<f64> = round
            .iter()
            .filter(|(id, _)| *id == family.id)
            .map(|(_, value)| *value)
            .collect();
        if values.is_empty() {
            continue;
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        total += mean * family.weight;
        weight_used += family.weight;
    }
    if weight_used <= 0.0 {
        return 0.0;
    }
    total / weight_used
}

// ---------------------------------------------------------------------------
// The plans.
//
// The scorecard, the read-family gate and anything else that wants to measure
// "the same workload as the scorecard" read this one table. A harness that
// wrote out its own copy of the SQL would be measuring a different query the
// first time either was edited, and two of the four instrument bugs this
// project has already paid for were exactly that shape.
// ---------------------------------------------------------------------------

/// Returns the row count one scale uses.
///
/// Small fits in the cache budget; medium exceeds it and fits in memory; large
/// exceeds the budget by enough that the page cache cannot hold the working set
/// and the file system has to answer.
pub fn rows_for(scale: &str) -> u32 {
    match scale {
        "small" => 5_000,
        "medium" => 100_000,
        _ => 600_000,
    }
}

/// Returns how many times a workload repeats at one scale.
///
/// The repeat counts are chosen so that one round takes a comparable amount of
/// time at every scale: a point read is cheap and runs many times, a full scan
/// of six hundred thousand rows is not and runs once.
pub fn repeats_for(scale: &str) -> (u32, u32, u32) {
    match scale {
        "small" => (4_000, 400, 2_000),
        "medium" => (4_000, 40, 2_000),
        _ => (2_000, 4, 1_000),
    }
}

/// Returns a subquery producing `seq` from 1 upwards, far enough for `wanted`.
///
/// One digit table joined to itself as many times as the count needs. Six
/// copies reach a million, which is above every scale here; fewer are used when
/// fewer will do, because a cross join nobody needs is a million rows nobody
/// reads.
pub fn counter_sql(wanted: u32) -> String {
    let mut digits = 1usize;
    while 10u64.pow(digits as u32) < u64::from(wanted) && digits < 7 {
        digits = digits.saturating_add(1);
    }
    let names: Vec<String> = (0..digits).map(|index| format!("d{index}")).collect();
    let mut expression = String::new();
    for (position, name) in names.iter().enumerate() {
        if position == 0 {
            expression.push_str(&format!("{name}.n"));
        } else {
            expression = format!("({expression} * 10 + {name}.n)");
        }
    }
    let from: Vec<String> = names.iter().map(|name| format!("digits {name}")).collect();
    format!("SELECT {expression} + 1 AS seq FROM {}", from.join(", "))
}

/// Returns the plan for one scale.
///
/// **One function per workload family, rather than one four-hundred-line
/// literal (task-1969, 7.2).** This was 450 lines and the ratchet in
/// `policy.rs` froze it there rather than shrinking it, which is how a
/// criterion that says "no production function over 300 lines" was met with
/// eight functions over 300. The seams were already in the list: every
/// `Workload` declares the family it belongs to, and the families were
/// already contiguous.
///
/// @param scale - `small`, `medium` or `large`
pub fn plan_for(scale: &str) -> Plan {
    let rows = rows_for(scale);
    let (point, scan, write) = repeats_for(scale);
    let mut workloads: Vec<Workload> = Vec::new();
    workloads.extend(preparing_workloads(point));
    workloads.extend(point_read_workloads(point));
    workloads.extend(range_read_workloads(point));
    workloads.extend(analytical_read_workloads(scan));
    workloads.extend(join_read_workloads(point));
    workloads.extend(correlated_read_workloads(point));
    workloads.extend(write_workloads(write));
    workloads.extend(transaction_workloads(write));
    workloads.extend(schema_workloads());
    workloads.extend(extension_workloads(point, write));
    workloads.extend(large_value_workloads(point, write));
    Plan {
        scale: scale.to_string(),
        rows,
        journal: "delete".to_string(),
        locking: "normal".to_string(),
        synchronous: "full".to_string(),
        page_size: 4096,
        cache_size: -2000,
        setup: setup_for(rows),
        workloads,
    }
}

/// Returns the statements that build the fixture every workload runs against.
///
/// Split out of [`plan_for`] because it is the half that is about the *data*
/// and the workloads are the half that is about the *queries*, and the two
/// change for different reasons (task-1969, 7.2).
///
/// @param rows - how many rows the main table ends with at this scale
fn setup_for(rows: u32) -> Vec<String> {
    let mut setup = vec![
        "CREATE TABLE main_table(id INTEGER PRIMARY KEY, key INTEGER NOT NULL, \
         category INTEGER NOT NULL, label TEXT NOT NULL, payload BLOB)"
            .to_string(),
        "CREATE INDEX main_key ON main_table(key)".to_string(),
        "CREATE INDEX main_category ON main_table(category, key)".to_string(),
        "CREATE TABLE side_table(id INTEGER PRIMARY KEY, owner INTEGER NOT NULL, note TEXT)"
            .to_string(),
        "CREATE INDEX side_owner ON side_table(owner)".to_string(),
        "CREATE TABLE wide(id INTEGER PRIMARY KEY, body TEXT)".to_string(),
    ];
    // The rows are generated by SQL rather than by a loop of inserts, so the
    // plan file stays small and both engines build the same rows from the same
    // expression. A cross join of a ten-row digit table is the portable way to
    // count: a recursive CTE reads better and is not accepted on the left of an
    // INSERT by both engines, and a plan that had to be written twice would be
    // the one thing this file exists to avoid.
    setup.push("CREATE TABLE digits(n INTEGER PRIMARY KEY)".to_string());
    setup.push("INSERT INTO digits(n) VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)".to_string());
    setup.push(format!(
        "INSERT INTO main_table(id, key, category, label, payload) \
         SELECT seq, (seq * 2654435761) % {rows}, seq % 64, \
                'row ' || seq || ' lorem ipsum dolor sit amet consectetur', zeroblob(48) \
         FROM ({counter}) WHERE seq <= {rows}",
        counter = counter_sql(rows)
    ));
    setup.push(format!(
        "INSERT INTO side_table(id, owner, note) \
         SELECT seq, ((seq * 7) % {rows}) + 1, 'note ' || seq \
         FROM ({counter}) WHERE seq <= {side}",
        side = rows / 4,
        counter = counter_sql(rows / 4)
    ));
    setup.push(format!(
        "INSERT INTO wide(id, body) \
         SELECT seq, replace(hex(zeroblob(2048)), '0', 'x') FROM ({counter}) WHERE seq <= 400",
        counter = counter_sql(400)
    ));
    setup.push("ANALYZE".to_string());

    setup
}

/// Returns the `open.prepare` workloads: preparing a statement, which is the cost every other family pays before it measures anything.
///
/// @param point - how many times a point workload repeats at this scale
fn preparing_workloads(point: u32) -> Vec<Workload> {
    vec![
        Workload {
            name: "prepare.trivial".to_string(),
            family: "open.prepare".to_string(),
            sql: "SELECT 1".to_string(),
            pre: None,
            post: None,
            repeat: point,
            grouping: Grouping::Autocommit,
            prepare_each: true,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "prepare.point".to_string(),
            family: "open.prepare".to_string(),
            sql: "SELECT label FROM main_table WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: point,
            grouping: Grouping::Autocommit,
            prepare_each: true,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
    ]
}

/// Returns the `read.point` workloads: reading one row by a key, the shape an application does most.
///
/// @param point - how many times a point workload repeats at this scale
fn point_read_workloads(point: u32) -> Vec<Workload> {
    vec![
        Workload {
            name: "point.rowid".to_string(),
            family: "read.point".to_string(),
            sql: "SELECT label FROM main_table WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: point,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "point.index".to_string(),
            family: "read.point".to_string(),
            sql: "SELECT id, label FROM main_table WHERE key = ?1".to_string(),
            pre: None,
            post: None,
            repeat: point,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "point.miss".to_string(),
            family: "read.point".to_string(),
            sql: "SELECT label FROM main_table WHERE id = ?1 + 100000000".to_string(),
            pre: None,
            post: None,
            repeat: point,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
    ]
}

/// Returns the `read.range` workloads: reading a run of rows, where an index either covers the query or does not.
///
/// @param point - how many times a point workload repeats at this scale
fn range_read_workloads(point: u32) -> Vec<Workload> {
    vec![
        Workload {
            name: "range.covering".to_string(),
            family: "read.range".to_string(),
            sql: "SELECT count(key) FROM main_table WHERE key BETWEEN ?1 AND ?1 + 200".to_string(),
            pre: None,
            post: None,
            repeat: point / 4,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "range.lookaside".to_string(),
            family: "read.range".to_string(),
            sql: "SELECT sum(length(label)) FROM main_table WHERE key BETWEEN ?1 AND ?1 + 200"
                .to_string(),
            pre: None,
            post: None,
            repeat: point / 8,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "range.reverse".to_string(),
            family: "read.range".to_string(),
            sql: "SELECT id FROM main_table WHERE id <= ?1 ORDER BY id DESC LIMIT 50".to_string(),
            pre: None,
            post: None,
            repeat: point / 4,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
    ]
}

/// Returns the `read.analytical` workloads: grouping and ordering over the whole table, where the sorter and the grouper are the cost.
///
/// @param scan - how many times a scan workload repeats at this scale
fn analytical_read_workloads(scan: u32) -> Vec<Workload> {
    vec![
        Workload {
            name: "scan.aggregate".to_string(),
            family: "read.analytical".to_string(),
            sql: "SELECT count(*), sum(key), max(category) FROM main_table".to_string(),
            pre: None,
            post: None,
            repeat: scan,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "scan.group".to_string(),
            family: "read.analytical".to_string(),
            sql: "SELECT category, count(*) FROM main_table GROUP BY category ORDER BY category"
                .to_string(),
            pre: None,
            post: None,
            repeat: scan,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "scan.sort".to_string(),
            family: "read.analytical".to_string(),
            sql: "SELECT id FROM main_table ORDER BY label LIMIT 100".to_string(),
            pre: None,
            post: None,
            repeat: scan,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "scan.distinct".to_string(),
            family: "read.analytical".to_string(),
            sql: "SELECT DISTINCT category FROM main_table ORDER BY category".to_string(),
            pre: None,
            post: None,
            repeat: scan,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
    ]
}

/// Returns the `read.join` workloads: joining two tables, where the planner's choice of driving table is the cost.
///
/// @param point - how many times a point workload repeats at this scale
fn join_read_workloads(point: u32) -> Vec<Workload> {
    vec![
        Workload {
            name: "join.selective".to_string(),
            family: "read.join".to_string(),
            sql: "SELECT count(*) FROM main_table JOIN side_table ON side_table.owner = \
                  main_table.id WHERE main_table.id = ?1"
                .to_string(),
            pre: None,
            post: None,
            repeat: point / 2,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
        Workload {
            name: "join.range".to_string(),
            family: "read.join".to_string(),
            sql: "SELECT count(side_table.note) FROM main_table JOIN side_table ON \
                  side_table.owner = main_table.id WHERE main_table.key BETWEEN ?1 AND ?1 + 200"
                .to_string(),
            pre: None,
            post: None,
            repeat: point / 8,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: false,
        },
    ]
}

/// Returns the `correlated` workloads: a block answered once per outer row.
///
/// **Graded against the join that answers the same question** (task-2066
/// §4.3.1). A correlated block and its join are one query written two ways, so
/// the join is the bar, and nothing measured either shape before this.
///
/// What it caught: a correlated `EXISTS` over 5,000 outer rows took 11,497 ms
/// and the three causes were a parameter set cloned per outer row, the same
/// set cloned again on every *read* of a parameter, and a structural choice
/// remade per row. It is 2,616 ms now. On this fixture `EXISTS` is 51.75 ms
/// against the join's 1.85, so the shape is still the expensive way to ask and
/// this is what will say when that changes.
///
/// **The outer table is `wide`, which holds 400 rows.** When these arms were
/// written the correlation operator sat *below* the filter - it computed a
/// block for every row the source produced and the `WHERE` then discarded most
/// of them - so `a.key BETWEEN ?1 AND ?1 + 200` over `main_table` added a
/// predicate and removed no work, and measured slower than the unbounded form
/// for the extra iterations alone. task-2076 moved every conjunct that reads no
/// subquery in front of the block, and the two `.selective` arms are the ones
/// that measure it.
///
/// `EXISTS` and `IN (SELECT ...)` both, because they reach different code:
/// `crate::correlate` answers the first and refuses the second, so a workload
/// with only one of them says nothing about the other.
///
/// **They belong to no weighted family, deliberately.** Folded into
/// `read.join` they took that family from 3.64x to 0.10x, which would be a
/// 36-fold regression in a published number caused by the workload set
/// changing rather than by the engine. And they cannot have a family of their
/// own either: `compat/perf/contract.toml` says in its own first paragraph
/// that the weights were fixed before any measurement was taken, and that a
/// weighting chosen after the results are in is not a weighting but a way of
/// writing down the results.
///
/// So the family name here is one no table knows. `report_results` prints
/// these two and compares their digests against SQLite like every other
/// workload; `report_families`, the floor and the headline iterate `FAMILIES`
/// and never see them. The correctness half is graded and the timing is
/// reported next to the join a reader should compare it with.
///
/// @param point - how many times a point workload repeats at this scale
fn correlated_read_workloads(point: u32) -> Vec<Workload> {
    // One iteration answers 400 correlated blocks and costs tens of
    // milliseconds, which is already at the top of what the other read
    // workloads cost for their whole repeat. The rounds are what provide the
    // samples.
    let repeat = (point / 4_000).max(1);
    vec![
        Workload {
            name: "correlated.exists".to_string(),
            family: "read.correlated".to_string(),
            sql: "SELECT count(*) FROM wide a WHERE EXISTS (SELECT 1 FROM side_table b WHERE                   b.owner = a.id)"
                .to_string(),
            pre: None,
            post: None,
            repeat,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "correlated.in".to_string(),
            family: "read.correlated".to_string(),
            sql: "SELECT count(*) FROM wide a WHERE a.id IN (SELECT b.owner FROM side_table b                   WHERE b.owner = a.id)"
                .to_string(),
            pre: None,
            post: None,
            repeat,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        // **The two arms with a selective filter beside the block** (task-2076).
        // `a.id % 100 = 0` keeps 4 of `wide`'s 400 rows, one in a hundred, and
        // no index or rowid range can answer a modulo, so it stays a residual
        // predicate. The arms above keep every outer row, so they cost
        // the same whether the correlation operator answers a block before or
        // after the filter; these two are the arms where that order is the
        // whole of the difference.
        //
        // Measured in a quiet window, release builds alternated, medians of 12
        // and then 30 rounds: `correlated.exists.selective` went from 54.90 ms
        // to 1.963 ms and `correlated.scalar.selective` from 55.48 ms to
        // 1.948 ms, which is 27.97x and 28.48x. The two unfiltered arms did not
        // move by more than two readings of the same build differ from each
        // other. SQLite answers both selective arms in about 26 us, so against
        // SQLite they are still 0.01x.
        //
        // Those timings were taken with `a.id + 0 > 396`, which keeps the same
        // four rows. task-2076 tried the modulo first and the gate refused it:
        // `Expr::General` read the connection's length limit through
        // `Params::context`, which counted against `Statement::rebindable`.
        // task-2081 made that read a setting, which does not count, and put
        // the modulo back.
        Workload {
            name: "correlated.exists.selective".to_string(),
            family: "read.correlated".to_string(),
            sql: "SELECT count(*) FROM wide a WHERE a.id % 100 = 0 AND EXISTS \
                  (SELECT 1 FROM side_table b WHERE b.owner = a.id)"
                .to_string(),
            pre: None,
            post: None,
            repeat,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "correlated.scalar.selective".to_string(),
            family: "read.correlated".to_string(),
            sql: "SELECT count(*) FROM wide a WHERE a.id % 100 = 0 AND a.id * 4 > \
                  (SELECT count(*) FROM side_table b WHERE b.owner = a.id)"
                .to_string(),
            pre: None,
            post: None,
            repeat,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
    ]
}

/// Returns the `write` workloads: inserting, updating and deleting, one statement at a time.
///
/// @param write - how many times a write workload repeats at this scale
fn write_workloads(write: u32) -> Vec<Workload> {
    vec![
        Workload {
            name: "write.insert.batch".to_string(),
            family: "write".to_string(),
            sql: "INSERT INTO main_table(id, key, category, label, payload) VALUES (?1, ?2, ?3, ?4, ?5)"
                .to_string(),
            pre: None,
            post: None,
            repeat: write,
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Counter, Bind::Int, Bind::Int, Bind::Text, Bind::Blob],
            mutates: true,
        },
        Workload {
            name: "write.insert.autocommit".to_string(),
            family: "write".to_string(),
            sql: "INSERT INTO side_table(owner, note) VALUES (?1, ?2)".to_string(),
            pre: None,
            post: None,
            repeat: (write / 20).max(20),
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Int, Bind::Text],
            mutates: true,
        },
        Workload {
            name: "write.update.indexed".to_string(),
            family: "write".to_string(),
            sql: "UPDATE main_table SET key = key + 1 WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: write,
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: true,
        },
        Workload {
            name: "write.delete".to_string(),
            family: "write".to_string(),
            sql: "DELETE FROM main_table WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: write,
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Scatter],
            mutates: true,
        },
        Workload {
            name: "write.upsert".to_string(),
            family: "write".to_string(),
            sql: "INSERT INTO wide(id, body) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET \
                  body = excluded.body"
                .to_string(),
            pre: None,
            post: None,
            repeat: (write / 4).max(20),
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Rowid, Bind::Text],
            mutates: true,
        },
    ]
}

/// Returns the `transaction` workloads: the same writes inside an explicit transaction, so the commit is amortised.
///
/// @param write - how many times a write workload repeats at this scale
fn transaction_workloads(write: u32) -> Vec<Workload> {
    // The `pre` that puts `side_table.note` back to what the fixture
    // builder wrote, so a workload here measures updates that change a
    // value rather than updates that write what is already there.
    let reset_notes = || Some("UPDATE side_table SET note = 'note ' || id".to_string());
    vec![
        Workload {
            name: "txn.autocommit".to_string(),
            family: "transaction".to_string(),
            sql: "UPDATE side_table SET note = ?2 WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: (write / 20).max(20),
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Scatter, Bind::Text],
            mutates: true,
        },
        Workload {
            name: "txn.batched".to_string(),
            family: "transaction".to_string(),
            sql: "UPDATE side_table SET note = ?2 WHERE id = ?1".to_string(),
            // **Put the notes back first, or this measures nothing.** The three
            // `transaction` workloads are deliberately the same statement at
            // three groupings, which is only a comparison if all three start
            // from the same rows - and they bind the same scattered rowids and
            // the same `row {iteration} lorem ipsum ...` text, so each one was
            // writing back what the one before it had already written.
            // Measured with `inillucent-execprofile`: an `UPDATE` that changes
            // a value cost 1,723 ns, and the same `UPDATE` writing back what was
            // already there cost 4,067. `pre` runs outside the timed region on
            // both arms, exactly as `sqlite_bench.c` runs it.
            pre: reset_notes(),
            post: None,
            repeat: write,
            grouping: Grouping::Every(10),
            prepare_each: false,
            binds: vec![Bind::Scatter, Bind::Text],
            mutates: true,
        },
        Workload {
            name: "txn.large".to_string(),
            family: "transaction".to_string(),
            sql: "UPDATE side_table SET note = ?2 WHERE id = ?1".to_string(),
            // See `txn.batched`: without this, every row already held the bytes
            // this was about to write.
            pre: reset_notes(),
            post: None,
            repeat: write,
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Scatter, Bind::Text],
            mutates: true,
        },
    ]
}

/// Returns the `schema` workloads: changing the schema, which invalidates every compiled statement.
///
fn schema_workloads() -> Vec<Workload> {
    vec![Workload {
        name: "schema.index".to_string(),
        family: "schema".to_string(),
        sql: "CREATE INDEX main_label ON main_table(label)".to_string(),
        pre: Some("DROP INDEX IF EXISTS main_label".to_string()),
        post: Some("DROP INDEX IF EXISTS main_label".to_string()),
        repeat: 1,
        grouping: Grouping::Autocommit,
        prepare_each: true,
        binds: Vec::new(),
        mutates: true,
    }]
}

/// Returns the `extension` workloads: the parts SQLite gets from extensions - JSON, full text, a table-valued function.
///
/// @param point - how many times a point workload repeats at this scale
/// @param write - how many times a write workload repeats at this scale
fn extension_workloads(point: u32, write: u32) -> Vec<Workload> {
    vec![
        Workload {
            name: "extension.json".to_string(),
            family: "extension".to_string(),
            sql: "SELECT json_extract('{\"a\":[1,2,3],\"b\":{\"c\":\"d\"}}', '$.b.c')".to_string(),
            pre: None,
            post: None,
            repeat: point,
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "extension.fts.build".to_string(),
            family: "extension".to_string(),
            sql: "INSERT INTO documents(title, body) VALUES (?1, ?2)".to_string(),
            pre: Some(
                "DROP TABLE IF EXISTS documents; \
                 CREATE VIRTUAL TABLE documents USING fts5(title, body)"
                    .to_string(),
            ),
            post: None,
            repeat: (write / 4).max(50),
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Text, Bind::Text],
            mutates: true,
        },
        Workload {
            name: "extension.fts.query".to_string(),
            family: "extension".to_string(),
            sql: "SELECT count(*) FROM documents WHERE documents MATCH 'lorem'".to_string(),
            pre: None,
            post: Some("DROP TABLE IF EXISTS documents".to_string()),
            repeat: (point / 8).max(20),
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: Vec::new(),
            mutates: false,
        },
        Workload {
            name: "extension.rtree.insert".to_string(),
            family: "extension".to_string(),
            sql: "INSERT INTO boxes(id, minX, maxX, minY, maxY) VALUES (?1, ?2, ?2 + 10, ?3, ?3 + 10)"
                .to_string(),
            pre: Some(
                "DROP TABLE IF EXISTS boxes; \
                 CREATE VIRTUAL TABLE boxes USING rtree(id, minX, maxX, minY, maxY)"
                    .to_string(),
            ),
            post: None,
            repeat: (write / 4).max(50),
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Counter, Bind::Int, Bind::Int],
            mutates: true,
        },
        Workload {
            name: "extension.rtree.query".to_string(),
            family: "extension".to_string(),
            sql: "SELECT count(*) FROM boxes WHERE minX > ?1 AND maxX < ?1 + 100000".to_string(),
            pre: None,
            post: Some("DROP TABLE IF EXISTS boxes".to_string()),
            repeat: (point / 8).max(20),
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Int],
            mutates: false,
        },
    ]
}

/// Returns the `large.values` workloads: rows whose payload does not fit a page, so the overflow chain is the cost.
///
/// @param point - how many times a point workload repeats at this scale
/// @param write - how many times a write workload repeats at this scale
fn large_value_workloads(point: u32, write: u32) -> Vec<Workload> {
    vec![
        Workload {
            name: "large.read".to_string(),
            family: "large.values".to_string(),
            sql: "SELECT length(body) FROM wide WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: (point / 2).max(20),
            grouping: Grouping::Autocommit,
            prepare_each: false,
            binds: vec![Bind::Rowid],
            mutates: false,
        },
        Workload {
            name: "large.write".to_string(),
            family: "large.values".to_string(),
            sql: "UPDATE wide SET body = ?2 || body WHERE id = ?1".to_string(),
            pre: None,
            post: None,
            repeat: (write / 20).max(20),
            grouping: Grouping::Single,
            prepare_each: false,
            binds: vec![Bind::Rowid, Bind::Text],
            mutates: true,
        },
    ]
}

/// Feeds one borrowed value into a result digest.
///
/// **The same bytes `eat` feeds for an owned value.** A gate that digested a
/// borrowed row differently from an owned one would report a difference between
/// two runs of the same query, so the tag and the length are written here
/// exactly as they are there.
///
/// @param digest - the digest being built
/// @param value - the value to feed it
pub fn eat_borrowed(digest: &mut Digest, value: &inillucent_tree::datum::Datum<'_>) {
    use inillucent_tree::datum::Datum;
    match value {
        Datum::Null => digest.tag(0),
        Datum::Int(number) => {
            digest.tag(1);
            digest.word(*number as u64);
        }
        Datum::Real(number) => {
            digest.tag(2);
            digest.word(number.to_bits());
        }
        Datum::Text(bytes) => {
            digest.tag(3);
            digest.word(bytes.len() as u64);
            digest.bytes(bytes);
        }
        Datum::Blob(bytes) => {
            digest.tag(4);
            digest.word(bytes.len() as u64);
            digest.bytes(bytes);
        }
    }
}

/// Runs the SQLite side of a plan and parses what it printed.
///
/// @param bench - the `sqlite-bench` binary
/// @param plan - the plan file both engines run
/// **Started through [`crate::affinity::spawn_on_same_cores`] (task-2085)**, so
/// a gate that pinned itself refuses to time a reference arm that is running on
/// other processors. The child inherits the gate's mask, and this reads it back.
///
/// @param database - the SQLite database it runs against
pub fn run_sqlite(
    bench: &std::path::Path,
    plan: &std::path::Path,
    database: &std::path::Path,
) -> Result<Vec<Sample>, String> {
    let child = crate::affinity::spawn_on_same_cores(
        std::process::Command::new(bench)
            .arg("run")
            .arg(plan)
            .arg(database)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
        "sqlite-bench",
    )?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("sqlite-bench did not finish: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "sqlite-bench failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(Sample::parse)
        .collect())
}

/// Builds the value one iteration binds, for a plan's declared bind kind.
///
/// **Deterministic, and the same on both engines.** Every number here comes out
/// of the iteration counter rather than a random source, so the two engines bind
/// the same values in the same order and a difference in the result digest is a
/// difference in the engines.
///
/// @param bind - what the plan asked to be bound
/// @param iteration - which iteration is binding
/// @param rows - how many rows the fixture holds, for the kinds that key on one
pub fn bind_value(bind: Bind, iteration: u32, rows: u32) -> inillucent_tree::datum::OwnedDatum {
    use inillucent_tree::datum::OwnedDatum;
    let iteration = u64::from(iteration);
    let rows64 = u64::from(rows);
    match bind {
        Bind::Rowid => OwnedDatum::Int(if rows > 0 {
            1 + (iteration % rows64) as i64
        } else {
            1
        }),
        Bind::Scatter => OwnedDatum::Int(if rows > 0 {
            1 + (iteration.wrapping_mul(2_654_435_761) % rows64) as i64
        } else {
            1
        }),
        Bind::Counter => OwnedDatum::Int((rows64 + 1 + iteration) as i64),
        Bind::Int => OwnedDatum::Int(
            (iteration.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7fff_ffff) as i64,
        ),
        Bind::Text => OwnedDatum::Text(
            format!("row {iteration} lorem ipsum dolor sit amet consectetur").into_bytes(),
        ),
        Bind::Blob => {
            OwnedDatum::Blob((0..64u64).map(|j| ((iteration + j) & 0xff) as u8).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan renders one line per key, with the SQL flattened.
    #[test]
    fn a_plan_renders_one_line_per_key() {
        let plan = Plan {
            scale: "small".to_string(),
            rows: 1000,
            journal: "delete".to_string(),
            locking: "normal".to_string(),
            synchronous: "full".to_string(),
            page_size: 4096,
            cache_size: -2000,
            setup: vec!["CREATE TABLE t(a INTEGER PRIMARY KEY,\n b TEXT)".to_string()],
            workloads: vec![Workload {
                name: "read.point".to_string(),
                family: "read.point".to_string(),
                sql: "SELECT b FROM t WHERE a = ?1".to_string(),
                pre: None,
                post: None,
                repeat: 100,
                grouping: Grouping::Autocommit,
                prepare_each: false,
                binds: vec![Bind::Rowid],
                mutates: false,
            }],
        };
        let rendered = plan.render();
        assert!(rendered.contains("rows\t1000\n"));
        assert!(rendered.contains("setup\tCREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)\n"));
        assert!(rendered.contains("bind\trowid\n"));
        assert!(rendered.contains("prepare\tonce\n"));
        assert!(!rendered.contains("\n \n"));
    }

    /// A sample survives the round trip through a driver's output line.
    #[test]
    fn a_sample_round_trips() {
        let sample = Sample {
            workload: "write.insert".to_string(),
            nanos: 1234.0,
            rows: 7,
            digest: 0xdead_beef_1234_5678,
        };
        let parsed = Sample::parse(&sample.render()).expect("it parses");
        assert_eq!(parsed, sample);
        assert!(Sample::parse("nonsense").is_none());
    }

    /// The digest is FNV-1a over tagged values, and it moves with the content.
    #[test]
    fn the_digest_notices_a_different_answer() {
        let mut first = Digest::new();
        first.tag(1);
        first.word(42);
        let mut second = Digest::new();
        second.tag(1);
        second.word(43);
        assert_ne!(first.finish(), second.finish());
        assert_eq!(Digest::new().finish(), 0xcbf2_9ce4_8422_2325);
    }

    /// The digest is the *published* FNV-1a, checked against a value computed
    /// outside this file.
    ///
    /// This is not ceremony. The prime was written `0x1000_0000_01b3` here,
    /// which is one hex digit too long and is a different, perfectly
    /// well-behaved hash - self-consistent, so every test that compared this
    /// implementation against itself passed. It was caught by the C arm
    /// disagreeing with it on every read workload in the scorecard's first run,
    /// and the only thing that would have caught it sooner is a vector.
    #[test]
    fn the_digest_is_the_published_function() {
        let mut hasher = Digest::new();
        hasher.bytes(b"a");
        assert_eq!(hasher.finish(), 0xaf63_dc4c_8601_ec8c);
        let mut longer = Digest::new();
        longer.bytes(b"foobar");
        assert_eq!(longer.finish(), 0x85944171_f73967e8);
    }

    /// One row of `SELECT 1` digests to the value the reference driver reports.
    ///
    /// The number on the right came out of `sqlite-bench` and was reproduced
    /// with an independent implementation of FNV-1a. It is the check that the
    /// two arms of the scorecard hash the same way, and it fails if either side
    /// changes its tagging.
    #[test]
    fn one_integer_row_matches_the_reference_driver() {
        let mut hasher = Digest::new();
        hasher.tag(1);
        hasher.word(1);
        assert_eq!(hasher.finish(), 0x7194_f3e5_9ae4_7dcd);
    }

    /// A ratio and a reciprocal average to no change on a log scale.
    #[test]
    fn a_win_and_a_loss_cancel() {
        let paired = Paired {
            workload: "x".to_string(),
            family: "y".to_string(),
            pairs: vec![(1.0, 2.0), (2.0, 1.0)],
            agreed: true,
            disagreement: String::new(),
        };
        assert!((paired.ratio() - 1.0).abs() < 1.0e-9);
    }

    /// The bootstrap interval brackets the centre and narrows with more data.
    #[test]
    fn the_bootstrap_brackets_the_centre() {
        let tight: Vec<f64> = (0..200)
            .map(|index| 0.5 + (index % 3) as f64 * 0.001)
            .collect();
        let (low, high) = bootstrap(&tight, 7);
        assert!(low < 0.5015 && high > 0.4995, "{low} {high}");
        let loose: Vec<f64> = (0..200)
            .map(|index| 0.5 + (index % 17) as f64 * 0.1)
            .collect();
        let (wide_low, wide_high) = bootstrap(&loose, 7);
        assert!(wide_high - wide_low > high - low);
    }

    /// Builds a workload whose every round has the given speedup, nudged by a
    /// small repeating amount so the rounds are not identical.
    ///
    /// @param name - the workload name
    /// @param speedup - SQLite's time over this engine's, before the nudge
    fn steady_workload(name: &str, speedup: f64) -> Paired {
        Paired {
            workload: name.to_string(),
            family: "read.join".to_string(),
            pairs: (0..30)
                .map(|round| {
                    let nudge = 1.0 + ((round % 5) as f64 - 2.0) * 0.005;
                    (1.0, speedup * nudge)
                })
                .collect(),
            agreed: true,
            disagreement: String::new(),
        }
    }

    /// Two workloads measured to within 1% give a family interval about as
    /// narrow as theirs, however far apart the two are (task-2086).
    ///
    /// These are `read.join`'s pinned figures from task-2082. A single list of
    /// all sixty rounds gave 2.8x to 6.5x for the same two workloads; this
    /// asserts the interval stays within 3% of the centre, and the pooled
    /// figure is computed beside it so the test fails if the two ever agree.
    #[test]
    fn a_family_interval_measures_noise_not_the_gap_between_workloads() {
        let selective = steady_workload("join.selective", 21.0);
        let range = steady_workload("join.range", 0.875);
        let members = [&selective, &range];
        let (centre, low, high) = family_interval(&members, 7);
        let expected = (21.0_f64 * 0.875).sqrt();
        assert!(
            (centre / expected - 1.0).abs() < 0.01,
            "{centre} {expected}"
        );
        assert!(
            low > centre * 0.97 && high < centre * 1.03,
            "{low} {centre} {high}"
        );
        assert!(low > 3.0, "{low}");
        let pooled: Vec<f64> = members
            .iter()
            .flat_map(|paired| paired.log_ratios())
            .collect();
        let (pooled_low, _) = bootstrap(&pooled, 7);
        assert!(pooled_low.exp() < 3.0, "{}", pooled_low.exp());
    }

    /// A family whose workloads ran different numbers of rounds is cut to the
    /// shortest, and a family with no rounds reports zeros.
    #[test]
    fn a_family_interval_uses_the_rounds_every_workload_has() {
        let long = steady_workload("join.selective", 4.0);
        let mut short = steady_workload("join.range", 1.0);
        short.pairs.truncate(10);
        let (centre, _, _) = family_interval(&[&long, &short], 7);
        assert!((centre - 2.0).abs() < 0.02, "{centre}");
        short.pairs.clear();
        assert_eq!(family_interval(&[&long, &short], 7), (0.0, 0.0, 0.0));
    }

    /// A round in which one workload has no usable time is left out whole
    /// (task-2093). Keeping the other workload's value would make that round
    /// 4.0x alone instead of about 2.0x, and move the centre to about 2.05x.
    #[test]
    fn a_family_interval_leaves_out_a_round_a_workload_is_missing_from() {
        let fast = steady_workload("join.selective", 4.0);
        let mut even = steady_workload("join.range", 1.0);
        if let Some(pair) = even.pairs.get_mut(3) {
            *pair = (0.0, 1.0);
        }
        let (centre, _, _) = family_interval(&[&fast, &even], 7);
        assert!((centre - 2.0).abs() < 0.01, "{centre}");
    }

    /// The verdicts are the thresholds the TDD names.
    #[test]
    fn the_verdicts_are_the_declared_thresholds() {
        assert_eq!(Verdict::of(1.25, 1.40), Verdict::Win);
        assert_eq!(Verdict::of(0.96, 1.03), Verdict::Equivalent);
        assert_eq!(Verdict::of(0.70, 0.90), Verdict::Loss);
        assert_eq!(Verdict::of(0.90, 1.30), Verdict::Inconclusive);
    }

    /// A contract whose weights do not sum to one is refused.
    #[test]
    fn a_contract_must_sum_to_one() {
        let text = "headline = \"1.50\"\nfloor = \"0.90\"\n\n[[family]]\nid = \"a\"\nweight = \"0.4\"\n\n[[family]]\nid = \"b\"\nweight = \"0.4\"\n";
        assert!(Contract::parse(text).is_err());
        let fixed = text.replace(
            "weight = \"0.4\"\n\n[[family]]\nid = \"b\"\nweight = \"0.4\"",
            "weight = \"0.5\"\n\n[[family]]\nid = \"b\"\nweight = \"0.5\"",
        );
        let contract = Contract::parse(&fixed).expect("it parses");
        assert_eq!(contract.families.len(), 2);
        assert!((contract.weight_of("a") - 0.5).abs() < 1.0e-9);
        assert!((contract.headline - 1.50).abs() < 1.0e-9);
    }

    /// Builds a paired result with the same ratio in every round.
    fn paired(workload: &str, family: &str, ratio: f64, rounds: usize, agreed: bool) -> Paired {
        Paired {
            workload: workload.to_string(),
            family: family.to_string(),
            pairs: if agreed {
                (0..rounds).map(|_| (1.0, ratio)).collect()
            } else {
                Vec::new()
            },
            agreed,
            disagreement: if agreed {
                String::new()
            } else {
                "row 1 differs".to_string()
            },
        }
    }

    /// A workload whose engines disagreed contributes nothing to the headline.
    #[test]
    fn a_disagreeing_workload_is_not_timed() {
        let measured = vec![
            paired("a", "read.point", 2.0, 4, true),
            paired("b", "read.point", 8.0, 0, false),
        ];
        let rounds = qualified_rounds(&measured);
        assert_eq!(
            rounds.len(),
            4,
            "the agreeing workload still has its rounds"
        );
        for round in &rounds {
            assert_eq!(round.len(), 1, "only the agreeing workload is in the round");
        }
    }

    /// A correctness failure does not silently become a headline of 1.000x.
    ///
    /// The depth is taken over the agreeing workloads. Taking it over all of
    /// them made one failure collapse the whole run to zero rounds, an empty
    /// bootstrap, and a reported speedup of exactly one.
    #[test]
    fn one_failure_does_not_report_parity() {
        let measured = vec![
            paired("a", "read.point", 2.0, 6, true),
            paired("b", "write", 2.0, 6, true),
            paired("c", "write", 1.0, 0, false),
        ];
        let contract = Contract::parse(
            "headline = \"1.50\"\nfloor = \"0.90\"\n\n[[family]]\nid = \"read.point\"\n\
             weight = \"0.5\"\nrequired = true\ndescription = \"point reads\"\n\n\
             [[family]]\nid = \"write\"\nweight = \"0.5\"\nrequired = true\n\
             description = \"writes\"\n",
        )
        .expect("the contract parses");
        let (centre, low, high) = weighted_headline(&qualified_rounds(&measured), &contract, 7);
        assert!(
            (centre - 2.0).abs() < 1.0e-6,
            "the headline is the agreeing workloads' ratio, not parity: {centre}"
        );
        assert!(low > 1.9 && high < 2.1, "[{low}, {high}]");
    }

    /// A round in which an engine reported no time at all is dropped.
    #[test]
    fn a_zero_duration_is_not_a_ratio() {
        let mut measured = vec![paired("a", "read.point", 2.0, 3, true)];
        if let Some(first) = measured.first_mut() {
            first.pairs.push((0.0, 5.0));
        }
        let rounds = qualified_rounds(&measured);
        assert_eq!(rounds.len(), 4);
        assert!(
            rounds.last().map(Vec::is_empty).unwrap_or(false),
            "the zero round contributes nothing"
        );
    }

    /// The headline weights families rather than counting workloads.
    #[test]
    fn the_headline_weights_families() {
        let text = "headline = \"1.50\"\nfloor = \"0.90\"\n\n[[family]]\nid = \"read\"\nweight = \"0.5\"\n\n[[family]]\nid = \"write\"\nweight = \"0.5\"\n";
        let contract = Contract::parse(text).expect("it parses");
        // Three read workloads at 2x and one write workload at 0.5x average to
        // no change, because the families weigh the same however many cases
        // each one happens to have.
        let round = vec![
            ("read".to_string(), 2.0f64.ln()),
            ("read".to_string(), 2.0f64.ln()),
            ("read".to_string(), 2.0f64.ln()),
            ("write".to_string(), 0.5f64.ln()),
        ];
        let (centre, _, _) = weighted_headline(&[round], &contract, 3);
        assert!((centre - 1.0).abs() < 1.0e-9, "{centre}");
    }
}
