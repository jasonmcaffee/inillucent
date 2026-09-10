//! The bytecode compiler: a physical plan in, a program out.
//!
//! Invariant: the compiler allocates every register and cursor it uses and
//! never reuses one across a live range, and every jump it emits is patched to
//! a real address before the program is returned. Those two properties are what
//! the verifier re-checks independently; the compiler is not trusted to be
//! right, it is checked.
//!
//! The shape of a compiled SELECT is one nested loop per FROM term, innermost
//! last, with each residual predicate tested at the shallowest level that can
//! evaluate it. Aggregation and ORDER BY hang off the innermost body: an
//! aggregate steps there and finalises after the loops, and an ORDER BY writes
//! the row into a sorter there and drains it afterwards.

use inillucent_base::{error, DbResult};
use inillucent_sql::ast::{
    BinaryOp, CompoundOp, ConflictAction, JoinKind, NullOrder, PatternOp, RaiseAction, SortOrder,
    UnaryOp,
};
use inillucent_sql::bind::{
    BoundAggregate, BoundExpr, BoundFrameBound, BoundOrderTerm, BoundResultColumn, BoundSelect,
    SubqueryKind, WindowCall as BoundWindowCall,
};
use inillucent_sql::catalog_view::TableInfo;
use inillucent_sql::plan::{
    is_outer, plan_select_with, AccessPath, AggregationMode, BoundKind, IndexSeekBranch, Levers,
    PhysicalPlan, RangeBound,
};
use inillucent_value::{Affinity, Collation};

use crate::program::{
    AggregateCall, Comparison, FrameEnd, IndexKey, Instruction, Opcode, Operand, Program,
    ProgramDependencies, ResultColumn, SortColumn, SortKey, WindowCall, WindowFrame, WindowPlan,
    WindowSlot,
};

/// A jump target that is patched once its address is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Label(pub(crate) usize);

/// The compiler's working state.
pub struct Compiler {
    /// Which planner optimizations this program has actually used so far.
    ///
    /// Set where the decision is made rather than inferred afterwards, because
    /// "did this plan use a covering index" is a question about a choice, and
    /// the only place that knows is the code that made it.
    pub(crate) used: u32,
    /// Which planner optimizations are switched on for this program.
    ///
    /// Compiler state rather than an argument because the write paths that
    /// consult it are several calls below the entry point that knows it, and
    /// an arm that reached the read planner but not the write planner would
    /// measure a mixture and report it as one number.
    pub(crate) levers: Levers,
    pub(crate) instructions: Vec<Instruction>,
    pub(crate) registers: u32,
    pub(crate) cursors: u32,
    pub(crate) sorters: u32,
    pub(crate) distincts: u32,
    pub(crate) ephemerals: u32,
    pub(crate) aggregates: u32,
    pub(crate) substitutions: Vec<(BoundExpr, u32)>,
    /// The foreign-key actions a `REPLACE` fires for the row it removes.
    ///
    /// It is compiler state rather than an argument because the two places a
    /// REPLACE deletes from are four calls below the statement that knows
    /// them, and threading one more parameter through those would say less
    /// than this does.
    pub(crate) replace_triggers: Vec<inillucent_sql::dml::BoundTrigger>,
    /// Where a `RAISE(IGNORE)` in the trigger body being compiled jumps to.
    ///
    /// `IGNORE` abandons the rest of the trigger program and, for a BEFORE
    /// trigger, the row that fired it - so the jump target is decided by the
    /// write that is emitting the body, not by the expression, and the labels
    /// collect here until that write patches them.
    pub(crate) ignore_jumps: Vec<Label>,
    /// Per index of the table being written, whether this statement may leave
    /// its entries alone.
    ///
    /// Empty for every statement but an UPDATE's own delete-and-rewrite, which
    /// sets it around those two emitters and restores it after. See
    /// `unaffected_indexes` in `compile_dml`.
    pub(crate) untouched_indexes: Vec<bool>,
    /// How many trigger bodies enclose the code being emitted.
    ///
    /// Anything written at a depth above zero is a trigger's doing, which
    /// decides whether its row counts towards `changes()`.
    pub(crate) firing_depth: u32,
    /// The table cursor each FROM term reads through.
    ///
    /// A source's number and its cursor's number are not the same thing, and
    /// assuming they were is a bug the verifier caught rather than a wrong
    /// answer: an access path that opens an index takes a second cursor, so
    /// the second FROM term's table cursor is number two rather than number
    /// one, and `SELECT b.label FROM a, b WHERE a.k = 'x'` addressed `a`'s
    /// index cursor as if it were `b`'s table.
    pub(crate) source_cursors: Vec<Option<SourceCursors>>,
    /// Where a covering path keeps each column it was chosen to carry.
    ///
    /// Keyed by the FROM term's statement-wide number, holding
    /// `(record slot in the table, slot in the index entry)`. A term that is
    /// not in this map is read from its table the ordinary way.
    covering_slots: std::collections::BTreeMap<usize, Vec<(u16, usize)>>,
    /// The level an outer join's loop must stop descending at.
    ///
    /// The levels below an outer join are emitted once, as a continuation the
    /// matched row and the null-extended row both enter, so the loop itself
    /// must not descend into them a second time.
    /// Who answers `best_index` for this compilation, when anybody does.
    virtual_planner: Option<Box<dyn VirtualPlanner>>,
    stop_at: Option<usize>,
    /// What each FROM term's columns read as when the record is too short.
    ///
    /// `ALTER TABLE ... ADD COLUMN c DEFAULT 5` does not rewrite the rows that
    /// already existed, so their records stop before `c`. SQLite reads the
    /// *default* back for those rows rather than NULL, and answering NULL would
    /// disagree with the reference on every row written before the column was
    /// added.
    source_defaults: Vec<Vec<Option<Operand>>>,
    /// Everything a nested query used as a value needs, by its bound number.
    ///
    /// The map is keyed by the binder's own number rather than by position,
    /// because one expression can be compiled more than once - an `ORDER BY`
    /// term that is also a result column, for instance - and the store must be
    /// opened exactly once however many times the expression is emitted.
    subquery_plans: std::collections::BTreeMap<usize, ValueSubquery>,
    /// The outer joins whose unmatched right rows still owe a pass.
    ///
    /// A `RIGHT` or `FULL` join keeps the rows of its right side that matched
    /// nothing, and those can only be known once the whole loop nest has run -
    /// so the pass is recorded here and emitted after it.
    antijoins: Vec<AntiJoin>,
    /// For each level, the outer join whose continuation tests its residual.
    ///
    /// A `WHERE` over a term that an outer join can null-extend runs *after*
    /// the join rather than inside its loop. For the join's own term that is
    /// because testing it in the loop would leave the match flag clear and emit
    /// a null-extended row for a row that did match. For a `RIGHT` join it goes
    /// further: every term to its left is null-extendable, so their `WHERE`
    /// terms move too - and until they did,
    /// `a RIGHT JOIN b ON ... WHERE a.name IS NULL` filtered every row of `a`
    /// out of the loop, so nothing was ever recorded as matched and the second
    /// pass then emitted every row of `b`.
    deferred: Vec<Option<usize>>,
    aggregate_registers: Vec<u32>,
    pub(crate) end_jumps: Vec<Label>,
}

impl Default for Compiler {
    /// Returns an empty compiler.
    fn default() -> Compiler {
        Compiler::new()
    }
}

/// Who answers `best_index` while a statement is being compiled.
///
/// The planner cannot ask - a bound plan has to stay a pure function of the SQL
/// and one catalog generation - so the question is put here, once, while the
/// program is being written. The answer is baked into the program, which is
/// also what makes a prepared statement's plan stable until the schema changes.
pub trait VirtualPlanner {
    /// Puts one offer to a module and reads its answer back.
    fn best_index(
        &mut self,
        reference: &crate::program::VirtualRef,
        query: &mut inillucent_sql::vtab::IndexQuery,
    ) -> DbResult<()>;
}

/// A planner that refuses, for a compilation with no connection behind it.
pub struct NoVirtualPlanner;

impl VirtualPlanner for NoVirtualPlanner {
    /// Refuses: there is no registry to ask.
    fn best_index(
        &mut self,
        reference: &crate::program::VirtualRef,
        _query: &mut inillucent_sql::vtab::IndexQuery,
    ) -> DbResult<()> {
        Err(error::misuse(format!(
            "no such module: {}",
            String::from_utf8_lossy(&reference.module.name)
        )))
    }
}

impl Compiler {
    /// Returns an empty compiler.
    pub fn new() -> Compiler {
        Compiler::with_levers(Levers::all())
    }

    /// Returns a compiler that plans under the given arm.
    /// @param levers - which optimizations are on
    pub fn with_levers(levers: Levers) -> Compiler {
        Compiler {
            used: 0,
            levers,
            instructions: Vec::new(),
            // Register 0 is never handed out, so a zero in an unset operand is
            // visibly wrong rather than silently the first register.
            registers: 1,
            cursors: 0,
            sorters: 0,
            distincts: 0,
            ephemerals: 0,
            aggregates: 0,
            substitutions: Vec::new(),
            replace_triggers: Vec::new(),
            ignore_jumps: Vec::new(),
            untouched_indexes: Vec::new(),
            firing_depth: 0,
            source_cursors: Vec::new(),
            covering_slots: std::collections::BTreeMap::new(),
            virtual_planner: None,
            stop_at: None,
            source_defaults: Vec::new(),
            deferred: Vec::new(),
            antijoins: Vec::new(),
            subquery_plans: std::collections::BTreeMap::new(),
            aggregate_registers: Vec::new(),
            end_jumps: Vec::new(),
        }
    }

    /// Points the compiler at who can answer `best_index`.
    pub fn with_virtual_planner(mut self, planner: Box<dyn VirtualPlanner>) -> Compiler {
        self.virtual_planner = Some(planner);
        self
    }

    /// Puts one offer to a module directly, outside a plan.
    ///
    /// The write paths need this: a `DELETE FROM t WHERE ...` scans the term
    /// without ever building a `PhysicalPlan`, and it still has to ask the
    /// module which of the predicates it can use.
    pub(crate) fn ask_module(
        &mut self,
        reference: &crate::program::VirtualRef,
        query: &mut inillucent_sql::vtab::IndexQuery,
    ) -> DbResult<()> {
        let mut planner = match self.virtual_planner.take() {
            Some(planner) => planner,
            None => Box::new(NoVirtualPlanner) as Box<dyn VirtualPlanner>,
        };
        let outcome = planner.best_index(reference, query);
        self.virtual_planner = Some(planner);
        outcome
    }

    /// Puts every virtual scan in a plan to its module, and records the answer.
    ///
    /// A plan with no virtual scan in it never reaches the planner at all, so a
    /// compilation with nobody to ask still succeeds for every statement that
    /// does not name a module - which is every statement the DML compilers'
    /// own tests run.
    pub(crate) fn resolve_plan(&mut self, plan: &mut PhysicalPlan) -> DbResult<()> {
        let mut planner = match self.virtual_planner.take() {
            Some(planner) => planner,
            None => Box::new(NoVirtualPlanner) as Box<dyn VirtualPlanner>,
        };
        let outcome = resolve_virtual_plans(plan, planner.as_mut());
        self.virtual_planner = Some(planner);
        outcome
    }

    /// Allocates one register.
    pub(crate) fn register(&mut self) -> u32 {
        let register = self.registers;
        self.registers = self.registers.saturating_add(1);
        register
    }

    /// Allocates a contiguous block of registers.
    pub(crate) fn register_block(&mut self, count: usize) -> u32 {
        let first = self.registers;
        self.registers = self.registers.saturating_add(count.max(1) as u32);
        first
    }

    /// Emits an instruction and returns its address.
    pub(crate) fn emit(&mut self, instruction: Instruction) -> usize {
        self.instructions.push(instruction);
        self.instructions.len().saturating_sub(1)
    }

    /// Emits a jumping instruction whose target is not known yet.
    pub(crate) fn emit_jump(&mut self, instruction: Instruction) -> Label {
        Label(self.emit(instruction))
    }

    /// Points a previously emitted jump at the current end of the program.
    pub(crate) fn patch_here(&mut self, label: Label) {
        let target = self.instructions.len() as i32;
        self.patch(label, target);
    }

    /// Points a previously emitted jump at an address.
    pub(crate) fn patch(&mut self, label: Label, target: i32) {
        if let Some(instruction) = self.instructions.get_mut(label.0) {
            instruction.p2 = target;
        }
    }

    /// Returns the table cursor a FROM term reads through.
    ///
    /// Falling back to the source's own number is what a DML program relies
    /// on: it opens one cursor for its target table and registers it, so the
    /// map is always consulted rather than the two ever being assumed equal.
    fn cursor_for_source(&self, source: usize) -> i32 {
        self.source_cursors
            .get(source)
            .copied()
            .flatten()
            .map_or(source as i32, |cursors| cursors.table as i32)
    }

    /// Allocates one cursor.
    pub(crate) fn take_cursor(&mut self) -> u32 {
        let cursor = self.cursors;
        self.cursors = self.cursors.saturating_add(1);
        cursor
    }

    /// Records that a FROM term's rows come from a module.
    pub(crate) fn register_virtual_source(&mut self, id: usize, cursor: u32) {
        self.register_source(
            id,
            SourceCursors {
                table: cursor,
                index: None,
                ephemeral: false,
                index_table: false,
                virtual_table: true,
                built: None,
                matched: None,
            },
        );
    }

    /// Allocates one ephemeral row store.
    pub(crate) fn ephemeral(&mut self) -> u32 {
        let store = self.ephemerals;
        self.ephemerals = self.ephemerals.saturating_add(1);
        store
    }

    /// Records the cursors one statement-wide FROM term reads through.
    pub(crate) fn register_source(&mut self, id: usize, cursors: SourceCursors) {
        if self.source_cursors.len() <= id {
            self.source_cursors.resize(id.saturating_add(1), None);
        }
        if let Some(slot) = self.source_cursors.get_mut(id) {
            *slot = Some(cursors);
        }
    }

    /// Records what a FROM term's columns read as when the record is short.
    fn register_defaults(&mut self, id: usize, table: &TableInfo) {
        let defaults: Vec<Option<Operand>> = table
            .columns
            .iter()
            .filter(|column| !column.generated || column.stored)
            .map(|column| {
                column
                    .default_sql
                    .as_ref()
                    .filter(|sql| !sql.is_empty())
                    .and_then(|sql| constant_operand(sql))
            })
            .collect();
        if defaults.iter().all(Option::is_none) {
            return;
        }
        if self.source_defaults.len() <= id {
            self.source_defaults
                .resize(id.saturating_add(1), Vec::new());
        }
        if let Some(slot) = self.source_defaults.get_mut(id) {
            *slot = defaults;
        }
    }

    /// Returns the default one FROM term's record slot reads as, when it has
    /// one.
    fn default_of(&self, id: usize, slot: u16) -> Option<Operand> {
        self.source_defaults
            .get(id)?
            .get(usize::from(slot))
            .cloned()
            .flatten()
    }

    /// Returns whether a FROM term's rows come from an index B-tree.
    ///
    /// True for a `WITHOUT ROWID` table, whose root *is* an index. It is asked
    /// of the recorded cursor rather than of the table, because the compiler
    /// reads columns long after the decision to open one was made.
    fn is_index_source(&self, id: usize) -> bool {
        self.source_cursors
            .get(id)
            .and_then(Option::as_ref)
            .is_some_and(|cursors| cursors.index_table)
    }

    /// Returns the index cursor a covering path reads a column through, and
    /// where in the entry that column sits.
    fn covering_read(&self, source: usize, slot: u16) -> Option<(u32, usize)> {
        let index = self.covering_index(source)?;
        let entry = self
            .covering_slots
            .get(&source)?
            .iter()
            .find(|(column, _)| *column == slot)
            .map(|(_, at)| *at)?;
        Some((index, entry))
    }

    /// Returns the index cursor of a covering path, when the term has one.
    fn covering_index(&self, source: usize) -> Option<u32> {
        if !self.covering_slots.contains_key(&source) {
            return None;
        }
        self.source_cursors
            .get(source)
            .and_then(Option::as_ref)
            .and_then(|cursors| cursors.index)
    }

    /// Returns whether a FROM term's rows come from an ephemeral store.
    fn is_virtual_source(&self, id: usize) -> bool {
        self.source_cursors
            .get(id)
            .and_then(Option::as_ref)
            .is_some_and(|cursors| cursors.virtual_table)
    }

    /// Returns whether a FROM term's rows come from an ephemeral store.
    fn is_ephemeral_source(&self, id: usize) -> bool {
        self.source_cursors
            .get(id)
            .copied()
            .flatten()
            .is_some_and(|cursors| cursors.ephemeral)
    }

    /// Returns the cursors a statement-wide FROM term reads through.
    fn cursors_of(&self, id: usize) -> DbResult<SourceCursors> {
        self.source_cursors
            .get(id)
            .copied()
            .flatten()
            .ok_or_else(|| error::misuse("a FROM term with no cursor"))
    }

    /// Returns the address the next instruction will be emitted at.
    pub(crate) fn here(&self) -> i32 {
        self.instructions.len() as i32
    }
}

/// Compiles a physical plan into a program.
pub fn compile(
    plan: &PhysicalPlan,
    dependencies: ProgramDependencies,
    parameters: u32,
) -> DbResult<Program> {
    compile_into(Compiler::new(), plan, dependencies, parameters)
}

/// Compiles a physical plan with a compiler the caller has already set up.
///
/// The only thing a caller sets up is who answers `best_index`, and the only
/// caller that does is the session. Splitting it here rather than adding a
/// parameter to `compile` keeps every existing call site - and every test that
/// compiles a plan with nothing behind it - reading the same way.
pub fn compile_into(
    mut compiler: Compiler,
    plan: &PhysicalPlan,
    dependencies: ProgramDependencies,
    parameters: u32,
) -> DbResult<Program> {
    let entry = compiler.emit_jump(Instruction::new(Opcode::Init, 0, -1, 0));
    compiler.patch_here(entry);
    compiler.emit(Instruction::new(Opcode::Transaction, 0, 0, 0));
    // Every cursor in the whole statement is opened before anything runs,
    // including the cursors of blocks nested inside it. A correlated subquery
    // rewinds its cursors each time it is re-run, so opening once is not only
    // cheaper - it is what makes the cursor map one flat vector indexed by the
    // statement-wide FROM-term number rather than a stack that has to be kept
    // in step with the recursion.
    compiler.open_all_cursors(plan)?;
    compiler.compile_block(plan, Sink::Result)?;
    let halt = compiler.here();
    compiler.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    for label in core::mem::take(&mut compiler.end_jumps) {
        compiler.patch(label, halt);
    }

    let result_columns = plan
        .select
        .columns
        .iter()
        .map(|column| ResultColumn {
            name: column.name.clone(),
            origin: column.origin.clone(),
            declared_type: column.declared_type.clone(),
        })
        .collect();
    Ok(Program {
        instructions: compiler.instructions,
        register_count: compiler.registers,
        cursor_count: compiler.cursors,
        sorter_count: compiler.sorters,
        distinct_count: compiler.distincts,
        ephemeral_count: compiler.ephemerals,
        aggregate_count: compiler.aggregates,
        result_columns,
        dependencies,
        readonly: true,
        optimizations_used: compiler.used,
        parameter_count: parameters,
    })
}

/// Where the rows a block produces go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sink {
    /// Out of the statement, as a result row.
    Result,
    /// Into an ephemeral store, keeping duplicates.
    Store(u32),
    /// Into an ephemeral store, dropping a row equal to one already there.
    UniqueStore(u32),
    /// Into an ephemeral store, with an affinity applied on the way in.
    ///
    /// `x IN (SELECT y ...)` compares `x` and `y` under one affinity, and
    /// applying it to `x` alone would compare a converted left against an
    /// unconverted right - which is how `'1' IN (SELECT 1)` comes back false.
    TypedStore(u32, Option<Affinity>),
}

