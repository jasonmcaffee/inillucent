//! Which result column a bare `ORDER BY` identifier names by its alias.
//!
//! Invariant: **in a simple `SELECT`'s `ORDER BY`, a bare identifier that
//! matches a written alias is that result column, before any column of the
//! `FROM` tables.** That is SQLite's `resolveAsName`, and it is the opposite
//! order from every other clause, where a table column wins and an alias is
//! the fallback. Measured against 3.53.4: `SELECT item, sum(quantity) AS
//! quantity FROM order_line GROUP BY item ORDER BY quantity DESC` sorts by the
//! sum, and `SELECT id AS q, q AS id FROM t ORDER BY q` sorts by `t.id`. An
//! implicit column name does not count, so `SELECT t.a, y.a FROM t, y ORDER BY
//! a` is still an ambiguous column, and a qualified name such as `t.a` or an
//! expression such as `quantity + 0` still reads the table.

use super::{Binder, BoundResultColumn};
use crate::ast::{self, Expr, ExprId, SelectBody};

impl Binder<'_> {
    /// Returns each written alias of a select's result list and the result
    /// column it names, folded for comparison.
    ///
    /// **By position when the list has no `*`.** A star expands to a number
    /// of columns only the binder knows, so with one in the list the alias is
    /// found by name instead, taking the first result column of that name.
    /// SQLite does the same, because it gives the columns a star expands to
    /// names of the same kind as an alias: `SELECT *, id AS q FROM t ORDER BY
    /// q` sorts by `t.q`, which the star put first.
    ///
    /// @param select - the statement as written
    /// @param columns - its bound result columns
    pub(super) fn order_aliases(
        &self,
        select: &ast::Select,
        columns: &[BoundResultColumn],
    ) -> Vec<(Vec<u8>, usize)> {
        let Some(SelectBody::Select {
            columns: written, ..
        }) = self.ast.core(select.first).map(|core| &core.body)
        else {
            return Vec::new();
        };
        let starred = written
            .iter()
            .any(|column| matches!(self.ast.expr(column.expr), Some(Expr::Star { .. })));
        let mut named = Vec::new();
        for (position, column) in written.iter().enumerate() {
            let Some(alias) = column.alias else {
                continue;
            };
            let folded = self.ast.folded(alias).to_vec();
            let at = match starred {
                false => Some(position),
                true => columns
                    .iter()
                    .position(|bound| bound.name.eq_ignore_ascii_case(&folded)),
            };
            if let Some(at) = at.filter(|at| *at < columns.len()) {
                named.push((folded, at));
            }
        }
        named
    }

    /// Returns the result column an `ORDER BY` term names by its alias, if it
    /// is a bare identifier that matches one.
    ///
    /// @param term - the term's expression
    /// @param aliases - from [`Binder::order_aliases`]
    pub(super) fn ordered_by_alias(
        &self,
        term: ExprId,
        aliases: &[(Vec<u8>, usize)],
    ) -> Option<usize> {
        let Some(Expr::Column {
            database: None,
            table: None,
            column,
        }) = self.ast.expr(term)
        else {
            return None;
        };
        let folded = self.ast.folded(*column);
        aliases
            .iter()
            .find(|(name, _)| name.as_slice() == folded)
            .map(|(_, at)| *at)
    }
}
