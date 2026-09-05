//! The bytecode: instructions, operands, and the program that holds them.
//!
//! Invariant: a program is immutable once compiled and describes everything it
//! will do. Its register count, cursor count and result metadata are fixed, its
//! jumps resolve to instructions inside it, and a read-only program contains no
//! opcode that writes. The verifier proves all of that before the machine runs
//! a single instruction, so the machine itself never has to ask.
//!
//! Opcode numbers are inillucent's own. Nothing persists a program, so there is no
//! compatibility promise here; the only contract is with the verifier and the
//! machine in this crate.

use inillucent_sql::ast::{BinaryOp, FrameExclude, FrameUnit, PatternOp};
use inillucent_sql::function::{
    AggregateFunc, JsonFunc, MathFunc, ScalarFunc, TimeFunc, WindowFunc,
};
use inillucent_value::{Affinity, Collation};

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
    /// `p1`: cursor, `p2`: jump when no row is at or before the key,
    /// `p3`: first key register, `p5`: key column count.
    ///
    /// The mirror of [`Opcode::SeekGe`], for a walk that runs backwards. A
    /// descending `ORDER BY` is the same B-tree read from the other end, and
    /// without a way to land on the last entry at or before a bound the walk
    /// would have to start at the end of the table and step back to it.
    SeekLe,
    /// As [`Opcode::SeekLe`], but strictly before.
    SeekLt,
    /// `p1`: cursor, `p2`: jump when the entry is past the key's upper bound,
    /// `p3`: first key register, `p5`: key column count.
    IdxGe,
    /// As [`Opcode::IdxGe`], but strictly after.
    IdxGt,
    /// `p1`: cursor, `p2`: jump when the entry is past the key's lower bound,
    /// `p3`: first key register, `p5`: key column count.
    ///
    /// The mirror of [`Opcode::IdxGe`]: the stopping test of a backward walk,
    /// which ends at the *low* end of the range rather than the high one.
    IdxLe,
    /// As [`Opcode::IdxLe`], but strictly before.
    IdxLt,
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
    /// `p1`: first argument register, `p2`: argument count, `p3`: destination,
    /// `p4`: the math function.
    MathCall,
    /// `p1`: first argument register, `p2`: argument count, `p3`: destination,
    /// `p4`: the date or time function.
    TimeCall,
    /// `p1`: first argument register, `p2`: argument count, `p3`: destination,
    /// `p4`: the JSON function.
    ///
    /// Its own opcode rather than a `Function` with a different tag because it
    /// is the one call that reads and writes the JSON mark on a register, and
    /// the one that can fail.
    JsonCall,
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
    /// `p1`: cursor. Make every column of the cursor read as NULL.
    ///
    /// This is how an outer join emits its unmatched row: rather than a second
    /// copy of the body that knows to substitute NULLs, the cursor is put into
    /// a state where every `Column` off it answers NULL and the same body runs.
    NullRow,
    /// `p1`: store, `p2`: column count, `p4`: the key when `p5` is 1.
    ///
    /// Opens an ephemeral row store. With `p5` of 1 it keeps an ordered index
    /// and can be probed and de-duplicated; without, it is append-and-scan.
    EphOpen,
    /// `p1`: store, `p2`: first register, `p3`: count. Append a row.
    EphInsert,
    /// `p1`: store, `p2`: jump when an equal row is already present,
    /// `p3`: first register, `p5`: count.
    EphInsertUnique,
    /// `p1`: store, `p2`: jump when the store is empty.
    EphRewind,
    /// `p1`: store, `p2`: jump when another row exists.
    EphNext,
    /// `p1`: store, `p2`: column, `p3`: destination register.
    EphColumn,
    /// `p1`: store, `p2`: jump when an equal row is present, `p3`: first
    /// register, `p5`: count.
    EphFound,
    /// `p1`: store, `p2`: jump when no equal row is present, `p3`: first
    /// register, `p5`: count.
    EphNotFound,
    /// `p1`: store, `p2`: jump when an equal row was present and removed,
    /// `p3`: first register, `p5`: count.
    EphRemove,
    /// `p1`: store. Forget every row, keeping the store open.
    EphClear,
    /// `p1`: store. Keep the first of every group of equal rows.
    EphDedup,
    /// `p1`: store, `p2`: destination register. Whether the store holds a NULL.
    EphSawNull,
    /// `p1`: store, `p4`: the sort key. Order the store's rows in place.
    EphSort,
    /// `p1`: register, `p4`: the column's type and name. Fail on a bad type.
    ///
    /// A `STRICT` table refuses a value whose storage class its column does not
    /// declare. The affinity has already been applied by the time this runs, so
    /// `'123'` in an `INT` column is an integer and passes, and `'abc'` is
    /// still text and does not - which is exactly the line SQLite draws.
    TypeCheck,
    /// `p1`: store, `p4`: the window pass. Append each row's window values.
    ///
    /// The store must already be sorted by the partition keys and then by the
    /// window's own `ORDER BY`, which is what `EphSort` immediately before it
    /// is for: the operator reads partitions and peer groups off the order
    /// rather than re-deriving them.
    Window,
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
    /// `p1`: register, `p2`: 0 to read the last insert rowid into it, 1 to
    /// write it back from it.
    ///
    /// A trigger body's own inserts are visible to `last_insert_rowid()` while
    /// the body runs and not afterwards, which SQLite gets from the frame it
    /// pushes. Trigger bodies are inlined here, so the save and the restore are
    /// emitted around the body instead.
    LastRowid,
    /// `p1`: table cursor, `p2`: `sqlite_sequence`'s root, `p3`: destination
    /// register, `p4`: the table's name.
    ///
    /// The rowid an `AUTOINCREMENT` table's next row gets: one more than the
    /// largest it has ever handed out, which is the larger of the value
    /// `sqlite_sequence` remembers and the largest rowid still in the table.
    /// An ordinary table reuses the numbers its deleted rows had; this is the
    /// whole difference, and it is why the number has to be remembered
    /// somewhere the rows are not.
    SeqRowid,
    /// `p1`: register holding a rowid just written, `p2`: `sqlite_sequence`'s
    /// root, `p4`: the table's name.
    ///
    /// Raises the remembered value when the row that was written went past it.
    /// It runs for an explicit rowid too: `sqlite_sequence` holds the largest
    /// ever used, not the largest this statement generated.
    SeqUpdate,
    /// `p1`: cursor, `p4`: which virtual table. Opens a module's cursor.
    VOpen,
    /// `p1`: cursor, `p2`: jump when the module produces no rows,
    /// `p3`: first argument register, `p5`: argument count,
    /// `p4`: the plan `best_index` chose.
    ///
    /// It is a jump like `Rewind`, and for the same reason: a module that
    /// produces nothing must skip the loop body rather than run it once on an
    /// unpositioned cursor.
    VFilter,
    /// `p1`: cursor, `p2`: jump when another row exists.
    VNext,
    /// `p1`: cursor, `p2`: column, `p3`: destination register.
    VColumn,
    /// `p1`: cursor, `p2`: destination register.
    VRowid,
    /// `p1`: first argument register, `p2`: how many, `p3`: destination
    /// register, `p4`: the function's folded name.
    ///
    /// A call to a function an application registered. The machine looks the
    /// name up in the table the connection handed it, which is why nothing
    /// about what the function does reaches the program.
    ExtCall,
    /// `p1`: cursor, `p2`: first argument register, `p3`: destination
    /// register, `p4`: the function's folded name, `p5`: argument count.
    ///
    /// One of a module's auxiliary functions - `bm25(docs)` and its cousins.
    /// It reads the cursor's current row, so it is the module rather than the
    /// value stack that answers, and a module that does not know the name
    /// refuses it rather than returning null.
    VAux,
    /// `p1`: first register of the change, `p2`: how many, `p3`: destination
    /// for the rowid an insert allocated, `p4`: which virtual table.
    ///
    /// The register block is SQLite's `xUpdate` argument vector: the old rowid,
    /// the new rowid, then one value per declared column. A block of one is a
    /// delete, and that is the whole encoding of the three operations.
    VUpdate,
    /// `p4`: which virtual table. Starts the module's transaction.
    VBegin,
    /// `p4`: which virtual table. Flushes the module before the commit.
    VSync,
    /// `p4`: which virtual table. Ends the module's transaction.
    VCommit,
    /// `p4`: which virtual table. Abandons the module's transaction.
    VRollback,
    /// `p1`: the savepoint number, `p3`: 0 to open, 1 to release, 2 to roll
    /// back to it, `p4`: which virtual table.
    VSavepoint,
}

