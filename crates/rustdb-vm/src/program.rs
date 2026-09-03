//! The bytecode: instructions, operands, and the program that holds them.
//!
//! Invariant: a program is immutable once compiled and describes everything it
//! will do. Its register count, cursor count and result metadata are fixed, its
//! jumps resolve to instructions inside it, and a read-only program contains no
//! opcode that writes. The verifier proves all of that before the machine runs
//! a single instruction, so the machine itself never has to ask.
//!
//! Opcode numbers are rust-db's own. Nothing persists a program, so there is no
//! compatibility promise here; the only contract is with the verifier and the
//! machine in this crate.

use rustdb_sql::ast::{BinaryOp, PatternOp};
use rustdb_sql::function::{AggregateFunc, ScalarFunc};
use rustdb_value::{Affinity, Collation};

/// What an instruction does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opcode {
    /// `p2`: jump to the program's real entry point.
    Init,
    /// `p2`: jump.
    Goto,
    /// `p1`: return-address register, `p2`: subroutine start.
    Gosub,
    /// `p1`: return-address register.
    Return,
    /// Stop, successfully.
    Halt,
    /// `p1`: database. Begin a read transaction.
    Transaction,
    /// `p1`: cursor, `p2`: root page, `p3`: database, `p4`: column count.
    OpenRead,
    /// `p1`: cursor, `p2`: root page, `p3`: database, `p4`: key description.
    OpenIndex,
    /// `p1`: cursor.
    Close,
    /// `p1`: cursor, `p2`: jump when the tree is empty.
    Rewind,
    /// `p1`: cursor, `p2`: jump when the tree is empty.
    Last,
    /// `p1`: cursor, `p2`: jump when another row exists.
    Next,
    /// `p1`: cursor, `p2`: jump when another row exists.
    Prev,
    /// `p1`: cursor, `p2`: jump when not found, `p3`: register holding a rowid.
    SeekRowid,
    /// `p1`: cursor, `p2`: jump when no row is at or after the key,
    /// `p3`: first key register, `p5`: key column count.
    SeekGe,
    /// As [`Opcode::SeekGe`], but strictly after.
    SeekGt,
    /// `p1`: cursor, `p2`: jump when the entry is past the key's upper bound,
    /// `p3`: first key register, `p5`: key column count.
    IdxGe,
    /// As [`Opcode::IdxGe`], but strictly after.
    IdxGt,
    /// `p1`: cursor, `p2`: destination register. Read the trailing rowid.
    IdxRowid,
    /// `p1`: cursor, `p2`: column, `p3`: destination register.
    ///
    /// `p5` of 1 widens an integer result back to a real. SQLite stores a REAL
    /// column whose value is an exact integer with an integer serial type and
    /// relies on the column's declared affinity to widen it again on read; a
    /// reader that returns the raw integer disagrees with `typeof()` on every
    /// such row.
    Column,
    /// `p1`: cursor, `p2`: index key column, `p3`: destination register.
    IdxColumn,
    /// `p1`: cursor, `p2`: destination register.
    Rowid,
    /// `p2`: destination register. Store NULL.
    Null,
    /// `p2`: destination register, `p4`: the value.
    Load,
    /// `p1`: source register, `p2`: destination register.
    ///
    /// `p5` selects a normalisation: 0 copies the value, 1 turns it into a
    /// LIMIT counter and 2 into an OFFSET counter. A NULL or negative LIMIT
    /// means "no limit" and a NULL or negative OFFSET means "no offset", and
    /// folding both into a counter here is what lets the loop test a plain
    /// integer instead of carrying the special cases through every path.
    Copy,
    /// `p1`: left, `p2`: right, `p3`: destination, for an arithmetic operator
    /// named by `p4`.
    Arithmetic,
    /// `p1`: operand, `p2`: destination. Arithmetic negation.
    Negate,
    /// `p1`: operand, `p2`: destination. Bitwise complement.
    BitNot,
    /// `p1`: left, `p2`: right, `p3`: destination, `p4`: the comparison.
    Compare,
    /// As [`Opcode::Compare`] but with `IS` semantics: never NULL.
    Is,
    /// `p1`: left, `p2`: right, `p3`: destination. Three-valued `AND`.
    And,
    /// `p1`: left, `p2`: right, `p3`: destination. Three-valued `OR`.
    Or,
    /// `p1`: operand, `p2`: destination. Three-valued `NOT`.
    Not,
    /// `p1`: operand, `p2`: destination, `p5`: 1 for `NOT NULL`.
    IsNull,
    /// `p1`: operand, `p2`: first list register, `p3`: destination,
    /// `p5`: list length, `p4`: the comparison, `p3` is set to NULL when the
    /// answer is unknown.
    InList,
    /// `p1`: register to test, `p2`: jump when true, `p5`: 1 to jump on NULL.
    If,
    /// `p1`: register to test, `p2`: jump when false, `p5`: 1 to jump on NULL.
    IfNot,
    /// `p1`: register, `p2`: jump when the register is NULL.
    IfNull,
    /// `p1`: register, `p2`: jump when the register is not NULL.
    IfNotNull,
    /// `p1`: counter register, `p2`: jump when it was positive, `p3`: amount to
    /// subtract when it was.
    IfPos,
    /// `p1`: counter register, `p2`: jump when it reaches zero.
    DecrJumpZero,
    /// `p1`: operand, `p2`: destination, `p4`: the target affinity.
    Cast,
    /// `p1`: first register, `p2`: count, `p4`: the affinity to apply in place.
    ApplyAffinity,
    /// `p1`: first argument register, `p2`: argument count, `p3`: destination,
    /// `p4`: the function.
    Function,
    /// `p1`: first argument register, `p2`: argument count, `p3`: destination,
    /// `p4`: the pattern operator, `p5`: 1 when negated.
    Pattern,
    /// `p1`: first argument register, `p2`: argument count, `p3`: accumulator,
    /// `p4`: the aggregate.
    AggStep,
    /// `p1`: accumulator, `p2`: destination, `p4`: the aggregate.
    AggFinal,
    /// `p1`: accumulator, `p4`: the aggregate. Start a fresh group.
    AggReset,
    /// `p1`: sorter, `p4`: the sort key description.
    SorterOpen,
    /// `p1`: sorter, `p2`: first register, `p3`: count.
    SorterInsert,
    /// `p1`: sorter, `p2`: jump when the sorter is empty.
    SorterSort,
    /// `p1`: sorter, `p2`: jump when another row exists.
    SorterNext,
    /// `p1`: sorter, `p2`: column, `p3`: destination register.
    SorterColumn,
    /// `p1`: set. Open a distinct set.
    DistinctOpen,
    /// `p1`: set, `p2`: jump when the row has been seen, `p3`: first register,
    /// `p5`: count.
    DistinctCheck,
    /// `p1`: first register, `p2`: count. Emit a result row.
    ResultRow,
    /// `p1`: cursor, `p2`: root page, `p3`: database, `p4`: column count.
    ///
    /// The write half of [`Opcode::OpenRead`]. It is a separate opcode rather
    /// than a flag so that the verifier can refuse it in a read-only program
    /// by asking the opcode alone, with nothing to read out of an operand and
    /// therefore nothing to get wrong.
    OpenWrite,
    /// `p1`: cursor, `p2`: root page, `p3`: database, `p4`: key description.
    OpenWriteIndex,
    /// `p1`: cursor, `p2`: destination register. Allocate an unused rowid.
    NewRowid,
    /// `p1`: first register, `p2`: count, `p3`: destination,
    /// `p4`: the affinity of each column, applied before encoding.
    MakeRecord,
    /// `p1`: cursor, `p2`: record register, `p3`: rowid register.
    ///
    /// `p5` of 1 promises the rowid is past every rowid already in the tree,
    /// which lets the append path skip the descent.
    InsertRow,
    /// `p1`: cursor. Delete the row the cursor is on.
    DeleteRow,
    /// `p1`: cursor, `p2`: record register. Insert an index entry.
    IdxInsert,
    /// `p1`: cursor, `p2`: record register. Delete an index entry.
    IdxDelete,
    /// `p1`: cursor, `p2`: jump when no row has this rowid, `p3`: rowid.
    NotExists,
    /// `p1`: index cursor, `p2`: jump when no entry has this key prefix,
    /// `p3`: first key register, `p5`: key column count.
    ///
    /// A NULL anywhere in the key also jumps, because a NULL never conflicts
    /// with anything in a UNIQUE index - which is why a unique column can hold
    /// any number of NULLs.
    NoConflict,
    /// `p1`: cursor, `p2`: destination register. The row's raw record bytes.
    RowData,
    /// `p1`: the extended result code, `p4`: the message. Stop, failing.
    HaltError,
    /// `p1`: database, `p3`: the new schema cookie.
    SetCookie,
    /// `p2`: destination register, `p3`: 0 for a table, 1 for an index.
    CreateBtree,
    /// `p1`: register holding the root page. Free every page of the tree.
    DestroyBtree,
    /// `p1`: register holding the root page. Empty the tree, keeping its root.
    ClearBtree,
    /// `p1`: register holding a rowid to add to the change count.
    ///
    /// `p2` of 1 also records the rowid as the last insert rowid. `p3` names
    /// the operation for the update hook - 0 delete, 1 insert, 2 update - and
    /// `p4` carries the table it happened to.
    CountChange,
}