/// The cursors one FROM term uses.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SourceCursors {
    /// The table cursor, or the ephemeral store when the term is a subquery.
    ///
    /// A covering path never opens it. That is deliberate rather than tidy: if
    /// the planner decided an index carried every column the query reads and it
    /// was wrong, the mistake shows up as a statement that will not run rather
    /// than as one that quietly reads the wrong bytes.
    table: u32,
    /// The index cursor an index path reads through.
    index: Option<u32>,
    /// Whether `table` numbers an ephemeral store rather than a B-tree cursor.
    ephemeral: bool,
    /// Whether `table` is an index cursor, which a `WITHOUT ROWID` table's is.
    index_table: bool,
    /// The register holding whether an uncorrelated block has been built.
    ///
    /// It is allocated and zeroed before the loops, not inside them. Zeroing it
    /// where the block is materialised put the initialisation *inside* the
    /// enclosing loop, so the flag was cleared on every outer row and the store
    /// accumulated one copy of the subquery per row of the query above it.
    built: Option<u32>,
    /// Whether the term's rows come from a module rather than from a b-tree.
    virtual_table: bool,
    /// The store recording which of this term's rows a `RIGHT` or `FULL` join
    /// matched.
    ///
    /// Opened before the loops for the same reason: a term on the right of an
    /// outer join is scanned once per row of everything above it, and opening
    /// its matched-set inside that loop emptied the set on every outer row -
    /// so the second pass then treated every row as unmatched.
    matched: Option<u32>,
}

impl SourceCursors {
    /// Returns the cursors of a term read through a table cursor alone.
    ///
    /// A DML statement opens exactly one cursor for its target and registers it
    /// here, so that the expression compiler asks the same map a SELECT does
    /// rather than assuming a FROM term's number and its cursor's number are
    /// the same thing.
    pub(crate) fn table_only(table: u32) -> SourceCursors {
        SourceCursors {
            table,
            index: None,
            ephemeral: false,
            index_table: false,
            virtual_table: false,
            built: None,
            matched: None,
        }
    }

    /// Returns the cursors of a `WITHOUT ROWID` target, whose root is an index.
    pub(crate) fn index_only(table: u32) -> SourceCursors {
        SourceCursors {
            table,
            index: Some(table),
            ephemeral: false,
            index_table: true,
            virtual_table: false,
            built: None,
            matched: None,
        }
    }
}

/// One `RIGHT` or `FULL` join's second pass.
#[derive(Clone, Copy, Debug)]
struct AntiJoin {
    /// The level whose unmatched rows the pass emits.
    level: usize,
    /// The store holding the keys of the rows that did match.
    matched: u32,
    /// The register holding the continuation's return address.
    ret: u32,
    /// The continuation the pass enters, which is the same one the matched
    /// rows enter: everything below this level, and this level's residual.
    continuation: i32,
}

/// A nested query used as a value, prepared before the block that reads it.
#[derive(Clone, Debug)]
struct ValueSubquery {
    /// The store its rows are collected in.
    store: u32,
    /// The plan that fills the store.
    plan: PhysicalPlan,
    /// Whether it reads a FROM term outside itself.
    correlated: bool,
    /// The register holding whether an uncorrelated block has been built.
    built: Option<u32>,
    /// The affinity applied to its rows, for an `IN`.
    affinity: Option<Affinity>,
}

/// Everything the loop nest of one block needs to know.
struct Body<'a> {
    plan: &'a PhysicalPlan,
    sorter: Option<u32>,
    distinct: Option<u32>,
    /// Where the previous emitted row is kept, and the flag saying there is
    /// one, when duplicates arrive next to each other and a set is not needed.
    adjacent: Option<(u32, u32)>,
    limit_register: Option<u32>,
    offset_register: Option<u32>,
    sink: Sink,
}

/// Puts every virtual scan's offer to its module, and records the answer.
///
/// It runs over the whole plan - nested blocks and compound arms included -
/// before a single instruction is written, so that the compiler never has to
/// reach outside itself. A scan whose module says it will produce the
/// statement's ordering also clears the sorter, which is the one place the
/// answer changes the shape of the program rather than only its operands.
pub fn resolve_virtual_plans(
    plan: &mut PhysicalPlan,
    planner: &mut dyn VirtualPlanner,
) -> DbResult<()> {
    let mut satisfied_order = false;
    for (level, source) in plan.sources.iter_mut().enumerate() {
        match &mut source.path {
            AccessPath::Subquery { plan: nested, .. } => {
                resolve_virtual_plans(nested, planner)?;
            }
            AccessPath::Recursive { seeds, steps, .. } => {
                for (_, arm) in seeds.iter_mut().chain(steps.iter_mut()) {
                    resolve_virtual_plans(arm, planner)?;
                }
            }
            AccessPath::VirtualScan {
                module,
                offer,
                order_by,
                chosen,
            } => {
                let reference = crate::program::VirtualRef {
                    database: source.table.database,
                    table: source.table.name.clone(),
                    module: module.clone(),
                };
                let mut query = inillucent_sql::vtab::IndexQuery::new(
                    offer.iter().map(|item| item.spec).collect(),
                    order_by.clone(),
                );
                planner.best_index(&reference, &mut query)?;
                let arguments = query.argument_order();
                let recheck = (0..offer.len())
                    .filter(|index| {
                        query
                            .usage
                            .get(*index)
                            .is_none_or(|usage| usage.argument == 0 || !usage.omit)
                    })
                    .collect();
                if query.ordered && level == 0 && !order_by.is_empty() {
                    satisfied_order = true;
                }
                *chosen = Some(inillucent_sql::plan::VirtualChoice {
                    index_number: query.index_number,
                    index_string: query.index_string,
                    arguments,
                    recheck,
                    ordered: query.ordered,
                });
            }
            _ => {}
        }
    }
    if satisfied_order {
        plan.needs_sort = false;
    }
    for (_, arm) in plan.compounds.iter_mut() {
        resolve_virtual_plans(arm, planner)?;
    }
    Ok(())
}

impl Compiler {
    /// Opens every cursor the plan and everything nested inside it will use.
    pub(crate) fn open_all_cursors(&mut self, plan: &PhysicalPlan) -> DbResult<()> {
        for source in &plan.sources {
            match &source.path {
                AccessPath::RecursiveSelf { cte } => {
                    // It reads the CTE's own store, at whatever row the fill
                    // loop is on, so it opens nothing of its own.
                    let cursors = self.cursors_of(*cte)?;
                    self.register_source(source.id, cursors);
                }
                AccessPath::VirtualScan { module, .. } => {
                    let cursor = self.cursors;
                    self.cursors = self.cursors.saturating_add(1);
                    self.register_source(
                        source.id,
                        SourceCursors {
                            table: cursor,
                            index: None,
                            ephemeral: false,
                            index_table: false,
                            virtual_table: true,
                            built: None,
                            matched: None,
                        },
                    );
                    self.emit(
                        Instruction::new(Opcode::VOpen, cursor as i32, 0, 0).with_p4(
                            Operand::Virtual(Box::new(crate::program::VirtualRef {
                                database: source.table.database,
                                table: source.table.name.clone(),
                                module: module.clone(),
                            })),
                        ),
                    );
                }
                AccessPath::Subquery {
                    plan: nested,
                    width,
                    correlated,
                } => {
                    let store = self.ephemeral();
                    let built = (!*correlated).then(|| {
                        let flag = self.register();
                        self.emit(
                            Instruction::new(Opcode::Load, 0, flag as i32, 0)
                                .with_p4(Operand::Integer(0)),
                        );
                        flag
                    });
                    self.register_source(
                        source.id,
                        SourceCursors {
                            table: store,
                            index: None,
                            ephemeral: true,
                            index_table: false,
                            virtual_table: false,
                            built,
                            matched: None,
                        },
                    );
                    self.emit(Instruction::new(
                        Opcode::EphOpen,
                        store as i32,
                        *width as i32,
                        0,
                    ));
                    self.open_all_cursors(nested)?;
                }
                AccessPath::Recursive {
                    seeds,
                    steps,
                    width,
                } => {
                    let store = self.ephemeral();
                    self.register_source(
                        source.id,
                        SourceCursors {
                            table: store,
                            index: None,
                            ephemeral: true,
                            index_table: false,
                            virtual_table: false,
                            built: None,
                            matched: None,
                        },
                    );
                    // The store keeps an index: a `UNION` step has to be able
                    // to ask whether a row it just produced is one the queue
                    // has already seen, which is the only thing that makes the
                    // recursion terminate.
                    self.emit(
                        Instruction::new(Opcode::EphOpen, store as i32, *width as i32, 0)
                            .with_p4(Operand::SortKey(store_key(*width)))
                            .with_p5(1),
                    );
                    for (_, arm) in seeds.iter().chain(steps.iter()) {
                        self.open_all_cursors(arm)?;
                    }
                }
                path => {
                    let table = self.cursors;
                    self.cursors = self.cursors.saturating_add(1);
                    let columns = source.table.columns.len() as i32;
                    // A WITHOUT ROWID table's root is an *index* b-tree - the
                    // key is its primary key and the record is the whole row -
                    // so it is opened as one. Opened as a table cursor it would
                    // be asked for rowids no cell in it carries.
                    if source.table.without_rowid {
                        self.emit(
                            Instruction::new(
                                Opcode::OpenIndex,
                                table as i32,
                                source.table.root as i32,
                                source.table.database as i32,
                            )
                            .with_p4(Operand::IndexKey(primary_key_of(&source.table))),
                        );
                    } else if !matches!(
                        path,
                        AccessPath::IndexSeek {
                            covering: Some(_),
                            ..
                        } | AccessPath::IndexSeekUnion {
                            covering: Some(_),
                            ..
                        }
                    ) {
                        self.emit(
                            Instruction::new(
                                Opcode::OpenRead,
                                table as i32,
                                source.table.root as i32,
                                source.table.database as i32,
                            )
                            .with_p4(Operand::Count(columns.max(0) as u32)),
                        );
                    }
                    let index = match path {
                        AccessPath::IndexSeek { index_root, .. }
                        | AccessPath::IndexSeekUnion { index_root, .. }
                            if source.table.without_rowid && *index_root == source.table.root =>
                        {
                            // The seek is on the table's own key, and the table
                            // cursor is already that index. A second cursor on
                            // the same root would work and would also make
                            // every row read cost two descents.
                            Some(table)
                        }
                        AccessPath::IndexSeek {
                            index_root,
                            collations,
                            descending,
                            ..
                        }
                        | AccessPath::IndexSeekUnion {
                            index_root,
                            collations,
                            descending,
                            ..
                        } => {
                            let cursor = self.cursors;
                            self.cursors = self.cursors.saturating_add(1);
                            let key = IndexKey {
                                columns: collations
                                    .iter()
                                    .zip(descending.iter())
                                    .map(|(collation, descending)| SortColumn {
                                        descending: *descending,
                                        nulls_first: true,
                                        collation: *collation,
                                    })
                                    .collect(),
                            };
                            self.emit(
                                Instruction::new(
                                    Opcode::OpenIndex,
                                    cursor as i32,
                                    *index_root as i32,
                                    source.table.database as i32,
                                )
                                .with_p4(Operand::IndexKey(key)),
                            );
                            Some(cursor)
                        }
                        _ => None,
                    };
                    // A `RIGHT` or `FULL` join owes a pass over the rows of
                    // this term that matched nothing, so the set of the ones
                    // that did is opened here, before any loop.
                    let matched =
                        matches!(source.join, JoinKind::Right | JoinKind::Full).then(|| {
                            let store = self.ephemeral();
                            self.emit(
                                Instruction::new(Opcode::EphOpen, store as i32, 1, 0)
                                    .with_p4(Operand::SortKey(store_key(1)))
                                    .with_p5(1),
                            );
                            store
                        });
                    self.register_source(
                        source.id,
                        SourceCursors {
                            table,
                            index,
                            ephemeral: false,
                            index_table: source.table.without_rowid,
                            virtual_table: false,
                            built: None,
                            matched,
                        },
                    );
                    if let AccessPath::IndexSeek {
                        covering: Some(slots),
                        ..
                    }
                    | AccessPath::IndexSeekUnion {
                        covering: Some(slots),
                        ..
                    } = path
                    {
                        self.covering_slots.insert(source.id, slots.clone());
                        self.used |= Levers::COVERING_INDEX;
                    }
                    self.register_defaults(source.id, &source.table);
                }
            }
        }
        for (_, arm) in &plan.compounds {
            self.open_all_cursors(arm)?;
        }
        self.open_value_subqueries(plan)?;
        Ok(())
    }

    /// Opens a store and cursors for every nested query used as a value.
    ///
    /// This runs before any body is compiled, for the same reason the FROM
    /// terms' cursors do: an expression may be emitted more than once, and a
    /// store opened at the point of use would be opened twice - which the
    /// verifier refuses, and rightly.
    fn open_value_subqueries(&mut self, plan: &PhysicalPlan) -> DbResult<()> {
        let mut found = Vec::new();
        collect_subqueries_of_plan(plan, &mut found);
        self.open_found_subqueries(found)
    }

    /// Opens a store for every subquery in a list of expressions.
    ///
    /// A write statement has no query block to walk: its `WHERE`, its
    /// assignments and its `CHECK`s hang off the statement itself, so it hands
    /// the expressions over directly. Without this, `DELETE FROM t WHERE id IN
    /// (SELECT ...)` compiles a probe against a store nobody opened.
    pub(crate) fn open_subqueries_in(&mut self, exprs: &[&BoundExpr]) -> DbResult<()> {
        let mut found = Vec::new();
        for expr in exprs {
            collect_subqueries(expr, &mut found);
        }
        self.open_found_subqueries(found)
    }

    /// Opens the stores for subqueries that have already been collected.
    fn open_found_subqueries(&mut self, found: Vec<BoundExpr>) -> DbResult<()> {
        for expr in found {
            let BoundExpr::Subquery {
                id,
                kind,
                block,
                affinity,
                collation,
                ..
            } = expr
            else {
                continue;
            };
            if self.subquery_plans.contains_key(&id) {
                continue;
            }
            let mut nested = plan_select_with((*block).clone(), self.levers);
            // A block used as a value is planned here rather than by the
            // planner that built the enclosing one, so it has not been shown
            // to any module yet.
            self.resolve_plan(&mut nested)?;
            let correlated = !block.correlations.is_empty() || !self.substitutions.is_empty();
            let store = self.ephemeral();
            let width = block.columns.len().max(1);
            // Only an `IN` set is ever probed, so only an `IN` set pays for an
            // index. `EXISTS` asks whether the store is empty and a scalar
            // reads its first row; both are answered by a scan.
            let indexed = kind == SubqueryKind::In;
            let mut open = Instruction::new(Opcode::EphOpen, store as i32, width as i32, 0);
            if indexed {
                open = open
                    .with_p4(Operand::SortKey(SortKey {
                        columns: vec![SortColumn {
                            descending: false,
                            nulls_first: true,
                            collation,
                        }],
                    }))
                    .with_p5(1);
            }
            self.emit(open);
            let built = (!correlated).then(|| {
                let flag = self.register();
                self.emit(
                    Instruction::new(Opcode::Load, 0, flag as i32, 0).with_p4(Operand::Integer(0)),
                );
                flag
            });
            self.subquery_plans.insert(
                id,
                ValueSubquery {
                    store,
                    plan: nested.clone(),
                    correlated,
                    built,
                    affinity: (kind == SubqueryKind::In).then_some(affinity).flatten(),
                },
            );
            self.open_all_cursors(&nested)?;
        }
        Ok(())
    }

    /// Compiles one query block, sending its rows wherever the sink says.
    ///
    /// A block owns its `LIMIT`: exhausting a subquery's limit ends that
    /// subquery, not the statement, so the jumps a limit produces are patched
    /// to the end of *this* block rather than to the program's `Halt`.
    pub(crate) fn compile_block(&mut self, plan: &PhysicalPlan, sink: Sink) -> DbResult<()> {
        let outer_jumps = core::mem::take(&mut self.end_jumps);
        let result = self.compile_block_inner(plan, sink);
        let block_end = self.here();
        for label in core::mem::take(&mut self.end_jumps) {
            self.patch(label, block_end);
        }
        self.end_jumps = outer_jumps;
        result
    }

    /// Compiles a block's compound arms, or its single arm.
    fn compile_block_inner(&mut self, plan: &PhysicalPlan, sink: Sink) -> DbResult<()> {
        if !plan.compounds.is_empty() {
            return self.compile_compound(plan, sink);
        }
        if !plan.select.windows.is_empty() {
            return self.compile_windowed(plan, sink);
        }
        let (limit_register, offset_register) = self.compile_limits(plan)?;
        let sorter = self.open_order_sorter(plan);
        let distinct = self.open_distinct(plan);
        let adjacent = self.open_adjacent(plan);
        let body = Body {
            plan,
            sorter,
            distinct,
            adjacent,
            limit_register,
            offset_register,
            sink,
        };
        self.deferred = deferral_map(plan);
        let outcome = self.compile_statement(&body);
        self.deferred = Vec::new();
        outcome
    }

    /// Compiles a block that computes window functions.
    ///
    /// A window runs after `WHERE`, `GROUP BY` and `HAVING` and before
    /// `DISTINCT`, `ORDER BY` and `LIMIT`, so the block is compiled twice over:
    /// once to collect a record per row into a store, and once to drain that
    /// store through the ordinary tail. Everything the output needs is in the
    /// record - the partition and order keys, the call arguments, the `FILTER`
    /// values, the frame offsets, and every subexpression of the result columns
    /// that is not itself a window value.
    fn compile_windowed(&mut self, plan: &PhysicalPlan, sink: Sink) -> DbResult<()> {
        let layout = WindowLayout::of(plan);
        let store = self.ephemeral();
        let width = layout.record.len();
        self.emit(Instruction::new(
            Opcode::EphOpen,
            store as i32,
            width as i32,
            0,
        ));

        // The collecting pass keeps the block's FROM, WHERE, GROUP BY and
        // HAVING and drops everything that belongs after the window.
        let mut collect = plan.clone();
        collect.select.order_by.clear();
        collect.needs_sort = false;
        collect.select.limit = None;
        collect.select.offset = None;
        collect.select.distinct = false;
        collect.select.windows.clear();
        collect.select.columns = layout
            .record
            .iter()
            .map(|expr| BoundResultColumn {
                expr: expr.clone(),
                name: Vec::new(),
                origin: None,
                declared_type: Vec::new(),
            })
            .collect();
        self.compile_block(&collect, Sink::Store(store))?;

        for pass in &layout.passes {
            self.emit(
                Instruction::new(Opcode::EphSort, store as i32, 0, 0)
                    .with_p4(Operand::SortOn(pass.sort.clone())),
            );
            self.emit(
                Instruction::new(Opcode::Window, store as i32, 0, 0)
                    .with_p4(Operand::Window(Box::new(pass.plan.clone()))),
            );
        }

        // The draining pass reads the record and the window values back into
        // registers, and compiles the real result columns against them.
        let total = width.saturating_add(plan.select.windows.len());
        let record = self.register_block(total.max(1));
        let mut drain = plan.clone();
        drain.sources.clear();
        drain.residuals.clear();
        drain.constant_filter = None;
        drain.aggregation = AggregationMode::None;
        drain.select.filter = None;
        drain.select.group_by.clear();
        drain.select.having = None;
        drain.select.aggregates.clear();
        drain.select.values.clear();
        drain.select.windows.clear();
        let (limit_register, offset_register) = self.compile_limits(&drain)?;

        let saved = core::mem::take(&mut self.substitutions);
        for (index, expr) in layout.record.iter().enumerate() {
            self.substitutions
                .push((expr.clone(), record.saturating_add(index as u32)));
        }
        for (slot, column) in layout.slots.iter().enumerate() {
            self.substitutions.push((
                BoundExpr::WindowRef { slot },
                record.saturating_add(*column as u32),
            ));
        }
        let sorter = self.open_order_sorter(&drain);
        let distinct = self.open_distinct(&drain);
        let adjacent = self.open_adjacent(&drain);
        let body = Body {
            plan: &drain,
            sorter,
            distinct,
            adjacent,
            limit_register,
            offset_register,
            sink,
        };
        let outcome = self.drain_window(&body, store, record, total);
        self.substitutions = saved;
        outcome
    }

    /// Reads each windowed row back and runs it through the block's tail.
    fn drain_window(
        &mut self,
        body: &Body<'_>,
        store: u32,
        record: u32,
        total: usize,
    ) -> DbResult<()> {
        let width = body.plan.select.columns.len();
        let block = self.register_block(width.max(1));
        let tail_return = self.register();
        let skip = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        let tail = self.here();
        self.compile_tail(body, block, width, true, tail_return)?;
        self.patch_here(skip);
        let empty = self.emit_jump(Instruction::new(Opcode::EphRewind, store as i32, -1, 0));
        let start = self.here();
        for index in 0..total {
            self.emit(Instruction::new(
                Opcode::EphColumn,
                store as i32,
                index as i32,
                record.saturating_add(index as u32) as i32,
            ));
        }
        self.build_result_row(body, block)?;
        self.emit(Instruction::new(Opcode::Gosub, tail_return as i32, tail, 0));
        let more = self.emit_jump(Instruction::new(Opcode::EphNext, store as i32, -1, 0));
        self.patch(more, start);
        self.patch_here(empty);
        self.drain_sorter(body, width)?;
        Ok(())
    }

