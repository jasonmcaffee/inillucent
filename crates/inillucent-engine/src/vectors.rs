//! What a statement's row images mean to a vector index a module owns.
//!
//! Invariant: **the table and the index it carries change together, in one
//! transaction.** A statement that stored a vector and could not maintain the
//! index is a statement that did not happen, so every function here is reached
//! inside the caller's transaction, before its commit record, and reports a
//! failure the way the statement itself fails rather than past it.
//!
//! One idea, in one place: the engine's half of `USING inillucent_hnsw`. A
//! write path cannot reach a module - it is holding the trees, and a module's
//! shadow tables are trees - so it reports what it stored and what it removed
//! and this applies both to the store. The read path is the other direction:
//! an `ORDER BY vector_distance_cos(v, ?) LIMIT k` becomes a probe of the store
//! and comes back as rowids to descend the table with.
//!
//! It lived in `lib.rs` until task-1911, which needed a few lines of it and
//! found `lib.rs` at its recorded ceiling. Extracting this was the extraction
//! that file's size guard asks for: these four functions name each other and
//! nothing else names them except the write path and the planner's one
//! question.

use std::collections::HashMap;

use inillucent_base::error::refusal;
use inillucent_base::DbResult;
use inillucent_exec::dml::Changes;
use inillucent_tree::datum::OwnedDatum;

use super::{argument_of, vtab, ImportedDatabase, Outcome, VectorIndex};

impl ImportedDatabase {
    /// Applies a statement's row images to every index a module owns.
    ///
    /// **The other half of `Changes::written`.** The write path cannot reach a
    /// module, so it reports what it stored and what it removed; this is where
    /// those become the module's own inserts and deletes. Removals go first,
    /// because an `UPDATE` reports both halves of the same key and the store
    /// would otherwise hold the old row and refuse the new one.
    ///
    /// A row whose vector is NULL is not in the index at all, which is what
    /// makes a partly-populated column work: the rows that have vectors are
    /// searchable and the rows that do not are simply absent.
    ///
    /// Returns whether it reached a module at all, so a caller knows whether
    /// there is anything to flush.
    ///
    /// @param changes - what the statement stored and removed
    pub(crate) fn follow_vector_indexes(&mut self, changes: &Changes) -> DbResult<bool> {
        if changes.written.is_empty() && changes.removed.is_empty() {
            return Ok(false);
        }
        let indexes: Vec<VectorIndex> = self
            .session_state
            .vector_indexes
            .values()
            .flatten()
            .cloned()
            .collect();
        let reached = !indexes.is_empty();
        for index in indexes {
            for row in &changes.removed {
                let Some(OwnedDatum::Int(rowid)) = row.get(index.rowid) else {
                    continue;
                };
                self.change_module(
                    &index.name,
                    &inillucent_sql::vtab::Change::Delete(inillucent_value::Value::Integer(*rowid)),
                )?;
            }
            for row in &changes.written {
                let Some(OwnedDatum::Int(rowid)) = row.get(index.rowid) else {
                    continue;
                };
                let Some(vector) = row.get(index.column) else {
                    continue;
                };
                let value = inillucent_value::Value::from(&vector.borrow()).into_owned()?;
                if matches!(value, inillucent_value::Value::Null) {
                    continue;
                }
                self.change_module(
                    &index.name,
                    &inillucent_sql::vtab::Change::Insert {
                        rowid: inillucent_value::Value::Integer(*rowid),
                        // `body` then the hidden query columns: the store's
                        // first declared column carries the source rowid as
                        // text, so a hit can name the row it came from, and the
                        // vector goes in the hidden `vector` column the module
                        // reads embeddings out of.
                        values: vec![
                            inillucent_value::Value::owned_text(rowid.to_string().as_bytes())?,
                            inillucent_value::Value::Null,
                            inillucent_value::Value::Null,
                            value,
                            inillucent_value::Value::Null,
                            inillucent_value::Value::Null,
                        ],
                    },
                )?;
            }
        }
        Ok(reached)
    }