impl Opcode {
    /// Returns whether the opcode writes to the database.
    ///
    /// The verifier refuses any opcode that says it does inside a program
    /// marked read-only, so this list is what makes `is_readonly()` a promise
    /// rather than a label the compiler attaches.
    pub fn writes(self) -> bool {
        matches!(
            self,
            Opcode::OpenWrite
                | Opcode::OpenWriteIndex
                | Opcode::InsertRow
                | Opcode::DeleteRow
                | Opcode::IdxInsert
                | Opcode::IdxDelete
                | Opcode::SetCookie
                | Opcode::CreateBtree
                | Opcode::DestroyBtree
                | Opcode::ClearBtree
        )
    }

    /// Returns whether `p2` is a jump target.
    pub fn jumps(self) -> bool {
        matches!(
            self,
            Opcode::Init
                | Opcode::Goto
                | Opcode::Gosub
                | Opcode::Rewind
                | Opcode::Last
                | Opcode::Next
                | Opcode::Prev
                | Opcode::SeekRowid
                | Opcode::SeekGe
                | Opcode::SeekGt
                | Opcode::IdxGe
                | Opcode::IdxGt
                | Opcode::If
                | Opcode::IfNot
                | Opcode::IfNull
                | Opcode::IfNotNull
                | Opcode::IfPos
                | Opcode::DecrJumpZero
                | Opcode::SorterSort
                | Opcode::SorterNext
                | Opcode::DistinctCheck
                | Opcode::NotExists
                | Opcode::NoConflict
        )
    }

