//! Where a retrieval index's resident bytes go, part by part.
//!
//! Invariant: **each part is read in the order an open reads it, and the
//! resident set is sampled between them.** The difference between two samples is
//! what that part cost, measured rather than derived from the file's size - a
//! file's bytes and a heap's bytes are different numbers, and the whole question
//! this answers is by how much.
//!
//! `docs/roadmap.md` item 3 says an index is 1.3 GB resident for 3.1 GB on disk
//! with the vectors left in the file, and that nothing has said which part that
//! is. This says. It is the first of the three landings that item asks for, and
//! the other two - the graph behind the buffer pool, then the postings - are
//! designs against whichever number this reports as the largest.
//!
//! **The parts are read directly rather than through `persist::load`**, because
//! `load` reads all five and hands back one `Index`: a sample taken after it
//! could only ever report the total. Each reader here is the same public
//! function `load` calls, in the same order, so what is measured is the same
//! work rather than a re-implementation of it.
//!
//! Usage: `inillucent-indexresidency <index directory> [--resident-vectors]`

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use inillucent_compat::procstat::{mebibytes, ProcessCost};

/// How large a read buffer each part gets, matching `persist::load_generation`.
const BUFFER: usize = 1 << 20;

/// The section tags `inillucent_core::persist` writes, which are private there.
///
/// Copied rather than reached for because they are a *format* constant - a file
/// written by any build carries them - and `opened` checks the byte it finds
/// against the one it expects, so a mismatch is a refusal rather than a wrong
/// number.
const KIND_STORE: u8 = 1;
const KIND_VECTORS: u8 = 2;
const KIND_GRAPH: u8 = 3;
const KIND_LEXICAL: u8 = 5;

/// Where the first vector sits in `vectors.bin`: the header, then the width and
/// the count.
const VECTOR_HEADER_BYTES: u64 = 8 + 4 + 1 + 8;

