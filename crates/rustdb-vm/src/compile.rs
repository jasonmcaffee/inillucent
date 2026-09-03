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

use rustdb_base::{error, DbResult};
use rustdb_sql::ast::{BinaryOp, NullOrder, PatternOp, SortOrder, UnaryOp};
use rustdb_sql::bind::{BoundAggregate, BoundExpr, BoundOrderTerm, BoundSelect};
use rustdb_sql::catalog_view::TableInfo;
use rustdb_sql::plan::{AccessPath, AggregationMode, BoundKind, PhysicalPlan, RangeBound};
use rustdb_value::{Affinity, Collation};

use crate::program::{
    AggregateCall, Comparison, IndexKey, Instruction, Opcode, Operand, Program,
    ProgramDependencies, ResultColumn, SortColumn, SortKey,
};

/// A jump target that is patched once its address is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Label(pub(crate) usize);

/// The compiler's working state.
pub struct Compiler {
    pub(crate) instructions: Vec<Instruction>,
    pub(crate) registers: u32,
    pub(crate) cursors: u32,
    pub(crate) sorters: u32,
    distincts: u32,
    aggregates: u32,
    pub(crate) substitutions: Vec<(BoundExpr, u32)>,
    /// The table cursor each FROM term reads through.
    ///
    /// A source's number and its cursor's number are not the same thing, and
    /// assuming they were is a bug the verifier caught rather than a wrong
    /// answer: an access path that opens an index takes a second cursor, so
    /// the second FROM term's table cursor is number two rather than number
    /// one, and `SELECT b.label FROM a, b WHERE a.k = 'x'` addressed `a`'s
    /// index cursor as if it were `b`'s table.
    pub(crate) source_cursors: Vec<u32>,
    aggregate_registers: Vec<u32>,
    pub(crate) end_jumps: Vec<Label>,
}

impl Default for Compiler {
    /// Returns an empty compiler.
    fn default() -> Compiler {
        Compiler::new()
    }
}

impl Compiler {
    /// Returns an empty compiler.
    pub fn new() -> Compiler {
        Compiler {
            instructions: Vec::new(),
            // Register 0 is never handed out, so a zero in an unset operand is
            // visibly wrong rather than silently the first register.
            registers: 1,
            cursors: 0,
            sorters: 0,
            distincts: 0,
            aggregates: 0,
            substitutions: Vec::new(),
            source_cursors: Vec::new(),
            aggregate_registers: Vec::new(),
            end_jumps: Vec::new(),
        }
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
            .map_or(source as i32, |cursor| cursor as i32)
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
    let mut compiler = Compiler::new();
    let entry = compiler.emit_jump(Instruction::new(Opcode::Init, 0, -1, 0));
    compiler.patch_here(entry);
    compiler.emit(Instruction::new(Opcode::Transaction, 0, 0, 0));
    let cursors = compiler.open_cursors(plan);
    let (limit_register, offset_register) = compiler.compile_limits(plan)?;
    let sorter = compiler.open_order_sorter(plan);
    let distinct = compiler.open_distinct(plan);
    let body = Body {
        plan,
        cursors: cursors.clone(),
        sorter,
        distinct,
        limit_register,
        offset_register,
    };
    compiler.compile_statement(&body)?;
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
        aggregate_count: compiler.aggregates,
        result_columns,
        dependencies,
        readonly: true,
        parameter_count: parameters,
    })
}

/// The cursors one FROM term uses.
#[derive(Clone, Copy, Debug)]
struct SourceCursors {
    table: u32,
    index: Option<u32>,
}

/// Everything the loop nest needs to know.
struct Body<'a> {
    plan: &'a PhysicalPlan,
    cursors: Vec<SourceCursors>,
    sorter: Option<u32>,
    distinct: Option<u32>,
    limit_register: Option<u32>,
    offset_register: Option<u32>,
}

