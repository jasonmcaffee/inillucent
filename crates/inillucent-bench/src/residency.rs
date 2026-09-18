//! What it costs to load the embedding model, and what that decides.
//!
//! The question this answers is a product one. An application that searches by
//! meaning has to have the embedding model in memory for the length of one
//! query and no longer than that - the model is 547 MB and the query takes
//! milliseconds - so the obvious arrangement is to load it, embed, and drop it.
//! Whether that arrangement is usable depends entirely on one number: how long
//! ONNX Runtime takes to open a session on this file. If it is 50 ms, loading
//! per query is free and nothing needs to stay resident. If it is two seconds,
//! a search that used to take 20 ms now takes two seconds and the model has to
//! stay.
//!
//! So this module measures that number, and the levers that move it, on real
//! files rather than from a model card:
//!
//! | lever | what it changes |
//! |---|---|
//! | graph optimization level | how many passes ONNX Runtime runs before the session is usable |
//! | a pre-optimized graph on disk | the same passes, paid once, at the cost of a second copy of the weights |
//! | the weights' precision | how many bytes are read and converted |
//! | the device | whether the weights are copied to a card as well |
//!
//! Every arm reports the same four times so they can be added up: opening the
//! session, the first embedding through it, a later embedding through it, and
//! dropping it. **The first embedding is measured separately from the later
//! ones on purpose** - ONNX Runtime defers a good deal of allocation to the
//! first `Run`, so an arm graded on its steady-state rate alone would hide work
//! that a load-per-query caller pays on every single query.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use inillucent_core::embed::Embedder;
use inillucent_core::embed_onnx::{Device, OnnxEmbedder, OnnxOptions, Optimization};
use inillucent_core::model::ModelManifest;

/// One configuration under test.
pub struct Arm {
    /// What the card calls it.
    pub name: String,
    /// The directory holding the weights and the tokenizer.
    pub dir: PathBuf,
    /// The weights file inside it, which is how a precision variant is selected.
    pub model_file: String,
    /// How much graph optimization runs at load.
    pub optimization: Optimization,
    /// Where the optimized graph is written, when this arm is producing one.
    pub optimized_out: Option<PathBuf>,
    /// Which processor.
    pub device: Device,
    /// Threads ONNX Runtime uses inside one operator, when the arm pins it.
    pub intra_threads: Option<usize>,
}

/// What one arm cost, in milliseconds, over `repeats` load and drop cycles.
pub struct Measurement {
    pub name: String,
    /// Bytes of weights read from disk.
    pub weights_bytes: u64,
    /// Opening the session: every millisecond a load-per-query caller pays
    /// before it can embed anything.
    pub open_ms: Vec<f64>,
    /// The first `Run` through a newly opened session.
    pub first_embed_ms: Vec<f64>,
    /// A later `Run` through the same session, which is what a resident caller
    /// pays per query.
    pub steady_embed_ms: Vec<f64>,
    /// Dropping the session.
    pub drop_ms: Vec<f64>,
    /// The query vector the arm produced, kept so two arms can be compared for
    /// having computed the same thing rather than only for having been fast.
    pub vector: Vec<f32>,
}

impl Measurement {
    /// The median of a set of times, which is what the card prints.
    ///
    /// A median rather than a mean because a load competes with whatever else
    /// the machine is doing and one descheduled run should not move the number
    /// the reader compares.
    /// @param values - the samples
    pub fn median(values: &[f64]) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let middle = sorted.len() / 2;
        let upper = sorted.get(middle).copied().unwrap_or(0.0);
        if sorted.len() % 2 == 1 {
            upper
        } else {
            let lower = sorted
                .get(middle.saturating_sub(1))
                .copied()
                .unwrap_or(upper);
            (lower + upper) / 2.0
        }
    }

    /// What one query costs a caller that loads the model for it and drops it
    /// afterwards: the whole cycle, which is the number the ticket asked for.
    pub fn on_demand_ms(&self) -> f64 {
        Self::median(&self.open_ms)
            + Self::median(&self.first_embed_ms)
            + Self::median(&self.drop_ms)
    }

    /// What one query costs a caller that keeps the model loaded.
    pub fn resident_ms(&self) -> f64 {
        Self::median(&self.steady_embed_ms)
    }

    /// Cosine similarity against another arm's vector, so a faster arm that
    /// computes something else is visible as such.
    /// @param other - the arm to compare against, normally the baseline
    pub fn agreement(&self, other: &Measurement) -> f64 {
        if self.vector.len() != other.vector.len() || self.vector.is_empty() {
            return f64::NAN;
        }
        self.vector
            .iter()
            .zip(&other.vector)
            .map(|(a, b)| f64::from(*a) * f64::from(*b))
            .sum()
    }
}

/// The text every arm embeds.
///
/// One realistic query rather than a synthetic string, because the ticket's own
/// example is a person asking their mail for a flight, and a query's token count
/// is what the first `Run` is charged for.
pub const QUERY: &str = "flight details and confirmation number for the trip to Seattle next week";