    /// Asks an index a module owns for the rowids nearest a vector.
    ///
    /// **The store's rowid is the table's rowid**, which is what makes this an
    /// answer rather than a lookup table: the index was written with the source
    /// row's number as its own, so the candidates come back ready to probe the
    /// table with.
    ///
    /// The query is the module's own vector-only shape - no query text, a
    /// vector, and a depth - and it is put through the ordinary planner, so
    /// there is one implementation of what asking this module means.
    ///
    /// @param index - the store's name
    /// @param probe - the vector to measure against
    /// @param depth - how many candidates to ask for
    pub(super) fn nearest_rowids(
        &self,
        index: &[u8],
        probe: &inillucent_tree::datum::Datum<'_>,
        depth: usize,
    ) -> DbResult<Option<Vec<i64>>> {
        let folded = index.to_ascii_lowercase();
        let Some(connected) = self.session_state.virtual_tables.get(&folded) else {
            return Ok(None);
        };
        let (inillucent_tree::datum::Datum::Blob(bytes)
        | inillucent_tree::datum::Datum::Text(bytes)) = probe
        else {
            // A probe that is not bytes cannot be a vector, and an index asked
            // for the nearest to a number has no answer rather than a wrong
            // one.
            return Ok(Some(Vec::new()));
        };
        let candidates = self.probe_module(connected, bytes, depth)?;
        if candidates.is_empty() {
            // **An index that holds nothing is not an answer of nothing.** A
            // nearest-neighbour probe returns the `k` nearest rows however far
            // away they are, so an empty candidate list means the store is
            // empty rather than that nothing matched - and `None` here is the
            // planner's word for "no index", which sends the query to the
            // exhaustive scan. Returning the empty list instead is how a
            // `CREATE INDEX` used to make a working search answer zero rows
            // with nothing failing and nothing logged. The cause of the empty
            // store is fixed; this is what stops the *symptom* being a wrong
            // answer if anything ever empties one again. A table that really
            // holds no rows gets the same answer from the scan.
            return Ok(None);
        }
        Ok(Some(candidates))
    }

    /// Puts the module's own vector-only query to one connected store.
    ///
    /// **Built here rather than compiled from text**, because this runs inside
    /// a read: the executor is holding the catalog, and compiling a statement
    /// would want the connection mutably. The constraints are exactly the three
    /// the module documents - no query text, a vector, and a depth - offered
    /// through `best_index` the way the planner offers them, so the module
    /// chooses its own plan rather than being told one.
    ///
    /// @param connected - the store
    /// @param probe - the vector's bytes
    /// @param depth - how many candidates to ask for
    pub(super) fn probe_module(
        &self,
        connected: &vtab::Connected,
        probe: &[u8],
        depth: usize,
    ) -> DbResult<Vec<i64>> {
        use inillucent_sql::vtab::{ConstraintOp, ConstraintSpec, IndexQuery, OrderSpec};
        let declaration = connected.table.declaration();
        let column_of = |wanted: &[u8]| -> Option<i32> {
            declaration
                .columns
                .iter()
                .position(|held| held.name.eq_ignore_ascii_case(wanted))
                .and_then(|at| i32::try_from(at).ok())
        };
        // The query column is the table's own name; the rest are named.
        let (Some(query), Some(k), Some(vector), Some(rank)) = (
            column_of(&connected.arguments.table),
            column_of(b"k"),
            column_of(b"vector"),
            column_of(b"rank"),
        ) else {
            return Err(refusal("the index's store is not a search table"));
        };
        let specs = vec![
            ConstraintSpec {
                column: query,
                op: ConstraintOp::Match,
                usable: true,
            },
            ConstraintSpec {
                column: vector,
                op: ConstraintOp::Eq,
                usable: true,
            },
            ConstraintSpec {
                column: k,
                op: ConstraintOp::Eq,
                usable: true,
            },
        ];
        let values = [
            inillucent_value::Value::owned_text(b"")?,
            inillucent_value::Value::owned_blob(probe)?,
            inillucent_value::Value::Integer(depth as i64),
        ];
        let mut query_plan = IndexQuery::new(
            specs,
            vec![OrderSpec {
                column: rank,
                descending: false,
            }],
        );
        connected.table.best_index(&mut query_plan)?;
        let mut arguments: Vec<inillucent_value::Value<'static>> = Vec::new();
        for position in query_plan.argument_order() {
            let Some(value) = values.get(position) else {
                continue;
            };
            arguments.push(value.clone());
        }
        let plan = inillucent_ext::vtab::FilterPlan {
            index_number: query_plan.index_number,
            index_string: query_plan.index_string.clone(),
            arguments,
        };
        let mut cursor = connected.table.open()?;
        let store = vtab::ReadStore {
            pool: self.storage.database.pool(),
            trees: &self.schema.trees,
        };
        let mut nowhere = inillucent_ext::vtab::WithStore { store };
        let mut context = inillucent_ext::vtab::Context {
            host: &mut nowhere,
            database: 0,
            limits: &self.pragmas.limits.borrow(),
            catalog: None,
        };
        cursor.filter(&mut context, &plan)?;
        let mut found = Vec::with_capacity(depth);
        while !cursor.eof() {
            found.push(cursor.rowid()?);
            cursor.next(&mut context)?;
        }
        Ok(found)
    }

