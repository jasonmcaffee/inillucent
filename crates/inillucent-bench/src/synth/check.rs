//! Checking the generated corpus before paying for the embedding run.
//!
//! Invariant: **every count here is produced by the function the graded run
//! uses.** Embedding the corpus takes hours, and a corpus that cannot supply
//! identifier queries, or whose chunk order does not correlate with source,
//! produces a score card with empty or misleading scenarios - which is found
//! out after the embedding run rather than before it. Reimplementing the
//! filters here and hoping the two agreed would make this check a second
//! opinion rather than the same measurement.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use inillucent_core::store::ChunkInput;

use super::{read_corpus, SynthChunk};

/// Report whether the corpus supports the three ground truths, and whether it has
/// the structural properties the scenarios rely on.
///
/// This exists because embedding the corpus takes hours. A corpus that cannot
/// supply identifier queries, or whose chunk order does not correlate with source,
/// produces a score card with empty or misleading scenarios, and finding that out
/// after the embedding run wastes most of a day. Every count here is produced by
/// the same functions the graded run uses, so agreement is not a matter of
/// reimplementing the filters and hoping they match.
///
/// **One helper per property, and each one prints what it found before it
/// refuses.** The order matters: the ingestion-order check names the sources,
/// and the answerability check reads the query sets the generation check
/// produced, so each stage takes what the one before it measured.
///
/// @param corpus_path - the chunk file to check
/// @param per_source - the per-source query count the graded run will ask for
pub fn check(corpus_path: &Path, per_source: usize) -> Result<()> {
    let chunks = read_corpus(corpus_path)?;
    anyhow::ensure!(!chunks.is_empty(), "the corpus file is empty");
    let inputs: Vec<ChunkInput> = chunks.iter().map(SynthChunk::to_input).collect();
    let keys: Vec<String> = chunks
        .iter()
        .map(|c| format!("{}#{}", c.doc_id, c.chunk_index))
        .collect();

    println!("corpus: {} chunks", chunks.len());
    check_keys_are_unique(&keys)?;
    let all_sources = check_the_ingestion_order_was_reproduced(&chunks)?;
    let families = check_every_ground_truth_is_generated(&inputs, &keys, per_source, &all_sources)?;
    check_every_ground_truth_is_answerable(&chunks, &keys, &families)?;
    check_chunks_carry_their_own_title_and_heading(&chunks)?;
    check_some_documents_are_soft_deleted(&chunks)?;

    println!("\nthe corpus supports every graded scenario");
    Ok(())
}

/// No two chunks answer to the same key.
///
/// @param keys - the corpus key per chunk, in corpus order
fn check_keys_are_unique(keys: &[String]) -> Result<()> {
    // Keys have to be unique, or two different chunks would be the same answer.
    let unique: std::collections::HashSet<&String> = keys.iter().collect();
    println!("  distinct keys: {} of {}", unique.len(), keys.len());
    anyhow::ensure!(
        unique.len() == keys.len(),
        "the corpus contains duplicate keys"
    );
    Ok(())
}

/// The corpus arrives one source at a time and then interleaves, and answers
/// with every source it holds.
///
/// **Asserted rather than assumed, because the scenarios sample with a stride
/// precisely because a prefix is one source.** A corpus written in source order
/// throughout, or interleaved throughout, would make strided sampling measure
/// nothing and nothing else would say so.
///
/// @param chunks - the corpus, in corpus order
fn check_the_ingestion_order_was_reproduced(
    chunks: &[SynthChunk],
) -> Result<std::collections::BTreeSet<&str>> {
    // Chunk order against source. The scenarios sample with a stride precisely
    // because a prefix is one source, so that property is asserted, not assumed.
    let tenth = chunks.len() / 10;
    let prefix_sources: std::collections::BTreeSet<&str> = chunks
        .get(..tenth)
        .unwrap_or(&[])
        .iter()
        .map(|c| c.source.as_str())
        .collect();
    let all_sources: std::collections::BTreeSet<&str> =
        chunks.iter().map(|c| c.source.as_str()).collect();
    println!(
        "  sources in the first tenth: {:?}, in the whole corpus: {:?}",
        prefix_sources, all_sources
    );
    anyhow::ensure!(
        prefix_sources.len() < all_sources.len(),
        "a prefix of the corpus covers every source, so the ingestion order was not reproduced \
         and strided sampling is measuring nothing"
    );

    // The last tenth should interleave, which is what makes a stride work.
    let tail_sources: std::collections::BTreeSet<&str> = chunks
        .get(chunks.len().saturating_sub(tenth)..)
        .unwrap_or(&[])
        .iter()
        .map(|c| c.source.as_str())
        .collect();
    println!("  sources in the last tenth: {:?}", tail_sources);
    anyhow::ensure!(
        tail_sources.len() == all_sources.len(),
        "the tail does not interleave every source"
    );
    Ok(all_sources)
}