    /// Compiles a compound: every arm into one store, then that store drained.
    ///
    /// The arms are combined left to right, which is the only associativity
    /// SQLite gives compound operators. `UNION`, `INTERSECT` and `EXCEPT` each
    /// de-duplicate everything to their left before they apply, so a chain that
    /// starts `UNION ALL` and later meets a `UNION` loses the duplicates the
    /// first operator kept - exactly as SQLite does.
    fn compile_compound(&mut self, plan: &PhysicalPlan, sink: Sink) -> DbResult<()> {
        let width = plan.select.columns.len();
        let key = compound_key(plan);
        let left = self.ephemeral();
        self.emit(
            Instruction::new(Opcode::EphOpen, left as i32, width as i32, 0)
                .with_p4(Operand::SortKey(key.clone()))
                .with_p5(1),
        );
        let mut arm = plan.clone();
        let arms = core::mem::take(&mut arm.compounds);
        // The first arm and the compound are the same struct, so the tail
        // clauses have to be taken off the arm before it is compiled. Leaving
        // them made `a UNION b ORDER BY 1 LIMIT 2 OFFSET 1` apply the limit to
        // `a` alone and then again to the union, which silently dropped a row.
        arm.select.order_by.clear();
        arm.select.limit = None;
        arm.select.offset = None;
        arm.needs_sort = false;
        self.compile_block(&arm, Sink::Store(left))?;
        for (op, next) in &arms {
            if *op != CompoundOp::UnionAll {
                self.emit(Instruction::new(Opcode::EphDedup, left as i32, 0, 0));
            }
            match op {
                CompoundOp::UnionAll => {
                    self.compile_block(next, Sink::Store(left))?;
                }
                CompoundOp::Union => {
                    self.compile_block(next, Sink::UniqueStore(left))?;
                }
                CompoundOp::Intersect | CompoundOp::Except => {
                    let right = self.ephemeral();
                    self.emit(
                        Instruction::new(Opcode::EphOpen, right as i32, width as i32, 0)
                            .with_p4(Operand::SortKey(key.clone()))
                            .with_p5(1),
                    );
                    self.compile_block(next, Sink::UniqueStore(right))?;
                    self.filter_against(left, right, width, *op == CompoundOp::Intersect)?;
                }
            }
        }
        // The compound's own ORDER BY, LIMIT and OFFSET apply to the combined
        // rows, so they are compiled here rather than on any arm.
        let mut drain = plan.clone();
        drain.compounds.clear();
        drain.sources.clear();
        drain.residuals.clear();
        drain.constant_filter = None;
        drain.aggregation = AggregationMode::None;
        drain.select.distinct = false;
        drain.select.filter = None;
        drain.select.group_by.clear();
        drain.select.having = None;
        drain.select.aggregates.clear();
        drain.select.values.clear();
        let (limit_register, offset_register) = self.compile_limits(&drain)?;
        let sorter = self.open_order_sorter(&drain);
        let body = Body {
            plan: &drain,
            sorter,
            distinct: None,
            adjacent: None,
            limit_register,
            offset_register,
            sink,
        };
        self.drain_store(&body, left, width)?;
        Ok(())
    }

    /// Rewrites the left store to the rows that are, or are not, in the right.
    fn filter_against(
        &mut self,
        left: u32,
        right: u32,
        width: usize,
        keep_present: bool,
    ) -> DbResult<()> {
        let block = self.register_block(width.max(1));
        let empty = self.emit_jump(Instruction::new(Opcode::EphRewind, left as i32, -1, 0));
        let start = self.here();
        for index in 0..width {
            self.emit(Instruction::new(
                Opcode::EphColumn,
                left as i32,
                index as i32,
                block.saturating_add(index as u32) as i32,
            ));
        }
        let opcode = if keep_present {
            Opcode::EphFound
        } else {
            Opcode::EphNotFound
        };
        // A row that fails the test is deleted from the left store, so what is
        // left at the end is the answer and no third store is needed.
        let keep = self.emit_jump(
            Instruction::new(opcode, right as i32, -1, block as i32).with_p5(width as u16),
        );
        self.emit_jump(Instruction::new(
            Opcode::EphRemove,
            left as i32,
            self.here().saturating_add(1),
            block as i32,
        ));
        if let Some(instruction) = self.instructions.last_mut() {
            instruction.p5 = width as u16;
        }
        self.patch_here(keep);
        let more = self.emit_jump(Instruction::new(Opcode::EphNext, left as i32, -1, 0));
        self.patch(more, start);
        self.patch_here(empty);
        Ok(())
    }

    /// Drains an ephemeral store through a block's tail.
    fn drain_store(&mut self, body: &Body<'_>, store: u32, width: usize) -> DbResult<()> {
        let block = self.register_block(width.max(1));
        let tail_return = self.register();
        let skip = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        let tail = self.here();
        self.compile_tail(body, block, width, true, tail_return)?;
        self.patch_here(skip);
        let empty = self.emit_jump(Instruction::new(Opcode::EphRewind, store as i32, -1, 0));
        let start = self.here();
        for index in 0..width {
            self.emit(Instruction::new(
                Opcode::EphColumn,
                store as i32,
                index as i32,
                block.saturating_add(index as u32) as i32,
            ));
        }
        self.emit(Instruction::new(Opcode::Gosub, tail_return as i32, tail, 0));
        let more = self.emit_jump(Instruction::new(Opcode::EphNext, store as i32, -1, 0));
        self.patch(more, start);
        self.patch_here(empty);
        self.drain_sorter(body, width)?;
        Ok(())
    }

    /// Evaluates LIMIT and OFFSET into counter registers.
    ///
    /// A NULL or negative LIMIT means no limit and a NULL or negative OFFSET
    /// means no offset, which the normalisation opcode turns into a very large
    /// counter and a zero so the loop needs no special case.
    fn compile_limits(&mut self, plan: &PhysicalPlan) -> DbResult<(Option<u32>, Option<u32>)> {
        let limit = match &plan.select.limit {
            Some(expr) => {
                let source = self.compile_expr(expr)?;
                let counter = self.register();
                self.emit(
                    Instruction::new(Opcode::Copy, source as i32, counter as i32, 0).with_p5(1),
                );
                Some(counter)
            }
            None => None,
        };
        let offset = match &plan.select.offset {
            Some(expr) => {
                let source = self.compile_expr(expr)?;
                let counter = self.register();
                self.emit(
                    Instruction::new(Opcode::Copy, source as i32, counter as i32, 0).with_p5(2),
                );
                Some(counter)
            }
            None => None,
        };
        Ok((limit, offset))
    }

    /// Opens the ORDER BY sorter, when the statement needs one.
    fn open_order_sorter(&mut self, plan: &PhysicalPlan) -> Option<u32> {
        if !plan.needs_sort {
            return None;
        }
        let sorter = self.sorters;
        self.sorters = self.sorters.saturating_add(1);
        let key = SortKey {
            columns: plan.select.order_by.iter().map(sort_column).collect(),
        };
        self.emit(
            Instruction::new(Opcode::SorterOpen, sorter as i32, 0, 0)
                .with_p4(Operand::SortKey(key)),
        );
        Some(sorter)
    }

    /// Reserves the registers an adjacent de-duplication needs.
    ///
    /// A `DISTINCT` whose duplicates arrive next to each other does not need a
    /// set that remembers every row: it needs the row before it. The flag is a
    /// register rather than an assumption because the first row has nothing to
    /// compare against, and a NULL-filled block would compare equal to a row of
    /// NULLs and drop it.
    /// @param plan - the block being compiled
    fn open_adjacent(&mut self, plan: &PhysicalPlan) -> Option<(u32, u32)> {
        if !plan.select.distinct || !plan.distinct_walk {
            return None;
        }
        let width = plan.select.columns.len().max(1);
        let previous = self.register_block(width);
        let started = self.register();
        self.emit(
            Instruction::new(Opcode::Load, 0, started as i32, 0).with_p4(Operand::Integer(0)),
        );
        for index in 0..width {
            self.emit(Instruction::new(
                Opcode::Null,
                0,
                previous.saturating_add(index as u32) as i32,
                0,
            ));
        }
        Some((previous, started))
    }

    /// Opens the DISTINCT set, when the statement needs one.
    fn open_distinct(&mut self, plan: &PhysicalPlan) -> Option<u32> {
        if !plan.select.distinct || plan.distinct_walk {
            return None;
        }
        let set = self.distincts;
        self.distincts = self.distincts.saturating_add(1);
        let key = SortKey {
            columns: plan
                .select
                .columns
                .iter()
                .map(|column| SortColumn {
                    descending: false,
                    nulls_first: true,
                    collation: inillucent_sql::bind::result_collation(&column.expr),
                })
                .collect(),
        };
        self.emit(
            Instruction::new(Opcode::DistinctOpen, set as i32, 0, 0).with_p4(Operand::SortKey(key)),
        );
        Some(set)
    }

    /// Compiles the whole statement body.
    fn compile_statement(&mut self, body: &Body<'_>) -> DbResult<()> {
        if !body.plan.select.values.is_empty() {
            return self.compile_values(body);
        }
        match body.plan.aggregation {
            AggregationMode::None => self.compile_scan(body),
            AggregationMode::Whole => self.compile_whole_aggregate(body),
            AggregationMode::Grouped => self.compile_grouped_aggregate(body),
        }
    }

    /// Compiles a `VALUES` arm: constant rows through the ordinary tail.
    fn compile_values(&mut self, body: &Body<'_>) -> DbResult<()> {
        let width = body.plan.select.values.first().map_or(0, |row| row.len());
        let block = self.register_block(width);
        let tail_return = self.register();
        let skip = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        let tail = self.here();
        self.compile_tail(body, block, width, true, tail_return)?;
        self.patch_here(skip);
        for row in &body.plan.select.values {
            for (index, value) in row.iter().enumerate() {
                let register = self.compile_expr(value)?;
                self.emit(Instruction::new(
                    Opcode::Copy,
                    register as i32,
                    block.saturating_add(index as u32) as i32,
                    0,
                ));
            }
            self.emit(Instruction::new(Opcode::Gosub, tail_return as i32, tail, 0));
        }
        self.drain_sorter(body, width)?;
        Ok(())
    }

    /// Compiles a plain scan: loops, predicates, and a result row.
    ///
    /// The tail is emitted where the row is built rather than as a subroutine
    /// jumped to. It used to be the latter, and the jump cost two instructions
    /// per row of every scan in the engine for a body with a single caller.
    fn compile_scan(&mut self, body: &Body<'_>) -> DbResult<()> {
        let width = body.plan.select.columns.len();
        let block = self.register_block(width);
        self.guard_constant_filter(body)?;
        let emit = InnerBody::InlineRow { block, width };
        self.compile_level(body, 0, &emit)?;
        self.compile_antijoins(body)?;
        self.drain_sorter(body, width)?;
        Ok(())
    }

    /// Compiles an aggregate over the whole input, which yields exactly one row
    /// even when the input is empty.
    fn compile_whole_aggregate(&mut self, body: &Body<'_>) -> DbResult<()> {
        self.reset_accumulators(&body.plan.select.aggregates);
        let skip_scan = self.guard_constant_filter_jump(body)?;
        let step = InnerBody::AggregateStep;
        self.compile_level(body, 0, &step)?;
        self.compile_antijoins(body)?;
        if let Some(label) = skip_scan {
            self.patch_here(label);
        }
        self.finalise_accumulators(&body.plan.select.aggregates)?;
        let width = body.plan.select.columns.len();
        let block = self.register_block(width);
        let tail_return = self.register();
        let skip = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        let tail = self.here();
        self.compile_tail(body, block, width, true, tail_return)?;
        self.patch_here(skip);
        let having_fail = self.compile_having(body)?;
        self.build_result_row(body, block)?;
        self.emit(Instruction::new(Opcode::Gosub, tail_return as i32, tail, 0));
        if let Some(label) = having_fail {
            self.patch_here(label);
        }
        self.drain_sorter(body, width)?;
        Ok(())
    }

    /// Compiles a grouped aggregate whose rows already arrive grouped.
    ///
    /// The same shape as the sorted form with the sorter taken out: the scan
    /// closes a group when the key changes rather than after everything has
    /// been collected and ordered. What makes that legal is a property of the
    /// *walk* - the planner only sets `grouped_walk` when the access path's
    /// leading keys are exactly the group columns, so every row of a group
    /// arrives before the next group starts.
    ///
    /// The subroutine that emits one group is shared with the sorted form and
    /// runs with the same substitutions, because by then the group key is no
    /// longer a row: it is the registers holding the key of the group that has
    /// just ended.
    /// @param body - the block being compiled
    fn compile_streamed_aggregate(&mut self, body: &Body<'_>) -> DbResult<()> {
        let group_count = body.plan.select.group_by.len();
        let width = body.plan.select.columns.len();
        let previous = self.register_block(group_count.max(1));
        let current = self.register_block(group_count.max(1));
        let result_block = self.register_block(width.max(1));
        let tail_return = self.register();
        let group_return = self.register();
        let started = self.register();

        let skip = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        let tail = self.here();
        self.substitutions = group_substitutions(&body.plan.select.group_by, previous);
        self.compile_tail(body, result_block, width, true, tail_return)?;
        let group_emit = self.here();
        self.emit_group_subroutine(body, result_block, tail, tail_return, group_return)?;
        self.substitutions.clear();
        self.patch_here(skip);

        self.emit(
            Instruction::new(Opcode::Load, 0, started as i32, 0).with_p4(Operand::Integer(0)),
        );
        // The key of the group being accumulated is written before the scan as
        // well as inside it. The first row overwrites it before anything reads
        // it - `started` sees to that - but the verifier proves "no register is
        // read before it is written" over the control flow graph, where the
        // path that enters the loop for the first time has not been round it.
        for index in 0..group_count {
            self.emit(Instruction::new(
                Opcode::Null,
                0,
                previous.saturating_add(index as u32) as i32,
                0,
            ));
        }
        self.reset_accumulators(&body.plan.select.aggregates);
        let collations: Vec<Collation> = body
            .plan
            .select
            .group_by
            .iter()
            .map(inillucent_sql::bind::result_collation)
            .collect();
        let step = InnerBody::GroupStream {
            previous,
            current,
            group_count,
            collations,
            group_emit,
            group_return,
            started,
        };
        self.guard_constant_filter(body)?;
        self.compile_level(body, 0, &step)?;
        self.compile_antijoins(body)?;

        // The last group has nothing after it to close it, and a scan that
        // matched no rows has no group to close at all - which is why `started`
        // is a register rather than an assumption.
        let none = self.emit_jump(Instruction::new(Opcode::IfPos, started as i32, -1, 0));
        let done = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        self.patch_here(none);
        self.emit(Instruction::new(
            Opcode::Gosub,
            group_return as i32,
            group_emit,
            0,
        ));
        self.patch_here(done);
        self.drain_sorter(body, width)?;
        Ok(())
    }

    /// Compiles a grouped aggregate: sort by the group key, then stream.
    ///
    /// The order matters and is easy to get wrong. The scan writes the group
    /// key and every aggregate argument into a sorter *without* substitutions,
    /// because there it is computing those values from a row. The subroutine
    /// that emits one group runs *with* substitutions, because there the group
    /// key is no longer a row - it is the registers holding the key of the
    /// group the sorter has just finished.
    fn compile_grouped_aggregate(&mut self, body: &Body<'_>) -> DbResult<()> {
        if body.plan.grouped_walk {
            return self.compile_streamed_aggregate(body);
        }
        let group_count = body.plan.select.group_by.len();
        let payload = self.aggregate_payload(&body.plan.select.aggregates);
        let sorter = self.sorters;
        self.sorters = self.sorters.saturating_add(1);
        let key = SortKey {
            columns: body
                .plan
                .select
                .group_by
                .iter()
                .map(|expr| SortColumn {
                    descending: false,
                    nulls_first: true,
                    collation: inillucent_sql::bind::result_collation(expr),
                })
                .collect(),
        };
        self.emit(
            Instruction::new(Opcode::SorterOpen, sorter as i32, 0, 0)
                .with_p4(Operand::SortKey(key)),
        );
        let record_width = group_count.saturating_add(payload.len());
        let block = self.register_block(record_width.max(1));
        let step = InnerBody::GroupInsert {
            sorter,
            block,
            group_count,
            payload: payload.clone(),
        };
        self.guard_constant_filter(body)?;
        self.compile_level(body, 0, &step)?;
        self.compile_antijoins(body)?;

        // Drain the sorter, one group at a time.
        let width = body.plan.select.columns.len();
        let previous = self.register_block(group_count.max(1));
        let current = self.register_block(group_count.max(1));
        let result_block = self.register_block(width.max(1));
        let tail_return = self.register();
        let group_return = self.register();
        let skip = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        let tail = self.here();
        self.substitutions = group_substitutions(&body.plan.select.group_by, previous);
        self.compile_tail(body, result_block, width, true, tail_return)?;
        let group_emit = self.here();
        self.emit_group_subroutine(body, result_block, tail, tail_return, group_return)?;
        self.substitutions.clear();
        self.patch_here(skip);

        let empty = self.emit_jump(Instruction::new(Opcode::SorterSort, sorter as i32, -1, 0));
        self.reset_accumulators(&body.plan.select.aggregates);
        for index in 0..group_count {
            self.emit(Instruction::new(
                Opcode::SorterColumn,
                sorter as i32,
                index as i32,
                previous.saturating_add(index as u32) as i32,
            ));
        }
        let loop_start = self.here();
        for index in 0..group_count {
            self.emit(Instruction::new(
                Opcode::SorterColumn,
                sorter as i32,
                index as i32,
                current.saturating_add(index as u32) as i32,
            ));
        }
        let collations: Vec<Collation> = body
            .plan
            .select
            .group_by
            .iter()
            .map(inillucent_sql::bind::result_collation)
            .collect();
        let same = self.compile_group_key_equal(group_count, previous, current, &collations);
        self.emit(Instruction::new(
            Opcode::Gosub,
            group_return as i32,
            group_emit,
            0,
        ));
        self.reset_accumulators(&body.plan.select.aggregates);
        for index in 0..group_count {
            self.emit(Instruction::new(
                Opcode::Copy,
                current.saturating_add(index as u32) as i32,
                previous.saturating_add(index as u32) as i32,
                0,
            ));
        }
        for label in same {
            self.patch_here(label);
        }
        self.step_from_sorter(body, sorter, group_count, &payload)?;
        let more = self.emit_jump(Instruction::new(Opcode::SorterNext, sorter as i32, -1, 0));
        self.patch(more, loop_start);
        self.emit(Instruction::new(
            Opcode::Gosub,
            group_return as i32,
            group_emit,
            0,
        ));
        self.patch_here(empty);
        self.drain_sorter(body, width)?;
        Ok(())
    }

    /// Emits the subroutine that finalises one group and emits its row.
    fn emit_group_subroutine(
        &mut self,
        body: &Body<'_>,
        block: u32,
        tail: i32,
        tail_return: u32,
        group_return: u32,
    ) -> DbResult<()> {
        self.finalise_accumulators(&body.plan.select.aggregates)?;
        let having_fail = self.compile_having(body)?;
        self.build_result_row(body, block)?;
        self.emit(Instruction::new(Opcode::Gosub, tail_return as i32, tail, 0));
        if let Some(label) = having_fail {
            self.patch_here(label);
        }
        self.emit(Instruction::new(Opcode::Return, group_return as i32, 0, 0));
        Ok(())
    }

