//! Query predicates, compiled against a store's dictionaries.
//!
//! `Filter` is what a caller writes, in strings. `CompiledFilter` is what the
//! engine evaluates, in integers, and it also knows how many chunks it passes.
//! That count is what lets the vector index choose between walking the graph and
//! scanning the passing set exhaustively.
//!
//! Two predicate shapes here are not equalities and still never put a string
//! comparison in the innermost loop. A substring author and a substring attribute
//! value are both resolved against their dictionary at compile time, once per
//! query, into the same dense id set every other predicate tests.
//!
//! Invariant: **a compiled filter answers the same question as the filter it
//! was compiled from, and it knows how many chunks pass.** The count is not a
//! convenience: it is what the vector index uses to choose between walking the
//! graph and scanning the passing set, so a count that disagreed with the
//! predicate would pick the wrong path and return the wrong neighbours.

use crate::store::{Store, NO_TIMESTAMP};

/// One constraint on a named multi-valued attribute, such as a mail corpus's
/// participant list.
///
/// `any_of` is overlap, the same semantics `labels` has. `contains` is the
/// substring form the baseline writes as `address ILIKE '%x%' OR display_name
/// ILIKE '%x%'`, which only works because both spellings of a person are interned
/// into the same attribute set. Setting both requires a document to satisfy each.
#[derive(Debug, Clone, Default)]
pub struct AttributeFilter {
    /// Which attribute set this constrains, such as `participant`.
    pub name: String,
    /// Exact values, any one of which admits a document. Empty means no
    /// constraint of this kind.
    pub any_of: Vec<String>,
    /// A substring, matched without case against every value in the set.
    pub contains: Option<String>,
}

impl AttributeFilter {
    /// A filter on `name` matching any value that contains `needle`.
    /// @param name - the attribute set, such as participant
    /// @param needle - the substring, matched without case
    pub fn containing(name: &str, needle: &str) -> Self {
        AttributeFilter {
            name: name.to_string(),
            any_of: Vec::new(),
            contains: Some(needle.to_string()),
        }
    }

    /// A filter on `name` matching any of these exact values.
    /// @param name - the attribute set
    /// @param values - the values, any one of which admits a document
    pub fn any_of(name: &str, values: &[&str]) -> Self {
        AttributeFilter {
            name: name.to_string(),
            any_of: values.iter().map(|v| v.to_string()).collect(),
            contains: None,
        }
    }
}

/// A filter as the caller expresses it. Mirrors `SearchFilters` in the current
/// stack field for field, including the two author forms.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// One source, by name.
    pub source: Option<String>,
    /// Several sources, any one of which admits a document. Set beside
    /// `source` it narrows further rather than widening.
    pub sources: Option<Vec<String>>,
    /// The space or workspace a document belongs to.
    pub space_key: Option<String>,
    /// An exact author identifier.
    pub author: Option<String>,
    /// Substring author, matched against display name and stable identifier
    /// alike. The exact `author` above is an equality; this is what a mail corpus
    /// actually asks, where the person is known by a name and the messages carry
    /// an address.
    pub author_contains: Option<String>,
    /// Strict per source author selection. A source named here is constrained to
    /// its own author identifiers; a source not named passes through
    /// unconstrained. This is the rule the baseline implements in SQL as
    /// `NOT (source = ANY(named)) OR (source = a AND author_id = b) OR ...`.
    pub authors: Option<Vec<(String, String)>>,
    /// Lower time bound, inclusive. A document with no timestamp is excluded.
    pub updated_after: Option<i64>,
    /// Upper time bound, inclusive. A document with no timestamp is excluded,
    /// matching `NULL <= x` being false rather than unknown-and-therefore-true.
    pub updated_before: Option<i64>,
    /// Overlap, not containment: a document passes if it carries any of these.
    pub labels: Option<Vec<String>>,
    /// Named boolean flags and the state each must be in.
    pub flags: Vec<(String, bool)>,
    /// Constraints on named multi-valued attribute sets.
    pub attributes: Vec<AttributeFilter>,
    /// Whether a document marked deleted still passes. False by default,
    /// because a deletion that a search still returns is not a deletion.
    pub include_deleted: bool,
}

impl Filter {
    /// A filter admitting one named source and nothing else.
    ///
    /// @param name - the source
    pub fn source(name: &str) -> Self {
        Filter {
            source: Some(name.to_string()),
            ..Default::default()
        }
    }

    /// Requires one named boolean flag to be in one state.
    /// @param name - the flag name
    /// @param state - whether the flag must be set or clear
    pub fn with_flag(mut self, name: &str, state: bool) -> Self {
        self.flags.push((name.to_string(), state));
        self
    }