impl Opcode {
    /// Returns whether the opcode reaches a virtual table's module.
    ///
    /// These are dispatched apart from the rest because they need the whole
    /// host - the module registry and the connected tables - rather than only
    /// the pagers, and borrowing the host for every instruction would mean no
    /// instruction could reach the pagers at all.
    pub fn is_virtual(self) -> bool {
        matches!(
            self,
            Opcode::VOpen
                | Opcode::VFilter
                | Opcode::VNext
                | Opcode::VColumn
                | Opcode::VRowid
                | Opcode::VAux
                | Opcode::VUpdate
                | Opcode::VBegin
                | Opcode::VSync
                | Opcode::VCommit
                | Opcode::VRollback
                | Opcode::VSavepoint
        )
    }
}

/// Which virtual table an instruction is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualRef {
    /// Which attached database the table lives in.
    pub database: usize,
    /// The table's name, or the module's name for an eponymous one.
    pub table: Vec<u8>,
    /// The module and the arguments its `CREATE` gave it.
    pub module: inillucent_sql::vtab::ModuleRef,
}

/// The plan `best_index` chose, as the program carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualPlan {
    /// The plan number.
    pub index_number: i32,
    /// The plan string.
    pub index_string: String,
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
                | Opcode::VUpdate
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
                | Opcode::SeekLe
                | Opcode::SeekLt
                | Opcode::IdxGe
                | Opcode::IdxGt
                | Opcode::IdxLe
                | Opcode::IdxLt
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
                | Opcode::EphInsertUnique
                | Opcode::EphRewind
                | Opcode::EphNext
                | Opcode::EphFound
                | Opcode::EphNotFound
                | Opcode::EphRemove
                | Opcode::VFilter
                | Opcode::VNext
        )
    }

    /// Returns the stable name used in `EXPLAIN` output.
    /// Every opcode, in declaration order.
    ///
    /// A profile records an opcode's discriminant, because that is what indexes
    /// an array cheaply on the hot path; turning one back into a name needs a
    /// list, and a derived one would be another dependency for a table that
    /// changes when somebody adds an instruction and the compiler will not
    /// notice. The exhaustive match in `name()` is what keeps this honest: a
    /// new variant fails to compile there first.
    pub const ALL: [Opcode; 112] = [
        Opcode::Init,
        Opcode::Goto,
        Opcode::Gosub,
        Opcode::Return,
        Opcode::Halt,
        Opcode::Transaction,
        Opcode::OpenRead,
        Opcode::OpenIndex,
        Opcode::Close,
        Opcode::Rewind,
        Opcode::Last,
        Opcode::Next,
        Opcode::Prev,
        Opcode::SeekRowid,
        Opcode::SeekGe,
        Opcode::SeekGt,
        Opcode::SeekLe,
        Opcode::SeekLt,
        Opcode::IdxGe,
        Opcode::IdxGt,
        Opcode::IdxLe,
        Opcode::IdxLt,
        Opcode::IdxRowid,
        Opcode::Column,
        Opcode::IdxColumn,
        Opcode::Rowid,
        Opcode::Null,
        Opcode::Load,
        Opcode::Copy,
        Opcode::Arithmetic,
        Opcode::Negate,
        Opcode::BitNot,
        Opcode::Compare,
        Opcode::Is,
        Opcode::And,
        Opcode::Or,
        Opcode::Not,
        Opcode::IsNull,
        Opcode::InList,
        Opcode::If,
        Opcode::IfNot,
        Opcode::IfNull,
        Opcode::IfNotNull,
        Opcode::IfPos,
        Opcode::DecrJumpZero,
        Opcode::Cast,
        Opcode::ApplyAffinity,
        Opcode::Function,
        Opcode::Pattern,
        Opcode::MathCall,
        Opcode::TimeCall,
        Opcode::JsonCall,
        Opcode::AggStep,
        Opcode::AggFinal,
        Opcode::AggReset,
        Opcode::SorterOpen,
        Opcode::SorterInsert,
        Opcode::SorterSort,
        Opcode::SorterNext,
        Opcode::SorterColumn,
        Opcode::NullRow,
        Opcode::EphOpen,
        Opcode::EphInsert,
        Opcode::EphInsertUnique,
        Opcode::EphRewind,
        Opcode::EphNext,
        Opcode::EphColumn,
        Opcode::EphFound,
        Opcode::EphNotFound,
        Opcode::EphRemove,
        Opcode::EphClear,
        Opcode::EphDedup,
        Opcode::EphSawNull,
        Opcode::EphSort,
        Opcode::TypeCheck,
        Opcode::Window,
        Opcode::DistinctOpen,
        Opcode::DistinctCheck,
        Opcode::ResultRow,
        Opcode::OpenWrite,
        Opcode::OpenWriteIndex,
        Opcode::NewRowid,
        Opcode::MakeRecord,
        Opcode::InsertRow,
        Opcode::DeleteRow,
        Opcode::IdxInsert,
        Opcode::IdxDelete,
        Opcode::NotExists,
        Opcode::NoConflict,
        Opcode::RowData,
        Opcode::HaltError,
        Opcode::SetCookie,
        Opcode::CreateBtree,
        Opcode::DestroyBtree,
        Opcode::ClearBtree,
        Opcode::CountChange,
        Opcode::LastRowid,
        Opcode::SeqRowid,
        Opcode::SeqUpdate,
        Opcode::VOpen,
        Opcode::VFilter,
        Opcode::VNext,
        Opcode::VColumn,
        Opcode::VRowid,
        Opcode::ExtCall,
        Opcode::VAux,
        Opcode::VUpdate,
        Opcode::VBegin,
        Opcode::VSync,
        Opcode::VCommit,
        Opcode::VRollback,
        Opcode::VSavepoint,
    ];

    /// Returns the opcode with a given discriminant, for a profile that
    /// recorded the number rather than the name.
    ///
    /// The table is walked rather than transmuted: this crate forbids unsafe
    /// code, and a lookup that runs once per row of a *report* does not need to
    /// be fast.
    pub fn from_index(index: usize) -> Option<Opcode> {
        Opcode::ALL
            .iter()
            .copied()
            .find(|opcode| *opcode as usize == index)
    }

    /// Returns the opcode's name, as `EXPLAIN` and a profile print it.
    ///
    /// The match is exhaustive on purpose: it is what makes `ALL` above stay
    /// complete, because a new variant fails to compile here first.
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
            Opcode::SeekLe => "SeekLE",
            Opcode::SeekLt => "SeekLT",
            Opcode::IdxGe => "IdxGE",
            Opcode::IdxGt => "IdxGT",
            Opcode::IdxLe => "IdxLE",
            Opcode::IdxLt => "IdxLT",
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
            Opcode::NullRow => "NullRow",
            Opcode::EphOpen => "OpenEphemeral",
            Opcode::EphInsert => "EphInsert",
            Opcode::EphInsertUnique => "EphInsertUnique",
            Opcode::EphRewind => "EphRewind",
            Opcode::EphNext => "EphNext",
            Opcode::EphColumn => "EphColumn",
            Opcode::EphFound => "EphFound",
            Opcode::EphNotFound => "EphNotFound",
            Opcode::EphRemove => "EphRemove",
            Opcode::EphClear => "EphClear",
            Opcode::EphDedup => "EphDedup",
            Opcode::EphSawNull => "EphSawNull",
            Opcode::EphSort => "EphSort",
            Opcode::TypeCheck => "TypeCheck",
            Opcode::Window => "Window",
            Opcode::If => "If",
            Opcode::IfNot => "IfNot",
            Opcode::IfNull => "IfNull",
            Opcode::IfNotNull => "IfNotNull",
            Opcode::IfPos => "IfPos",
            Opcode::DecrJumpZero => "DecrJumpZero",
            Opcode::Cast => "Cast",
            Opcode::ApplyAffinity => "Affinity",
            Opcode::Function => "Function",
            Opcode::JsonCall => "JsonCall",
            Opcode::VOpen => "VOpen",
            Opcode::VFilter => "VFilter",
            Opcode::VNext => "VNext",
            Opcode::VColumn => "VColumn",
            Opcode::ExtCall => "ExtCall",
            Opcode::VRowid => "VRowid",
            Opcode::VAux => "VAux",
            Opcode::VUpdate => "VUpdate",
            Opcode::VBegin => "VBegin",
            Opcode::VSync => "VSync",
            Opcode::VCommit => "VCommit",
            Opcode::VRollback => "VRollback",
            Opcode::VSavepoint => "VSavepoint",
            Opcode::Pattern => "Pattern",
            Opcode::MathCall => "Function",
            Opcode::TimeCall => "Function",
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
            Opcode::LastRowid => "LastRowid",
            Opcode::SeqRowid => "SeqRowid",
            Opcode::SeqUpdate => "SeqUpdate",
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregateCall {
    /// Which aggregate.
    pub func: AggregateFunc,
    /// The name, when the aggregate is one an application registered.
    ///
    /// This is what stops the struct being `Copy`, and it is worth it: an
    /// application's aggregate is named at run time, and an id would have to be
    /// allocated somewhere that outlives the program.
    pub external: Option<Vec<u8>>,
    /// Whether `DISTINCT` was written.
    pub distinct: bool,
    /// The collation the aggregate compares with.
    pub collation: Collation,
}