    /// Emits the comparison that decides whether the group key changed.
    ///
    /// Group keys compare with `IS` semantics, so two NULL keys are the same
    /// group. Using ordinary equality would give every NULL its own group,
    /// which is not what `GROUP BY` does.
    fn compile_group_key_equal(
        &mut self,
        count: usize,
        previous: u32,
        current: u32,
        collations: &[Collation],
    ) -> Vec<Label> {
        let mut equal = Vec::new();
        let mut differs = Vec::new();
        for index in 0..count {
            let result = self.register();
            self.emit(
                Instruction::new(
                    Opcode::Is,
                    previous.saturating_add(index as u32) as i32,
                    current.saturating_add(index as u32) as i32,
                    result as i32,
                )
                .with_p4(Operand::Comparison(Comparison {
                    op: BinaryOp::Equal,
                    affinity: None,
                    // The boundary must use the same collation the sorter
                    // clustered with, or a group the sort put together is split
                    // apart again one row later.
                    collation: collations.get(index).copied().unwrap_or(Collation::Binary),
                })),
            );
            differs.push(self.emit_jump(Instruction::new(Opcode::IfNot, result as i32, -1, 0)));
        }
        equal.push(self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0)));
        for label in differs {
            self.patch_here(label);
        }
        equal
    }

    /// Returns the payload expressions a grouped aggregate stores per row.
    fn aggregate_payload(&self, aggregates: &[BoundAggregate]) -> Vec<BoundExpr> {
        let mut payload = Vec::new();
        for aggregate in aggregates {
            for argument in &aggregate.arguments {
                payload.push(argument.clone());
            }
        }
        payload
    }

    /// Emits the `AggStep` calls that read one sorted row.
    fn step_from_sorter(
        &mut self,
        body: &Body<'_>,
        sorter: u32,
        group_count: usize,
        payload: &[BoundExpr],
    ) -> DbResult<()> {
        let mut cursor = 0usize;
        for (slot, aggregate) in body.plan.select.aggregates.iter().enumerate() {
            let count = aggregate.arguments.len();
            let block = self.register_block(count.max(1));
            for index in 0..count {
                self.emit(Instruction::new(
                    Opcode::SorterColumn,
                    sorter as i32,
                    group_count.saturating_add(cursor).saturating_add(index) as i32,
                    block.saturating_add(index as u32) as i32,
                ));
            }
            cursor = cursor.saturating_add(count);
            self.emit(
                Instruction::new(Opcode::AggStep, block as i32, count as i32, slot as i32).with_p4(
                    Operand::Aggregate(AggregateCall {
                        func: aggregate.func,
                        external: aggregate.external.clone(),
                        distinct: aggregate.distinct,
                        collation: aggregate.collation,
                    }),
                ),
            );
        }
        let _ = payload;
        Ok(())
    }

    /// Steps every aggregate from the row the cursors are standing on.
    ///
    /// The same code the whole-table aggregate runs per row and the streaming
    /// group runs per row of its group - one place, because two would be two
    /// answers to "what does this row contribute".
    /// @param body - the block being compiled
    fn step_aggregates(&mut self, body: &Body<'_>) -> DbResult<()> {
        let aggregates = body.plan.select.aggregates.clone();
        self.aggregates = self.aggregates.max(aggregates.len() as u32);
        for (slot, aggregate) in aggregates.iter().enumerate() {
            let count = aggregate.arguments.len();
            let block = self.register_block(count.max(1));
            for (index, argument) in aggregate.arguments.iter().enumerate() {
                let register = self.compile_expr(argument)?;
                self.emit(Instruction::new(
                    Opcode::Copy,
                    register as i32,
                    block.saturating_add(index as u32) as i32,
                    0,
                ));
            }
            self.emit(
                Instruction::new(Opcode::AggStep, block as i32, count as i32, slot as i32).with_p4(
                    Operand::Aggregate(AggregateCall {
                        func: aggregate.func,
                        external: aggregate.external.clone(),
                        distinct: aggregate.distinct,
                        collation: aggregate.collation,
                    }),
                ),
            );
        }
        Ok(())
    }

    /// Resets every accumulator, which starts a fresh group.
    fn reset_accumulators(&mut self, aggregates: &[BoundAggregate]) {
        self.aggregates = self.aggregates.max(aggregates.len() as u32);
        for (slot, aggregate) in aggregates.iter().enumerate() {
            self.emit(
                Instruction::new(Opcode::AggReset, slot as i32, 0, 0).with_p4(Operand::Aggregate(
                    AggregateCall {
                        func: aggregate.func,
                        external: aggregate.external.clone(),
                        distinct: aggregate.distinct,
                        collation: aggregate.collation,
                    },
                )),
            );
        }
    }

    /// Finalises every accumulator into a register the result columns read.
    fn finalise_accumulators(&mut self, aggregates: &[BoundAggregate]) -> DbResult<()> {
        self.aggregate_registers.clear();
        for (slot, aggregate) in aggregates.iter().enumerate() {
            let register = self.register();
            self.emit(
                Instruction::new(Opcode::AggFinal, slot as i32, register as i32, 0).with_p4(
                    Operand::Aggregate(AggregateCall {
                        func: aggregate.func,
                        external: aggregate.external.clone(),
                        distinct: aggregate.distinct,
                        collation: aggregate.collation,
                    }),
                ),
            );
            self.aggregate_registers.push(register);
        }
        Ok(())
    }

    /// Emits the `HAVING` test, returning the jump taken when it fails.
    fn compile_having(&mut self, body: &Body<'_>) -> DbResult<Option<Label>> {
        let Some(having) = &body.plan.select.having else {
            return Ok(None);
        };
        let register = self.compile_expr(having)?;
        Ok(Some(self.emit_jump(
            Instruction::new(Opcode::IfNot, register as i32, -1, 0).with_p5(1),
        )))
    }

    /// Emits the constant filter, jumping over the loops when it is false.
    fn guard_constant_filter(&mut self, body: &Body<'_>) -> DbResult<()> {
        if let Some(label) = self.guard_constant_filter_jump(body)? {
            self.end_jumps.push(label);
        }
        Ok(())
    }

    /// Emits the constant filter, returning the jump taken when it is false.
    fn guard_constant_filter_jump(&mut self, body: &Body<'_>) -> DbResult<Option<Label>> {
        let Some(filter) = &body.plan.constant_filter else {
            return Ok(None);
        };
        let register = self.compile_expr(filter)?;
        Ok(Some(self.emit_jump(
            Instruction::new(Opcode::IfNot, register as i32, -1, 0).with_p5(1),
        )))
    }

    /// Drains the ORDER BY sorter, applying OFFSET and LIMIT as it goes.
    fn drain_sorter(&mut self, body: &Body<'_>, width: usize) -> DbResult<()> {
        let Some(sorter) = body.sorter else {
            return Ok(());
        };
        let key_count = body.plan.select.order_by.len();
        let block = self.register_block(width);
        let empty = self.emit_jump(Instruction::new(Opcode::SorterSort, sorter as i32, -1, 0));
        let loop_start = self.here();
        for index in 0..width {
            self.emit(Instruction::new(
                Opcode::SorterColumn,
                sorter as i32,
                key_count.saturating_add(index) as i32,
                block.saturating_add(index as u32) as i32,
            ));
        }
        let skip = self.emit_offset_check(body);
        let done = self.emit_limit_precheck(body);
        let dropped = self.emit_sink(body, block, width);
        let exhausted = self.emit_limit_decrement(body);
        for label in skip.into_iter().chain(dropped) {
            self.patch_here(label);
        }
        let more = self.emit_jump(Instruction::new(Opcode::SorterNext, sorter as i32, -1, 0));
        self.patch(more, loop_start);
        for label in done.into_iter().chain(exhausted) {
            self.patch_here(label);
        }
        self.patch_here(empty);
        Ok(())
    }

    /// Compiles the tail every emitted row passes through.
    ///
    /// With an ORDER BY the tail writes the row into the sorter and returns;
    /// without one it applies OFFSET and LIMIT and emits the row.
    fn compile_tail(
        &mut self,
        body: &Body<'_>,
        block: u32,
        width: usize,
        subroutine: bool,
        return_register: u32,
    ) -> DbResult<()> {
        let mut skip: Vec<Label> = Vec::new();
        if let Some(set) = body.distinct {
            skip.push(
                self.emit_jump(
                    Instruction::new(Opcode::DistinctCheck, set as i32, -1, block as i32)
                        .with_p5(width as u16),
                ),
            );
        }
        if let Some((previous, started)) = body.adjacent {
            let collations: Vec<Collation> = body
                .plan
                .select
                .columns
                .iter()
                .map(|column| inillucent_sql::bind::result_collation(&column.expr))
                .collect();
            let compare = self.emit_jump(Instruction::new(Opcode::IfPos, started as i32, -1, 0));
            let first = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
            self.patch_here(compare);
            let same = self.compile_group_key_equal(width, previous, block, &collations);
            // Falling out of the comparison means the rows differ.
            let differs = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
            for label in same {
                self.patch_here(label);
            }
            skip.push(self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0)));
            self.patch_here(differs);
            self.patch_here(first);
            for index in 0..width {
                self.emit(Instruction::new(
                    Opcode::Copy,
                    block.saturating_add(index as u32) as i32,
                    previous.saturating_add(index as u32) as i32,
                    0,
                ));
            }
            self.emit(
                Instruction::new(Opcode::Load, 0, started as i32, 0).with_p4(Operand::Integer(1)),
            );
        }
        if let Some(sorter) = body.sorter {
            let key_count = body.plan.select.order_by.len();
            let record = self.register_block(key_count.saturating_add(width));
            for (index, term) in body.plan.select.order_by.clone().iter().enumerate() {
                let register = self.compile_order_key(term, block, body)?;
                self.emit(Instruction::new(
                    Opcode::Copy,
                    register as i32,
                    record.saturating_add(index as u32) as i32,
                    0,
                ));
            }
            for index in 0..width {
                self.emit(Instruction::new(
                    Opcode::Copy,
                    block.saturating_add(index as u32) as i32,
                    record
                        .saturating_add(key_count as u32)
                        .saturating_add(index as u32) as i32,
                    0,
                ));
            }
            self.emit(Instruction::new(
                Opcode::SorterInsert,
                sorter as i32,
                record as i32,
                key_count.saturating_add(width) as i32,
            ));
            for label in skip {
                self.patch_here(label);
            }
            if subroutine {
                self.emit(Instruction::new(
                    Opcode::Return,
                    return_register as i32,
                    0,
                    0,
                ));
            }
            return Ok(());
        }
        let offset_skip = self.emit_offset_check(body);
        let done = self.emit_limit_precheck(body);
        let dropped = self.emit_sink(body, block, width);
        let exhausted = self.emit_limit_decrement(body);
        for label in skip.into_iter().chain(offset_skip).chain(dropped) {
            self.patch_here(label);
        }
        if subroutine {
            self.emit(Instruction::new(
                Opcode::Return,
                return_register as i32,
                0,
                0,
            ));
        }
        for label in done.into_iter().chain(exhausted) {
            self.end_jumps.push(label);
        }
        Ok(())
    }

    /// Compiles one ORDER BY key, reading a result column where the term is
    /// one so the key and the row cannot disagree.
    fn compile_order_key(
        &mut self,
        term: &BoundOrderTerm,
        block: u32,
        body: &Body<'_>,
    ) -> DbResult<u32> {
        // A compound's `ORDER BY` names a result column by number, because the
        // arms do not share a FROM clause for it to name anything else in. The
        // number indexes the block the row was just read into.
        if let BoundExpr::SorterColumn { column } = &term.expr {
            return Ok(block.saturating_add(u32::from(*column)));
        }
        if let Some(index) = body
            .plan
            .select
            .columns
            .iter()
            .position(|column| column.expr == term.expr)
        {
            return Ok(block.saturating_add(index as u32));
        }
        self.compile_expr(&term.expr)
    }

    /// Emits whatever the block does with a finished row.
    ///
    /// The returned labels jump past the limit decrement: a row the sink
    /// dropped as a duplicate did not count against `LIMIT`, and counting it
    /// would make `SELECT a UNION SELECT b LIMIT 1` stop after the duplicate
    /// rather than after the row.
    fn emit_sink(&mut self, body: &Body<'_>, block: u32, width: usize) -> Vec<Label> {
        match body.sink {
            Sink::Result => {
                self.emit(Instruction::new(
                    Opcode::ResultRow,
                    block as i32,
                    width as i32,
                    0,
                ));
                Vec::new()
            }
            Sink::Store(store) => {
                self.emit(Instruction::new(
                    Opcode::EphInsert,
                    store as i32,
                    block as i32,
                    width as i32,
                ));
                Vec::new()
            }
            Sink::UniqueStore(store) => {
                vec![self.emit_jump(
                    Instruction::new(Opcode::EphInsertUnique, store as i32, -1, block as i32)
                        .with_p5(width as u16),
                )]
            }
            Sink::TypedStore(store, affinity) => {
                if let Some(affinity) = affinity {
                    self.emit(
                        Instruction::new(Opcode::ApplyAffinity, block as i32, width as i32, 0)
                            .with_p4(Operand::Affinity(affinity)),
                    );
                }
                self.emit(Instruction::new(
                    Opcode::EphInsert,
                    store as i32,
                    block as i32,
                    width as i32,
                ));
                Vec::new()
            }
        }
    }

    /// Emits the OFFSET test, returning the jump that skips a row.
    fn emit_offset_check(&mut self, body: &Body<'_>) -> Vec<Label> {
        let Some(offset) = body.offset_register else {
            return Vec::new();
        };
        vec![self.emit_jump(Instruction::new(Opcode::IfPos, offset as i32, -1, 1))]
    }

    /// Emits the LIMIT test taken before a row is emitted.
    fn emit_limit_precheck(&mut self, body: &Body<'_>) -> Vec<Label> {
        let Some(limit) = body.limit_register else {
            return Vec::new();
        };
        let proceed = self.emit_jump(Instruction::new(Opcode::IfPos, limit as i32, -1, 0));
        let done = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        self.patch_here(proceed);
        vec![done]
    }

    /// Emits the LIMIT decrement taken after a row is emitted.
    fn emit_limit_decrement(&mut self, body: &Body<'_>) -> Vec<Label> {
        let Some(limit) = body.limit_register else {
            return Vec::new();
        };
        vec![self.emit_jump(Instruction::new(Opcode::DecrJumpZero, limit as i32, -1, 0))]
    }

    /// Compiles the loop for one FROM term, and then the level inside it.
    ///
    /// An outer join splits in two: the levels below it become a subroutine,
    /// so that the matched row and the null-extended row can both run the same
    /// body without a second copy of it in the program.
    fn compile_level(&mut self, body: &Body<'_>, level: usize, inner: &InnerBody) -> DbResult<()> {
        if level >= body.plan.sources.len() {
            return self.compile_inner(body, inner);
        }
        let Some(source) = body.plan.sources.get(level) else {
            return Err(error::misuse("plan level out of range"));
        };
        if self.stop_at == Some(level) {
            return self.compile_inner(body, inner);
        }
        if is_outer(source.join) {
            return self.compile_outer_join(body, level, inner);
        }
        self.compile_loop(body, level, inner)
    }

    /// Compiles an outer join level: the loop, then the rows it owes.
    ///
    /// `LEFT` owes a null-extended row for every left row that matched nothing,
    /// which is known as soon as its loop ends. `RIGHT` owes one for every
    /// *right* row that matched nothing, which is not known until the whole
    /// nest has run - so the matching rows are recorded as they go and the pass
    /// over the rest is emitted afterwards. `FULL` owes both.
    fn compile_outer_join(
        &mut self,
        body: &Body<'_>,
        level: usize,
        inner: &InnerBody,
    ) -> DbResult<()> {
        let join = body
            .plan
            .sources
            .get(level)
            .map_or(JoinKind::Left, |source| source.join);
        let keeps_left = matches!(join, JoinKind::Left | JoinKind::Full);
        let keeps_right = matches!(join, JoinKind::Right | JoinKind::Full);
        let matched = self.register();
        let ret = self.register();
        let on = body
            .plan
            .sources
            .get(level)
            .and_then(|source| source.on.clone());
        // The right side's matched rows are recorded by their rowid, which is
        // the only identity a row has that survives the cursor moving away and
        // coming back on a second pass. The store was opened with the cursors,
        // before any loop.
        let matched_set = if keeps_right {
            self.cursors_of(source_id(body, level)?)?.matched
        } else {
            None
        };
        // The continuation is emitted with the join's own limits cleared, so
        // that a nested outer join inside it is compiled as a whole rather than
        // stopping where this one stops.
        // The continuation tests every residual this join deferred - its own
        // term's, and for a `RIGHT` join every term to its left as well.
        let owed: Vec<usize> = self
            .deferred
            .iter()
            .enumerate()
            .filter_map(|(candidate, owner)| (*owner == Some(level)).then_some(candidate))
            .collect();
        let saved_deferred = core::mem::take(&mut self.deferred);
        let saved_stop = self.stop_at.take();
        let skip = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        let continuation = self.here();
        let mut residual = Vec::new();
        for candidate in &owed {
            residual.extend(self.compile_residual(body, *candidate)?);
        }
        self.compile_level(body, level.saturating_add(1), inner)?;
        for label in residual {
            self.patch_here(label);
        }
        self.emit(Instruction::new(Opcode::Return, ret as i32, 0, 0));
        self.patch_here(skip);
        self.emit(
            Instruction::new(Opcode::Load, 0, matched as i32, 0).with_p4(Operand::Integer(0)),
        );
        let entered = InnerBody::JoinMatch {
            matched,
            ret,
            continuation,
            on,
            matched_set,
            source: source_id(body, level)?,
        };
        self.deferred = saved_deferred;
        self.stop_at = Some(level.saturating_add(1));
        let outcome = self.compile_loop(body, level, &entered);
        self.stop_at = saved_stop;
        outcome?;
        if keeps_left {
            let done = self.emit_jump(Instruction::new(Opcode::If, matched as i32, -1, 0));
            let cursors = self.cursors_of(source_id(body, level)?)?;
            if !cursors.ephemeral {
                self.emit(Instruction::new(
                    Opcode::NullRow,
                    cursors.table as i32,
                    0,
                    0,
                ));
            }
            self.emit(Instruction::new(Opcode::Gosub, ret as i32, continuation, 0));
            self.patch_here(done);
        }
        if let Some(store) = matched_set {
            self.antijoins.push(AntiJoin {
                level,
                matched: store,
                ret,
                continuation,
            });
        }
        Ok(())
    }

    /// Emits the second pass every `RIGHT` or `FULL` join in a block owes.
    ///
    /// It scans the join's own term, skips the rows the nest already matched,
    /// and null-rows every term *before* it - an unmatched right row pairs with
    /// nothing on the left. The terms after it are joined normally, which is
    /// why the pass enters the same continuation the matched rows did rather
    /// than emitting the body a second time.
    fn compile_antijoins(&mut self, body: &Body<'_>) -> DbResult<()> {
        let pending = core::mem::take(&mut self.antijoins);
        for pass in pending.iter().rev() {
            let cursors = self.cursors_of(source_id(body, pass.level)?)?;
            if cursors.ephemeral {
                return Err(error::misuse(
                    "a right join over a materialised term is not supported",
                ));
            }
            let key = self.register();
            let empty = self.emit_jump(Instruction::new(
                Opcode::Rewind,
                cursors.table as i32,
                -1,
                0,
            ));
            let start = self.here();
            self.emit(Instruction::new(
                Opcode::Rowid,
                cursors.table as i32,
                key as i32,
                0,
            ));
            let seen = self.emit_jump(
                Instruction::new(Opcode::EphFound, pass.matched as i32, -1, key as i32).with_p5(1),
            );
            for earlier in 0..pass.level {
                let before = self.cursors_of(source_id(body, earlier)?)?;
                if !before.ephemeral {
                    self.emit(Instruction::new(Opcode::NullRow, before.table as i32, 0, 0));
                }
            }
            self.emit(Instruction::new(
                Opcode::Gosub,
                pass.ret as i32,
                pass.continuation,
                0,
            ));
            self.patch_here(seen);
            let more = self.emit_jump(Instruction::new(Opcode::Next, cursors.table as i32, -1, 0));
            self.patch(more, start);
            self.patch_here(empty);
        }
        Ok(())
    }

    /// Compiles one FROM term's loop, whatever the join that attached it.
    fn compile_loop(&mut self, body: &Body<'_>, level: usize, inner: &InnerBody) -> DbResult<()> {
        let Some(source) = body.plan.sources.get(level) else {
            return Err(error::misuse("plan level out of range"));
        };
        let cursors = self.cursors_of(source.id)?;
        let path = source.path.clone();
        if let AccessPath::Subquery {
            plan,
            width,
            correlated,
        } = &path
        {
            return self.compile_subquery_level(
                body,
                level,
                cursors,
                plan,
                *width,
                *correlated,
                inner,
            );
        }
        if let AccessPath::Recursive { seeds, steps, .. } = &path {
            return self.compile_recursive_level(body, level, cursors, seeds, steps, inner);
        }
        if let AccessPath::RecursiveSelf { .. } = &path {
            // One row, already positioned: the body runs once, against the row
            // the enclosing fill loop is standing on.
            let skip = self.compile_residual(body, level)?;
            self.compile_level(body, level.saturating_add(1), inner)?;
            for label in skip {
                self.patch_here(label);
            }
            return Ok(());
        }
        // Only the outermost term can be walked backwards: an inner loop
        // restarts for every outer row, and the order inside one of those runs
        // is not the order of the result.
        let reverse = body.plan.reverse && level == 0;
        if level == 0 && !body.plan.needs_sort && !body.plan.select.order_by.is_empty() {
            self.used |= Levers::ORDERED_WALK;
        }
        if level == 0 && (body.plan.grouped_walk || body.plan.distinct_walk) {
            self.used |= Levers::STREAMING_GROUP;
        }
        match path {
            AccessPath::TableScan { .. } => {
                let (first, step) = if reverse {
                    (Opcode::Last, Opcode::Prev)
                } else {
                    (Opcode::Rewind, Opcode::Next)
                };
                let empty = self.emit_jump(Instruction::new(first, cursors.table as i32, -1, 0));
                let start = self.here();
                let skip = self.compile_residual(body, level)?;
                self.compile_level(body, level.saturating_add(1), inner)?;
                for label in skip {
                    self.patch_here(label);
                }
                let more = self.emit_jump(Instruction::new(step, cursors.table as i32, -1, 0));
                self.patch(more, start);
                self.patch_here(empty);
            }
            AccessPath::RowidSeek { key, .. } => {
                let register = self.compile_expr(&key)?;
                let missing = self.emit_jump(Instruction::new(
                    Opcode::SeekRowid,
                    cursors.table as i32,
                    -1,
                    register as i32,
                ));
                let skip = self.compile_residual(body, level)?;
                self.compile_level(body, level.saturating_add(1), inner)?;
                for label in skip {
                    self.patch_here(label);
                }
                self.patch_here(missing);
            }
            AccessPath::RowidRange { low, high, .. } => {
                self.compile_rowid_range(body, level, cursors, low, high, reverse, inner)?;
            }
            AccessPath::IndexSeek {
                equalities,
                low,
                high,
                columns,
                descending,
                without_rowid,
                key_entry_slots,
                covering,
                ..
            } => {
                self.compile_index_seek(
                    body,
                    level,
                    cursors,
                    &equalities,
                    low,
                    high,
                    &named_columns(columns),
                    &descending,
                    without_rowid,
                    &key_entry_slots,
                    covering.is_some(),
                    reverse,
                    None,
                    inner,
                )?;
            }
            AccessPath::RowidSeekUnion { keys, .. } => {
                self.compile_rowid_seek_union(body, level, cursors, &keys, inner)?;
            }
            AccessPath::IndexSeekUnion {
                branches,
                collations,
                columns,
                descending,
                without_rowid,
                key_entry_slots,
                covering,
                dedup,
                ..
            } => {
                self.compile_index_seek_union(
                    body,
                    level,
                    cursors,
                    &branches,
                    collations.first().copied().unwrap_or(Collation::Binary),
                    &named_columns(columns),
                    &descending,
                    without_rowid,
                    &key_entry_slots,
                    covering.is_some(),
                    reverse,
                    dedup,
                    inner,
                )?;
            }
            AccessPath::VirtualScan { offer, chosen, .. } => {
                self.compile_virtual_scan(body, level, cursors, &offer, chosen.as_ref(), inner)?;
            }
            AccessPath::Subquery { .. }
            | AccessPath::Recursive { .. }
            | AccessPath::RecursiveSelf { .. } => {
                return Err(error::misuse("a materialised path reached the table loop"));
            }
            // The old engine has no vector index and never plans one: the
            // planner only offers this path for a table whose catalog carries
            // one, and only the new engine's catalog ever does.
            AccessPath::VectorProbe { .. } => {
                return Err(error::misuse("a vector index on the bytecode engine"));
            }
        }
        Ok(())
    }

    /// Compiles a rowid range scan.
    fn compile_rowid_range(
        &mut self,
        body: &Body<'_>,
        level: usize,
        cursors: SourceCursors,
        low: Option<RangeBound>,
        high: Option<RangeBound>,
        reverse: bool,
        inner: &InnerBody,
    ) -> DbResult<()> {
        // Backwards, the two ends of the range swap jobs: the walk starts at
        // the high bound and stops at the low one. Everything else about the
        // loop is the same shape, which is the point of naming them by role.
        let (from, until) = if reverse {
            (&high, &low)
        } else {
            (&low, &high)
        };
        let empty = match from {
            Some(bound) => {
                let register = self.compile_expr(&bound.value)?;
                self.emit(
                    Instruction::new(Opcode::Cast, register as i32, register as i32, 0)
                        .with_p4(Operand::Affinity(Affinity::Integer)),
                );
                let opcode = match (reverse, bound.kind) {
                    (false, BoundKind::Greater) => Opcode::SeekGt,
                    (false, _) => Opcode::SeekGe,
                    (true, BoundKind::Less) => Opcode::SeekLt,
                    (true, _) => Opcode::SeekLe,
                };
                self.emit_jump(
                    Instruction::new(opcode, cursors.table as i32, -1, register as i32).with_p5(1),
                )
            }
            None => self.emit_jump(Instruction::new(
                if reverse {
                    Opcode::Last
                } else {
                    Opcode::Rewind
                },
                cursors.table as i32,
                -1,
                0,
            )),
        };
        let start = self.here();
        let mut done: Vec<Label> = Vec::new();
        if let Some(bound) = until {
            let limit = self.compile_expr(&bound.value)?;
            let rowid = self.register();
            self.emit(Instruction::new(
                Opcode::Rowid,
                cursors.table as i32,
                rowid as i32,
                0,
            ));
            let result = self.register();
            let op = match (reverse, bound.kind) {
                (false, BoundKind::Less) => BinaryOp::Less,
                (false, _) => BinaryOp::LessEqual,
                (true, BoundKind::Greater) => BinaryOp::Greater,
                (true, _) => BinaryOp::GreaterEqual,
            };
            self.emit(
                Instruction::new(Opcode::Compare, rowid as i32, limit as i32, result as i32)
                    .with_p4(Operand::Comparison(Comparison {
                        op,
                        affinity: Some(Affinity::Integer),
                        collation: Collation::Binary,
                    })),
            );
            done.push(
                self.emit_jump(Instruction::new(Opcode::IfNot, result as i32, -1, 0).with_p5(1)),
            );
        }
        let skip = self.compile_residual(body, level)?;
        self.compile_level(body, level.saturating_add(1), inner)?;
        for label in skip {
            self.patch_here(label);
        }
        let more = self.emit_jump(Instruction::new(
            if reverse { Opcode::Prev } else { Opcode::Next },
            cursors.table as i32,
            -1,
            0,
        ));
        self.patch(more, start);
        for label in done {
            self.patch_here(label);
        }
        self.patch_here(empty);
        Ok(())
    }

    /// Compiles an index seek: position, walk, stop at the far bound, and
    /// fetch the table row for each index entry.
    #[allow(clippy::too_many_arguments)]
    fn compile_index_seek(
        &mut self,
        body: &Body<'_>,
        level: usize,
        cursors: SourceCursors,
        equalities: &[BoundExpr],
        low: Option<RangeBound>,
        high: Option<RangeBound>,
        columns: &[u16],
        descending: &[bool],
        without_rowid: bool,
        key_slots: &[usize],
        covering: bool,
        reverse: bool,
        dedup: Option<u32>,
        inner: &InnerBody,
    ) -> DbResult<()> {
        let Some(index_cursor) = cursors.index else {
            return Err(error::misuse("an index path with no index cursor"));
        };
        // Backwards, the two ends of the range swap jobs: the walk is seeked to
        // the high bound and stops at the low one.
        let (from, until) = if reverse {
            (&high, &low)
        } else {
            (&low, &high)
        };
        // A bound on a column never matches a NULL in it - `k < 5` is unknown
        // for a NULL k, not true - and an index holds its NULLs at the front. A
        // forward walk carrying only a far bound therefore has to start *past*
        // them rather than at the beginning, which is a seek strictly after
        // NULL. Without it `WHERE k < 5` returned every NULL row in the table
        // ahead of its answer, silently, on any index and with no ORDER BY in
        // sight.
        //
        // Backwards there is nothing to do: the NULLs sit at the far end of that
        // walk, and the bound's own stopping test reaches them and stops.
        // Only an *ascending* column keeps its NULLs at the front; a
        // descending one keeps them at the back, where a forward walk reaches
        // them last and the seek would jump clean past the answer.
        let ranged_descending =
            (low.is_some() || high.is_some()) && descending.last().copied().unwrap_or(false);
        let skip_nulls = !reverse && from.is_none() && until.is_some() && !ranged_descending;
        let key_len = equalities
            .len()
            .saturating_add(usize::from(from.is_some() || skip_nulls));
        let key = self.register_block(key_len.max(1));
        for (position, expr) in equalities.iter().enumerate() {
            let register = self.compile_expr(expr)?;
            self.emit(Instruction::new(
                Opcode::Copy,
                register as i32,
                key.saturating_add(position as u32) as i32,
                0,
            ));
        }
        let mut seek_len = equalities.len();
        let mut opcode = if reverse {
            Opcode::SeekLe
        } else {
            Opcode::SeekGe
        };
        if let Some(bound) = from {
            let register = self.compile_expr(&bound.value)?;
            self.emit(Instruction::new(
                Opcode::Copy,
                register as i32,
                key.saturating_add(equalities.len() as u32) as i32,
                0,
            ));
            seek_len = seek_len.saturating_add(1);
            opcode = match (reverse, bound.kind) {
                (false, BoundKind::Greater) => Opcode::SeekGt,
                (false, _) => Opcode::SeekGe,
                (true, BoundKind::Less) => Opcode::SeekLt,
                (true, _) => Opcode::SeekLe,
            };
        } else if skip_nulls {
            self.emit(Instruction::new(
                Opcode::Null,
                0,
                key.saturating_add(equalities.len() as u32) as i32,
                0,
            ));
            seek_len = seek_len.saturating_add(1);
            opcode = Opcode::SeekGt;
        }
        self.apply_index_affinity(body, level, columns, key, seek_len)?;
        // A NULL equality key matches nothing at all. The index *stores* NULLs
        // and orders them together, so a seek on one finds them - but `x = NULL`
        // is unknown, not true, and the rows must not be returned. Without this
        // guard `a JOIN b ON a.team = b.team` paired the row whose team is NULL
        // on one side with the row whose team is NULL on the other, but only
        // once the planner started choosing the index for that equality.
        let mut null_key: Vec<Label> = Vec::new();
        for position in 0..equalities.len() {
            null_key.push(self.emit_jump(Instruction::new(
                Opcode::IfNull,
                key.saturating_add(position as u32) as i32,
                -1,
                0,
            )));
        }
        // One branch of a seek union, skipped whole when its own key has
        // already been probed by an earlier branch. Checked (and recorded)
        // on the equality prefix alone, before the seek runs - a repeated key
        // seeks to the same starting entry and walks the same run of matches
        // every time, so a branch whose key was already used cannot produce a
        // row the earlier branch did not already produce.
        if let Some(set) = dedup {
            null_key.push(
                self.emit_jump(
                    Instruction::new(Opcode::DistinctCheck, set as i32, -1, key as i32)
                        .with_p5(equalities.len() as u16),
                ),
            );
        }
        // With nothing to seek to, the path is a scan of the whole index -
        // which is worth choosing when the index carries every column the query
        // reads, because an entry is narrower than a row.
        let empty = if seek_len == 0 {
            self.emit_jump(Instruction::new(
                if reverse {
                    Opcode::Last
                } else {
                    Opcode::Rewind
                },
                index_cursor as i32,
                -1,
                0,
            ))
        } else {
            self.emit_jump(
                Instruction::new(opcode, index_cursor as i32, -1, key as i32)
                    .with_p5(seek_len as u16),
            )
        };
        let start = self.here();
        let mut done: Vec<Label> = Vec::new();
        // The equality prefix has to be re-checked on every entry, because a
        // seek positions at the first matching entry and the scan runs past the
        // last one.
        if !equalities.is_empty() {
            done.push(
                self.emit_jump(
                    Instruction::new(
                        if reverse {
                            Opcode::IdxLt
                        } else {
                            Opcode::IdxGt
                        },
                        index_cursor as i32,
                        -1,
                        key as i32,
                    )
                    .with_p5(equalities.len() as u16),
                ),
            );
        }
        if let Some(bound) = until {
            let stop_key = self.register_block(equalities.len().saturating_add(1));
            for index in 0..equalities.len() {
                self.emit(Instruction::new(
                    Opcode::Copy,
                    key.saturating_add(index as u32) as i32,
                    stop_key.saturating_add(index as u32) as i32,
                    0,
                ));
            }
            let register = self.compile_expr(&bound.value)?;
            self.emit(Instruction::new(
                Opcode::Copy,
                register as i32,
                stop_key.saturating_add(equalities.len() as u32) as i32,
                0,
            ));
            let length = equalities.len().saturating_add(1);
            self.apply_index_affinity(body, level, columns, stop_key, length)?;
            let opcode = match (reverse, bound.kind) {
                (false, BoundKind::Less) => Opcode::IdxGe,
                (false, _) => Opcode::IdxGt,
                (true, BoundKind::Greater) => Opcode::IdxLe,
                (true, _) => Opcode::IdxLt,
            };
            done.push(
                self.emit_jump(
                    Instruction::new(opcode, index_cursor as i32, -1, stop_key as i32)
                        .with_p5(length as u16),
                ),
            );
        }
        let mut skip: Vec<Label> = Vec::new();
        // A range never matches a NULL: `k < 5` is unknown for a NULL k, not
        // true. Seeking past them covers the common case at no per-row cost,
        // but only in one of the four combinations of direction and key order,
        // so the guard is what actually makes it right - and a walk that
        // reaches a NULL from the far end has no seek that could have skipped
        // it. One read of an index column the entry already holds.
        if low.is_some() || high.is_some() {
            let value = self.register();
            self.emit(Instruction::new(
                Opcode::IdxColumn,
                index_cursor as i32,
                equalities.len() as i32,
                value as i32,
            ));
            skip.push(self.emit_jump(Instruction::new(Opcode::IfNull, value as i32, -1, 0)));
        }
        if covering {
            // The entry holds everything the query reads, so there is no row
            // to fetch - which is the whole of what makes a covering path
            // worth choosing. The table cursor was not even opened.
        } else if !without_rowid {
            let rowid = self.register();
            self.emit(Instruction::new(
                Opcode::IdxRowid,
                index_cursor as i32,
                rowid as i32,
                0,
            ));
            skip.push(self.emit_jump(Instruction::new(
                Opcode::SeekRowid,
                cursors.table as i32,
                -1,
                rowid as i32,
            )));
        } else if !key_slots.is_empty() {
            // The entry ends with the row's primary key; read it back out and
            // seek the table - which is itself an index - to that key.
            let block = self.register_block(key_slots.len());
            for (offset, slot) in key_slots.iter().enumerate() {
                self.emit(Instruction::new(
                    Opcode::IdxColumn,
                    index_cursor as i32,
                    *slot as i32,
                    block.saturating_add(offset as u32) as i32,
                ));
            }
            skip.push(
                self.emit_jump(
                    Instruction::new(Opcode::NoConflict, cursors.table as i32, -1, block as i32)
                        .with_p5(key_slots.len() as u16),
                ),
            );
        }
        skip.extend(self.compile_residual(body, level)?);
        self.compile_level(body, level.saturating_add(1), inner)?;
        for label in skip {
            self.patch_here(label);
        }
        let more = self.emit_jump(Instruction::new(
            if reverse { Opcode::Prev } else { Opcode::Next },
            index_cursor as i32,
            -1,
            0,
        ));
        self.patch(more, start);
        for label in done {
            self.patch_here(label);
        }
        self.patch_here(empty);
        for label in null_key {
            self.patch_here(label);
        }
        Ok(())
    }

    /// Opens a distinct set sized for one seek-key column, for a seek union
    /// that has to skip a branch whose key an earlier branch already used.
    /// @param collation - the collation the key column compares under
    fn open_seek_key_distinct(&mut self, collation: Collation) -> u32 {
        let set = self.distincts;
        self.distincts = self.distincts.saturating_add(1);
        let key = SortKey {
            columns: vec![SortColumn {
                descending: false,
                nulls_first: true,
                collation,
            }],
        };
        self.emit(
            Instruction::new(Opcode::DistinctOpen, set as i32, 0, 0).with_p4(Operand::SortKey(key)),
        );
        set
    }

    /// Compiles a union of rowid seeks: one branch per key, run one after
    /// another, each skipped whole when an earlier branch already used its
    /// key.
    ///
    /// Every key is exactly what a lone [`AccessPath::RowidSeek`] would seek
    /// to; a rowid is unique by construction, so a repeated key is the only
    /// way two branches could produce the same row, and skipping the second
    /// probe of a key already probed is what keeps that from happening. A
    /// union of one key opens no set at all - there is nothing for it to
    /// repeat against.
    /// @param keys - the keys to probe, in order
    fn compile_rowid_seek_union(
        &mut self,
        body: &Body<'_>,
        level: usize,
        cursors: SourceCursors,
        keys: &[BoundExpr],
        inner: &InnerBody,
    ) -> DbResult<()> {
        let set = (keys.len() > 1).then(|| self.open_seek_key_distinct(Collation::Binary));
        for key in keys {
            let register = self.compile_expr(key)?;
            let mut skip_branch: Vec<Label> = Vec::new();
            if let Some(set) = set {
                // A rowid is always compared as an integer, so the key is cast
                // before it is recorded - otherwise `IN ('5', 5)` would be
                // recorded as two different keys and probed twice.
                self.emit(
                    Instruction::new(Opcode::Cast, register as i32, register as i32, 0)
                        .with_p4(Operand::Affinity(Affinity::Integer)),
                );
                skip_branch.push(
                    self.emit_jump(
                        Instruction::new(Opcode::DistinctCheck, set as i32, -1, register as i32)
                            .with_p5(1),
                    ),
                );
            }
            let missing = self.emit_jump(Instruction::new(
                Opcode::SeekRowid,
                cursors.table as i32,
                -1,
                register as i32,
            ));
            let skip = self.compile_residual(body, level)?;
            self.compile_level(body, level.saturating_add(1), inner)?;
            for label in skip {
                self.patch_here(label);
            }
            self.patch_here(missing);
            for label in skip_branch {
                self.patch_here(label);
            }
        }
        Ok(())
    }

    /// Compiles a union of index seeks: one branch after another, each
    /// exactly what a lone [`AccessPath::IndexSeek`] would compile to.
    ///
    /// The branches share one index and one table, which is why every
    /// parameter past `branches` is the same for all of them - only the
    /// equality prefix and the range differ, and those live on each branch.
    /// @param dedup - whether a branch's key can repeat one an earlier branch
    ///   already used and so has to be checked before it is probed; `false`
    ///   for the keyset-range shape, which is proven disjoint at plan time
    #[allow(clippy::too_many_arguments)]
    fn compile_index_seek_union(
        &mut self,
        body: &Body<'_>,
        level: usize,
        cursors: SourceCursors,
        branches: &[IndexSeekBranch],
        collation: Collation,
        columns: &[u16],
        descending: &[bool],
        without_rowid: bool,
        key_slots: &[usize],
        covering: bool,
        reverse: bool,
        dedup: bool,
        inner: &InnerBody,
    ) -> DbResult<()> {
        let set = (dedup && branches.len() > 1).then(|| self.open_seek_key_distinct(collation));
        for branch in branches {
            self.compile_index_seek(
                body,
                level,
                cursors,
                &branch.equalities,
                branch.low.clone(),
                branch.high.clone(),
                columns,
                descending,
                without_rowid,
                key_slots,
                covering,
                reverse,
                set,
                inner,
            )?;
        }
        Ok(())
    }

    /// Applies the indexed columns' affinities to a seek key.
    ///
    /// A seek key has to be converted the way the index's own values were, or
    /// the comparison compares a text `'5'` against an integer 5 and finds
    /// nothing. This is the single most common way an index seek silently
    /// returns no rows.
    fn apply_index_affinity(
        &mut self,
        body: &Body<'_>,
        level: usize,
        columns: &[u16],
        key: u32,
        length: usize,
    ) -> DbResult<()> {
        let Some(source) = body.plan.sources.get(level) else {
            return Ok(());
        };
        for position in 0..length {
            let Some(column) = columns.get(position).copied() else {
                continue;
            };
            let Some(info) = source.table.column(column) else {
                continue;
            };
            self.emit(
                Instruction::new(
                    Opcode::ApplyAffinity,
                    key.saturating_add(position as u32) as i32,
                    1,
                    0,
                )
                .with_p4(Operand::Affinity(info.affinity)),
            );
        }
        Ok(())
    }

    /// Emits the residual predicate for one loop level.
    fn compile_residual(&mut self, body: &Body<'_>, level: usize) -> DbResult<Vec<Label>> {
        if self.deferred.get(level).copied().flatten().is_some() {
            return Ok(Vec::new());
        }
        let Some(Some(residual)) = body.plan.residuals.get(level) else {
            return Ok(Vec::new());
        };
        let residual = residual.clone();
        let register = self.compile_expr(&residual)?;
        Ok(vec![self.emit_jump(
            Instruction::new(Opcode::IfNot, register as i32, -1, 0).with_p5(1),
        )])
    }

    /// Compiles the loop that runs one virtual table's chosen plan.
    ///
    /// `VFilter` is a jump like `Rewind`: a module that produces nothing skips
    /// the body rather than running it once on an unpositioned cursor. What
    /// follows it is the body, then the predicates the module did not promise
    /// to apply, then `VNext` back to the top.
    fn compile_virtual_scan(
        &mut self,
        body: &Body<'_>,
        level: usize,
        cursors: SourceCursors,
        offer: &[inillucent_sql::plan::VirtualConstraint],
        chosen: Option<&inillucent_sql::plan::VirtualChoice>,
        inner: &InnerBody,
    ) -> DbResult<()> {
        let Some(chosen) = chosen else {
            return Err(error::misuse(
                "a virtual scan reached the compiler with no plan",
            ));
        };
        // The arguments are evaluated once, before the scan starts, into one
        // contiguous block: `filter` reads them positionally and a value
        // computed inside the loop would be a value that changed under it.
        let block = self.register_block(chosen.arguments.len().max(1));
        for (position, index) in chosen.arguments.iter().enumerate() {
            let Some(constraint) = offer.get(*index) else {
                continue;
            };
            let register = self.compile_expr(&constraint.value)?;
            self.emit(Instruction::new(
                Opcode::Copy,
                register as i32,
                block.saturating_add(position as u32) as i32,
                0,
            ));
        }
        let empty = self.emit_jump(
            Instruction::new(Opcode::VFilter, cursors.table as i32, -1, block as i32)
                .with_p5(chosen.arguments.len().min(usize::from(u16::MAX)) as u16)
                .with_p4(Operand::VirtualPlan(Box::new(
                    crate::program::VirtualPlan {
                        index_number: chosen.index_number,
                        index_string: chosen.index_string.clone(),
                    },
                ))),
        );
        let start = self.here();
        // Everything the module did not promise to apply is tested here, in
        // the order it was offered, before the residual the planner left.
        let mut skip = Vec::new();
        for index in &chosen.recheck {
            let Some(constraint) = offer.get(*index) else {
                continue;
            };
            let register = self.compile_expr(&constraint.predicate)?;
            skip.push(
                self.emit_jump(Instruction::new(Opcode::IfNot, register as i32, -1, 0).with_p5(1)),
            );
        }
        skip.extend(self.compile_residual(body, level)?);
        self.compile_level(body, level.saturating_add(1), inner)?;
        for label in skip {
            self.patch_here(label);
        }
        let more = self.emit_jump(Instruction::new(Opcode::VNext, cursors.table as i32, -1, 0));
        self.patch(more, start);
        self.patch_here(empty);
        Ok(())
    }

    /// Materialises a nested block into its store, then scans the store.
    ///
    /// An uncorrelated block is built once, guarded by a flag register; a
    /// correlated one is cleared and rebuilt every time the loop that encloses
    /// it produces a row, because the outer cursors it reads have moved.
    #[allow(clippy::too_many_arguments)]
    fn compile_subquery_level(
        &mut self,
        body: &Body<'_>,
        level: usize,
        cursors: SourceCursors,
        plan: &PhysicalPlan,
        width: usize,
        correlated: bool,
        inner: &InnerBody,
    ) -> DbResult<()> {
        let store = cursors.table;
        let mut built: Option<Label> = None;
        if correlated {
            self.emit(Instruction::new(Opcode::EphClear, store as i32, 0, 0));
        } else if let Some(once) = cursors.built {
            // The flag was zeroed before the loops, so a block with no
            // correlation is built exactly once however many times the loop
            // that encloses it runs.
            built = Some(self.emit_jump(Instruction::new(Opcode::If, once as i32, -1, 0)));
            self.emit(
                Instruction::new(Opcode::Load, 0, once as i32, 0).with_p4(Operand::Integer(1)),
            );
        }
        self.compile_block(plan, Sink::Store(store))?;
        if let Some(label) = built {
            self.patch_here(label);
        }
        let empty = self.emit_jump(Instruction::new(Opcode::EphRewind, store as i32, -1, 0));
        let start = self.here();
        let skip = self.compile_residual(body, level)?;
        self.compile_level(body, level.saturating_add(1), inner)?;
        for label in skip {
            self.patch_here(label);
        }
        let more = self.emit_jump(Instruction::new(Opcode::EphNext, store as i32, -1, 0));
        self.patch(more, start);
        self.patch_here(empty);
        let _ = width;
        Ok(())
    }

    /// Fills a recursive CTE's store, then scans it.
    ///
    /// The queue is the store itself. The seed arms append to it, and the fill
    /// loop walks it forward with the step arms appending behind the cursor -
    /// so a row produced by a step is visited in its turn without a second
    /// structure, and the walk ends exactly when nothing new was appended.
    fn compile_recursive_level(
        &mut self,
        body: &Body<'_>,
        level: usize,
        cursors: SourceCursors,
        seeds: &[(CompoundOp, PhysicalPlan)],
        steps: &[(CompoundOp, PhysicalPlan)],
        inner: &InnerBody,
    ) -> DbResult<()> {
        let store = cursors.table;
        self.emit(Instruction::new(Opcode::EphClear, store as i32, 0, 0));
        for (op, arm) in seeds {
            let sink = sink_for(*op, store);
            self.compile_block(arm, sink)?;
        }
        if !steps.is_empty() {
            let idle = self.emit_jump(Instruction::new(Opcode::EphRewind, store as i32, -1, 0));
            let again = self.here();
            for (op, arm) in steps {
                let sink = sink_for(*op, store);
                self.compile_block(arm, sink)?;
            }
            let more = self.emit_jump(Instruction::new(Opcode::EphNext, store as i32, -1, 0));
            self.patch(more, again);
            self.patch_here(idle);
        }
        let empty = self.emit_jump(Instruction::new(Opcode::EphRewind, store as i32, -1, 0));
        let start = self.here();
        let skip = self.compile_residual(body, level)?;
        self.compile_level(body, level.saturating_add(1), inner)?;
        for label in skip {
            self.patch_here(label);
        }
        let more = self.emit_jump(Instruction::new(Opcode::EphNext, store as i32, -1, 0));
        self.patch(more, start);
        self.patch_here(empty);
        Ok(())
    }

    /// Emits whatever the innermost loop body does.
    fn compile_inner(&mut self, body: &Body<'_>, inner: &InnerBody) -> DbResult<()> {
        match inner {
            InnerBody::JoinMatch {
                matched,
                ret,
                continuation,
                on,
                matched_set,
                source,
            } => {
                // The `ON` condition is tested here rather than as a residual:
                // a row that fails it is not a match, and the loop has to carry
                // on looking rather than the statement dropping the row.
                let mut refused = Vec::new();
                if let Some(on) = on {
                    let register = self.compile_expr(&on.clone())?;
                    refused.push(self.emit_jump(
                        Instruction::new(Opcode::IfNot, register as i32, -1, 0).with_p5(1),
                    ));
                }
                self.emit(
                    Instruction::new(Opcode::Load, 0, *matched as i32, 0)
                        .with_p4(Operand::Integer(1)),
                );
                if let Some(store) = matched_set {
                    let cursors = self.cursors_of(*source)?;
                    let key = self.register();
                    self.emit(Instruction::new(
                        Opcode::Rowid,
                        cursors.table as i32,
                        key as i32,
                        0,
                    ));
                    let duplicate = self.emit_jump(
                        Instruction::new(Opcode::EphInsertUnique, *store as i32, -1, key as i32)
                            .with_p5(1),
                    );
                    self.patch_here(duplicate);
                }
                self.emit(Instruction::new(
                    Opcode::Gosub,
                    *ret as i32,
                    *continuation,
                    0,
                ));
                for label in refused {
                    self.patch_here(label);
                }
                Ok(())
            }
            InnerBody::InlineRow { block, width } => {
                self.build_result_row(body, *block)?;
                self.compile_tail(body, *block, *width, false, 0)
            }
            InnerBody::AggregateStep => self.step_aggregates(body),
            InnerBody::GroupStream {
                previous,
                current,
                group_count,
                collations,
                group_emit,
                group_return,
                started,
            } => {
                let group_by = body.plan.select.group_by.clone();
                for (index, expr) in group_by.iter().enumerate() {
                    let register = self.compile_expr(expr)?;
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        register as i32,
                        current.saturating_add(index as u32) as i32,
                        0,
                    ));
                }
                // The first row has no previous group to close, so it only
                // adopts the key. Every later row either continues the group it
                // is in or ends it.
                let started_already =
                    self.emit_jump(Instruction::new(Opcode::IfPos, *started as i32, -1, 0));
                for index in 0..*group_count {
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        current.saturating_add(index as u32) as i32,
                        previous.saturating_add(index as u32) as i32,
                        0,
                    ));
                }
                self.emit(
                    Instruction::new(Opcode::Load, 0, *started as i32, 0)
                        .with_p4(Operand::Integer(1)),
                );
                let to_step = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
                self.patch_here(started_already);
                let same =
                    self.compile_group_key_equal(*group_count, *previous, *current, collations);
                self.emit(Instruction::new(
                    Opcode::Gosub,
                    *group_return as i32,
                    *group_emit,
                    0,
                ));
                self.reset_accumulators(&body.plan.select.aggregates);
                for index in 0..*group_count {
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        current.saturating_add(index as u32) as i32,
                        previous.saturating_add(index as u32) as i32,
                        0,
                    ));
                }
                for label in same {
                    self.patch_here(label);
                }
                self.patch_here(to_step);
                self.step_aggregates(body)?;
                Ok(())
            }
            InnerBody::GroupInsert {
                sorter,
                block,
                group_count,
                payload,
            } => {
                let group_by = body.plan.select.group_by.clone();
                for (index, expr) in group_by.iter().enumerate() {
                    let register = self.compile_expr(expr)?;
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        register as i32,
                        block.saturating_add(index as u32) as i32,
                        0,
                    ));
                }
                for (index, expr) in payload.iter().enumerate() {
                    let register = self.compile_expr(expr)?;
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        register as i32,
                        block
                            .saturating_add(*group_count as u32)
                            .saturating_add(index as u32) as i32,
                        0,
                    ));
                }
                let width = group_count.saturating_add(payload.len());
                self.emit(Instruction::new(
                    Opcode::SorterInsert,
                    *sorter as i32,
                    *block as i32,
                    width as i32,
                ));
                Ok(())
            }
        }
    }

    /// Builds the result columns into a contiguous block.
    fn build_result_row(&mut self, body: &Body<'_>, block: u32) -> DbResult<()> {
        let columns = body.plan.select.columns.clone();
        for (index, column) in columns.iter().enumerate() {
            let register = self.compile_expr(&column.expr)?;
            self.emit(Instruction::new(
                Opcode::Copy,
                register as i32,
                block.saturating_add(index as u32) as i32,
                0,
            ));
        }
        Ok(())
    }
}