    /// Returns the stable name used in `EXPLAIN` output.
    pub fn name(self) -> &'static str {
        match self {
            Opcode::Init => "Init",
            Opcode::Goto => "Goto",
            Opcode::Gosub => "Gosub",
            Opcode::Return => "Return",
            Opcode::Halt => "Halt",
            Opcode::Transaction => "Transaction",
            Opcode::OpenRead => "OpenRead",
            Opcode::OpenIndex => "OpenIndex",
            Opcode::Close => "Close",
            Opcode::Rewind => "Rewind",
            Opcode::Last => "Last",
            Opcode::Next => "Next",
            Opcode::Prev => "Prev",
            Opcode::SeekRowid => "SeekRowid",
            Opcode::SeekGe => "SeekGE",
            Opcode::SeekGt => "SeekGT",
            Opcode::IdxGe => "IdxGE",
            Opcode::IdxGt => "IdxGT",
            Opcode::IdxRowid => "IdxRowid",
            Opcode::Column => "Column",
            Opcode::IdxColumn => "IdxColumn",
            Opcode::Rowid => "Rowid",
            Opcode::Null => "Null",
            Opcode::Load => "Load",
            Opcode::Copy => "Copy",
            Opcode::Arithmetic => "Arithmetic",
            Opcode::Negate => "Negate",
            Opcode::BitNot => "BitNot",
            Opcode::Compare => "Compare",
            Opcode::Is => "Is",
            Opcode::And => "And",
            Opcode::Or => "Or",
            Opcode::Not => "Not",
            Opcode::IsNull => "IsNull",
            Opcode::InList => "InList",
            Opcode::If => "If",
            Opcode::IfNot => "IfNot",
            Opcode::IfNull => "IfNull",
            Opcode::IfNotNull => "IfNotNull",
            Opcode::IfPos => "IfPos",
            Opcode::DecrJumpZero => "DecrJumpZero",
            Opcode::Cast => "Cast",
            Opcode::ApplyAffinity => "Affinity",
            Opcode::Function => "Function",
            Opcode::Pattern => "Pattern",
            Opcode::AggStep => "AggStep",
            Opcode::AggFinal => "AggFinal",
            Opcode::AggReset => "AggReset",
            Opcode::SorterOpen => "SorterOpen",
            Opcode::OpenWrite => "OpenWrite",
            Opcode::OpenWriteIndex => "OpenWriteIndex",
            Opcode::NewRowid => "NewRowid",
            Opcode::MakeRecord => "MakeRecord",
            Opcode::InsertRow => "InsertRow",
            Opcode::DeleteRow => "DeleteRow",
            Opcode::IdxInsert => "IdxInsert",
            Opcode::IdxDelete => "IdxDelete",
            Opcode::NotExists => "NotExists",
            Opcode::NoConflict => "NoConflict",
            Opcode::RowData => "RowData",
            Opcode::HaltError => "HaltError",
            Opcode::SetCookie => "SetCookie",
            Opcode::CreateBtree => "CreateBtree",
            Opcode::DestroyBtree => "DestroyBtree",
            Opcode::ClearBtree => "ClearBtree",
            Opcode::CountChange => "CountChange",
            Opcode::SorterInsert => "SorterInsert",
            Opcode::SorterSort => "SorterSort",
            Opcode::SorterNext => "SorterNext",
            Opcode::SorterColumn => "SorterColumn",
            Opcode::DistinctOpen => "DistinctOpen",
            Opcode::DistinctCheck => "DistinctCheck",
            Opcode::ResultRow => "ResultRow",
        }
    }
}