/// The three query sets a graded run generates, and whether the corpus supplies
/// enough of each.
///
/// @param inputs - the corpus as the index takes it
/// @param keys - the corpus key per chunk, in corpus order
/// @param per_source - the per-source query count the graded run will ask for
/// @param all_sources - every source the corpus holds
fn check_every_ground_truth_is_generated(
    inputs: &[ChunkInput],
    keys: &[String],
    per_source: usize,
    all_sources: &std::collections::BTreeSet<&str>,
) -> Result<GeneratedFamilies> {
    println!("\nground truth");
    let identity = crate::queryset::document_identity_queries(inputs, keys, per_source, 11);
    let mut by_source: HashMap<&str, usize> = HashMap::new();
    for q in &identity {
        *by_source.entry(q.source.as_str()).or_insert(0) += 1;
    }
    println!("  document identity queries: {} in total", identity.len());
    // Iterating the sources present in the tally would skip a source that supplies
    // none, which is exactly the failure worth catching: the code shaped source
    // contributed nothing until its titles were given a second word.
    for source in all_sources {
        let count = by_source.get(source).copied().unwrap_or(0);
        println!("    {source:<11} {count}");
        anyhow::ensure!(
            count >= per_source.min(10),
            "{source} supplies only {count} document identity queries; its titles are not usable \
             as queries, which empties the ground truth that grades fusion"
        );
    }

    let headings = crate::queryset::heading_queries(inputs, keys, per_source * 3, 12);
    println!("  natural language heading queries: {}", headings.len());
    anyhow::ensure!(
        headings.len() >= per_source,
        "only {} heading queries; headings need to be 15 or more characters and three or more \
         words, appearing in at most four chunks",
        headings.len()
    );

    let identifiers = crate::queryset::identifier_queries(inputs, keys, per_source * 3, 13);
    println!("  rare identifier queries: {}", identifiers.len());
    anyhow::ensure!(
        identifiers.len() >= per_source,
        "only {} identifier queries; the corpus needs more rare literal tokens, which come from \
         source code and from the ticket keys threaded through the prose sources",
        identifiers.len()
    );
    if let Some(example) = identifiers.first() {
        println!(
            "    for example {:?}, correct in {} chunk(s)",
            example.text,
            example.correct.len()
        );
    }
    Ok(GeneratedFamilies {
        identity,
        headings,
        identifiers,
    })
}

/// The query sets `check` generated, so the answerability pass can read them.
struct GeneratedFamilies {
    /// A document's own title as the query.
    identity: Vec<crate::queryset::GradedQuery>,
    /// A section heading as the query.
    headings: Vec<crate::queryset::GradedQuery>,
    /// A rare literal token as the query.
    identifiers: Vec<crate::queryset::GradedQuery>,
}