    /// Adds one constraint on a named multi-valued attribute set.
    /// @param attribute - the constraint
    pub fn with_attribute(mut self, attribute: AttributeFilter) -> Self {
        self.attributes.push(attribute);
        self
    }

    /// Reports whether this filter constrains anything at all.
    ///
    /// An empty filter is the one case the vector index can skip predicate
    /// evaluation for entirely, which is why it is a question rather than a
    /// walk of every field at each node.
    pub fn is_empty(&self) -> bool {
        self.source.is_none()
            && self.sources.is_none()
            && self.space_key.is_none()
            && self.author.is_none()
            && self.author_contains.is_none()
            && self.authors.is_none()
            && self.updated_after.is_none()
            && self.updated_before.is_none()
            && self.labels.is_none()
            && self.flags.is_empty()
            && self.attributes.is_empty()
    }
}

/// One compiled attribute constraint: the attribute's name id, and the value ids
/// a document must carry at least one of, per clause.
///
/// Two clauses rather than one merged set because `any_of` and `contains` are
/// conjunctive when both are given, and a merged id set would silently turn that
/// into a union.
#[derive(Debug)]
struct CompiledAttribute {
    /// The attribute set's interned name id.
    name: u32,
    /// Each clause is an ascending id set, tested with a binary search.
    clauses: Vec<Vec<u32>>,
}

/// A filter resolved against one store's dictionaries.
///
/// Every string the caller wrote has become an interned id, so the predicate a
/// traversal evaluates at each node is a comparison of integers. A value the
/// store has never seen resolves to no id at all, which is how a filter that
/// cannot match anything is known before the first node is visited.
#[derive(Debug)]
pub struct CompiledFilter {
    sources: Option<Vec<u32>>,
    space_key: Option<u32>,
    author: Option<(Option<u32>, Option<u32>)>,
    /// Author display-name ids and author identifier ids whose interned value
    /// contains the requested substring, both ascending.
    ///
    /// Ascending because they are tested once per chunk in the innermost loop, and
    /// a substring can match a large share of a dictionary: a participant filter
    /// of one letter resolved to tens of thousands of identifiers, and a linear
    /// `contains` over that, per chunk, measured at **1,044 ms** for one query
    /// against **28 ms** for the same query binary-searched.
    author_contains: Option<(Vec<u32>, Vec<u32>)>,
    authors: Option<(Vec<u32>, Vec<(u32, u32)>)>,
    updated_after: Option<i64>,
    updated_before: Option<i64>,
    labels: Option<Vec<u32>>,
    /// Bit masks that must be set, and bit masks that must be clear.
    flags_set: u32,
    flags_clear: u32,
    attributes: Vec<CompiledAttribute>,
    include_deleted: bool,
    /// Set when the filter names a value the corpus never contained.
    dead: bool,
    /// Number of chunks that pass, computed once at compile time.
    pass_count: usize,
    /// Whether any predicate at all is present.
    trivial: bool,
}