/// Returns the statement-wide id of one level of a block.
fn source_id(body: &Body<'_>, level: usize) -> DbResult<usize> {
    body.plan
        .sources
        .get(level)
        .map(|source| source.id)
        .ok_or_else(|| error::misuse("plan level out of range"))
}

/// What the innermost loop body does with a row.
enum InnerBody {
    /// Record that this outer-join level matched, and run the continuation.
    JoinMatch {
        /// The register holding whether any row matched.
        matched: u32,
        /// The register holding the continuation's return address.
        ret: u32,
        /// The continuation's address.
        continuation: i32,
        /// The join's `ON` condition, which decides what counts as a match.
        on: Option<BoundExpr>,
        /// The store recording which rows of this term matched, for a `RIGHT`
        /// or `FULL` join's second pass.
        matched_set: Option<u32>,
        /// This term's statement-wide number.
        source: usize,
    },
    /// Build a result row and run the tail where it stands, without a call.
    ///
    /// A plain scan reaches its tail from exactly one place in the program - an
    /// outer join reaches it through a continuation, which is itself emitted
    /// once - so the call was a `Gosub` and a `Return` around a body with one
    /// caller. That is two of the five instructions a one-column scan ran per
    /// row, against the three the pinned SQLite runs: `Column`, `ResultRow`,
    /// `Next`. Emitting the tail where it is used removes both.
    InlineRow {
        /// The register block the row is built in.
        block: u32,
        /// How many columns the row has.
        width: usize,
    },
    /// Step every aggregate.
    AggregateStep,
    /// Close the previous group when the key changes, then step the aggregates
    /// from this row.
    ///
    /// What the sorter's drain loop does, done as the rows arrive - which is
    /// only correct when the walk already brings each group's rows together.
    GroupStream {
        /// The registers holding the key of the group being accumulated.
        previous: u32,
        /// The registers this row's key is read into.
        current: u32,
        /// How many columns the key has.
        group_count: usize,
        /// The collation each key column is compared with.
        collations: Vec<Collation>,
        /// The address of the subroutine that emits a finished group.
        group_emit: i32,
        /// The register holding that subroutine's return address.
        group_return: u32,
        /// The register that is zero until the first row has been absorbed.
        started: u32,
    },
    /// Write the group key and aggregate arguments into a sorter.
    GroupInsert {
        /// The sorter.
        sorter: u32,
        /// The register block the record is built in.
        block: u32,
        /// How many leading columns are the group key.
        group_count: usize,
        /// The expressions stored after the key.
        payload: Vec<BoundExpr>,
    },
}