    /// Rebuilds the map of indexes a module owns from the connected tables.
    ///
    /// Called wherever the catalog changes. The association is read back out of
    /// the arguments the engine itself wrote when the index was created, which
    /// is why this does not have to parse a module's argument grammar in
    /// general: it only recognises the two arguments it put there.
    pub(crate) fn refresh_vector_indexes(&mut self) {
        let mut found: HashMap<u32, Vec<VectorIndex>> = HashMap::new();
        for (name, connected) in &self.session_state.virtual_tables {
            let Some(source) = argument_of(&connected.arguments.arguments, b"source") else {
                continue;
            };
            let Some(column) = argument_of(&connected.arguments.arguments, b"source_column") else {
                continue;
            };
            let folded = source.to_ascii_lowercase();
            let Some(table) = self.schema.tables.iter().find(|held| held.folded == folded) else {
                continue;
            };
            let Some(layout) = self.schema.layouts.get(&table.root) else {
                continue;
            };
            let wanted = column.to_ascii_lowercase();
            let Some(position) = table.columns.iter().position(|held| held.folded == wanted) else {
                continue;
            };
            let (Some(slot), Some(rowid)) =
                (layout.slots.get(position).copied().flatten(), layout.rowid)
            else {
                continue;
            };
            found.entry(table.root).or_default().push(VectorIndex {
                name: name.clone(),
                column: slot,
                rowid,
                declared: position as u16,
            });
        }
        // **Published into the catalog as well, because the planner reads the
        // catalog and not this map.** An index a module owns is an `IndexInfo`
        // with `IndexOrigin::Module` on the table it indexes: none of the
        // b-tree paths apply to it, and the one path that does looks for
        // exactly that origin.
        for table in &mut self.schema.tables {
            table
                .indexes
                .retain(|held| held.origin != inillucent_sql::catalog_view::IndexOrigin::Module);
            let Some(indexes) = found.get(&table.root) else {
                continue;
            };
            for index in indexes {
                table.indexes.push(inillucent_sql::catalog_view::IndexInfo {
                    folded: index.name.to_ascii_lowercase(),
                    name: index.name.clone(),
                    root: 0,
                    unique: false,
                    columns: vec![inillucent_sql::catalog_view::IndexColumnInfo {
                        column: Some(index.declared),
                        collation: b"binary".to_vec(),
                        descending: false,
                        declared_descending: false,
                        expr_sql: None,
                    }],
                    partial_sql: None,
                    origin: inillucent_sql::catalog_view::IndexOrigin::Module,
                    conflict: None,
                    prefix_rows: Vec::new(),
                    analysed_rows: None,
                    metric: self
                        .session_state
                        .virtual_tables
                        .get(&index.name.to_ascii_lowercase())
                        .map(declared_metric),
                });
            }
        }
        self.session_state.vector_indexes = found;
    }

