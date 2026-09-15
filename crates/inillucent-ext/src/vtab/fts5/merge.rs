//! What a write does to the FTS5 index.
//!
//! Invariant: **a row's postings are written under the same lock its content
//! row is.** `add` and `remove` stage into the pending buffer and the buffer is
//! flushed as one, so a reader never sees a term whose document is not there
//! yet, or a document whose terms are gone.

use inillucent_base::DbResult;
use inillucent_value::Value;

use super::super::{failure, Context, VirtualTable};

use super::doclist::{decode_doclist, encode_doclist, write_varint, DocEntry};
use super::index::*;
use super::*;

impl Fts5Table {
    /// Runs one of the module's special commands.
    ///
    /// `integrity-check` and `rebuild` do what they say. The merge family -
    /// `optimize`, `merge`, `automerge`, `crisismerge`, `usermerge`, `pgsz` -
    /// is accepted and does nothing, because this index keeps one doclist per
    /// term rather than a stack of segments to merge: there is no work for them
    /// to ask for. They are accepted rather than refused so that an application
    /// written against SQLite runs unchanged, and they are listed here rather
    /// than ignored silently so the next reader knows the difference is
    /// deliberate.
    pub(crate) fn command(
        &mut self,
        context: &mut Context<'_>,
        command: &[u8],
        argument: Option<Value<'static>>,
    ) -> DbResult<()> {
        let name = String::from_utf8_lossy(command).into_owned();
        let argument = argument.filter(|value| !matches!(value, Value::Null));
        match name.as_str() {
            "integrity-check" => match self.integrity(context)? {
                Some(problem) => Err(failure(problem)),
                None => Ok(()),
            },
            "rebuild" => self.rebuild(context),
            // There is nothing to merge or flush: one doclist per term is the
            // whole index, so the segment machinery these ask about does not
            // exist here. They are accepted rather than refused so that an
            // application written against SQLite runs unchanged.
            "optimize" | "flush" => Ok(()),
            "merge" => match argument {
                Some(_) => Ok(()),
                None => Err(unrecognized()),
            },
            // The settings *are* kept, because `%_config` is a table an
            // application reads. Nothing here acts on them - see above - but a
            // value that was written and cannot be read back would be a
            // difference a reader can see.
            "automerge" | "crisismerge" | "deletemerge" | "pgsz" | "rank" | "secure-delete"
            | "usermerge" => {
                let Some(value) = argument else {
                    return Err(unrecognized());
                };
                self.shadows.write_keyed(
                    context,
                    b"config",
                    1,
                    &[Value::owned_text(name.as_bytes())?, value],
                )
            }
            "delete-all" => Err(failure(
                "'delete-all' may only be used with a contentless or external content fts5 table",
            )),
            _ => Err(unrecognized()),
        }
    }