/// The six types a `STRICT` table's column may declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrictType {
    /// `INT` or `INTEGER`.
    Int,
    /// `REAL`.
    Real,
    /// `TEXT`.
    Text,
    /// `BLOB`.
    Blob,
    /// `ANY`, which stores whatever it is given with no affinity applied.
    Any,
}

impl StrictType {
    /// Returns the type a declared name spells, when it is one of the six.
    pub fn of(declared: &[u8]) -> Option<StrictType> {
        let folded = declared.to_ascii_uppercase();
        let kind = match folded.as_slice() {
            b"INT" | b"INTEGER" => StrictType::Int,
            b"REAL" => StrictType::Real,
            b"TEXT" => StrictType::Text,
            b"BLOB" => StrictType::Blob,
            b"ANY" => StrictType::Any,
            _ => return None,
        };
        Some(kind)
    }

    /// Returns the name the error message uses.
    pub fn as_str(self) -> &'static str {
        match self {
            StrictType::Int => "INT",
            StrictType::Real => "REAL",
            StrictType::Text => "TEXT",
            StrictType::Blob => "BLOB",
            StrictType::Any => "ANY",
        }
    }
}

/// Which family a window call belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowSlot {
    /// An aggregate over the frame.
    Aggregate(AggregateFunc),
    /// One of the eleven functions that only exist in a window.
    Plain(WindowFunc),
}