/// Runs the measurement and prints the table.
fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let Some(directory) = arguments.next() else {
        eprintln!("usage: inillucent-indexresidency <index directory> [--resident-vectors]");
        return ExitCode::from(2);
    };
    let resident_vectors = arguments.any(|flag| flag == "--resident-vectors");
    match run(Path::new(&directory), resident_vectors) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("indexresidency: {failure}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the generation directory the index's `current` file names.
///
/// @param directory - the index directory
fn generation(directory: &Path) -> Result<PathBuf, String> {
    let named = std::fs::read_to_string(directory.join("current"))
        .map_err(|failure| format!("{} has no `current` file: {failure}", directory.display()))?;
    let name = named.trim();
    if name.is_empty() {
        return Err(format!("{}'s `current` file is empty", directory.display()));
    }
    Ok(directory.join(name))
}

/// Opens one part of the generation, past its header.
///
/// @param generation - the generation directory
/// @param name - the file
/// @param kind - the header byte that file carries
fn opened(generation: &Path, name: &str, kind: u8) -> Result<BufReader<std::fs::File>, String> {
    let file = std::fs::File::open(generation.join(name))
        .map_err(|failure| format!("{name}: {failure}"))?;
    let mut reader = BufReader::with_capacity(BUFFER, file);
    // Eight magic bytes, a four byte format version and a one byte section tag,
    // which is `persist::check_header`'s layout. The tag is checked rather than
    // skipped, so a file that is not the one it is named after is a refusal
    // rather than a wrong number.
    let mut header = [0u8; 13];
    reader
        .read_exact(&mut header)
        .map_err(|failure| format!("{name}: {failure}"))?;
    match header.last() {
        Some(held) if *held == kind => Ok(reader),
        Some(held) => Err(format!(
            "{name} carries section {held} where {kind} was expected, so it is not the file it is \
             named after"
        )),
        None => Err(format!("{name} is too short to hold a header")),
    }
}

/// Reads each part in turn and prints what it cost.
///
/// @param directory - the index directory
/// @param resident_vectors - whether to hold the vectors on the heap
fn run(directory: &Path, resident_vectors: bool) -> Result<(), String> {
    let generation = generation(directory)?;
    println!("  index      : {}", directory.display());
    println!("  generation : {}", generation.display());
    println!(
        "  vectors    : {}",
        match resident_vectors {
            true => "held on the heap",
            false => "left in the file, which is the default",
        }
    );

    let mut rows: Vec<(String, u64, u64, u64)> = Vec::new();
    let opening = ProcessCost::now();

    // The store: the chunk text and its dictionaries.
    let store = {
        let mut reader = opened(&generation, "store.bin", KIND_STORE)?;
        inillucent_core::store::Store::read_from(&mut reader)
            .map_err(|failure| format!("reading the store: {failure}"))?
    };
    let after_store = ProcessCost::now();
    rows.push((
        "store.bin (the chunks and their dictionaries)".to_string(),
        file_size(&generation, "store.bin"),
        after_store.working_set.saturating_sub(opening.working_set),
        after_store.peak_working_set,
    ));

    // The vectors, filed by default: the header is read either way, because the
    // width and the count are what say where a vector is.
    let vectors = {
        let path = generation.join("vectors.bin");
        let mut reader = opened(&generation, "vectors.bin", KIND_VECTORS)?;
        let mut four = [0u8; 4];
        reader
            .read_exact(&mut four)
            .map_err(|failure| format!("reading the vector width: {failure}"))?;
        let dims = u32::from_le_bytes(four) as usize;
        reader
            .read_exact(&mut four)
            .map_err(|failure| format!("reading the vector count: {failure}"))?;
        let count = u32::from_le_bytes(four) as usize;
        let metric = inillucent_core::distance::Metric::Cosine;
        if resident_vectors {
            let held = inillucent_core::binio::read_pod_vec::<f32>(&mut reader, dims * count)
                .map_err(|failure| format!("reading the vectors: {failure}"))?;
            inillucent_core::vectors::VectorSet::from_raw(dims, metric, held)
        } else {
            let file = std::fs::File::open(&path)
                .map_err(|failure| format!("reopening the vectors: {failure}"))?;
            inillucent_core::vectors::VectorSet::from_file(
                dims,
                metric,
                count,
                file,
                VECTOR_HEADER_BYTES,
            )
        }
    };
    let after_vectors = ProcessCost::now();
    rows.push((
        "vectors.bin".to_string(),
        file_size(&generation, "vectors.bin"),
        after_vectors
            .working_set
            .saturating_sub(after_store.working_set),
        after_vectors.peak_working_set,
    ));

    // The graph: the HNSW adjacency lists, one per node per layer.
    let graph = {
        let mut reader = opened(&generation, "graph.bin", KIND_GRAPH)?;
        inillucent_core::hnsw::Hnsw::read_graph(
            &mut reader,
            inillucent_core::hnsw::HnswParams::default(),
        )
        .map_err(|failure| format!("reading the graph: {failure}"))?
    };
    let after_graph = ProcessCost::now();
    rows.push((
        "graph.bin (the HNSW adjacency)".to_string(),
        file_size(&generation, "graph.bin"),
        after_graph
            .working_set
            .saturating_sub(after_vectors.working_set),
        after_graph.peak_working_set,
    ));

    // The postings: the BM25 term dictionary and its doclists.
    let lexical = {
        let mut reader = opened(&generation, "lexical.bin", KIND_LEXICAL)?;
        inillucent_core::bm25::Bm25Index::read_from(&mut reader)
            .map_err(|failure| format!("reading the postings: {failure}"))?
    };
    let after_lexical = ProcessCost::now();
    rows.push((
        "lexical.bin (the BM25 postings)".to_string(),
        file_size(&generation, "lexical.bin"),
        after_lexical
            .working_set
            .saturating_sub(after_graph.working_set),
        after_lexical.peak_working_set,
    ));

    println!("\n## what each part costs, read in the order an open reads them");
    println!(
        "  {:<46} {:>12} {:>14} {:>14}",
        "part", "on disk MiB", "resident MiB", "peak so far MiB"
    );
    let mut resident = 0u64;
    for (name, bytes, added, peak) in &rows {
        resident = resident.saturating_add(*added);
        println!(
            "  {:<46} {:>12.1} {:>14.1} {:>14.1}",
            name,
            mebibytes(*bytes),
            mebibytes(*added),
            mebibytes(*peak)
        );
    }
    let total_disk: u64 = rows.iter().map(|(_, bytes, _, _)| *bytes).sum();
    println!(
        "  {:<46} {:>12.1} {:>14.1} {:>14.1}",
        "total",
        mebibytes(total_disk),
        mebibytes(resident),
        mebibytes(ProcessCost::now().peak_working_set)
    );

    // Held to here so nothing is dropped before the last sample, which would
    // make a part look free because the one after it released its bytes.
    println!(
        "\n  {} chunks, {} vectors at {} dimensions, {} terms",
        store.n_chunks(),
        vectors.len(),
        vectors.dims(),
        lexical.n_terms()
    );
    drop(graph);
    Ok(())
}

/// Returns one file's size in bytes, or zero when it cannot be read.
///
/// @param generation - the generation directory
/// @param name - the file
fn file_size(generation: &Path, name: &str) -> u64 {
    std::fs::metadata(generation.join(name))
        .map(|held| held.len())
        .unwrap_or(0)
}