impl CompiledFilter {
    /// Resolves a caller's filter against one store's dictionaries.
    ///
    /// A value the store has never interned resolves to no id, and a clause
    /// that can match no id makes the whole filter dead - which is answered by
    /// `is_dead` before a single node is visited rather than by walking the
    /// graph and finding nothing.
    ///
    /// @param filter - what the caller wrote
    /// @param store - the corpus whose dictionaries the strings resolve against
    pub fn compile(filter: &Filter, store: &Store) -> CompiledFilter {
        let mut dead = false;

        // A named source that the corpus lacks makes the whole filter dead. An
        // empty `sources` array is treated as no source filter, matching the
        // baseline.
        let sources = match (&filter.sources, &filter.source) {
            (Some(list), _) if !list.is_empty() => {
                let ids: Vec<u32> = list.iter().filter_map(|s| store.sources.get(s)).collect();
                if ids.is_empty() {
                    dead = true;
                }
                Some(ids)
            }
            (_, Some(one)) => match store.sources.get(one) {
                Some(id) => Some(vec![id]),
                None => {
                    dead = true;
                    Some(vec![])
                }
            },
            _ => None,
        };

        let space_key = match &filter.space_key {
            Some(s) => match store.spaces.get(s) {
                Some(id) => Some(id),
                None => {
                    dead = true;
                    None
                }
            },
            None => None,
        };

        // The single `author` form matches display name OR stable identifier,
        // which is what `d.author ILIKE $n OR d.author_id = $n` does.
        let author = filter.author.as_ref().map(|a| {
            let by_name = store.authors.get(a);
            let by_id = store.author_ids.get(a);
            if by_name.is_none() && by_id.is_none() {
                dead = true;
            }
            (by_name, by_id)
        });

        // The substring form. Resolved here against two dictionaries of tens of
        // thousands of entries, never inside the traversal.
        // `find_containing` already returns ascending ids, which is what lets the
        // per-chunk test below be a binary search.
        let author_contains = filter.author_contains.as_ref().map(|needle| {
            let by_name = store.authors.find_containing(needle);
            let by_id = store.author_ids.find_containing(needle);
            if by_name.is_empty() && by_id.is_empty() {
                dead = true;
            }
            (by_name, by_id)
        });

        let authors = filter.authors.as_ref().and_then(|pairs| {
            if pairs.is_empty() {
                return None;
            }
            let named: Vec<u32> = pairs
                .iter()
                .filter_map(|(s, _)| store.sources.get(s))
                .collect();
            let tuples: Vec<(u32, u32)> = pairs
                .iter()
                .filter_map(|(s, a)| Some((store.sources.get(s)?, store.author_ids.get(a)?)))
                .collect();
            Some((named, tuples))
        });

        let labels = match &filter.labels {
            Some(list) if !list.is_empty() => {
                let ids: Vec<u32> = list.iter().filter_map(|l| store.labels.get(l)).collect();
                if ids.is_empty() {
                    dead = true;
                }
                Some(ids)
            }
            _ => None,
        };

        // A flag the corpus never set has no bit. Requiring it selects nothing;
        // requiring its absence is satisfied by every document, so it compiles
        // away rather than becoming a mask no document could match.
        let mut flags_set = 0u32;
        let mut flags_clear = 0u32;
        for (name, state) in &filter.flags {
            match store.flag_bit(name) {
                Some(bit) if *state => flags_set |= bit,
                Some(bit) => flags_clear |= bit,
                None if *state => dead = true,
                None => {}
            }
        }

        let mut attributes = Vec::new();
        for constraint in &filter.attributes {
            // A constraint that constrains nothing is absent, not a match on
            // everything and not a reason to select nothing.
            if constraint.any_of.is_empty() && constraint.contains.is_none() {
                continue;
            }
            match compile_attribute(constraint, store) {
                Some(compiled) => attributes.push(compiled),
                None => dead = true,
            }
        }

        // Trivial means "no predicate has to be evaluated per node", and the soft
        // delete check is a predicate the moment anything is tombstoned. Treating
        // an unfiltered search as trivial regardless is what let a tombstoned
        // chunk come back from every branch that takes the fast path: the lexical
        // scorer and the unfiltered graph walk both skip `passes` entirely when
        // this is set. Before tombstoning existed nothing could reach that state,
        // so it was invisible.
        let trivial = filter.is_empty() && (filter.include_deleted || store.deleted_chunks == 0);

        let mut compiled = CompiledFilter {
            sources,
            space_key,
            author,
            author_contains,
            authors,
            updated_after: filter.updated_after,
            updated_before: filter.updated_before,
            labels,
            flags_set,
            flags_clear,
            attributes,
            include_deleted: filter.include_deleted,
            dead,
            pass_count: 0,
            trivial,
        };

        // The commonest predicates can report their pass count from the counts
        // the store already maintains. Only a filter that constrains something
        // else has to scan, and the scan is what used to sit on every query.
        compiled.pass_count = if dead {
            0
        } else if let Some(n) = compiled.pass_count_without_scanning(store) {
            n
        } else {
            (0..store.n_chunks() as u32)
                .filter(|c| compiled.passes(*c, store))
                .count()
        };
        compiled
    }

    /// The pass count from precomputed totals, when the filter constrains at most
    /// the source. Returns `None` when a scan is unavoidable.
    fn pass_count_without_scanning(&self, store: &Store) -> Option<usize> {
        let only_source_constrained = self.space_key.is_none()
            && self.author.is_none()
            && self.author_contains.is_none()
            && self.authors.is_none()
            && self.updated_after.is_none()
            && self.updated_before.is_none()
            && self.labels.is_none()
            && self.flags_set == 0
            && self.flags_clear == 0
            && self.attributes.is_empty()
            && !self.include_deleted;
        if !only_source_constrained {
            return None;
        }
        match &self.sources {
            None => Some(store.live_chunks as usize),
            Some(ids) => Some(
                ids.iter()
                    .map(|s| store.live_chunks_for_source(*s) as usize)
                    .sum(),
            ),
        }
    }

    /// Returns how many chunks pass this filter.
    ///
    /// The number the index uses to choose between walking the graph and
    /// scanning the passing set exhaustively, so it is counted at compile time
    /// rather than estimated.
    pub fn pass_count(&self) -> usize {
        self.pass_count
    }

    /// Reports whether this filter can match nothing at all.
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// Fraction of the corpus this filter admits, from 0 to 1.
    pub fn selectivity(&self, store: &Store) -> f32 {
        if store.n_chunks() == 0 {
            return 0.0;
        }
        self.pass_count as f32 / store.n_chunks() as f32
    }

