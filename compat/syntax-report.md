# inillucent syntax obligations

Measured against sqlite-3.53.4. 60 productions: 60 parsed, 0 omitted, 297 examples.

A production is `parsed` when the parser accepts every positive example and refuses every negative one. The examples are the evidence; a row with none cannot claim coverage.

| Production | Status | Positive | Negative | Source |
|---|---|---:|---:|---|
| `alter-table-stmt` | parsed | 7 | 2 | [lang_altertable](https://sqlite.org/lang_altertable.html) |
| `analyze-stmt` | parsed | 3 | 1 | [lang_analyze](https://sqlite.org/lang_analyze.html) |
| `attach-stmt` | parsed | 3 | 1 | [lang_attach](https://sqlite.org/lang_attach.html) |
| `begin-stmt` | parsed | 5 | 1 | [lang_transaction](https://sqlite.org/lang_transaction.html) |
| `commit-stmt` | parsed | 4 | 0 | [lang_transaction](https://sqlite.org/lang_transaction.html) |
| `create-index-stmt` | parsed | 3 | 2 | [lang_createindex](https://sqlite.org/lang_createindex.html) |
| `create-table-stmt` | parsed | 6 | 2 | [lang_createtable](https://sqlite.org/lang_createtable.html) |
| `create-trigger-stmt` | parsed | 3 | 1 | [lang_createtrigger](https://sqlite.org/lang_createtrigger.html) |
| `create-view-stmt` | parsed | 2 | 1 | [lang_createview](https://sqlite.org/lang_createview.html) |
| `create-virtual-table-stmt` | parsed | 3 | 1 | [lang_createvtab](https://sqlite.org/lang_createvtab.html) |
| `delete-stmt` | parsed | 4 | 2 | [lang_delete](https://sqlite.org/lang_delete.html) |
| `delete-stmt-limited` | parsed | 2 | 0 | [lang_delete](https://sqlite.org/lang_delete.html) |
| `detach-stmt` | parsed | 2 | 1 | [lang_detach](https://sqlite.org/lang_detach.html) |
| `drop-index-stmt` | parsed | 2 | 1 | [lang_dropindex](https://sqlite.org/lang_dropindex.html) |
| `drop-table-stmt` | parsed | 2 | 1 | [lang_droptable](https://sqlite.org/lang_droptable.html) |
| `drop-trigger-stmt` | parsed | 2 | 1 | [lang_droptrigger](https://sqlite.org/lang_droptrigger.html) |
| `drop-view-stmt` | parsed | 2 | 1 | [lang_dropview](https://sqlite.org/lang_dropview.html) |
| `insert-stmt` | parsed | 7 | 2 | [lang_insert](https://sqlite.org/lang_insert.html) |
| `pragma-stmt` | parsed | 5 | 1 | [pragma](https://sqlite.org/pragma.html) |
| `reindex-stmt` | parsed | 3 | 0 | [lang_reindex](https://sqlite.org/lang_reindex.html) |
| `release-stmt` | parsed | 2 | 1 | [lang_savepoint](https://sqlite.org/lang_savepoint.html) |
| `rollback-stmt` | parsed | 4 | 1 | [lang_transaction](https://sqlite.org/lang_transaction.html) |
| `savepoint-stmt` | parsed | 1 | 1 | [lang_savepoint](https://sqlite.org/lang_savepoint.html) |
| `select-stmt` | parsed | 7 | 2 | [lang_select](https://sqlite.org/lang_select.html) |
| `update-stmt` | parsed | 6 | 2 | [lang_update](https://sqlite.org/lang_update.html) |
| `update-stmt-limited` | parsed | 1 | 0 | [lang_update](https://sqlite.org/lang_update.html) |
| `vacuum-stmt` | parsed | 4 | 1 | [lang_vacuum](https://sqlite.org/lang_vacuum.html) |
| `explain-stmt` | parsed | 2 | 1 | [lang_explain](https://sqlite.org/lang_explain.html) |
| `column-def` | parsed | 5 | 0 | [column-def](https://sqlite.org/syntax/column-def.html) |
| `column-constraint` | parsed | 10 | 1 | [column-constraint](https://sqlite.org/syntax/column-constraint.html) |
| `table-constraint` | parsed | 5 | 0 | [table-constraint](https://sqlite.org/syntax/table-constraint.html) |
| `foreign-key-clause` | parsed | 5 | 0 | [foreign-key-clause](https://sqlite.org/syntax/foreign-key-clause.html) |
| `conflict-clause` | parsed | 5 | 1 | [conflict-clause](https://sqlite.org/syntax/conflict-clause.html) |
| `type-name` | parsed | 6 | 1 | [type-name](https://sqlite.org/syntax/type-name.html) |
| `indexed-column` | parsed | 4 | 0 | [indexed-column](https://sqlite.org/syntax/indexed-column.html) |
| `common-table-expression` | parsed | 5 | 1 | [common-table-expression](https://sqlite.org/syntax/common-table-expression.html) |
| `compound-operator` | parsed | 4 | 1 | [compound-operator](https://sqlite.org/syntax/compound-operator.html) |
| `expr` | parsed | 18 | 3 | [lang_expr](https://sqlite.org/lang_expr.html) |
| `filter-clause` | parsed | 1 | 1 | [filter-clause](https://sqlite.org/syntax/filter-clause.html) |
| `frame-spec` | parsed | 6 | 1 | [frame-spec](https://sqlite.org/syntax/frame-spec.html) |
| `join-clause` | parsed | 10 | 1 | [join-clause](https://sqlite.org/syntax/join-clause.html) |
| `join-constraint` | parsed | 2 | 1 | [join-constraint](https://sqlite.org/syntax/join-constraint.html) |
| `literal-value` | parsed | 1 | 2 | [literal-value](https://sqlite.org/syntax/literal-value.html) |
| `numeric-literal` | parsed | 1 | 2 | [numeric-literal](https://sqlite.org/syntax/numeric-literal.html) |
| `ordering-term` | parsed | 3 | 1 | [ordering-term](https://sqlite.org/syntax/ordering-term.html) |
| `over-clause` | parsed | 4 | 0 | [over-clause](https://sqlite.org/syntax/over-clause.html) |
| `qualified-table-name` | parsed | 3 | 1 | [qualified-table-name](https://sqlite.org/syntax/qualified-table-name.html) |
| `raise-function` | parsed | 4 | 1 | [raise-function](https://sqlite.org/syntax/raise-function.html) |
| `result-column` | parsed | 1 | 1 | [result-column](https://sqlite.org/syntax/result-column.html) |
| `returning-clause` | parsed | 3 | 1 | [returning-clause](https://sqlite.org/syntax/returning-clause.html) |
| `select-core` | parsed | 4 | 0 | [select-core](https://sqlite.org/syntax/select-core.html) |
| `signed-number` | parsed | 3 | 0 | [signed-number](https://sqlite.org/syntax/signed-number.html) |
| `table-or-subquery` | parsed | 7 | 1 | [table-or-subquery](https://sqlite.org/syntax/table-or-subquery.html) |
| `upsert-clause` | parsed | 4 | 1 | [lang_upsert](https://sqlite.org/lang_upsert.html) |
| `window-defn` | parsed | 3 | 1 | [window-defn](https://sqlite.org/syntax/window-defn.html) |
| `window-function-invocation` | parsed | 3 | 0 | [window-function-invocation](https://sqlite.org/syntax/window-function-invocation.html) |
| `comment` | parsed | 4 | 0 | [lang_comment](https://sqlite.org/lang_comment.html) |
| `quoted-identifier` | parsed | 3 | 2 | [lang_keywords](https://sqlite.org/lang_keywords.html) |
| `statement-tail` | parsed | 4 | 1 | [prepare](https://sqlite.org/c3ref/prepare.html) |
| `bind-parameter-tcl-suffix` | parsed | 1 | 0 | [lang_expr](https://sqlite.org/lang_expr.html) |

## Not compared against the pinned build

- `delete-stmt-limited` - the reference build is not compiled with SQLITE_ENABLE_UPDATE_DELETE_LIMIT
- `update-stmt-limited` - the reference build is not compiled with SQLITE_ENABLE_UPDATE_DELETE_LIMIT