impl Compiler {
    /// Opens one cursor per FROM term, and a second for an index path.
    fn open_cursors(&mut self, plan: &PhysicalPlan) -> Vec<SourceCursors> {
        let mut cursors = Vec::with_capacity(plan.sources.len());
        for source in &plan.sources {
            let table = self.cursors;
            self.cursors = self.cursors.saturating_add(1);
            let columns = source.table.columns.len() as i32;
            self.emit(
                Instruction::new(Opcode::OpenRead, table as i32, source.table.root as i32, 0)
                    .with_p4(Operand::Count(columns.max(0) as u32)),
            );
            let index = match &source.path {
                AccessPath::IndexSeek {
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
                        Instruction::new(Opcode::OpenIndex, cursor as i32, *index_root as i32, 0)
                            .with_p4(Operand::IndexKey(key)),
                    );
                    Some(cursor)
                }
                _ => None,
            };
            cursors.push(SourceCursors { table, index });
            self.source_cursors.push(table);
        }
        cursors
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
                    Instruction::new(Opcode::Cast, source as i32, source as i32, 0)
                        .with_p4(Operand::Affinity(Affinity::Integer)),
                );
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
                    Instruction::new(Opcode::Cast, source as i32, source as i32, 0)
                        .with_p4(Operand::Affinity(Affinity::Integer)),
                );
                self.emit(
                    Instruction::new(Opcode::Copy, source as i32, counter as i32, 0).with_p5(2),
                );
                Some(counter)
            }
            None => None,
        };
        Ok((limit, offset))
    }

    /// Opens the sorter an ORDER BY needs.
    fn open_order_sorter(&mut self, plan: &PhysicalPlan) -> Option<u32> {
        if plan.select.order_by.is_empty() {
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

    /// Opens the set a DISTINCT needs, with one collation per result column.
    fn open_distinct(&mut self, plan: &PhysicalPlan) -> Option<u32> {
        if !plan.select.distinct {
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
                    collation: rustdb_sql::bind::result_collation(&column.expr),
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
    fn compile_scan(&mut self, body: &Body<'_>) -> DbResult<()> {
        let width = body.plan.select.columns.len();
        let block = self.register_block(width);
        let tail_return = self.register();
        let skip = self.emit_jump(Instruction::new(Opcode::Goto, 0, -1, 0));
        let tail = self.here();
        self.compile_tail(body, block, width, true, tail_return)?;
        self.patch_here(skip);
        self.guard_constant_filter(body)?;
        let emit = InnerBody::Row {
            block,
            tail,
            tail_return,
        };
        self.compile_level(body, 0, &emit)?;
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

    /// Compiles a grouped aggregate: sort by the group key, then stream.
    ///
    /// The order matters and is easy to get wrong. The scan writes the group
    /// key and every aggregate argument into a sorter *without* substitutions,
    /// because there it is computing those values from a row. The subroutine
    /// that emits one group runs *with* substitutions, because there the group
    /// key is no longer a row - it is the registers holding the key of the
    /// group the sorter has just finished.
    fn compile_grouped_aggregate(&mut self, body: &Body<'_>) -> DbResult<()> {
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
                    collation: rustdb_sql::bind::result_collation(expr),
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
            .map(|expr| rustdb_sql::bind::result_collation(expr))
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
                        distinct: aggregate.distinct,
                        collation: aggregate.collation,
                    }),
                ),
            );
        }
        let _ = payload;
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
        self.emit(Instruction::new(
            Opcode::ResultRow,
            block as i32,
            width as i32,
            0,
        ));
        let exhausted = self.emit_limit_decrement(body);
        for label in skip {
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
        self.emit(Instruction::new(
            Opcode::ResultRow,
            block as i32,
            width as i32,
            0,
        ));
        let exhausted = self.emit_limit_decrement(body);
        for label in skip.into_iter().chain(offset_skip) {
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
    fn compile_level(&mut self, body: &Body<'_>, level: usize, inner: &InnerBody) -> DbResult<()> {
        if level >= body.plan.sources.len() {
            return self.compile_inner(body, inner);
        }
        let Some(source) = body.plan.sources.get(level) else {
            return Err(error::misuse("plan level out of range"));
        };
        let Some(cursors) = body.cursors.get(level).copied() else {
            return Err(error::misuse("cursor level out of range"));
        };
        let path = source.path.clone();
        match path {
            AccessPath::TableScan { .. } => {
                let empty = self.emit_jump(Instruction::new(
                    Opcode::Rewind,
                    cursors.table as i32,
                    -1,
                    0,
                ));
                let start = self.here();
                let skip = self.compile_residual(body, level)?;
                self.compile_level(body, level.saturating_add(1), inner)?;
                for label in skip {
                    self.patch_here(label);
                }
                let more =
                    self.emit_jump(Instruction::new(Opcode::Next, cursors.table as i32, -1, 0));
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
                self.compile_rowid_range(body, level, cursors, low, high, inner)?;
            }
            AccessPath::IndexSeek {
                equalities,
                low,
                high,
                columns,
                without_rowid,
                ..
            } => {
                self.compile_index_seek(
                    body,
                    level,
                    cursors,
                    &equalities,
                    low,
                    high,
                    &columns,
                    without_rowid,
                    inner,
                )?;
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
        inner: &InnerBody,
    ) -> DbResult<()> {
        let empty = match &low {
            Some(bound) => {
                let register = self.compile_expr(&bound.value)?;
                self.emit(
                    Instruction::new(Opcode::Cast, register as i32, register as i32, 0)
                        .with_p4(Operand::Affinity(Affinity::Integer)),
                );
                let opcode = if bound.kind == BoundKind::Greater {
                    Opcode::SeekGt
                } else {
                    Opcode::SeekGe
                };
                self.emit_jump(
                    Instruction::new(opcode, cursors.table as i32, -1, register as i32).with_p5(1),
                )
            }
            None => self.emit_jump(Instruction::new(
                Opcode::Rewind,
                cursors.table as i32,
                -1,
                0,
            )),
        };
        let start = self.here();
        let mut done: Vec<Label> = Vec::new();
        if let Some(bound) = &high {
            let limit = self.compile_expr(&bound.value)?;
            let rowid = self.register();
            self.emit(Instruction::new(
                Opcode::Rowid,
                cursors.table as i32,
                rowid as i32,
                0,
            ));
            let result = self.register();
            let op = if bound.kind == BoundKind::Less {
                BinaryOp::Less
            } else {
                BinaryOp::LessEqual
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
        let more = self.emit_jump(Instruction::new(Opcode::Next, cursors.table as i32, -1, 0));
        self.patch(more, start);
        for label in done {
            self.patch_here(label);
        }
        self.patch_here(empty);
        Ok(())
    }

    /// Compiles an index seek: position, walk, stop at the upper bound, and
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
        without_rowid: bool,
        inner: &InnerBody,
    ) -> DbResult<()> {
        let Some(index_cursor) = cursors.index else {
            return Err(error::misuse("an index path with no index cursor"));
        };
        let key_len = equalities.len().saturating_add(usize::from(low.is_some()));
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
        let mut opcode = Opcode::SeekGe;
        if let Some(bound) = &low {
            let register = self.compile_expr(&bound.value)?;
            self.emit(Instruction::new(
                Opcode::Copy,
                register as i32,
                key.saturating_add(equalities.len() as u32) as i32,
                0,
            ));
            seek_len = seek_len.saturating_add(1);
            opcode = if bound.kind == BoundKind::Greater {
                Opcode::SeekGt
            } else {
                Opcode::SeekGe
            };
        }
        self.apply_index_affinity(body, level, columns, key, seek_len)?;
        let empty = self.emit_jump(
            Instruction::new(opcode, index_cursor as i32, -1, key as i32).with_p5(seek_len as u16),
        );
        let start = self.here();
        let mut done: Vec<Label> = Vec::new();
        // The equality prefix has to be re-checked on every entry, because a
        // seek positions at the first matching entry and the scan runs past the
        // last one.
        if !equalities.is_empty() {
            done.push(
                self.emit_jump(
                    Instruction::new(Opcode::IdxGt, index_cursor as i32, -1, key as i32)
                        .with_p5(equalities.len() as u16),
                ),
            );
        }
        if let Some(bound) = &high {
            let high_key = self.register_block(equalities.len().saturating_add(1));
            for index in 0..equalities.len() {
                self.emit(Instruction::new(
                    Opcode::Copy,
                    key.saturating_add(index as u32) as i32,
                    high_key.saturating_add(index as u32) as i32,
                    0,
                ));
            }
            let register = self.compile_expr(&bound.value)?;
            self.emit(Instruction::new(
                Opcode::Copy,
                register as i32,
                high_key.saturating_add(equalities.len() as u32) as i32,
                0,
            ));
            let length = equalities.len().saturating_add(1);
            self.apply_index_affinity(body, level, columns, high_key, length)?;
            let opcode = if bound.kind == BoundKind::Less {
                Opcode::IdxGe
            } else {
                Opcode::IdxGt
            };
            done.push(
                self.emit_jump(
                    Instruction::new(opcode, index_cursor as i32, -1, high_key as i32)
                        .with_p5(length as u16),
                ),
            );
        }
        let mut skip: Vec<Label> = Vec::new();
        if !without_rowid {
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
        }
        skip.extend(self.compile_residual(body, level)?);
        self.compile_level(body, level.saturating_add(1), inner)?;
        for label in skip {
            self.patch_here(label);
        }
        let more = self.emit_jump(Instruction::new(Opcode::Next, index_cursor as i32, -1, 0));
        self.patch(more, start);
        for label in done {
            self.patch_here(label);
        }
        self.patch_here(empty);
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
        let Some(Some(residual)) = body.plan.residuals.get(level) else {
            return Ok(Vec::new());
        };
        let residual = residual.clone();
        let register = self.compile_expr(&residual)?;
        Ok(vec![self.emit_jump(
            Instruction::new(Opcode::IfNot, register as i32, -1, 0).with_p5(1),
        )])
    }

    /// Emits whatever the innermost loop body does.
    fn compile_inner(&mut self, body: &Body<'_>, inner: &InnerBody) -> DbResult<()> {
        match inner {
            InnerBody::Row {
                block,
                tail,
                tail_return,
            } => {
                self.build_result_row(body, *block)?;
                self.emit(Instruction::new(
                    Opcode::Gosub,
                    *tail_return as i32,
                    *tail,
                    0,
                ));
                Ok(())
            }
            InnerBody::AggregateStep => {
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
                        Instruction::new(Opcode::AggStep, block as i32, count as i32, slot as i32)
                            .with_p4(Operand::Aggregate(AggregateCall {
                                func: aggregate.func,
                                distinct: aggregate.distinct,
                                collation: aggregate.collation,
                            })),
                    );
                }
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

/// What the innermost loop body does with a row.
enum InnerBody {
    /// Build a result row and run the tail.
    Row {
        /// The register block the row is built in.
        block: u32,
        /// The tail subroutine's address.
        tail: i32,
        /// The register holding the tail's return address.
        tail_return: u32,
    },
    /// Step every aggregate.
    AggregateStep,
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

/// Returns the sorter column description one ORDER BY term needs.
fn sort_column(term: &BoundOrderTerm) -> SortColumn {
    SortColumn {
        descending: term.order == SortOrder::Descending,
        nulls_first: term.nulls == NullOrder::First,
        collation: term.collation,
    }
}

impl Compiler {
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
            BoundExpr::Null => Ok(self.emit_load(Operand::Null)),
            BoundExpr::Integer(value) => Ok(self.emit_load(Operand::Integer(*value))),
            BoundExpr::Real(value) => Ok(self.emit_load(Operand::Real(*value))),
            BoundExpr::Text(text) => Ok(self.emit_load(Operand::Text(text.clone()))),
            BoundExpr::Blob(bytes) => Ok(self.emit_load(Operand::Blob(bytes.clone()))),
            BoundExpr::Parameter(index) => Ok(self.emit_load(Operand::Parameter(*index))),
            BoundExpr::Column {
                source,
                column,
                affinity,
                ..
            } => {
                let register = self.register();
                // A REAL column widens an integer back to a real on read; see
                // the note on the opcode.
                let widen = u16::from(*affinity == Affinity::Real);
                let cursor = self.cursor_for_source(*source);
                self.emit(
                    Instruction::new(Opcode::Column, cursor, *column as i32, register as i32)
                        .with_p5(widen),
                );
                Ok(register)
            }
            BoundExpr::Rowid { source } => {
                let register = self.register();
                let cursor = self.cursor_for_source(*source);
                self.emit(Instruction::new(Opcode::Rowid, cursor, register as i32, 0));
                Ok(register)
            }
            BoundExpr::Aggregate { slot } => self
                .aggregate_registers
                .get(*slot)
                .copied()
                .ok_or_else(|| error::misuse("an aggregate referenced before it was finalised")),
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
    let plan = rustdb_sql::plan::plan_select(select);
    let program = compile(&plan, dependencies, parameters)?;
    Ok((program, plan))
}

/// Returns the table a planned source names, for diagnostics.
pub fn source_table(plan: &PhysicalPlan, level: usize) -> Option<&TableInfo> {
    plan.sources.get(level).map(|source| &source.table)
}

/// Returns whether a pattern operator is case-insensitive by default.
pub fn pattern_is_case_insensitive(op: PatternOp) -> bool {
    op == PatternOp::Like
}
