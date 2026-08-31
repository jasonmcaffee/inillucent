//! Query predicates, compiled against a store's dictionaries.
//!
//! `Filter` is what a caller writes, in strings. `CompiledFilter` is what the
//! engine evaluates, in integers, and it also knows how many chunks it passes.
//! That count is what lets the vector index choose between walking the graph and
//! scanning the passing set exhaustively.

use crate::store::{Store, NO_TIMESTAMP};

/// A filter as the caller expresses it. Mirrors `SearchFilters` in the current
/// stack field for field, including the two author forms.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub source: Option<String>,
    pub sources: Option<Vec<String>>,
    pub space_key: Option<String>,
    pub author: Option<String>,
    /// Strict per source author selection. A source named here is constrained to
    /// its own author identifiers; a source not named passes through
    /// unconstrained. This is the rule the baseline implements in SQL as
    /// `NOT (source = ANY(named)) OR (source = a AND author_id = b) OR ...`.
    pub authors: Option<Vec<(String, String)>>,
    pub updated_after: Option<i64>,
    /// Overlap, not containment: a document passes if it carries any of these.
    pub labels: Option<Vec<String>>,
    pub include_deleted: bool,
}

impl Filter {
    pub fn source(name: &str) -> Self {
        Filter {
            source: Some(name.to_string()),
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.source.is_none()
            && self.sources.is_none()
            && self.space_key.is_none()
            && self.author.is_none()
            && self.authors.is_none()
            && self.updated_after.is_none()
            && self.labels.is_none()
    }
}

#[derive(Debug)]
pub struct CompiledFilter {
    sources: Option<Vec<u32>>,
    space_key: Option<u32>,
    author: Option<(Option<u32>, Option<u32>)>,
    authors: Option<(Vec<u32>, Vec<(u32, u32)>)>,
    updated_after: Option<i64>,
    labels: Option<Vec<u32>>,
    include_deleted: bool,
    /// Set when the filter names a value the corpus never contained.
    dead: bool,
    /// Number of chunks that pass, computed once at compile time.
    pass_count: usize,
    /// Whether any predicate at all is present.
    trivial: bool,
}

impl CompiledFilter {
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
                .filter_map(|(s, a)| {
                    Some((store.sources.get(s)?, store.author_ids.get(a)?))
                })
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

        let trivial = filter.is_empty() && !filter.include_deleted;

        let mut compiled = CompiledFilter {
            sources,
            space_key,
            author,
            authors,
            updated_after: filter.updated_after,
            labels,
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
            && self.authors.is_none()
            && self.updated_after.is_none()
            && self.labels.is_none()
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

    pub fn pass_count(&self) -> usize {
        self.pass_count
    }

    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// Fraction of the corpus this filter admits, in `[0, 1]`.
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
        let doc = &store.documents[store.chunks[chunk as usize].doc as usize];

        if doc.deleted && !self.include_deleted {
            return false;
        }
        if let Some(sources) = &self.sources {
            if !sources.contains(&doc.source) {
                return false;
            }
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
        if let Some(wanted) = &self.labels {
            let have = store.labels_of(doc_index(store, chunk));
            if !wanted.iter().any(|w| have.contains(w)) {
                return false;
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

fn doc_index(store: &Store, chunk: u32) -> u32 {
    store.chunks[chunk as usize].doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ChunkInput;

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
            labels: labels.into_iter().map(|s| s.to_string()).collect(),
            deleted,
        };
        s.add_chunks(vec![
            mk("confluence", "c1", "Ada", "u1", 1000, vec!["design"], false),
            mk("confluence", "c2", "Bob", "u2", 2000, vec!["ops"], false),
            mk("slack", "s1", "Ada", "u1", 3000, vec![], false),
            mk("jira", "j1", "Cy", "u3", 4000, vec!["design", "ops"], false),
            mk("slack", "s2", "Bob", "u2", 5000, vec![], true),
        ]);
        s
    }

    fn passing(f: &Filter, s: &Store) -> Vec<u32> {
        let c = CompiledFilter::compile(f, s);
        (0..s.n_chunks() as u32).filter(|i| c.passes(*i, s)).collect()
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
            Filter { sources: Some(vec!["slack".into(), "jira".into()]), ..Default::default() },
            Filter { include_deleted: true, ..Default::default() },
            Filter { updated_after: Some(3000), ..Default::default() },
            Filter { author: Some("Ada".into()), ..Default::default() },
            Filter { labels: Some(vec!["design".into()]), ..Default::default() },
            Filter {
                source: Some("confluence".into()),
                labels: Some(vec!["ops".into()]),
                ..Default::default()
            },
        ];
        for f in shapes {
            let c = CompiledFilter::compile(&f, &s);
            let scanned = (0..s.n_chunks() as u32).filter(|i| c.passes(*i, &s)).count();
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
}