/// Runs one arm and returns what it cost.
///
/// @param arm - the configuration under test
/// @param manifest - the model contract, which fixes prefixes, pooling and width
/// @param repeats - load and drop cycles; the median of these is reported
/// @param steady - later embeddings taken per cycle, after the first
pub fn measure(
    arm: &Arm,
    manifest: &ModelManifest,
    repeats: usize,
    steady: usize,
) -> Result<Measurement> {
    let weights = arm.dir.join(&arm.model_file);
    let weights_bytes = std::fs::metadata(&weights)
        .with_context(|| format!("looking at {}", weights.display()))?
        .len();

    let mut measurement = Measurement {
        name: arm.name.clone(),
        weights_bytes,
        open_ms: Vec::new(),
        first_embed_ms: Vec::new(),
        steady_embed_ms: Vec::new(),
        drop_ms: Vec::new(),
        vector: Vec::new(),
    };

    for _ in 0..repeats {
        let options = OnnxOptions {
            optimization: arm.optimization,
            optimized_model_path: arm.optimized_out.clone(),
            device: arm.device,
            intra_threads: arm.intra_threads,
            batch_size: 1,
            ..OnnxOptions::for_model(manifest)
        };

        let started = Instant::now();
        let embedder = OnnxEmbedder::open_model(&arm.dir, &arm.model_file, options)
            .with_context(|| format!("opening {} for arm {}", weights.display(), arm.name))?;
        measurement.open_ms.push(millis(started.elapsed()));

        let started = Instant::now();
        let vector = embedder.embed_query(QUERY)?;
        measurement.first_embed_ms.push(millis(started.elapsed()));
        measurement.vector = vector;

        for _ in 0..steady {
            let started = Instant::now();
            let _ = embedder.embed_query(QUERY)?;
            measurement.steady_embed_ms.push(millis(started.elapsed()));
        }

        let started = Instant::now();
        drop(embedder);
        measurement.drop_ms.push(millis(started.elapsed()));
    }

    Ok(measurement)
}

/// A duration in milliseconds, as a float, which is what every column here is.
/// @param elapsed - the measured span
fn millis(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1_000.0
}

/// Builds the arms this study runs, skipping any whose weights file is absent.
///
/// A missing precision variant is skipped rather than fatal because the
/// quantized exports are a download and the fp32 one is what the engine ships
/// against: a machine with only the baseline weights still produces the numbers
/// that decide the residency policy.
///
/// @param dir - the model directory
/// @param optimized_dir - where a pre-optimized graph may be written
/// @param devices - the processors to run every arm on
pub fn arms(dir: &Path, optimized_dir: &Path, devices: &[Device]) -> Vec<Arm> {
    let mut arms = Vec::new();
    for device in devices {
        let suffix = if devices.len() > 1 {
            format!(" on {}", device.label())
        } else {
            String::new()
        };
        for (level, label) in [
            (Optimization::All, "fp32, all optimizations (the default)"),
            (Optimization::Extended, "fp32, extended optimizations"),
            (Optimization::Basic, "fp32, basic optimizations"),
            (Optimization::Disable, "fp32, no optimization"),
        ] {
            arms.push(Arm {
                name: format!("{label}{suffix}"),
                dir: dir.to_path_buf(),
                model_file: "model.onnx".to_string(),
                optimization: level,
                optimized_out: None,
                device: *device,
                intra_threads: None,
            });
        }

        // The pre-optimized graph. The first arm writes it and is not itself
        // interesting - writing 547 MB is not what a caller does per query - so
        // it is produced outside the measured set by `prepare_optimized`.
        let optimized = optimized_dir.join("model-optimized.onnx");
        if optimized.exists() {
            arms.push(Arm {
                name: format!("fp32, pre-optimized graph, no optimization at load{suffix}"),
                dir: optimized_dir.to_path_buf(),
                model_file: "model-optimized.onnx".to_string(),
                optimization: Optimization::Disable,
                optimized_out: None,
                device: *device,
                intra_threads: None,
            });
        }

        // How many threads ONNX Runtime is allowed inside one operator. It is
        // in the table because the obvious guess - that a load is a file read
        // and a thread count cannot move it - is worth checking against the
        // constant folding a load also does.
        for threads in [1usize, 4] {
            arms.push(Arm {
                name: format!("fp32, all optimizations, {threads} intra-op threads{suffix}"),
                dir: dir.to_path_buf(),
                model_file: "model.onnx".to_string(),
                optimization: Optimization::All,
                optimized_out: None,
                device: *device,
                intra_threads: Some(threads),
            });
        }

        // The precision variants are looked for beside the optimized graph as
        // well as in the model directory, because they are a separate download
        // and writing them into the directory a manifest seals would move that
        // manifest's digest - which would make every cache on disk unreadable
        // to a comparison, for a reason that has nothing to do with any model.
        for (file, label) in [
            ("model_fp16.onnx", "fp16 weights, all optimizations"),
            ("model_int8.onnx", "int8 weights, all optimizations"),
            (
                "model_quantized.onnx",
                "quantized weights, all optimizations",
            ),
        ] {
            for home in [dir, optimized_dir] {
                if home.join(file).exists() {
                    arms.push(Arm {
                        name: format!("{label}{suffix}"),
                        dir: home.to_path_buf(),
                        model_file: file.to_string(),
                        optimization: Optimization::All,
                        optimized_out: None,
                        device: *device,
                        intra_threads: None,
                    });
                    break;
                }
            }
        }
    }
    arms
}