/// Returns the substitutions a grouped query's result columns compile with:
/// every group-key expression reads the register the sorter left its key in.
fn group_substitutions(group_by: &[BoundExpr], previous: u32) -> Vec<(BoundExpr, u32)> {
    group_by
        .iter()
        .enumerate()
        .map(|(index, expr)| (expr.clone(), previous.saturating_add(index as u32)))
        .collect()
}

/// Collects every nested query used as a value anywhere in a plan.
///
/// It walks the whole block - the filter, the residuals, the result columns,
/// the group and order keys, the aggregate arguments and the join conditions -
/// because a subquery may appear in any of them and one that is missed is a
/// store the compiler never opens.
fn collect_subqueries_of_plan(plan: &PhysicalPlan, into: &mut Vec<BoundExpr>) {
    if let Some(filter) = &plan.constant_filter {
        collect_subqueries(filter, into);
    }
    for residual in plan.residuals.iter().flatten() {
        collect_subqueries(residual, into);
    }
    for source in &plan.sources {
        if let Some(on) = &source.on {
            collect_subqueries(on, into);
        }
    }
    let select = &plan.select;
    if let Some(filter) = &select.filter {
        collect_subqueries(filter, into);
    }
    if let Some(having) = &select.having {
        collect_subqueries(having, into);
    }
    for expr in &select.group_by {
        collect_subqueries(expr, into);
    }
    for column in &select.columns {
        collect_subqueries(&column.expr, into);
    }
    for term in &select.order_by {
        collect_subqueries(&term.expr, into);
    }
    for aggregate in &select.aggregates {
        for argument in &aggregate.arguments {
            collect_subqueries(argument, into);
        }
    }
    for row in &select.values {
        for value in row {
            collect_subqueries(value, into);
        }
    }
    if let Some(expr) = &select.limit {
        collect_subqueries(expr, into);
    }
    if let Some(expr) = &select.offset {
        collect_subqueries(expr, into);
    }
    for (_, arm) in &plan.compounds {
        collect_subqueries_of_plan(arm, into);
    }
}

/// Collects every nested query used as a value inside one expression.
fn collect_subqueries(expr: &BoundExpr, into: &mut Vec<BoundExpr>) {
    match expr {
        BoundExpr::Subquery { operand, .. } => {
            if let Some(operand) = operand {
                collect_subqueries(operand, into);
            }
            into.push(expr.clone());
        }
        BoundExpr::Unary { operand, .. }
        | BoundExpr::Not(operand)
        | BoundExpr::IsNull { operand, .. }
        | BoundExpr::Collate { operand, .. }
        | BoundExpr::Cast { operand, .. } => collect_subqueries(operand, into),
        BoundExpr::Arithmetic { left, right, .. }
        | BoundExpr::Compare { left, right, .. }
        | BoundExpr::Is { left, right, .. }
        | BoundExpr::And(left, right)
        | BoundExpr::Or(left, right) => {
            collect_subqueries(left, into);
            collect_subqueries(right, into);
        }
        BoundExpr::Between {
            operand, low, high, ..
        } => {
            collect_subqueries(operand, into);
            collect_subqueries(low, into);
            collect_subqueries(high, into);
        }
        BoundExpr::InList { operand, list, .. } => {
            collect_subqueries(operand, into);
            for item in list {
                collect_subqueries(item, into);
            }
        }
        BoundExpr::Case {
            operand,
            branches,
            otherwise,
            ..
        } => {
            if let Some(operand) = operand {
                collect_subqueries(operand, into);
            }
            for (when, then) in branches {
                collect_subqueries(when, into);
                collect_subqueries(then, into);
            }
            if let Some(otherwise) = otherwise {
                collect_subqueries(otherwise, into);
            }
        }
        BoundExpr::Pattern {
            operand,
            pattern,
            escape,
            ..
        } => {
            collect_subqueries(operand, into);
            collect_subqueries(pattern, into);
            if let Some(escape) = escape {
                collect_subqueries(escape, into);
            }
        }
        BoundExpr::Function { arguments, .. }
        | BoundExpr::Math { arguments, .. }
        | BoundExpr::Json { arguments, .. }
        | BoundExpr::Time { arguments, .. } => {
            for argument in arguments {
                collect_subqueries(argument, into);
            }
        }
        _ => {}
    }
}

/// The record one block's window passes collect, and where everything lives.
///
/// Every value a pass or the output needs gets one column, de-duplicated by
/// expression: two calls over the same `PARTITION BY` share its columns, and a
/// result column that also appears in an `ORDER BY` is stored once. The
/// de-duplication is not only a saving - the drain substitutes *by expression*,
/// so two columns holding the same expression would make which one is read
/// arbitrary.
///
/// Calls are grouped into passes by the window they are computed over, because
/// two windows in one `SELECT` may partition differently and each partitioning
/// needs its own sort. Each pass appends its own calls' values to every row, so
/// a call's slot number and the column its value ends up in are not the same
/// number and the map between them is kept here.
struct WindowLayout {
    /// The expressions the record holds, in column order.
    record: Vec<BoundExpr>,
    /// The passes, in the order they run.
    passes: Vec<WindowPass>,
    /// For each window slot, the record column its value is appended at.
    slots: Vec<usize>,
}

/// One sort-and-compute pass over the collected records.
struct WindowPass {
    /// The columns the pass sorts by, in order: partition keys then ordering.
    sort: Vec<(usize, SortColumn)>,
    /// The pass the machine runs.
    plan: WindowPlan,
}