    /// Builds an index a module owns, and backfills it from the table.
    ///
    /// One statement of sugar for three things that already work: a search
    /// store, the `source=` marker that makes the association durable, and a
    /// pass over the rows that are already there. Everything after this is
    /// ordinary - a write reports its images and `follow_vector_indexes`
    /// applies them.
    ///
    /// The store's declared column is `body`, and it holds the source row's
    /// **rowid as text** so a hit can name the row it came from. That is what
    /// makes the index answerable without a second map: the store's own rowid
    /// is the source rowid too, so a delete needs no lookup at all.
    ///
    /// @param name - the index's name, which is the store's name
    /// @param table - the table being indexed
    /// @param columns - the key columns, of which there must be exactly one
    /// @param exists - whether an index of this name is already there
    /// @param if_not_exists - whether the statement said `IF NOT EXISTS`
    pub(super) fn create_vector_index(
        &mut self,
        module: &[u8],
        name: &[u8],
        table: &[u8],
        columns: &[inillucent_sql::directive::IndexKeyColumn],
        settings: &[(Vec<u8>, Vec<u8>)],
        exists: bool,
        if_not_exists: bool,
    ) -> DbResult<Outcome> {
        if exists {
            if if_not_exists {
                return Ok(Outcome::empty());
            }
            return Err(refusal(format!(
                "index {} already exists",
                String::from_utf8_lossy(name)
            )));
        }
        let [key] = columns else {
            return Err(
                refusal("an index USING inillucent_hnsw takes exactly one column")
                    .with_unsupported("a multi-column vector index"),
            );
        };
        let folded = table.to_ascii_lowercase();
        let owner = self
            .schema
            .tables
            .iter()
            .find(|held| held.folded == folded)
            .cloned()
            .ok_or_else(|| refusal(format!("no such table: {}", String::from_utf8_lossy(table))))?;
        // A module-backed index takes a column, not an expression: the store
        // is declared over a table column's vectors and there is nothing for it
        // to compute one from.
        let key_column = key.column.ok_or_else(|| {
            refusal("an index USING inillucent_hnsw takes a column, not an expression")
        })?;
        let column = owner
            .columns
            .get(usize::from(key_column))
            .ok_or_else(|| refusal("the indexed column is not in the table"))?;
        // **The width has to be declared.** A store is created with a fixed
        // number of dimensions and every vector it is given is checked against
        // it, so an index over a column that never said how wide its vectors
        // are would have to guess from the first row - and be wrong for the
        // rest of them.
        let dims = column.vector_dimensions().ok_or_else(|| {
            refusal(format!(
                "{}.{} is not declared VECTOR(N), so an index cannot know how wide its vectors are",
                String::from_utf8_lossy(table),
                String::from_utf8_lossy(&column.name)
            ))
        })?;
        // The storage parameters go through as the store's own options, which
        // is what they are: `WITH (m = 32)` and `USING inillucent_search(...,
        // m=32)` reach the same graph, so the index form is a spelling of the
        // table form rather than a second path into it.
        let mut declared = String::new();
        for (option, value) in settings {
            declared.push_str(&format!(
                ", {}={}",
                String::from_utf8_lossy(option),
                String::from_utf8_lossy(value)
            ));
        }
        // **The structure the index named is the module the store uses.** An
        // `ivfflat` is an inverted file and needs no lexical half, so it is its
        // own module with its own three shadow tables; `inillucent_hnsw` is the
        // graph, which is `inillucent_search` with a vector width and no text.
        // Both answer the engine's vector probe the same way, which is the only
        // thing above this line knows about either.
        let store = if module == b"ivfflat" {
            format!(
                "CREATE VIRTUAL TABLE {} USING ivfflat(dims={}, source={}, source_column={}{declared})",
                String::from_utf8_lossy(name),
                dims,
                String::from_utf8_lossy(&owner.name),
                String::from_utf8_lossy(&column.name)
            )
        } else {
            format!(
                "CREATE VIRTUAL TABLE {} USING inillucent_search(body, dims={}, source={}, source_column={}{declared})",
                String::from_utf8_lossy(name),
                dims,
                String::from_utf8_lossy(&owner.name),
                String::from_utf8_lossy(&column.name)
            )
        };
        self.execute_any(&store, &inillucent_exec::physical::Params::new())?;
        // The association is only visible once the store is connected, and the
        // backfill below has to be seen by it.
        self.refresh_vector_indexes();
        // **Read as rows and written as module changes, not as an
        // `INSERT ... SELECT`.** A module's insert takes values, so the engine
        // refuses an `INSERT ... SELECT` into a virtual table - and the rows
        // that are already in the table are exactly the case an index has to
        // cover, or a `CREATE INDEX` on a full table would build an empty one.
        let query = format!(
            "SELECT rowid, {} FROM {} WHERE {} IS NOT NULL",
            String::from_utf8_lossy(&column.name),
            String::from_utf8_lossy(&owner.name),
            String::from_utf8_lossy(&column.name)
        );
        let existing = self
            .execute_any(&query, &inillucent_exec::physical::Params::new())?
            .rows;
        let changes = inillucent_exec::dml::Changes {
            written: existing
                .into_iter()
                .map(|row| {
                    vec![
                        row.first().cloned().unwrap_or(OwnedDatum::Null),
                        row.get(1).cloned().unwrap_or(OwnedDatum::Null),
                    ]
                })
                .collect(),
            ..Default::default()
        };
        // The rowid is column 0 and the vector column 1 of what was just read,
        // which is not the table's layout - so the index is told where they are
        // for this one call rather than being asked to agree with the layout.
        let held = self.session_state.vector_indexes.clone();
        self.session_state.vector_indexes = std::collections::HashMap::from([(
            owner.root,
            vec![super::VectorIndex::at(name.to_ascii_lowercase(), 1, 0)],
        )]);
        let outcome = self.follow_vector_indexes(&changes);
        self.session_state.vector_indexes = held;
        outcome?;
        // **Folded before it is sealed**, so a `CREATE INDEX` over a full table
        // leaves a published generation rather than a delta log as long as the
        // table. Without this the index was correct and useless: every query
        // replayed all 2,661 entries of `examples/rag-agent` into a graph, and
        // the indexed search took 2.34 s where the exhaustive scan took 0.66.
        // Inside a batch the fold waits for `COMMIT`, because `sync_modules`
        // ends by telling every module its transaction is over and the batch's
        // is not.
        if self.writing.batch.get().is_none() {
            self.sync_modules()?;
        }
        // **The backfill is a write, so it needs the commit record every other
        // directive's does.** This arm used to return here, and the rows it had
        // just read back into the store were logged and never committed: a
        // `CREATE INDEX` over a full table built an index that held every row
        // until the file was reopened and none afterwards, with nothing failing
        // and nothing logged. Inside a batch `seal` is already a no-op, which
        // is why the fault only showed on a `CREATE INDEX` run on its own - and
        // why, run inside a batch, the *next* statement's commit rescued it and
        // made the whole thing look like a lost last row.
        self.seal()?;
        Ok(Outcome::empty())
    }
}