/// One end of a frame, as the machine reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameEnd {
    /// `UNBOUNDED PRECEDING`.
    UnboundedPreceding,
    /// `CURRENT ROW`.
    CurrentRow,
    /// `UNBOUNDED FOLLOWING`.
    UnboundedFollowing,
    /// `expr PRECEDING` or `expr FOLLOWING`.
    Offset {
        /// The record column holding the offset, evaluated once per row.
        column: usize,
        /// Whether the offset counts backwards.
        preceding: bool,
    },
}

/// A frame specification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowFrame {
    /// `ROWS`, `RANGE` or `GROUPS`.
    pub unit: FrameUnit,
    /// The start.
    pub start: FrameEnd,
    /// The end.
    pub end: FrameEnd,
    /// The `EXCLUDE` clause.
    pub exclude: FrameExclude,
}

/// One window call, addressed entirely by record column numbers.
///
/// Nothing here is an expression. Every value the call needs - its arguments,
/// its `FILTER`, its frame offsets - was computed into the record while the
/// rows were being collected, so the operator reads values rather than
/// evaluating anything, and cannot reach a cursor that has since moved.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowCall {
    /// What it computes.
    pub func: WindowSlot,
    /// Whether `DISTINCT` was written.
    pub distinct: bool,
    /// The collation its comparisons use.
    pub collation: Collation,
    /// The record columns holding its arguments.
    pub arguments: Vec<usize>,
    /// The record column holding its `FILTER` value.
    pub filter: Option<usize>,
    /// The record columns holding the window's `ORDER BY` values, with the
    /// rules each is ordered by.
    pub order: Vec<(usize, SortColumn)>,
    /// The frame.
    pub frame: WindowFrame,
}