    /// Whether a chunk passes. Ordered cheapest predicate first, and the
    /// soft delete check first of all because it rejects the most rows for free.
    pub fn passes(&self, chunk: u32, store: &Store) -> bool {
        if self.dead {
            return false;
        }
        // A chunk the store does not hold passes nothing (task-1932, H9).
        let Some(doc_id) = store.doc_of(chunk) else {
            return false;
        };
        let Some(doc) = store.documents.get(doc_id as usize) else {
            return false;
        };

        if doc.deleted && !self.include_deleted {
            return false;
        }
        if let Some(sources) = &self.sources {
            if !sources.contains(&doc.source) {
                return false;
            }
        }
        if doc.flags & self.flags_set != self.flags_set {
            return false;
        }
        if doc.flags & self.flags_clear != 0 {
            return false;
        }
        if let Some(space) = self.space_key {
            if doc.space_key != Some(space) {
                return false;
            }
        }
        if let Some(after) = self.updated_after {
            if doc.updated_at == NO_TIMESTAMP || doc.updated_at < after {
                return false;
            }
        }
        if let Some(before) = self.updated_before {
            if doc.updated_at == NO_TIMESTAMP || doc.updated_at > before {
                return false;
            }
        }
        if let Some((named, tuples)) = &self.authors {
            // A source not named in the list is unconstrained.
            if named.contains(&doc.source) {
                let ok = match doc.author_id {
                    Some(aid) => tuples.iter().any(|(s, a)| *s == doc.source && *a == aid),
                    None => false,
                };
                if !ok {
                    return false;
                }
            }
        } else if let Some((by_name, by_id)) = &self.author {
            let matches = (by_name.is_some() && doc.author == *by_name)
                || (by_id.is_some() && doc.author_id == *by_id);
            if !matches {
                return false;
            }
        }
        if let Some((by_name, by_id)) = &self.author_contains {
            let matches = doc
                .author
                .is_some_and(|a| by_name.binary_search(&a).is_ok())
                || doc
                    .author_id
                    .is_some_and(|a| by_id.binary_search(&a).is_ok());
            if !matches {
                return false;
            }
        }
        if let Some(wanted) = &self.labels {
            let have = store.labels_of(doc_id);
            if !wanted.iter().any(|w| have.contains(w)) {
                return false;
            }
        }
        if !self.attributes.is_empty() {
            let have = store.attributes_of(doc_id);
            for constraint in &self.attributes {
                for clause in &constraint.clauses {
                    let satisfied = have.iter().any(|(name, value)| {
                        *name == constraint.name && clause.binary_search(value).is_ok()
                    });
                    if !satisfied {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// The chunks a scan needs to look at, when the filter names sources and the
    /// store can list them directly. `None` means every chunk has to be visited.
    ///
    /// This narrows what an exhaustive scan iterates; it does not replace the
    /// predicate, which is still evaluated on each candidate, so a filter that
    /// also constrains labels or a timestamp stays correct.
    pub fn candidate_chunks<'a>(&self, store: &'a Store) -> Option<Vec<&'a [u32]>> {
        let ids = self.sources.as_ref()?;
        if ids.is_empty() {
            return None;
        }
        Some(ids.iter().map(|s| store.chunks_of_source(*s)).collect())
    }

    /// True when the filter constrains nothing at all, in which case the caller
    /// can skip evaluating it per node.
    pub fn is_trivial(&self) -> bool {
        self.trivial
    }
}

/// Resolves one attribute constraint against the store's dictionaries.
///
/// Returns `None` when the constraint cannot be satisfied by anything the corpus
/// holds: an unknown attribute name, or a clause whose values are all absent.
/// That makes the whole filter dead, which is the same rule every other predicate
/// follows — a value the corpus lacks selects nothing rather than everything. The
/// caller has already dropped constraints that constrain nothing at all.
/// @param constraint - the constraint as the caller wrote it
/// @param store - the store whose dictionaries resolve it
fn compile_attribute(constraint: &AttributeFilter, store: &Store) -> Option<CompiledAttribute> {
    let name = store.attribute_names.get(&constraint.name)?;
    let dictionary = store.attribute_values.get(name as usize)?;

    let mut clauses = Vec::new();
    if !constraint.any_of.is_empty() {
        let mut ids: Vec<u32> = constraint
            .any_of
            .iter()
            .filter_map(|v| dictionary.get(v))
            .collect();
        if ids.is_empty() {
            return None;
        }
        // Ascending, so the per-chunk membership test is a binary search.
        ids.sort_unstable();
        ids.dedup();
        clauses.push(ids);
    }
    if let Some(needle) = &constraint.contains {
        let ids = dictionary.find_containing(needle);
        if ids.is_empty() {
            return None;
        }
        clauses.push(ids);
    }
    Some(CompiledAttribute { name, clauses })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ChunkInput, Dictionary};

    fn store_with_fixtures() -> Store {
        let mut s = Store::default();
        let mk = |source: &str,
                  doc: &str,
                  author: &str,
                  author_id: &str,
                  updated: i64,
                  labels: Vec<&str>,
                  deleted: bool| ChunkInput {
            source: source.to_string(),
            external_doc_id: doc.to_string(),
            chunk_index: 0,
            heading_path: vec![],
            content: format!("content of {doc}"),
            title: doc.to_string(),
            url: format!("https://x/{doc}"),
            space_key: Some("ENG".to_string()),
            author: Some(author.to_string()),
            author_id: Some(author_id.to_string()),
            updated_at: Some(updated),
            external_chunk_id: None,
            labels: labels.into_iter().map(|s| s.to_string()).collect(),
            attributes: Vec::new(),
            flags: Vec::new(),
            deleted,
        };
        s.add_chunks(vec![
            mk("confluence", "c1", "Ada", "u1", 1000, vec!["design"], false),
            mk("confluence", "c2", "Bob", "u2", 2000, vec!["ops"], false),
            mk("slack", "s1", "Ada", "u1", 3000, vec![], false),
            mk("jira", "j1", "Cy", "u3", 4000, vec!["design", "ops"], false),
            mk("slack", "s2", "Bob", "u2", 5000, vec![], true),
        ])
        .expect("the chunks are added");
        s
    }

    fn passing(f: &Filter, s: &Store) -> Vec<u32> {
        let c = CompiledFilter::compile(f, s);
        (0..s.n_chunks() as u32)
            .filter(|i| c.passes(*i, s))
            .collect()
    }

    #[test]
    fn soft_deleted_documents_are_excluded_by_default() {
        let s = store_with_fixtures();
        let all = passing(&Filter::default(), &s);
        assert_eq!(all, vec![0, 1, 2, 3]); // chunk 4 is deleted
    }

    #[test]
    fn source_filter_selects_one_source() {
        let s = store_with_fixtures();
        assert_eq!(passing(&Filter::source("slack"), &s), vec![2]);
    }

    #[test]
    fn several_sources_at_once() {
        let s = store_with_fixtures();
        let f = Filter {
            sources: Some(vec!["slack".into(), "jira".into()]),
            ..Default::default()
        };
        assert_eq!(passing(&f, &s), vec![2, 3]);
    }

    #[test]
    fn an_unknown_source_selects_nothing_rather_than_everything() {
        let s = store_with_fixtures();
        let f = Filter::source("sharepoint");
        let c = CompiledFilter::compile(&f, &s);
        assert!(c.is_dead());
        assert_eq!(c.pass_count(), 0);
        assert!(passing(&f, &s).is_empty());
    }

    #[test]
    fn updated_after_is_inclusive_of_the_boundary() {
        let s = store_with_fixtures();
        let f = Filter {
            updated_after: Some(3000),
            ..Default::default()
        };
        assert_eq!(passing(&f, &s), vec![2, 3]);
    }

    #[test]
    fn author_matches_display_name_or_identifier() {
        let s = store_with_fixtures();
        let by_name = Filter {
            author: Some("Ada".into()),
            ..Default::default()
        };
        assert_eq!(passing(&by_name, &s), vec![0, 2]);
        let by_id = Filter {
            author: Some("u1".into()),
            ..Default::default()
        };
        assert_eq!(passing(&by_id, &s), vec![0, 2]);
    }

    #[test]
    fn strict_per_source_authors_leaves_unnamed_sources_alone() {
        let s = store_with_fixtures();
        // Constrain confluence to Bob. slack and jira are not named, so they pass.
        let f = Filter {
            authors: Some(vec![("confluence".into(), "u2".into())]),
            ..Default::default()
        };
        assert_eq!(passing(&f, &s), vec![1, 2, 3]);
    }

    #[test]
    fn labels_are_overlap_not_containment() {
        let s = store_with_fixtures();
        let f = Filter {
            labels: Some(vec!["design".into()]),
            ..Default::default()
        };
        assert_eq!(passing(&f, &s), vec![0, 3]);
        let both = Filter {
            labels: Some(vec!["design".into(), "ops".into()]),
            ..Default::default()
        };
        assert_eq!(passing(&both, &s), vec![0, 1, 3]);
    }

    #[test]
    fn include_deleted_admits_the_soft_deleted_row() {
        let s = store_with_fixtures();
        let f = Filter {
            include_deleted: true,
            ..Default::default()
        };
        assert_eq!(passing(&f, &s), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn selectivity_and_pass_count_agree_with_evaluation() {
        let s = store_with_fixtures();
        let c = CompiledFilter::compile(&Filter::source("confluence"), &s);
        assert_eq!(c.pass_count(), 2);
        assert!((c.selectivity(&s) - 2.0 / 5.0).abs() < 1e-6);
    }

    /// The fast path and the scan must agree for every filter shape, or the
    /// choice between walking the graph and scanning is made on a wrong number.
    #[test]
    fn the_precomputed_pass_count_agrees_with_a_full_scan() {
        let s = store_with_fixtures();
        let shapes = vec![
            Filter::default(),
            Filter::source("confluence"),
            Filter::source("slack"),
            Filter {
                sources: Some(vec!["slack".into(), "jira".into()]),
                ..Default::default()
            },
            Filter {
                include_deleted: true,
                ..Default::default()
            },
            Filter {
                updated_after: Some(3000),
                ..Default::default()
            },
            Filter {
                author: Some("Ada".into()),
                ..Default::default()
            },
            Filter {
                labels: Some(vec!["design".into()]),
                ..Default::default()
            },
            Filter {
                source: Some("confluence".into()),
                labels: Some(vec!["ops".into()]),
                ..Default::default()
            },
        ];
        for f in shapes {
            let c = CompiledFilter::compile(&f, &s);
            let scanned = (0..s.n_chunks() as u32)
                .filter(|i| c.passes(*i, &s))
                .count();
            assert_eq!(c.pass_count(), scanned, "disagreement for {f:?}");
        }
    }

    #[test]
    fn combined_predicates_intersect() {
        let s = store_with_fixtures();
        let f = Filter {
            source: Some("confluence".into()),
            labels: Some(vec!["ops".into()]),
            ..Default::default()
        };
        assert_eq!(passing(&f, &s), vec![1]);
    }

    /// A mail-shaped store: two named flags, a participant attribute carrying
    /// both addresses and display names, and one document with no timestamp.
    fn mail_store() -> Store {
        let mut s = Store::default();
        let mk = |doc: &str,
                  from_name: &str,
                  from_address: &str,
                  sent: Option<i64>,
                  participants: Vec<&str>,
                  flags: Vec<&str>| ChunkInput {
            source: "email".to_string(),
            external_doc_id: doc.to_string(),
            chunk_index: 0,
            heading_path: vec![],
            content: format!("content of {doc}"),
            title: doc.to_string(),
            url: format!("https://x/{doc}"),
            space_key: Some("thread-1".to_string()),
            author: Some(from_name.to_string()),
            author_id: Some(from_address.to_string()),
            updated_at: sent,
            external_chunk_id: None,
            labels: vec![],
            attributes: vec![(
                "participant".to_string(),
                participants.into_iter().map(|p| p.to_string()).collect(),
            )],
            flags: flags.into_iter().map(|f| f.to_string()).collect(),
            deleted: false,
        };
        s.add_chunks(vec![
            mk(
                "m1",
                "Terri Shaw",
                "terri.shaw@example.org",
                Some(1000),
                vec!["terri.shaw@example.org", "Terri Shaw", "jason@example.com"],
                vec!["has_attachment"],
            ),
            mk(
                "m2",
                "Jean Platt",
                "jean@example.net",
                Some(2000),
                vec!["jean@example.net", "Jean Platt"],
                vec![],
            ),
            mk(
                "m3",
                "Terri Shaw (Work)",
                "tshaw@work.example",
                Some(3000),
                vec!["tshaw@work.example", "Terri Shaw (Work)"],
                vec!["has_attachment"],
            ),
            mk(
                "m4",
                "Nobody",
                "nobody@example.com",
                None,
                vec!["nobody@example.com"],
                vec![],
            ),
        ])
        .expect("the chunks are added");
        s
    }

    #[test]
    fn updated_before_is_inclusive_and_excludes_a_missing_timestamp() {
        let s = mail_store();
        let f = Filter {
            updated_before: Some(2000),
            ..Default::default()
        };
        // m4 has no timestamp: NULL <= x is not true, so it must not pass.
        assert_eq!(passing(&f, &s), vec![0, 1]);
    }

    #[test]
    fn an_upper_and_lower_time_bound_intersect_into_a_window() {
        let s = mail_store();
        let f = Filter {
            updated_after: Some(2000),
            updated_before: Some(3000),
            ..Default::default()
        };
        assert_eq!(passing(&f, &s), vec![1, 2]);
    }

    #[test]
    fn a_boolean_flag_selects_and_deselects_without_touching_labels() {
        let s = mail_store();
        assert_eq!(
            passing(&Filter::default().with_flag("has_attachment", true), &s),
            vec![0, 2]
        );
        assert_eq!(
            passing(&Filter::default().with_flag("has_attachment", false), &s),
            vec![1, 3]
        );
        // The flag is not a label, so it never appears in the label vocabulary a
        // filter drawer reads back.
        assert!(s.labels.is_empty());
    }

    #[test]
    fn requiring_a_flag_the_corpus_never_set_selects_nothing() {
        let s = mail_store();
        let c = CompiledFilter::compile(&Filter::default().with_flag("starred", true), &s);
        assert!(c.is_dead());
        assert_eq!(c.pass_count(), 0);
    }

    #[test]
    fn requiring_the_absence_of_an_unknown_flag_admits_everything() {
        // No document carries it, so every document satisfies "not set". Compiling
        // this to a dead filter would silently return nothing.
        let s = mail_store();
        let f = Filter::default().with_flag("starred", false);
        assert_eq!(passing(&f, &s), vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_substring_author_matches_display_name_and_address_alike() {
        let s = mail_store();
        // "Terri Shaw" is the display name of m1 and a prefix of m3's.
        let by_name = Filter {
            author_contains: Some("Terri Shaw".into()),
            ..Default::default()
        };
        assert_eq!(passing(&by_name, &s), vec![0, 2]);
        // The same filter written as part of an address reaches only that sender.
        let by_address = Filter {
            author_contains: Some("tshaw@".into()),
            ..Default::default()
        };
        assert_eq!(passing(&by_address, &s), vec![2]);
    }

    #[test]
    fn a_substring_author_is_matched_without_case() {
        let s = mail_store();
        let f = Filter {
            author_contains: Some("terri shaw".into()),
            ..Default::default()
        };
        assert_eq!(passing(&f, &s), vec![0, 2]);
    }

    #[test]
    fn a_substring_author_the_corpus_lacks_selects_nothing_rather_than_everything() {
        let s = mail_store();
        let f = Filter {
            author_contains: Some("nobody by this name".into()),
            ..Default::default()
        };
        let c = CompiledFilter::compile(&f, &s);
        assert!(c.is_dead());
        assert!(passing(&f, &s).is_empty());
    }

    #[test]
    fn a_participant_attribute_matches_anyone_on_the_message() {
        let s = mail_store();
        // jason is a recipient of m1 and its sender is somebody else, which is
        // exactly what an author filter cannot express.
        let f = Filter::default().with_attribute(AttributeFilter::containing(
            "participant",
            "jason@example.com",
        ));
        assert_eq!(passing(&f, &s), vec![0]);
    }

    #[test]
    fn a_participant_substring_reaches_every_spelling_of_one_person() {
        let s = mail_store();
        let f = Filter::default()
            .with_attribute(AttributeFilter::containing("participant", "Terri Shaw"));
        assert_eq!(passing(&f, &s), vec![0, 2]);
    }

    #[test]
    fn an_exact_participant_list_is_overlap_not_containment() {
        let s = mail_store();
        let f = Filter::default().with_attribute(AttributeFilter::any_of(
            "participant",
            &["jean@example.net", "jason@example.com"],
        ));
        assert_eq!(passing(&f, &s), vec![0, 1]);
    }

    #[test]
    fn an_attribute_value_the_corpus_lacks_selects_nothing() {
        let s = mail_store();
        let f = Filter::default().with_attribute(AttributeFilter::containing(
            "participant",
            "stranger@nowhere",
        ));
        let c = CompiledFilter::compile(&f, &s);
        assert!(c.is_dead());
        assert!(passing(&f, &s).is_empty());
    }

    #[test]
    fn an_attribute_name_the_corpus_lacks_selects_nothing() {
        let s = mail_store();
        let f = Filter::default().with_attribute(AttributeFilter::containing("assignee", "Ada"));
        assert!(CompiledFilter::compile(&f, &s).is_dead());
    }

    #[test]
    fn an_attribute_constraint_that_constrains_nothing_is_absent() {
        let s = mail_store();
        let f = Filter::default().with_attribute(AttributeFilter {
            name: "participant".into(),
            any_of: Vec::new(),
            contains: None,
        });
        let c = CompiledFilter::compile(&f, &s);
        assert!(!c.is_dead());
        assert_eq!(passing(&f, &s), vec![0, 1, 2, 3]);
    }

    #[test]
    fn two_clauses_on_one_attribute_are_conjunctive() {
        let s = mail_store();
        let f = Filter::default().with_attribute(AttributeFilter {
            name: "participant".into(),
            any_of: vec!["jason@example.com".into()],
            contains: Some("Terri".into()),
        });
        // Only m1 both includes jason and names Terri.
        assert_eq!(passing(&f, &s), vec![0]);
    }

    /// Every new predicate has to agree with a full scan, or the graph-versus-scan
    /// choice is made on a wrong number and queries are silently mis-routed.
    #[test]
    fn the_precomputed_pass_count_agrees_with_a_full_scan_for_the_new_predicates() {
        let s = mail_store();
        let shapes = vec![
            Filter::default(),
            Filter {
                updated_before: Some(2000),
                ..Default::default()
            },
            Filter {
                updated_after: Some(1500),
                updated_before: Some(3000),
                ..Default::default()
            },
            Filter::default().with_flag("has_attachment", true),
            Filter::default().with_flag("has_attachment", false),
            Filter {
                author_contains: Some("Terri".into()),
                ..Default::default()
            },
            Filter::default().with_attribute(AttributeFilter::containing("participant", "Terri")),
            Filter::default().with_attribute(AttributeFilter::any_of(
                "participant",
                &["jean@example.net"],
            )),
            Filter {
                source: Some("email".into()),
                updated_before: Some(2000),
                ..Default::default()
            },
        ];
        for f in shapes {
            let c = CompiledFilter::compile(&f, &s);
            let scanned = (0..s.n_chunks() as u32)
                .filter(|i| c.passes(*i, &s))
                .count();
            assert_eq!(c.pass_count(), scanned, "disagreement for {f:?}");
        }
    }

    /// The fast path exists so an unfiltered search skips per-node predicate
    /// evaluation. A tombstone is a predicate, so a corpus holding one is not
    /// unfiltered — and the branches that take this path do not call `passes` at
    /// all, so getting it wrong makes deleted chunks reappear in the results.
    #[test]
    fn an_unfiltered_search_stops_being_trivial_once_anything_is_tombstoned() {
        let mut s = mail_store();
        assert!(CompiledFilter::compile(&Filter::default(), &s).is_trivial());

        let doc = s.find_document("email", "m1").unwrap();
        s.tombstone_document(doc);

        assert!(
            !CompiledFilter::compile(&Filter::default(), &s).is_trivial(),
            "the soft delete check would have been skipped"
        );
        // Asking for the deleted rows as well genuinely constrains nothing again.
        let all = Filter {
            include_deleted: true,
            ..Default::default()
        };
        assert!(CompiledFilter::compile(&all, &s).is_trivial());
    }

    /// The compiled id sets are tested once per chunk in the innermost loop, so
    /// they are binary-searched and therefore must be ascending. A substring can
    /// match most of a dictionary - a participant filter of one letter did - and a
    /// linear scan of that per chunk measured at 1,044 ms for one query.
    #[test]
    fn a_broad_substring_still_matches_every_value_it_should() {
        let s = mail_store();
        // "e" appears in every address and display name in the fixture, which is
        // the shape that used to be slow and is the shape a binary search gets
        // wrong if the ids are not sorted.
        let by_author = Filter {
            author_contains: Some("e".into()),
            ..Default::default()
        };
        assert_eq!(passing(&by_author, &s), vec![0, 1, 2, 3]);

        let by_participant =
            Filter::default().with_attribute(AttributeFilter::containing("participant", "e"));
        assert_eq!(passing(&by_participant, &s), vec![0, 1, 2, 3]);

        // And a compiled set really is ascending, which is what the search assumes.
        let ids = s
            .attribute_dictionary("participant")
            .unwrap()
            .find_containing("e");
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "not ascending: {ids:?}"
        );
        assert!(ids.len() > 1);
    }

    #[test]
    fn an_exact_attribute_clause_is_ascending_whatever_order_it_was_written_in() {
        let s = mail_store();
        // Written newest-first on purpose; the compiled clause has to sort it.
        let f = Filter::default().with_attribute(AttributeFilter::any_of(
            "participant",
            &[
                "tshaw@work.example",
                "jean@example.net",
                "terri.shaw@example.org",
            ],
        ));
        assert_eq!(passing(&f, &s), vec![0, 1, 2]);
    }

    #[test]
    fn a_substring_match_ignores_case_for_ascii_and_for_anything_else() {
        let mut d = Dictionary::default();
        d.intern("Terri Shaw");
        d.intern("JOSÉ GARCÍA");
        d.intern("plain");
        assert_eq!(d.find_containing("terri").len(), 1);
        assert_eq!(d.find_containing("TERRI").len(), 1);
        // Non-ASCII takes the fully Unicode-aware path rather than an ASCII fold.
        assert_eq!(d.find_containing("josé").len(), 1);
        assert_eq!(d.find_containing("garcía").len(), 1);
        assert!(d.find_containing("garcia").is_empty());
    }

    #[test]
    fn find_containing_distinguishes_no_match_from_every_match() {
        let mut d = Dictionary::default();
        d.intern("Terri Shaw");
        d.intern("Terri Shaw (Work)");
        d.intern("Jean Platt");
        assert_eq!(d.find_containing("Terri").len(), 2);
        assert_eq!(d.find_containing("shaw").len(), 2);
        assert!(d.find_containing("nobody").is_empty());
        // An empty needle is a caller error, not a wildcard.
        assert!(d.find_containing("").is_empty());
    }
}