/// A comparison, with everything it needs decided at compile time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Comparison {
    /// Which comparison.
    pub op: BinaryOp,
    /// The affinity applied to both sides first, when there is one.
    pub affinity: Option<Affinity>,
    /// The collation text is compared with.
    pub collation: Collation,
}

/// How a sorter orders one column of its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortColumn {
    /// Whether the column sorts descending.
    pub descending: bool,
    /// Whether NULLs sort first.
    pub nulls_first: bool,
    /// The collation text is compared with.
    pub collation: Collation,
}

/// A sorter's key: how many leading columns are keys and how each is ordered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SortKey {
    /// One entry per key column, in key order.
    pub columns: Vec<SortColumn>,
}

/// How an index's key columns are ordered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexKey {
    /// One entry per key column, in key order.
    pub columns: Vec<SortColumn>,
}

/// An aggregate call, with everything it needs decided at compile time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AggregateCall {
    /// Which aggregate.
    pub func: AggregateFunc,
    /// Whether `DISTINCT` was written.
    pub distinct: bool,
    /// The collation the aggregate compares with.
    pub collation: Collation,
}

/// The `p4` operand of an instruction.
#[derive(Clone, Debug, PartialEq)]
pub enum Operand {
    /// No operand.
    None,
    /// A literal NULL.
    Null,
    /// A literal integer.
    Integer(i64),
    /// A literal real.
    Real(f64),
    /// A literal text value.
    Text(Vec<u8>),
    /// A literal blob.
    Blob(Vec<u8>),
    /// A bound parameter, by one-based index.
    Parameter(u32),
    /// A comparison.
    Comparison(Comparison),
    /// An affinity.
    Affinity(Affinity),
    /// A scalar function, with the collation its comparisons use.
    Scalar(ScalarFunc, Collation),
    /// A pattern operator.
    Pattern(PatternOp),
    /// An aggregate call.
    Aggregate(AggregateCall),
    /// A sorter key.
    SortKey(SortKey),
    /// An index key.
    IndexKey(IndexKey),
    /// An arithmetic operator.
    Arithmetic(BinaryOp),
    /// A column count.
    Count(u32),
    /// The affinity of each column of a record, in column order.
    Affinities(Vec<Affinity>),
    /// A row change: which operation, and the table it happened to.
    Change(RowChangeKind, Vec<u8>),
}

/// What a row change did, as the update hook reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowChangeKind {
    /// A row was inserted.
    Insert,
    /// A row was updated in place.
    Update,
    /// A row was deleted.
    Delete,
}

impl RowChangeKind {
    /// Returns the operand number the compiler writes for this kind.
    pub fn as_operand(self) -> i32 {
        match self {
            RowChangeKind::Delete => 0,
            RowChangeKind::Insert => 1,
            RowChangeKind::Update => 2,
        }
    }

    /// Returns the kind an operand number names, defaulting to an update.
    pub fn from_operand(value: i32) -> RowChangeKind {
        match value {
            0 => RowChangeKind::Delete,
            1 => RowChangeKind::Insert,
            _ => RowChangeKind::Update,
        }
    }

    /// Returns the name SQLite's authorizer and hooks use.
    pub fn as_str(self) -> &'static str {
        match self {
            RowChangeKind::Insert => "INSERT",
            RowChangeKind::Update => "UPDATE",
            RowChangeKind::Delete => "DELETE",
        }
    }
}

/// One row change, as the update hook sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RowChange {
    /// What happened to the row.
    pub kind: RowChangeKind,
    /// The table it happened to.
    pub table: Vec<u8>,
    /// The rowid of the row.
    pub rowid: i64,
}