impl WindowLayout {
    /// Works out the record and the passes for one block's windows.
    fn of(plan: &PhysicalPlan) -> WindowLayout {
        let mut layout = WindowLayout {
            record: Vec::new(),
            passes: Vec::new(),
            slots: vec![0; plan.select.windows.len()],
        };
        // Whatever the output needs is interned first, so that the record's
        // leading columns are stable whatever windows the query happens to
        // carry - which keeps the drain's substitutions independent of the
        // window layout.
        for column in &plan.select.columns {
            layout.intern_bases(&column.expr);
        }
        for term in &plan.select.order_by {
            layout.intern_bases(&term.expr);
        }

        // One pass per distinct window: the same `PARTITION BY` and the same
        // `ORDER BY` can share a sort, and anything else cannot.
        let mut groups: Vec<(Vec<usize>, Vec<(usize, SortColumn)>, Vec<usize>)> = Vec::new();
        for (slot, window) in plan.select.windows.iter().enumerate() {
            let partition: Vec<usize> = window
                .partition_by
                .iter()
                .map(|expr| layout.intern(expr))
                .collect();
            let order: Vec<(usize, SortColumn)> = window
                .order_by
                .iter()
                .map(|term| (layout.intern(&term.expr), sort_column(term)))
                .collect();
            match groups
                .iter_mut()
                .find(|(existing, sort, _)| *existing == partition && *sort == order)
            {
                Some((_, _, members)) => members.push(slot),
                None => groups.push((partition, order, vec![slot])),
            }
        }

        // Which column each window value lands in cannot be known until every
        // group has been walked: interning a call's arguments, its `FILTER` and
        // its frame offsets all grow the record, and the appended values come
        // after all of it. Numbering them as the loop went made the first
        // window value read whichever expression happened to be interned next -
        // a frame offset, most often, so `sum(x) OVER (ROWS 2 PRECEDING)`
        // summed the number 2.
        let mut order_of_append: Vec<usize> = Vec::new();
        for (partition, order, members) in groups {
            let mut calls = Vec::new();
            for slot in &members {
                let Some(window) = plan.select.windows.get(*slot) else {
                    continue;
                };
                let arguments = window
                    .arguments
                    .iter()
                    .map(|expr| layout.intern(expr))
                    .collect();
                let filter = window.filter.as_ref().map(|expr| layout.intern(expr));
                let start = layout.frame_end(&window.start);
                let end = layout.frame_end(&window.end);
                calls.push(WindowCall {
                    func: match window.call {
                        BoundWindowCall::Aggregate(func) => WindowSlot::Aggregate(func),
                        BoundWindowCall::Plain(func) => WindowSlot::Plain(func),
                    },
                    distinct: window.distinct,
                    collation: window.collation,
                    arguments,
                    filter,
                    order: order.clone(),
                    frame: WindowFrame {
                        unit: window.unit,
                        start,
                        end,
                        exclude: window.exclude,
                    },
                });
                order_of_append.push(*slot);
            }
            let mut sort: Vec<(usize, SortColumn)> = partition
                .iter()
                .map(|column| {
                    (
                        *column,
                        SortColumn {
                            descending: false,
                            nulls_first: true,
                            collation: layout.collation_of(*column),
                        },
                    )
                })
                .collect();
            sort.extend(order.iter().copied());
            let partition_key = partition
                .iter()
                .map(|column| (*column, layout.collation_of(*column)))
                .collect();
            layout.passes.push(WindowPass {
                sort,
                plan: WindowPlan {
                    partition: partition_key,
                    calls,
                },
            });
        }
        let width = layout.record.len();
        for (position, slot) in order_of_append.iter().enumerate() {
            if let Some(destination) = layout.slots.get_mut(*slot) {
                *destination = width.saturating_add(position);
            }
        }
        layout
    }

    /// Returns the collation the expression in one record column carries.
    fn collation_of(&self, column: usize) -> Collation {
        self.record
            .get(column)
            .map_or(Collation::Binary, inillucent_sql::bind::result_collation)
    }

    /// Returns the record column an expression lives in, adding it if new.
    fn intern(&mut self, expr: &BoundExpr) -> usize {
        if let Some(index) = self.record.iter().position(|existing| existing == expr) {
            return index;
        }
        self.record.push(expr.clone());
        self.record.len().saturating_sub(1)
    }

    /// Stores the maximal subexpressions of one output expression that hold no
    /// window value.
    ///
    /// Maximal rather than leaf: `a + b` is one column, not two, so the sum is
    /// computed once while the rows are collected instead of the operands being
    /// carried through the pass and added again on the way out. `a + count(*)
    /// OVER ()` holds a window value, so it recurses.
    fn intern_bases(&mut self, expr: &BoundExpr) {
        if !holds_window(expr) {
            let literal = matches!(
                expr,
                BoundExpr::Null
                    | BoundExpr::Integer(_)
                    | BoundExpr::Real(_)
                    | BoundExpr::Text(_)
                    | BoundExpr::Blob(_)
                    | BoundExpr::Parameter(_)
            );
            if !literal {
                self.intern(expr);
            }
            return;
        }
        for child in children_of(expr) {
            self.intern_bases(&child);
        }
    }

    /// Returns the machine's form of one end of a frame.
    fn frame_end(&mut self, bound: &BoundFrameBound) -> FrameEnd {
        match bound {
            BoundFrameBound::UnboundedPreceding => FrameEnd::UnboundedPreceding,
            BoundFrameBound::UnboundedFollowing => FrameEnd::UnboundedFollowing,
            BoundFrameBound::CurrentRow => FrameEnd::CurrentRow,
            BoundFrameBound::Preceding(expr) => FrameEnd::Offset {
                column: self.intern(expr),
                preceding: true,
            },
            BoundFrameBound::Following(expr) => FrameEnd::Offset {
                column: self.intern(expr),
                preceding: false,
            },
        }
    }
}

/// Returns a stored `DEFAULT` as a constant operand, when it is one.
///
/// Only a constant can be read back for a row that predates the column, and
/// only a constant is allowed on `ADD COLUMN` - so anything else leaves the
/// column reading NULL, which is what SQLite does for a default that was legal
/// when the table was created and is not constant.
fn constant_operand(sql: &[u8]) -> Option<Operand> {
    let limits = inillucent_base::limits::Limits::default();
    let (ast, expr) = inillucent_sql::parser::parse_expression(sql, &limits).ok()?;
    let (node, negate) = match ast.expr(expr)? {
        inillucent_sql::ast::Expr::Unary {
            op: inillucent_sql::ast::UnaryOp::Negate,
            operand,
        } => (ast.expr(*operand)?, true),
        other => (other, false),
    };
    let inillucent_sql::ast::Expr::Literal(literal) = node else {
        return None;
    };
    let operand = match literal {
        inillucent_sql::ast::Literal::Null => return None,
        inillucent_sql::ast::Literal::Boolean(value) => Operand::Integer(i64::from(*value)),
        inillucent_sql::ast::Literal::Integer(text) => {
            let value = core::str::from_utf8(text).ok()?.parse::<i64>().ok()?;
            Operand::Integer(if negate { value.checked_neg()? } else { value })
        }
        inillucent_sql::ast::Literal::Float(text) => {
            let value = core::str::from_utf8(text).ok()?.parse::<f64>().ok()?;
            Operand::Real(if negate { -value } else { value })
        }
        inillucent_sql::ast::Literal::String(text) if !negate => Operand::Text(text.clone()),
        inillucent_sql::ast::Literal::Blob(bytes) if !negate => Operand::Blob(bytes.clone()),
        _ => return None,
    };
    Some(operand)
}

/// Returns the key description of a table's primary key.
///
/// Only meaningful for a `WITHOUT ROWID` table, whose root is an index b-tree
/// ordered by exactly these columns with exactly these collations. Getting the
/// collations wrong here would order the table differently from the file.
pub(crate) fn primary_key_of(table: &TableInfo) -> IndexKey {
    IndexKey {
        columns: table
            .primary_key()
            .into_iter()
            .map(|position| {
                let collation = table
                    .column(position)
                    .map(|column| {
                        Collation::from_name(
                            core::str::from_utf8(&column.collation).unwrap_or("BINARY"),
                        )
                        .unwrap_or(Collation::Binary)
                    })
                    .unwrap_or(Collation::Binary);
                SortColumn {
                    descending: false,
                    nulls_first: true,
                    collation,
                }
            })
            .collect(),
    }
}

/// Returns whether an expression reads a window value anywhere inside it.
fn holds_window(expr: &BoundExpr) -> bool {
    if matches!(expr, BoundExpr::WindowRef { .. }) {
        return true;
    }
    children_of(expr).iter().any(holds_window)
}

/// Returns an expression's direct children.
fn children_of(expr: &BoundExpr) -> Vec<BoundExpr> {
    let mut out = Vec::new();
    match expr {
        BoundExpr::Unary { operand, .. }
        | BoundExpr::Not(operand)
        | BoundExpr::IsNull { operand, .. }
        | BoundExpr::Collate { operand, .. }
        | BoundExpr::Cast { operand, .. } => out.push((**operand).clone()),
        BoundExpr::Arithmetic { left, right, .. }
        | BoundExpr::Compare { left, right, .. }
        | BoundExpr::Is { left, right, .. }
        | BoundExpr::And(left, right)
        | BoundExpr::Or(left, right) => {
            out.push((**left).clone());
            out.push((**right).clone());
        }
        BoundExpr::Between {
            operand, low, high, ..
        } => {
            out.push((**operand).clone());
            out.push((**low).clone());
            out.push((**high).clone());
        }
        BoundExpr::InList { operand, list, .. } => {
            out.push((**operand).clone());
            out.extend(list.iter().cloned());
        }
        BoundExpr::Case {
            operand,
            branches,
            otherwise,
            ..
        } => {
            if let Some(operand) = operand {
                out.push((**operand).clone());
            }
            for (when, then) in branches {
                out.push(when.clone());
                out.push(then.clone());
            }
            if let Some(otherwise) = otherwise {
                out.push((**otherwise).clone());
            }
        }
        BoundExpr::Pattern {
            operand,
            pattern,
            escape,
            ..
        } => {
            out.push((**operand).clone());
            out.push((**pattern).clone());
            if let Some(escape) = escape {
                out.push((**escape).clone());
            }
        }
        BoundExpr::Function { arguments, .. }
        | BoundExpr::Math { arguments, .. }
        | BoundExpr::Json { arguments, .. }
        | BoundExpr::Time { arguments, .. } => out.extend(arguments.iter().cloned()),
        BoundExpr::Subquery {
            operand: Some(operand),
            ..
        } => out.push((**operand).clone()),
        _ => {}
    }
    out
}

/// Returns, for each level, the outer join whose continuation owes its residual.
///
/// A `LEFT` join defers its own term's `WHERE`; a `RIGHT` join defers its own
/// and every term to its left, because all of them can be null-extended by it.
/// Later joins win, because a term inside two outer joins is filtered after
/// both of them.
fn deferral_map(plan: &PhysicalPlan) -> Vec<Option<usize>> {
    let mut deferred = vec![None; plan.sources.len()];
    for (level, source) in plan.sources.iter().enumerate() {
        if !is_outer(source.join) {
            continue;
        }
        if matches!(source.join, JoinKind::Left | JoinKind::Full) {
            if let Some(slot) = deferred.get_mut(level) {
                *slot = Some(level);
            }
        }
        if matches!(source.join, JoinKind::Right | JoinKind::Full) {
            for earlier in 0..=level {
                if let Some(slot) = deferred.get_mut(earlier) {
                    *slot = Some(level);
                }
            }
        }
    }
    deferred
}

/// Returns the sink one compound operator writes an arm's rows through.
fn sink_for(op: CompoundOp, store: u32) -> Sink {
    match op {
        CompoundOp::UnionAll => Sink::Store(store),
        _ => Sink::UniqueStore(store),
    }
}

/// Returns a binary-collated key over a given number of columns.
///
/// A recursive CTE's queue de-duplicates on the whole row, and it has no
/// declared collations to consult: the columns came from a compound whose arms
/// need not agree about one. BINARY is the collation SQLite falls back to for
/// exactly the same reason.
fn store_key(width: usize) -> SortKey {
    SortKey {
        columns: (0..width)
            .map(|_| SortColumn {
                descending: false,
                nulls_first: true,
                collation: Collation::Binary,
            })
            .collect(),
    }
}

/// Returns the key a compound's set operations compare rows with.
///
/// Every column's collation comes from the left-most arm, which is SQLite's
/// rule: the arms may disagree about a column's declared collation, and the
/// operator has to pick one before it can say whether two rows are the same.
fn compound_key(plan: &PhysicalPlan) -> SortKey {
    SortKey {
        columns: plan
            .select
            .columns
            .iter()
            .map(|column| SortColumn {
                descending: false,
                nulls_first: true,
                collation: inillucent_sql::bind::result_collation(&column.expr),
            })
            .collect(),
    }
}

/// Returns the sorter column description one ORDER BY term needs.
fn sort_column(term: &BoundOrderTerm) -> SortColumn {
    SortColumn {
        descending: term.order == SortOrder::Descending,
        nulls_first: term.nulls == NullOrder::First,
        collation: term.collation,
    }
}

impl Compiler {
    /// Fills a value subquery's store, and answers the question it stands for.
    ///
    /// A block with no correlation is built once, behind a flag: it cannot
    /// change while the statement runs, and rebuilding it per row of the
    /// enclosing query is the difference between one scan and one scan per row.
    /// A correlated block is cleared and rebuilt, because the outer cursors it
    /// reads have moved.
    fn compile_value_subquery(
        &mut self,
        id: usize,
        kind: SubqueryKind,
        negated: bool,
        operand: Option<&BoundExpr>,
    ) -> DbResult<u32> {
        let Some(prepared) = self.subquery_plans.get(&id).cloned() else {
            return Err(error::misuse("a subquery with no prepared store"));
        };
        let store = prepared.store;
        // The `IN` operand is evaluated before the store is filled, because a
        // correlated block clears the store and the operand may not read it.
        let probe = match operand {
            Some(operand) => {
                let register = self.compile_expr(operand)?;
                let slot = self.register();
                self.emit(Instruction::new(
                    Opcode::Copy,
                    register as i32,
                    slot as i32,
                    0,
                ));
                if let Some(affinity) = prepared.affinity {
                    self.emit(
                        Instruction::new(Opcode::ApplyAffinity, slot as i32, 1, 0)
                            .with_p4(Operand::Affinity(affinity)),
                    );
                }
                Some(slot)
            }
            None => None,
        };
        let mut skip_fill = None;
        if prepared.correlated {
            self.emit(Instruction::new(Opcode::EphClear, store as i32, 0, 0));
        } else if let Some(flag) = prepared.built {
            skip_fill = Some(self.emit_jump(Instruction::new(Opcode::If, flag as i32, -1, 0)));
            self.emit(
                Instruction::new(Opcode::Load, 0, flag as i32, 0).with_p4(Operand::Integer(1)),
            );
        }
        let sink = match kind {
            SubqueryKind::In => Sink::TypedStore(store, prepared.affinity),
            _ => Sink::Store(store),
        };
        self.compile_block(&prepared.plan, sink)?;
        if let Some(label) = skip_fill {
            self.patch_here(label);
        }
        let answer = self.register();
        match kind {
            SubqueryKind::Exists => {
                let empty =
                    self.emit_jump(Instruction::new(Opcode::EphRewind, store as i32, -1, 0));
                self.emit(
                    Instruction::new(Opcode::Load, 0, answer as i32, 0)
                        .with_p4(Operand::Integer(i64::from(!negated))),
                );
                let done = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
                self.patch_here(empty);
                self.emit(
                    Instruction::new(Opcode::Load, 0, answer as i32, 0)
                        .with_p4(Operand::Integer(i64::from(negated))),
                );
                self.patch_here(done);
            }
            SubqueryKind::Scalar => {
                let empty =
                    self.emit_jump(Instruction::new(Opcode::EphRewind, store as i32, -1, 0));
                self.emit(Instruction::new(
                    Opcode::EphColumn,
                    store as i32,
                    0,
                    answer as i32,
                ));
                let done = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
                self.patch_here(empty);
                self.emit(Instruction::new(Opcode::Null, 0, answer as i32, 0));
                self.patch_here(done);
            }
            SubqueryKind::In => {
                let Some(probe) = probe else {
                    return Err(error::misuse("an IN subquery with no operand"));
                };
                self.compile_in_answer(store, probe, answer, negated);
            }
        }
        Ok(answer)
    }