    /// Throws the index away and builds it again from the content.
    ///
    /// The content table is the truth: it holds the rows exactly as they were
    /// inserted, and everything else - the doclists, the term dictionary, the
    /// sizes, the totals - is derived from it. That is what makes a rebuild
    /// possible at all, and what makes it the repair for an index that has
    /// drifted.
    fn rebuild(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        // Every row is about to go, staged ones included: a doclist left in
        // the buffer would be written back after the wipe. This is also the
        // one place every remaining `%_data` term row - the ones an older
        // build left and nothing has touched since - is cleared in one pass,
        // because everything `rebuild` writes from here on goes through
        // `%_idx` directly.
        if let Ok(mut held) = self.pending.lock() {
            held.doclists.clear();
            held.bytes = 0;
        }
        let width = self.options.columns.len();
        let offsets = self.offsets(context);
        let mut rows: Vec<(i64, Vec<Value<'static>>)> = Vec::new();
        let suffix = self.content.clone();
        self.shadows.scan(context, &suffix, |rowid, values| {
            rows.push((
                rowid,
                offsets
                    .iter()
                    .map(|at| values.get(*at).cloned().unwrap_or(Value::Null))
                    .collect(),
            ));
            Ok(true)
        })?;
        let mut doclists = Vec::new();
        self.shadows.scan(context, b"data", |rowid, _| {
            if rowid != TOTALS {
                doclists.push(rowid);
            }
            Ok(true)
        })?;
        for rowid in doclists {
            self.shadows.delete_row(context, b"data", rowid)?;
        }
        // The staged dictionary rows go out first, so the scan below sees
        // every term there is - a delete-all that missed one would leave it in
        // `%_idx` naming a `%_data` row that has been removed.
        flush_doclists(context, &self.shadows, &self.pending)?;
        let mut terms = Vec::new();
        self.shadows.scan_keyed(context, b"idx", 2, |values| {
            if let Some(term) = values.get(1).and_then(Value::as_blob) {
                terms.push(term.raw().to_vec());
            }
            Ok(true)
        })?;
        for term in terms {
            self.shadows.delete_keyed(
                context,
                b"idx",
                &[Value::Integer(SEGMENT), Value::owned_blob(&term)?],
            )?;
        }
        let mut sizes = Vec::new();
        self.shadows.scan(context, b"docsize", |rowid, _| {
            sizes.push(rowid);
            Ok(true)
        })?;
        for rowid in sizes {
            self.shadows.delete_row(context, b"docsize", rowid)?;
        }
        put_totals(context, &self.shadows, &Totals::empty(width))?;
        for (rowid, values) in rows {
            self.add(context, rowid, &values)?;
        }
        Ok(())
    }

    /// Adds one row to the content and to the index.
    pub(crate) fn add(
        &mut self,
        context: &mut Context<'_>,
        rowid: i64,
        values: &[Value<'static>],
    ) -> DbResult<()> {
        let width = self.options.columns.len();
        let started = std::time::Instant::now();
        // **Nothing is stored when the rows are somebody else's.** An
        // external content table's rows are written through the owner, and a
        // copy written here would be a second, diverging one.
        if !self.borrows_rows() {
            let mut content = vec![Value::Null];
            for index in 0..width {
                content.push(values.get(index).cloned().unwrap_or(Value::Null));
            }
            self.shadows
                .write_row(context, b"content", rowid, &content)?;
        }
        let content_ns = started.elapsed().as_nanos();

        let started = std::time::Instant::now();
        let mut sizes = vec![0i64; width];
        let mut postings: Vec<(Vec<u8>, usize, u32)> = Vec::new();
        for (index, column) in self.options.columns.iter().enumerate() {
            if column.unindexed {
                continue;
            }
            let Some(text) = values.get(index).and_then(text_of) else {
                continue;
            };
            for (position, token) in self.tokenizer.tokens(&text).into_iter().enumerate() {
                if let Some(size) = sizes.get_mut(index) {
                    *size = size.saturating_add(1);
                }
                postings.push((token, index, position as u32));
            }
        }
        let tokenize_ns = started.elapsed().as_nanos();

        let started = std::time::Instant::now();
        let mut encoded = Vec::new();
        for size in &sizes {
            write_varint(&mut encoded, *size as u64);
        }
        self.shadows.write_row(
            context,
            b"docsize",
            rowid,
            &[Value::Null, Value::owned_blob(&encoded)?],
        )?;
        let docsize_ns = started.elapsed().as_nanos();

        let started = std::time::Instant::now();
        let mut terms_ns = 0u128;
        postings.sort_unstable();
        let mut at = 0usize;
        while at < postings.len() {
            // **The run of postings for one term, as a slice rather than a
            // copy.** They are sorted by term, then column, then position, so
            // one term's occurrences are contiguous and already in the order a
            // doclist entry writes them - which is what lets the append below
            // read them in place. Building a `DocEntry` here allocated a
            // `Vec<(usize, Vec<u32>)>` per term per document, and a term that
            // occurs once is two allocations to describe one number.
            let start = at;
            let Some((term, _, _)) = postings.get(at) else {
                break;
            };
            while matches!(postings.get(at), Some((candidate, _, _)) if candidate == term) {
                at = at.saturating_add(1);
            }
            let merged = std::time::Instant::now();
            let run = postings.get(start..at).unwrap_or_default();
            self.merge_postings(context, rowid, run)?;
            terms_ns = terms_ns.saturating_add(merged.elapsed().as_nanos());
        }
        let group_ns = started.elapsed().as_nanos().saturating_sub(terms_ns);

        let started = std::time::Instant::now();
        let mut totals = buffered_totals(context, &self.shadows, &self.pending, width);
        totals.rows = totals.rows.saturating_add(1);
        totals.tokens.resize(width, 0);
        for (index, size) in sizes.iter().enumerate() {
            if let Some(total) = totals.tokens.get_mut(index) {
                *total = total.saturating_add(*size);
            }
        }
        stage_totals(&self.pending, totals);
        let totals_ns = started.elapsed().as_nanos();

        record_stage(|stages| {
            stages.rows = stages.rows.saturating_add(1);
            stages.content = stages.content.saturating_add(content_ns);
            stages.tokenize = stages.tokenize.saturating_add(tokenize_ns);
            stages.docsize = stages.docsize.saturating_add(docsize_ns);
            stages.group = stages.group.saturating_add(group_ns);
            stages.terms = stages.terms.saturating_add(terms_ns);
            stages.totals = stages.totals.saturating_add(totals_ns);
        });
        Ok(())
    }

    /// Adds one document's occurrences of one term to that term's doclist.
    ///
    /// **The bulk-load path takes one lock and allocates nothing.** It used to
    /// take three - `term_row`, `append_staged` and `buffer_is_full` each
    /// locked the buffer - and build a `Vec<(usize, Vec<u32>)>` to describe the
    /// occurrences before writing them out as varints. Neither is much on its
    /// own and both are per term per document: a five-hundred-document build
    /// over a ten-term vocabulary is fifteen thousand lock/unlock pairs and ten
    /// thousand allocations, and `extension.fts.build` spent 3.5 of its 8.7 ms
    /// in here.
    ///
    /// Everything the fast path needs is in the buffer, so it is one critical
    /// section: find the term's page, append the entry to the staged doclist,
    /// and read back whether the buffer is now full. The flush - which needs
    /// the host and cannot be done under the lock - happens after it.
    ///
    /// @param context - the host
    /// @param rowid - the document being indexed
    /// @param run - this term's postings, sorted by column then position
    fn merge_postings(
        &self,
        context: &mut Context<'_>,
        rowid: i64,
        run: &[(Vec<u8>, usize, u32)],
    ) -> DbResult<()> {
        let Some((term, _, _)) = run.first() else {
            return Ok(());
        };
        match append_in_one_lock(&self.pending, term, rowid, run) {
            Appended::Done { full } => {
                if full {
                    flush_doclists(context, &self.shadows, &self.pending)?;
                }
                return Ok(());
            }
            Appended::No => {}
        }
        let term = term.clone();
        let entry = DocEntry {
            rowid,
            columns: columns_of(run),
        };
        let started = std::time::Instant::now();
        let answer = self.merge_term(context, &term, entry);
        let spent = started.elapsed().as_nanos();
        record_stage(|stages| {
            stages.new_terms = stages.new_terms.saturating_add(spent);
            stages.new_term_count = stages.new_term_count.saturating_add(1);
        });
        answer
    }

    /// Adds one entry to a term's doclist, keeping it in rowid order.
    ///
    /// **`term_row` staging what it finds is what makes the fast path apply
    /// twice.** `append_staged` is tried before `term_row` because a term
    /// this transaction has already staged answers there directly; it is
    /// tried again straight after, because `term_row` - when the term
    /// exists - has just staged what it read, and the ordinary case for a
    /// bulk load is a document whose rowid sorts after everything already in
    /// the list, which is exactly what the retried fast path handles without
    /// a decode. What is left after both tries is a term with no row yet, or
    /// one whose new entry does not sort last - and both go through the
    /// general decode-and-reinsert path below.
    fn merge_term(&self, context: &mut Context<'_>, term: &[u8], entry: DocEntry) -> DbResult<()> {
        if append_staged(&self.pending, term, &entry) {
            if buffer_is_full(&self.pending) {
                flush_doclists(context, &self.shadows, &self.pending)?;
            }
            return Ok(());
        }
        let Some(held) = term_row(context, &self.shadows, &self.pending, term, true)? else {
            return Ok(());
        };
        if !held.fresh && append_staged(&self.pending, term, &entry) {
            if buffer_is_full(&self.pending) {
                flush_doclists(context, &self.shadows, &self.pending)?;
            }
            return Ok(());
        }
        // A term whose dictionary row was created a moment ago has no
        // doclist to merge into; one that failed the retry above has an
        // entry that does not sort last. Decoding the whole doclist to
        // insert in the middle and then re-encoding it costs time and
        // allocation proportional to how many documents already contain the
        // term, so a bulk index build was quadratic: measured at 100, 200,
        // 400, 800 and 1,600 documents sharing a vocabulary, the cost of one
        // insert rose 1.00x, 1.36x, 2.03x, 3.32x, 6.24x, and the total went
        // from 19 ms to 1,925 ms for sixteen times the documents. That is the
        // cost of an out-of-order document, not of the ordinary bulk load the
        // fast path above already leaves this branch for.
        let existing: Option<Vec<u8>> = if held.fresh {
            None
        } else {
            read_doclist(context, &self.shadows, &self.pending, term)?
        };
        let mut entries = match existing.as_deref() {
            Some(bytes) => decode_doclist(bytes),
            None => Vec::new(),
        };
        match entries.binary_search_by_key(&entry.rowid, |existing| existing.rowid) {
            Ok(position) => {
                if let Some(slot) = entries.get_mut(position) {
                    *slot = entry;
                }
            }
            Err(position) => entries.insert(position, entry),
        }
        let encoded = encode_doclist(&entries);
        stage_doclist(&self.pending, term, encoded);
        if buffer_is_full(&self.pending) {
            flush_doclists(context, &self.shadows, &self.pending)?;
        }
        Ok(())
    }

    /// Removes one row from the content and from every doclist it is in.
    pub(crate) fn remove(&mut self, context: &mut Context<'_>, rowid: i64) -> DbResult<()> {
        let offsets = self.offsets(context);
        let suffix = self.content.clone();
        let Some(content) = self.shadows.read_row(context, &suffix, rowid)? else {
            return Ok(());
        };
        let width = self.options.columns.len();
        let mut terms: Vec<Vec<u8>> = Vec::new();
        for (index, column) in self.options.columns.iter().enumerate() {
            if column.unindexed {
                continue;
            }
            let Some(text) = offsets
                .get(index)
                .and_then(|at| content.get(*at))
                .and_then(text_of)
            else {
                continue;
            };
            terms.extend(self.tokenizer.tokens(&text));
        }
        terms.sort();
        terms.dedup();
        for term in &terms {
            // The buffer first: a term this transaction has already staged
            // has no on-disk `%_idx` row to read, whether it is brand new or
            // already rewritten in the inline form.
            let staged = self
                .pending
                .lock()
                .ok()
                .and_then(|held| held.doclists.get(term.as_slice()).map(|s| s.bytes.clone()));
            // The `%_data` page a legacy row still names, freed below only
            // when this pass is the one that reads it - a row already staged
            // this transaction was resolved, and its legacy page freed, the
            // first time it was touched.
            let (bytes, legacy_page) = match staged {
                Some(bytes) => (bytes, None),
                None => {
                    let key = [Value::Integer(SEGMENT), Value::owned_blob(term)?];
                    let Some(row) = self.shadows.read_keyed(context, b"idx", &key, 3)? else {
                        continue;
                    };
                    let legacy_page = match term_value(&row) {
                        Some(TermValue::Page(page)) => Some(page),
                        _ => None,
                    };
                    let Some(bytes) = resolve_doclist(context, &self.shadows, &row)? else {
                        continue;
                    };
                    (bytes, legacy_page)
                }
            };
            let mut entries = decode_doclist(&bytes);
            entries.retain(|entry| entry.rowid != rowid);
            if entries.is_empty() {
                // Staged as well as stored: the row may exist only in the
                // buffer, and a delete that left it there would write it back.
                forget_doclist(&self.pending, term);
                self.shadows.delete_keyed(
                    context,
                    b"idx",
                    &[Value::Integer(SEGMENT), Value::owned_blob(term)?],
                )?;
                if let Some(page) = legacy_page {
                    self.shadows.delete_row(context, b"data", page)?;
                }
                continue;
            }
            let encoded = encode_doclist(&entries);
            stage_doclist(&self.pending, term, encoded);
        }

        let sizes = match self.shadows.read_row(context, b"docsize", rowid)? {
            Some(row) => row
                .get(1)
                .and_then(Value::as_blob)
                .map(|blob| decode_sizes(blob.raw(), width))
                .unwrap_or_else(|| vec![0; width]),
            None => vec![0; width],
        };
        self.shadows.delete_row(context, b"docsize", rowid)?;
        if !self.borrows_rows() {
            self.shadows.delete_row(context, b"content", rowid)?;
        }
        let mut totals = buffered_totals(context, &self.shadows, &self.pending, width);
        totals.rows = (totals.rows - 1).max(0);
        totals.tokens.resize(width, 0);
        for (index, size) in sizes.iter().enumerate() {
            if let Some(total) = totals.tokens.get_mut(index) {
                *total = (*total - size).max(0);
            }
        }
        stage_totals(&self.pending, totals);
        Ok(())
    }
}