/// Returns the distance a connected vector index's own store was declared to
/// minimise, for the planner to weigh against an `ORDER BY` function.
///
/// **Only `inillucent_search` gets a real answer out of this.** `WITH (metric
/// = ...)` reaches every module's arguments the same way `source` and
/// `source_column` do - `ivfflat`'s own argument parser silently accepts and
/// ignores any key it does not recognise rather than refusing it (see
/// `Settings::of` in `inillucent-ext/src/vtab/ivfflat.rs`), so a `metric`
/// argument sitting on an `ivfflat` table's declaration is not evidence that
/// `ivfflat` computes anything by it. Reporting it anyway would let the
/// planner probe an `ivfflat` index for `ORDER BY vector_distance_l2` while
/// the structure underneath still ranks by cosine - a wrong answer with
/// nothing to show it happened, which is exactly the failure this ticket
/// exists to close off rather than open a second copy of. `inillucent_search`
/// is the one store `crates/inillucent-search/src/merge.rs::configuration`
/// was taught to build the graph under the declared metric, so it is the one
/// module whose declared text is trusted here.
/// @param connected - the module the index is backed by
fn declared_metric(connected: &vtab::Connected) -> inillucent_sql::catalog_view::IndexMetric {
    use inillucent_sql::catalog_view::IndexMetric;
    if !connected
        .arguments
        .module
        .eq_ignore_ascii_case(b"inillucent_search")
    {
        return IndexMetric::Cosine;
    }
    match argument_of(&connected.arguments.arguments, b"metric") {
        Some(raw) => {
            match inillucent_search::options::Metric::parse(&String::from_utf8_lossy(&raw)) {
                Ok(inillucent_search::options::Metric::Cosine) => IndexMetric::Cosine,
                // **Now reports what the graph actually does.** Until this
                // ticket finished, `crates/inillucent-search/src/merge.rs::configuration`
                // built and probed every graph under cosine regardless of what
                // a table declared, so reporting `L2` here would have let the
                // planner answer `ORDER BY vector_distance_l2` out of a
                // structure that ranked by something else - a wrong answer
                // with nothing to show it happened. `configuration` now sets
                // `IndexConfig::metric` from the table's own declaration
                // (`core_metric` in `merge.rs`), `VectorSet::push` reads it to
                // decide whether to normalize, and `crates/inillucent-compat/tests/vector_metric.rs`
                // is the test that has to fail if the two ever disagree again.
                Ok(inillucent_search::options::Metric::L2) => IndexMetric::L2,
                // Unreachable in practice - a value the store itself would have
                // refused at connect time, so the table could not be sitting here
                // connected with it. Cosine is the metric every table had before
                // this setting existed, so it is the safe default if it ever is.
                Err(_) => IndexMetric::Cosine,
            }
        }
        None => IndexMetric::Cosine,
    }
}