    /// Emits the three-valued answer an `IN` over a materialised set gives.
    ///
    /// The order of the tests is the whole of the semantics. A hit is true
    /// whatever else the set holds. A miss is *unknown* rather than false when
    /// either the operand or the set holds a NULL, and false only when neither
    /// does - which is why `x NOT IN (empty set)` is true even for a NULL `x`.
    fn compile_in_answer(&mut self, store: u32, probe: u32, answer: u32, negated: bool) {
        let yes = i64::from(!negated);
        let no = i64::from(negated);
        // An empty set is decided before anything else: `NULL NOT IN ()` is
        // true, even though `NULL` compares with nothing. Testing nullness
        // first would answer unknown, which is the classic way to get this
        // wrong in both directions at once.
        let empty = self.emit_jump(Instruction::new(Opcode::EphRewind, store as i32, -1, 0));
        // Not empty. A NULL operand matches nothing and excludes nothing, so
        // the answer is unknown whatever the set holds.
        let null_operand = self.emit_jump(Instruction::new(Opcode::IfNull, probe as i32, -1, 0));
        let hit = self.emit_jump(
            Instruction::new(Opcode::EphFound, store as i32, -1, probe as i32).with_p5(1),
        );
        // A miss is false only when the set holds no NULL; with one, the row
        // it stands for might have been the match, so the answer is unknown.
        let saw_null = self.register();
        self.emit(Instruction::new(
            Opcode::EphSawNull,
            store as i32,
            saw_null as i32,
            0,
        ));
        let unknown = self.emit_jump(Instruction::new(Opcode::If, saw_null as i32, -1, 0));
        self.emit(
            Instruction::new(Opcode::Load, 0, answer as i32, 0).with_p4(Operand::Integer(no)),
        );
        let done_miss = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));

        self.patch_here(empty);
        self.emit(
            Instruction::new(Opcode::Load, 0, answer as i32, 0).with_p4(Operand::Integer(no)),
        );
        let done_empty = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));

        self.patch_here(hit);
        self.emit(
            Instruction::new(Opcode::Load, 0, answer as i32, 0).with_p4(Operand::Integer(yes)),
        );
        let done_hit = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));

        self.patch_here(null_operand);
        self.patch_here(unknown);
        self.emit(Instruction::new(Opcode::Null, 0, answer as i32, 0));

        self.patch_here(done_miss);
        self.patch_here(done_empty);
        self.patch_here(done_hit);
    }

    /// Emits a `RAISE(...)`, which never returns a value.
    ///
    /// Three of the four actions stop the statement, so the register handed back
    /// is never read; `IGNORE` jumps instead, to a label the enclosing write
    /// patches once it knows where the row ends. Returning a register anyway is
    /// what lets RAISE sit in a result column like any other expression.
    fn emit_raise(
        &mut self,
        action: RaiseAction,
        message: Option<&[u8]>,
        foreign_key: bool,
    ) -> DbResult<u32> {
        let register = self.register();
        self.emit(Instruction::new(Opcode::Null, 0, register as i32, 0));
        match action {
            RaiseAction::Ignore => {
                let label = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
                self.ignore_jumps.push(label);
            }
            RaiseAction::Rollback | RaiseAction::Abort | RaiseAction::Fail => {
                let conflict = match action {
                    RaiseAction::Rollback => ConflictAction::Rollback,
                    RaiseAction::Fail => ConflictAction::Fail,
                    _ => ConflictAction::Abort,
                };
                let text = message.unwrap_or_default().to_vec();
                // Which constraint asked is the only difference between the
                // two: a foreign key reports its own code, and a written
                // RAISE reports the trigger one.
                let code = if foreign_key {
                    crate::compile_dml::codes::FOREIGN_KEY
                } else {
                    crate::compile_dml::codes::TRIGGER
                };
                self.emit(
                    Instruction::new(
                        Opcode::HaltError,
                        code,
                        0,
                        crate::compile_dml::conflict_code(conflict),
                    )
                    .with_p4(Operand::Text(text)),
                );
            }
        }
        Ok(register)
    }

    /// Compiles one expression into a register.
    pub fn compile_expr(&mut self, expr: &BoundExpr) -> DbResult<u32> {
        if let Some((_, register)) = self
            .substitutions
            .iter()
            .find(|(candidate, _)| candidate == expr)
        {
            return Ok(*register);
        }
        match expr {
            BoundExpr::Raise {
                action,
                message,
                foreign_key,
            } => self.emit_raise(*action, message.as_deref(), *foreign_key),
            BoundExpr::Null => Ok(self.emit_load(Operand::Null)),
            BoundExpr::Integer(value) => Ok(self.emit_load(Operand::Integer(*value))),
            BoundExpr::Real(value) => Ok(self.emit_load(Operand::Real(*value))),
            BoundExpr::Text(text) => Ok(self.emit_load(Operand::Text(text.clone()))),
            BoundExpr::Blob(bytes) => Ok(self.emit_load(Operand::Blob(bytes.clone()))),
            BoundExpr::Parameter(index) => Ok(self.emit_load(Operand::Parameter(*index))),
            BoundExpr::Column {
                source,
                column: declared,
                slot,
                affinity,
                ..
            } => {
                let column = slot;
                let register = self.register();
                // A REAL column widens an integer back to a real on read; see
                // the note on the opcode.
                let widen = u16::from(*affinity == Affinity::Real);
                let cursor = self.cursor_for_source(*source);
                // A covering path never opened the table, so the column is read
                // out of the index entry the cursor is standing on. The slot is
                // the entry's, not the row's: an index holds the columns it was
                // declared over, in that order, and nothing else.
                if let Some((index_cursor, slot)) = self.covering_read(*source, *column) {
                    let register = self.register();
                    if slot == inillucent_sql::plan::ROWID_ENTRY_SLOT {
                        // The column *is* the rowid, which an entry keeps as
                        // its key rather than as a field.
                        self.emit(Instruction::new(
                            Opcode::IdxRowid,
                            index_cursor as i32,
                            register as i32,
                            0,
                        ));
                        return Ok(register);
                    }
                    self.emit(
                        Instruction::new(
                            Opcode::IdxColumn,
                            index_cursor as i32,
                            slot as i32,
                            register as i32,
                        )
                        .with_p5(widen),
                    );
                    return Ok(register);
                }
                // A subquery's rows live in an ephemeral store rather than
                // under a B-tree cursor, and the value is already a value: no
                // record to parse, and no affinity to re-apply on the way out.
                let opcode = if self.is_virtual_source(*source) {
                    // A module answers a column by number and returns a value,
                    // not a record slot: there is nothing to decode and no
                    // affinity to re-apply, and the number it is asked for is
                    // the *declared* position rather than a record slot -
                    // hidden columns take a position like any other.
                    Opcode::VColumn
                } else if self.is_ephemeral_source(*source) {
                    Opcode::EphColumn
                } else if self.is_index_source(*source) {
                    // A WITHOUT ROWID table is read through an index cursor, so
                    // its columns are read with the index opcode. Both read the
                    // same record at the same slot; the verifier separates them
                    // so that a table cursor is never asked for an index's key
                    // and the other way round.
                    Opcode::IdxColumn
                } else {
                    Opcode::Column
                };
                let column = if opcode == Opcode::VColumn {
                    // The declared position, not the record slot.
                    declared
                } else {
                    column
                };
                let mut read = Instruction::new(opcode, cursor, *column as i32, register as i32)
                    .with_p5(
                        if opcode == Opcode::EphColumn || opcode == Opcode::VColumn {
                            0
                        } else {
                            widen
                        },
                    );
                if opcode == Opcode::Column {
                    if let Some(default) = self.default_of(*source, *slot) {
                        read = read.with_p4(default);
                    }
                }
                self.emit(read);
                Ok(register)
            }
            BoundExpr::External { name, arguments } => {
                // The arguments go in a contiguous block, which is the shape
                // every call in this engine hands its implementation.
                let first = self.register_block(arguments.len().max(1));
                for (offset, argument) in arguments.iter().enumerate() {
                    let value = self.compile_expr(argument)?;
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        value as i32,
                        first as i32 + offset as i32,
                        0,
                    ));
                }
                let register = self.register();
                self.emit(
                    Instruction::new(
                        Opcode::ExtCall,
                        first as i32,
                        arguments.len() as i32,
                        register as i32,
                    )
                    .with_p4(Operand::Text(name.clone())),
                );
                Ok(register)
            }
            BoundExpr::VirtualFunction {
                source,
                name,
                arguments,
            } => {
                // The arguments go in a contiguous block because that is what
                // the module is handed: one slice, in the order they were
                // written, with the table itself already accounted for by the
                // cursor the opcode names.
                let first = self.register_block(arguments.len().max(1));
                for (offset, argument) in arguments.iter().enumerate() {
                    let value = self.compile_expr(argument)?;
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        value as i32,
                        first as i32 + offset as i32,
                        0,
                    ));
                }
                let register = self.register();
                let cursor = self.cursor_for_source(*source);
                self.emit(
                    Instruction::new(Opcode::VAux, cursor, first as i32, register as i32)
                        .with_p4(Operand::Text(name.clone()))
                        .with_p5(arguments.len() as u16),
                );
                Ok(register)
            }
            BoundExpr::Rowid { source } => {
                let register = self.register();
                let cursor = self.cursor_for_source(*source);
                if self.is_ephemeral_source(*source) {
                    // A materialised block has no rowid; nothing can have named
                    // one, because the binder refuses `rowid` on a subquery.
                    self.emit(Instruction::new(Opcode::Null, 0, register as i32, 0));
                    return Ok(register);
                }
                if self.is_virtual_source(*source) {
                    self.emit(Instruction::new(Opcode::VRowid, cursor, register as i32, 0));
                    return Ok(register);
                }
                if let Some(index_cursor) = self.covering_index(*source) {
                    // An index entry over a rowid table ends with the rowid,
                    // which is how the row would have been found had the query
                    // needed anything else from it.
                    self.emit(Instruction::new(
                        Opcode::IdxRowid,
                        index_cursor as i32,
                        register as i32,
                        0,
                    ));
                    return Ok(register);
                }
                self.emit(Instruction::new(Opcode::Rowid, cursor, register as i32, 0));
                Ok(register)
            }
            BoundExpr::Aggregate { slot } => self
                .aggregate_registers
                .get(*slot)
                .copied()
                .ok_or_else(|| error::misuse("an aggregate referenced before it was finalised")),
            BoundExpr::Subquery {
                id,
                kind,
                negated,
                operand,
                ..
            } => self.compile_value_subquery(*id, *kind, *negated, operand.as_deref()),
            BoundExpr::WindowRef { slot } => Err(error::misuse(format!(
                "window value {slot} was read outside a window pass"
            ))),
            BoundExpr::SorterColumn { column } => Err(error::misuse(format!(
                "a sorter column {column} escaped its sorter"
            ))),
            BoundExpr::Unary { op, operand } => {
                let source = self.compile_expr(operand)?;
                let register = self.register();
                let opcode = match op {
                    UnaryOp::Negate => Opcode::Negate,
                    UnaryOp::BitNot => Opcode::BitNot,
                    UnaryOp::Identity => Opcode::Copy,
                    UnaryOp::Not => Opcode::Not,
                };
                self.emit(Instruction::new(opcode, source as i32, register as i32, 0));
                Ok(register)
            }
            // A collation wrapper changes no value; it exists so the
            // comparison above it knows which collation to use.
            BoundExpr::Collate { operand, .. } => self.compile_expr(operand),
            BoundExpr::Not(operand) => {
                let source = self.compile_expr(operand)?;
                let register = self.register();
                self.emit(Instruction::new(
                    Opcode::Not,
                    source as i32,
                    register as i32,
                    0,
                ));
                Ok(register)
            }
            BoundExpr::Arithmetic { op, left, right } => {
                let left = self.compile_expr(left)?;
                let right = self.compile_expr(right)?;
                let register = self.register();
                self.emit(
                    Instruction::new(
                        Opcode::Arithmetic,
                        left as i32,
                        right as i32,
                        register as i32,
                    )
                    .with_p4(Operand::Arithmetic(*op)),
                );
                Ok(register)
            }
            BoundExpr::Compare {
                op,
                left,
                right,
                affinity,
                collation,
            } => {
                let left = self.compile_expr(left)?;
                let right = self.compile_expr(right)?;
                let register = self.register();
                self.emit(
                    Instruction::new(Opcode::Compare, left as i32, right as i32, register as i32)
                        .with_p4(Operand::Comparison(Comparison {
                            op: *op,
                            affinity: *affinity,
                            collation: *collation,
                        })),
                );
                Ok(register)
            }
            BoundExpr::Is {
                negated,
                left,
                right,
                affinity,
                collation,
            } => {
                let left = self.compile_expr(left)?;
                let right = self.compile_expr(right)?;
                let register = self.register();
                self.emit(
                    Instruction::new(Opcode::Is, left as i32, right as i32, register as i32)
                        .with_p4(Operand::Comparison(Comparison {
                            op: if *negated {
                                BinaryOp::NotEqual
                            } else {
                                BinaryOp::Equal
                            },
                            affinity: *affinity,
                            collation: *collation,
                        })),
                );
                Ok(register)
            }
            BoundExpr::And(left, right) => self.compile_logical(Opcode::And, left, right),
            BoundExpr::Or(left, right) => self.compile_logical(Opcode::Or, left, right),
            BoundExpr::IsNull { negated, operand } => {
                let source = self.compile_expr(operand)?;
                let register = self.register();
                self.emit(
                    Instruction::new(Opcode::IsNull, source as i32, register as i32, 0)
                        .with_p5(u16::from(*negated)),
                );
                Ok(register)
            }
            BoundExpr::Between {
                negated,
                operand,
                low,
                high,
                affinity,
                collation,
            } => self.compile_between(*negated, operand, low, high, *affinity, *collation),
            BoundExpr::InList {
                negated,
                operand,
                list,
                affinity,
                collation,
            } => {
                let value = self.compile_expr(operand)?;
                let block = self.register_block(list.len().max(1));
                for (index, item) in list.iter().enumerate() {
                    let register = self.compile_expr(item)?;
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        register as i32,
                        block.saturating_add(index as u32) as i32,
                        0,
                    ));
                }
                let register = self.register();
                self.emit(
                    Instruction::new(Opcode::InList, value as i32, block as i32, register as i32)
                        .with_p4(Operand::Comparison(Comparison {
                            op: if *negated {
                                BinaryOp::NotEqual
                            } else {
                                BinaryOp::Equal
                            },
                            affinity: *affinity,
                            collation: *collation,
                        }))
                        .with_p5(list.len() as u16),
                );
                Ok(register)
            }
            BoundExpr::Case {
                operand,
                branches,
                otherwise,
                collation,
            } => self.compile_case(
                operand.as_deref(),
                branches,
                otherwise.as_deref(),
                *collation,
            ),
            BoundExpr::Cast { operand, affinity } => {
                let source = self.compile_expr(operand)?;
                let register = self.register();
                self.emit(
                    Instruction::new(Opcode::Cast, source as i32, register as i32, 0)
                        .with_p4(Operand::Affinity(*affinity)),
                );
                Ok(register)
            }
            BoundExpr::Pattern {
                negated,
                op,
                operand,
                pattern,
                escape,
            } => {
                let count = 2usize.saturating_add(usize::from(escape.is_some()));
                let block = self.register_block(count);
                // SQLite's LIKE takes the pattern first, and the argument order
                // is visible through the `like()` function, so it is kept.
                let pattern_register = self.compile_expr(pattern)?;
                self.emit(Instruction::new(
                    Opcode::Copy,
                    pattern_register as i32,
                    block as i32,
                    0,
                ));
                let operand_register = self.compile_expr(operand)?;
                self.emit(Instruction::new(
                    Opcode::Copy,
                    operand_register as i32,
                    block.saturating_add(1) as i32,
                    0,
                ));
                if let Some(escape) = escape {
                    let escape_register = self.compile_expr(escape)?;
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        escape_register as i32,
                        block.saturating_add(2) as i32,
                        0,
                    ));
                }
                let register = self.register();
                self.emit(
                    Instruction::new(Opcode::Pattern, block as i32, count as i32, register as i32)
                        .with_p4(Operand::Pattern(*op))
                        .with_p5(u16::from(*negated)),
                );
                Ok(register)
            }
            BoundExpr::Time { func, arguments } => {
                let block = self.register_block(arguments.len().max(1));
                for (index, argument) in arguments.iter().enumerate() {
                    let register = self.compile_expr(argument)?;
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        register as i32,
                        block.saturating_add(index as u32) as i32,
                        0,
                    ));
                }
                let register = self.register();
                self.emit(
                    Instruction::new(
                        Opcode::TimeCall,
                        block as i32,
                        arguments.len() as i32,
                        register as i32,
                    )
                    .with_p4(Operand::Time(*func)),
                );
                Ok(register)
            }
            BoundExpr::Math { func, arguments } => {
                let block = self.register_block(arguments.len().max(1));
                for (index, argument) in arguments.iter().enumerate() {
                    let register = self.compile_expr(argument)?;
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        register as i32,
                        block.saturating_add(index as u32) as i32,
                        0,
                    ));
                }
                let register = self.register();
                self.emit(
                    Instruction::new(
                        Opcode::MathCall,
                        block as i32,
                        arguments.len() as i32,
                        register as i32,
                    )
                    .with_p4(Operand::Math(*func)),
                );
                Ok(register)
            }
            BoundExpr::Json { func, arguments } => {
                let block = self.register_block(arguments.len().max(1));
                for (index, argument) in arguments.iter().enumerate() {
                    let register = self.compile_expr(argument)?;
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        register as i32,
                        block.saturating_add(index as u32) as i32,
                        0,
                    ));
                }
                let register = self.register();
                self.emit(
                    Instruction::new(
                        Opcode::JsonCall,
                        block as i32,
                        arguments.len() as i32,
                        register as i32,
                    )
                    .with_p4(Operand::Json(*func)),
                );
                Ok(register)
            }
            BoundExpr::Function {
                func,
                arguments,
                collation,
            } => {
                let block = self.register_block(arguments.len().max(1));
                for (index, argument) in arguments.iter().enumerate() {
                    let register = self.compile_expr(argument)?;
                    self.emit(Instruction::new(
                        Opcode::Copy,
                        register as i32,
                        block.saturating_add(index as u32) as i32,
                        0,
                    ));
                }
                let register = self.register();
                self.emit(
                    Instruction::new(
                        Opcode::Function,
                        block as i32,
                        arguments.len() as i32,
                        register as i32,
                    )
                    .with_p4(Operand::Scalar(*func, *collation)),
                );
                Ok(register)
            }
        }
    }

    /// Emits a literal load into a fresh register.
    pub(crate) fn emit_load(&mut self, operand: Operand) -> u32 {
        let register = self.register();
        self.emit(Instruction::new(Opcode::Load, 0, register as i32, 0).with_p4(operand));
        register
    }

    /// Compiles `AND` or `OR`, evaluating both sides.
    ///
    /// SQLite short-circuits these in the branch opcodes rather than in the
    /// value opcodes, and a three-valued `AND` has to see both sides anyway
    /// when the first is NULL, so both are evaluated here.
    fn compile_logical(
        &mut self,
        opcode: Opcode,
        left: &BoundExpr,
        right: &BoundExpr,
    ) -> DbResult<u32> {
        let left = self.compile_expr(left)?;
        let right = self.compile_expr(right)?;
        let register = self.register();
        self.emit(Instruction::new(
            opcode,
            left as i32,
            right as i32,
            register as i32,
        ));
        Ok(register)
    }

    /// Compiles `BETWEEN`, evaluating the operand exactly once.
    fn compile_between(
        &mut self,
        negated: bool,
        operand: &BoundExpr,
        low: &BoundExpr,
        high: &BoundExpr,
        affinity: Option<Affinity>,
        collation: Collation,
    ) -> DbResult<u32> {
        let value = self.compile_expr(operand)?;
        let low = self.compile_expr(low)?;
        let high = self.compile_expr(high)?;
        let above = self.register();
        self.emit(
            Instruction::new(Opcode::Compare, value as i32, low as i32, above as i32).with_p4(
                Operand::Comparison(Comparison {
                    op: BinaryOp::GreaterEqual,
                    affinity,
                    collation,
                }),
            ),
        );
        let below = self.register();
        self.emit(
            Instruction::new(Opcode::Compare, value as i32, high as i32, below as i32).with_p4(
                Operand::Comparison(Comparison {
                    op: BinaryOp::LessEqual,
                    affinity,
                    collation,
                }),
            ),
        );
        let both = self.register();
        self.emit(Instruction::new(
            Opcode::And,
            above as i32,
            below as i32,
            both as i32,
        ));
        if !negated {
            return Ok(both);
        }
        let register = self.register();
        self.emit(Instruction::new(
            Opcode::Not,
            both as i32,
            register as i32,
            0,
        ));
        Ok(register)
    }

    /// Compiles `CASE`, which is the only expression with real control flow.
    fn compile_case(
        &mut self,
        operand: Option<&BoundExpr>,
        branches: &[(BoundExpr, BoundExpr)],
        otherwise: Option<&BoundExpr>,
        collation: Collation,
    ) -> DbResult<u32> {
        let result = self.register();
        let base = match operand {
            Some(expr) => Some(self.compile_expr(expr)?),
            None => None,
        };
        let mut ends = Vec::new();
        for (when, then) in branches {
            let test = match base {
                Some(base) => {
                    let candidate = self.compile_expr(when)?;
                    let matched = self.register();
                    let affinity = None;
                    self.emit(
                        Instruction::new(
                            Opcode::Compare,
                            base as i32,
                            candidate as i32,
                            matched as i32,
                        )
                        .with_p4(Operand::Comparison(Comparison {
                            op: BinaryOp::Equal,
                            affinity,
                            collation,
                        })),
                    );
                    matched
                }
                None => self.compile_expr(when)?,
            };
            let next =
                self.emit_jump(Instruction::new(Opcode::IfNot, test as i32, -1, 0).with_p5(1));
            let value = self.compile_expr(then)?;
            self.emit(Instruction::new(
                Opcode::Copy,
                value as i32,
                result as i32,
                0,
            ));
            ends.push(self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0)));
            self.patch_here(next);
        }
        match otherwise {
            Some(expr) => {
                let value = self.compile_expr(expr)?;
                self.emit(Instruction::new(
                    Opcode::Copy,
                    value as i32,
                    result as i32,
                    0,
                ));
            }
            None => {
                self.emit(Instruction::new(Opcode::Null, 0, result as i32, 0));
            }
        }
        for label in ends {
            self.patch_here(label);
        }
        Ok(result)
    }
}

/// Compiles a bound SELECT end to end, planning it first.
pub fn compile_select(
    select: BoundSelect,
    dependencies: ProgramDependencies,
    parameters: u32,
) -> DbResult<(Program, PhysicalPlan)> {
    compile_select_with(select, dependencies, parameters, None)
}

/// Compiles a bound `SELECT`, with somebody to ask about virtual tables.
pub fn compile_select_with(
    select: BoundSelect,
    dependencies: ProgramDependencies,
    parameters: u32,
    planner: Option<Box<dyn VirtualPlanner>>,
) -> DbResult<(Program, PhysicalPlan)> {
    let levers = inillucent_sql::plan::Levers::without(dependencies.levers);
    let mut plan = inillucent_sql::plan::plan_select_with(select, levers);
    let mut compiler = Compiler::with_levers(levers);
    if let Some(planner) = planner {
        compiler = compiler.with_virtual_planner(planner);
    }
    compiler.resolve_plan(&mut plan)?;
    let program = compile_into(compiler, &plan, dependencies, parameters)?;
    Ok((program, plan))
}

/// Builds a program that emits a fixed set of rows.
///
/// `EXPLAIN` answers with rows about a statement rather than by running it, and
/// so does anything else that reports rather than computes. Rendering the rows
/// here and emitting them as constants keeps the statement an ordinary prepared
/// statement - it steps, it has result-column metadata, it resets - instead of
/// a second path through the session that would have to reimplement all of it.
pub fn compile_rows(
    columns: &[&str],
    rows: &[Vec<Operand>],
    dependencies: ProgramDependencies,
) -> DbResult<Program> {
    let mut compiler = Compiler::new();
    let entry = compiler.emit_jump(Instruction::new(Opcode::Init, 0, -1, 0));
    compiler.patch_here(entry);
    let width = columns.len();
    let block = compiler.register_block(width.max(1));
    for row in rows {
        for index in 0..width {
            let value = row.get(index).cloned().unwrap_or(Operand::Null);
            compiler.emit(
                Instruction::new(
                    Opcode::Load,
                    0,
                    block.saturating_add(index as u32) as i32,
                    0,
                )
                .with_p4(value),
            );
        }
        compiler.emit(Instruction::new(
            Opcode::ResultRow,
            block as i32,
            width as i32,
            0,
        ));
    }
    compiler.emit(Instruction::new(Opcode::Halt, 0, 0, 0));
    Ok(Program {
        instructions: compiler.instructions,
        register_count: compiler.registers,
        cursor_count: 0,
        sorter_count: 0,
        distinct_count: 0,
        ephemeral_count: 0,
        aggregate_count: 0,
        result_columns: columns
            .iter()
            .map(|name| ResultColumn {
                name: name.as_bytes().to_vec(),
                origin: None,
                declared_type: Vec::new(),
            })
            .collect(),
        dependencies,
        readonly: true,
        optimizations_used: compiler.used,
        parameter_count: 0,
    })
}

/// Returns the table a planned source names, for diagnostics.
pub fn source_table(plan: &PhysicalPlan, level: usize) -> Option<&TableInfo> {
    plan.sources.get(level).map(|source| &source.table)
}

/// Returns whether a pattern operator is case-insensitive by default.
pub fn pattern_is_case_insensitive(op: PatternOp) -> bool {
    op == PatternOp::Like
}

/// Returns the table column each index key names, dropping the ones it computes.
///
/// **The old engine has no expression-index support and is not going to grow
/// one.** `AccessPath::IndexSeek::columns` became an `Option` per key when the
/// new engine learned to seek on `lower(a)`; here a computed key can only ever
/// appear if the planner offered this compiler a path it cannot build, and the
/// planner does not, because an expression index is not in a schema this engine
/// loads. Taking the columns it can name keeps the two engines compiling from
/// one plan type without pretending this one understands the new form.
///
/// @param columns - the plan's per-key columns
pub(crate) fn named_columns(columns: Vec<Option<u16>>) -> Vec<u16> {
    columns.into_iter().flatten().collect()
}