/// Whether a correct chunk actually contains the query text.
///
/// @param chunks - the corpus, in corpus order
/// @param keys - the corpus key per chunk, in corpus order
/// @param families - the query sets the generation pass produced
fn check_every_ground_truth_is_answerable(
    chunks: &[SynthChunk],
    keys: &[String],
    families: &GeneratedFamilies,
) -> Result<()> {
    let identity = &families.identity;
    let headings = &families.headings;
    let identifiers = &families.identifiers;
    // Can the ground truths be *answered*, not merely generated?
    //
    // This is the check that was missing, and its absence cost two full embedding
    // runs. The generation checks above pass happily on a corpus where the queries
    // are unanswerable: a title query whose document never mentions its own title, or
    // a heading query whose chunks do not contain the heading. Both engines then
    // score near zero together, which reads like a corpus that is merely harder.
    //
    // The corpus this one reproduces had both properties at 100%: every chunk carried
    // its document title in its body, and every chunk with a heading carried that
    // heading. So the check is that a correct answer contains the query text, which
    // is the weakest thing that makes the question answerable at all.
    println!("\nare the ground truths answerable");
    let content_of: HashMap<&str, &str> = keys
        .iter()
        .zip(chunks.iter())
        .map(|(k, c)| (k.as_str(), c.content.as_str()))
        .collect();

    // An identifier query is not a substring of the chunk. The generator strips every
    // character that is not alphanumeric, a dash or an underscore from a whitespace
    // separated word, so `exists_method(return_value)` yields the token
    // `exists_methodreturn_value`, which appears nowhere literally. Checking those by
    // substring would report a defect that is really a property of the generator, so
    // they are checked by applying the same filtering to the chunk's own words.
    let filtered_words = |content: &str| -> Vec<String> {
        content
            .split_whitespace()
            .map(|raw| {
                raw.chars()
                    .filter(|ch| ch.is_alphanumeric() || *ch == '-' || *ch == '_')
                    .collect::<String>()
            })
            .collect()
    };

    for (label, queries, substring, floor) in [
        ("document identity", &identity, true, 0.99),
        ("natural language headings", &headings, true, 0.99),
        ("rare identifiers", &identifiers, false, 0.99),
    ] {
        let mut answerable = 0usize;
        let mut total = 0usize;
        for q in queries.iter() {
            let needle = q.text.trim().to_lowercase();
            let reachable = q.correct.iter().any(|k| {
                content_of
                    .get(k.as_str())
                    .map(|c| {
                        if substring {
                            c.to_lowercase().contains(&needle)
                        } else {
                            // Lowercased the same way the needle was, rather than
                            // compared ASCII case insensitively. Tokens are cut with
                            // `char::is_alphanumeric`, which is Unicode aware, so a
                            // token can hold a letter outside ASCII: `Bogotá-2019`
                            // lowercases to `bogotá-2019` while an ASCII fold leaves
                            // the accented letter alone and the two never match. That
                            // reported a perfectly answerable query as unanswerable.
                            filtered_words(c).iter().any(|w| w.to_lowercase() == needle)
                        }
                    })
                    .unwrap_or(false)
            });
            total += 1;
            answerable += usize::from(reachable);
        }
        let share = if total == 0 {
            0.0
        } else {
            answerable as f64 / total as f64
        };
        println!("  {label:<26} {answerable}/{total} have a correct chunk containing the query text ({:.1}%)", share * 100.0);
        anyhow::ensure!(
            share >= floor,
            "only {:.1}% of {label} queries have any correct chunk that contains the query text. \
             Those queries cannot be answered by either engine, so the scenario measures nothing \
             and both engines will score near zero on it. In the corpus this one reproduces, every \
             chunk carried its document title and its heading in its own text",
            share * 100.0
        );
    }
    Ok(())
}

/// Every chunk carries its document title, and every chunk with a heading
/// carries that heading.
///
/// **The two properties the answerability pass above rests on, asserted
/// directly so a change to the chunk format cannot quietly remove them.**
///
/// @param chunks - the corpus, in corpus order
fn check_chunks_carry_their_own_title_and_heading(chunks: &[SynthChunk]) -> Result<()> {
    // The two structural properties that make the above true, asserted directly so a
    // change to the chunk format cannot quietly remove them.
    let mut with_title = 0usize;
    let mut with_heading = 0usize;
    let mut heading_bearing = 0usize;
    for c in chunks {
        if c.content
            .to_lowercase()
            .contains(&c.title.trim().to_lowercase())
        {
            with_title += 1;
        }
        if let Some(leaf) = c.heading_path.last() {
            heading_bearing += 1;
            if c.content
                .to_lowercase()
                .contains(&leaf.trim().to_lowercase())
            {
                with_heading += 1;
            }
        }
    }
    println!(
        "  chunks carrying their own title: {with_title}/{} ({:.1}%)",
        chunks.len(),
        100.0 * with_title as f64 / chunks.len() as f64
    );
    println!(
        "  chunks carrying their own leaf heading: {with_heading}/{heading_bearing} ({:.1}%)",
        100.0 * with_heading as f64 / heading_bearing.max(1) as f64
    );
    anyhow::ensure!(
        with_title * 100 >= chunks.len() * 99,
        "only {with_title} of {} chunks contain their document title; the original corpus had all \
         of them, and the document identity scenario depends on it",
        chunks.len()
    );
    anyhow::ensure!(
        heading_bearing == 0 || with_heading * 100 >= heading_bearing * 99,
        "only {with_heading} of {heading_bearing} chunks with a heading contain that heading; the \
         original corpus had all of them, and the heading scenarios depend on it"
    );
    Ok(())
}

/// Some documents are soft deleted.
///
/// They exist so that a filter forgetting to exclude them fails a gate rather
/// than passing quietly.
///
/// @param chunks - the corpus, in corpus order
fn check_some_documents_are_soft_deleted(chunks: &[SynthChunk]) -> Result<()> {
    // Deleted documents exist so that a filter forgetting to exclude them fails a
    // test rather than passing quietly.
    let deleted = chunks.iter().filter(|c| c.deleted).count();
    println!("\n  soft deleted chunks: {deleted}");
    anyhow::ensure!(
        deleted > 0,
        "no soft deleted documents, so no scenario can catch a filter that forgets them"
    );
    Ok(())
}