/// Everything one window pass needs.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowPlan {
    /// The record columns holding the partition keys, with their collations.
    pub partition: Vec<(usize, Collation)>,
    /// The calls, in the order their values are appended to each row.
    pub calls: Vec<WindowCall>,
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
    /// A math function.
    Math(MathFunc),
    /// A date or time function.
    Time(TimeFunc),
    /// A JSON function.
    Json(JsonFunc),
    /// Which virtual table an instruction is about.
    Virtual(Box<VirtualRef>),
    /// The plan a virtual scan runs.
    VirtualPlan(Box<VirtualPlan>),
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
    /// A window pass.
    Window(Box<WindowPlan>),
    /// A `STRICT` column's declared type and its qualified name.
    Strict(StrictType, Vec<u8>),
    /// A sort that names the columns it orders by.
    ///
    /// The ordinary sort key compares column `i` of the key against column `i`
    /// of the row, which is right for a sorter whose record was built for it.
    /// A window record is built once and sorted several times, by different
    /// columns each time, so its sort has to name them.
    SortOn(Vec<(usize, SortColumn)>),
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
    /// Which planner optimizations were switched *off* when it was compiled.
    ///
    /// This belongs with the schema cookie rather than beside it: a program is
    /// only reusable while everything it was compiled against still holds, and
    /// the arm it was planned under is one of those things. A cached plan built
    /// with covering indexes disabled is not the plan the next statement wants
    /// once they are enabled again, and a cache that compared only the schema
    /// would hand it over anyway.
    pub levers: u32,
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
    /// How many ephemeral row stores the machine must allocate.
    pub ephemeral_count: u32,
    /// How many aggregate accumulators the machine must allocate.
    pub aggregate_count: u32,
    /// The result columns, in order.
    pub result_columns: Vec<ResultColumn>,
    /// What the program depends on.
    pub dependencies: ProgramDependencies,
    /// Whether the program writes.
    pub readonly: bool,
    /// Which planner optimizations this program actually used.
    ///
    /// The counter the A/B arms are read against. "The covering-index lever is
    /// on" is a setting; "this statement used it" is an observation, and only
    /// the second one can show that an arm did anything. A workload whose
    /// programs report the same mask under both arms measured nothing, however
    /// different the two timings came out.
    pub optimizations_used: u32,
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