/// One instruction.
#[derive(Clone, Debug, PartialEq)]
pub struct Instruction {
    /// What to do.
    pub opcode: Opcode,
    /// The first operand, usually a register or cursor.
    pub p1: i32,
    /// The second operand, usually a register or a jump target.
    pub p2: i32,
    /// The third operand.
    pub p3: i32,
    /// The typed operand.
    pub p4: Operand,
    /// Flags.
    pub p5: u16,
}

impl Instruction {
    /// Returns an instruction with no typed operand and no flags.
    pub fn new(opcode: Opcode, p1: i32, p2: i32, p3: i32) -> Instruction {
        Instruction {
            opcode,
            p1,
            p2,
            p3,
            p4: Operand::None,
            p5: 0,
        }
    }

    /// Returns the instruction with a typed operand attached.
    pub fn with_p4(mut self, operand: Operand) -> Instruction {
        self.p4 = operand;
        self
    }

    /// Returns the instruction with flags attached.
    pub fn with_p5(mut self, flags: u16) -> Instruction {
        self.p5 = flags;
        self
    }
}

/// One column of a statement's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultColumn {
    /// The name the column reports.
    pub name: Vec<u8>,
    /// The database, table and column it came from, when it came from one.
    pub origin: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
    /// The declared type it reports.
    pub declared_type: Vec<u8>,
}

/// What a program depends on, for invalidation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgramDependencies {
    /// The `(database index, schema cookie)` pairs it was compiled against.
    pub schemas: Vec<(usize, u32)>,
    /// The catalog generation it was compiled against.
    pub generation: u64,
}

/// A compiled program.
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    /// The instructions, in order.
    pub instructions: Vec<Instruction>,
    /// How many registers the machine must allocate.
    pub register_count: u32,
    /// How many cursors the machine must allocate.
    pub cursor_count: u32,
    /// How many sorters the machine must allocate.
    pub sorter_count: u32,
    /// How many distinct sets the machine must allocate.
    pub distinct_count: u32,
    /// How many aggregate accumulators the machine must allocate.
    pub aggregate_count: u32,
    /// The result columns, in order.
    pub result_columns: Vec<ResultColumn>,
    /// What the program depends on.
    pub dependencies: ProgramDependencies,
    /// Whether the program writes.
    pub readonly: bool,
    /// The highest parameter index the statement uses.
    pub parameter_count: u32,
}

impl Program {
    /// Returns the instruction at an address.
    pub fn instruction(&self, address: usize) -> Option<&Instruction> {
        self.instructions.get(address)
    }

    /// Renders the program the way `EXPLAIN` does.
    pub fn explain(&self) -> Vec<String> {
        self.instructions
            .iter()
            .enumerate()
            .map(|(address, instruction)| {
                format!(
                    "{address}\t{}\t{}\t{}\t{}\t{}",
                    instruction.opcode.name(),
                    instruction.p1,
                    instruction.p2,
                    instruction.p3,
                    instruction.p5
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every jumping opcode has to say so, because the verifier only checks
    /// `p2` on the opcodes that claim to jump. An opcode that jumps without
    /// declaring it would have an unchecked target.
    #[test]
    fn the_jumping_opcodes_declare_themselves() {
        assert!(Opcode::Goto.jumps());
        assert!(Opcode::Rewind.jumps());
        assert!(Opcode::DecrJumpZero.jumps());
        assert!(!Opcode::Column.jumps());
        assert!(!Opcode::ResultRow.jumps());
    }

    /// Every opcode that touches the database declares that it writes, and no
    /// opcode that only reads does.
    ///
    /// The list is spelled out rather than derived, because the whole value of
    /// `readonly` is that it is checked against something independent of the
    /// compiler that set it.
    #[test]
    fn the_writing_opcodes_declare_themselves() {
        for opcode in [
            Opcode::OpenWrite,
            Opcode::OpenWriteIndex,
            Opcode::InsertRow,
            Opcode::DeleteRow,
            Opcode::IdxInsert,
            Opcode::IdxDelete,
            Opcode::SetCookie,
            Opcode::CreateBtree,
            Opcode::DestroyBtree,
            Opcode::ClearBtree,
        ] {
            assert!(
                opcode.writes(),
                "{} does not declare its writes",
                opcode.name()
            );
        }
        for opcode in [
            Opcode::Init,
            Opcode::Column,
            Opcode::ResultRow,
            Opcode::SorterInsert,
            Opcode::OpenRead,
            Opcode::NewRowid,
            Opcode::MakeRecord,
            Opcode::RowData,
            Opcode::NotExists,
            Opcode::NoConflict,
            Opcode::CountChange,
        ] {
            assert!(
                !opcode.writes(),
                "{} claims a write it does not make",
                opcode.name()
            );
        }
    }
}