/// Writes the optimized graph once, so the arm that loads it has something to
/// load.
///
/// @param dir - the model directory
/// @param manifest - the model contract
/// @param out_dir - where the optimized graph and a copy of the tokenizer go
pub fn prepare_optimized(dir: &Path, manifest: &ModelManifest, out_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let target = out_dir.join("model-optimized.onnx");
    // The tokenizer travels with the graph, because `OnnxEmbedder::open_model`
    // reads both out of one directory and an optimized graph beside somebody
    // else's tokenizer is a wrong answer rather than a missing file.
    std::fs::copy(dir.join("tokenizer.json"), out_dir.join("tokenizer.json"))
        .context("copying the tokenizer beside the optimized graph")?;
    if target.exists() {
        return Ok(target);
    }
    let options = OnnxOptions {
        optimization: Optimization::All,
        optimized_model_path: Some(target.clone()),
        batch_size: 1,
        ..OnnxOptions::for_model(manifest)
    };
    let embedder = OnnxEmbedder::open_model(dir, &manifest.model_file, options)
        .context("opening the model to write its optimized graph")?;
    // ONNX Runtime writes the file while the session is being built, so the
    // session exists only to have caused it.
    drop(embedder);
    anyhow::ensure!(
        target.exists(),
        "ONNX Runtime did not write {}; this build may not support serializing an optimized graph",
        target.display()
    );
    Ok(target)
}

/// Prints the card, in markdown, on standard output.
///
/// @param measurements - every arm that ran, the first of which is the baseline
pub fn report(measurements: &[Measurement]) {
    let Some(baseline) = measurements.first() else {
        return;
    };
    println!("| arm | weights | first open | open | first embed | later embed | drop | load per query | resident per query | agrees with the baseline |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for m in measurements {
        println!(
            "| {} | {:.0} MB | {:.0} ms | {:.0} ms | {:.1} ms | {:.1} ms | {:.0} ms | **{:.0} ms** | **{:.1} ms** | {:.4} |",
            m.name,
            m.weights_bytes as f64 / (1024.0 * 1024.0),
            m.open_ms.first().copied().unwrap_or(0.0),
            Measurement::median(&m.open_ms),
            Measurement::median(&m.first_embed_ms),
            Measurement::median(&m.steady_embed_ms),
            Measurement::median(&m.drop_ms),
            m.on_demand_ms(),
            m.resident_ms(),
            m.agreement(baseline),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The median is the middle sample, and the mean of the two middles when
    /// there is no single middle.
    #[test]
    fn the_median_is_the_middle_sample() {
        assert_eq!(Measurement::median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(Measurement::median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(Measurement::median(&[]), 0.0);
    }

    /// A load-per-query caller pays the open, the first embedding and the drop,
    /// and never the steady-state one.
    #[test]
    fn a_load_per_query_caller_pays_the_open_the_first_embed_and_the_drop() {
        let m = Measurement {
            name: "x".to_string(),
            weights_bytes: 0,
            open_ms: vec![100.0],
            first_embed_ms: vec![20.0],
            steady_embed_ms: vec![5.0],
            drop_ms: vec![3.0],
            vector: Vec::new(),
        };
        assert_eq!(m.on_demand_ms(), 123.0);
        assert_eq!(m.resident_ms(), 5.0);
    }

    /// Two arms that produced the same unit vector agree exactly; two that
    /// produced different ones do not.
    #[test]
    fn agreement_is_the_cosine_between_two_arms_vectors() {
        let a = Measurement {
            name: "a".to_string(),
            weights_bytes: 0,
            open_ms: Vec::new(),
            first_embed_ms: Vec::new(),
            steady_embed_ms: Vec::new(),
            drop_ms: Vec::new(),
            vector: vec![1.0, 0.0],
        };
        let b = Measurement {
            name: "b".to_string(),
            vector: vec![0.0, 1.0],
            ..clone_of(&a)
        };
        assert!((a.agreement(&a) - 1.0).abs() < 1e-6);
        assert!(a.agreement(&b).abs() < 1e-6);
    }

    /// `Measurement` holds no handle, so a test copy is a field copy.
    fn clone_of(m: &Measurement) -> Measurement {
        Measurement {
            name: m.name.clone(),
            weights_bytes: m.weights_bytes,
            open_ms: m.open_ms.clone(),
            first_embed_ms: m.first_embed_ms.clone(),
            steady_embed_ms: m.steady_embed_ms.clone(),
            drop_ms: m.drop_ms.clone(),
            vector: m.vector.clone(),
        }
    }
}
