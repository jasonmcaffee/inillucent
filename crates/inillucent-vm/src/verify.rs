//! The bytecode verifier.
//!
//! Invariant: the machine runs no program the verifier has not accepted, and
//! the verifier accepts nothing that could make the machine index out of
//! bounds, jump outside the program, read a register that was never written,
//! use a cursor as the wrong kind, or write in a read-only program.
//!
//! This exists because the compiler is not trusted. A compiler bug that emits a
//! jump one past the end, or reads register 40 out of a 39-register frame, is a
//! wrong answer or a panic at run time and a rejected program here — and the
//! difference between those two is the difference between a bug report and a
//! silent corruption.
//!
//! The register check is a definite-assignment pass over the control-flow
//! graph. It is deliberately conservative: a register is "written" only when
//! every path into an instruction has written it, so a register written on one
//! arm of a branch and read after the join is rejected. The compiler never does
//! that, and requiring it to prove so is cheaper than a dataflow lattice.
//!
//! The set of written registers is a bitmap rather than a `BTreeSet`. That is
//! not a micro-optimisation: the pass merges a set at every address on every
//! round of the fixed-point, and with a tree that was 4,795 allocations to
//! verify one ordinary statement - most of the cost of `prepare`.

use crate::program::{Opcode, Operand, Program};

/// Why a program was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyError {
    /// The instruction the problem is at.
    pub address: usize,
    /// What is wrong.
    pub reason: String,
}

impl VerifyError {
    /// Returns a failure at an address.
    fn at(address: usize, reason: impl Into<String>) -> VerifyError {
        VerifyError {
            address,
            reason: reason.into(),
        }
    }
}

/// What kind of thing a cursor is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorKind {
    Table,
    Index,
}

/// Checks a program, returning every problem found.
pub fn verify(program: &Program) -> Vec<VerifyError> {
    let mut problems = Vec::new();
    check_shape(program, &mut problems);
    check_cursors(program, &mut problems);
    check_registers(program, &mut problems);
    problems
}

/// Checks jump targets, operand ranges, and the read-only claim.
fn check_shape(program: &Program, problems: &mut Vec<VerifyError>) {
    let length = program.instructions.len();
    if length == 0 {
        problems.push(VerifyError::at(0, "a program with no instructions"));
        return;
    }
    if program
        .instructions
        .first()
        .is_none_or(|instruction| instruction.opcode != Opcode::Init)
    {
        problems.push(VerifyError::at(
            0,
            "a program that does not begin with Init",
        ));
    }
    if !program
        .instructions
        .iter()
        .any(|instruction| instruction.opcode == Opcode::Halt)
    {
        problems.push(VerifyError::at(0, "a program with no Halt"));
    }
    for (address, instruction) in program.instructions.iter().enumerate() {
        if program.readonly && instruction.opcode.writes() {
            problems.push(VerifyError::at(
                address,
                format!(
                    "{} writes in a read-only program",
                    instruction.opcode.name()
                ),
            ));
        }
        if instruction.opcode.jumps() && (instruction.p2 < 0 || instruction.p2 as usize > length) {
            problems.push(VerifyError::at(
                address,
                format!("jump target {} is outside the program", instruction.p2),
            ));
        }
        check_operand_ranges(program, address, problems);
        check_store_range(program, address, problems);
    }
}

/// Checks that every ephemeral-store operand is one the program declared.
///
/// The stores are a separate numbering space from the cursors: a program with
/// one cursor and three stores is ordinary, and checking a store number against
/// the cursor count would either reject it or, worse, accept a store number
/// that indexes past the end of the store vector.
fn check_store_range(program: &Program, address: usize, problems: &mut Vec<VerifyError>) {
    let Some(instruction) = program.instructions.get(address) else {
        return;
    };
    let uses_store = matches!(
        instruction.opcode,
        Opcode::EphOpen
            | Opcode::EphInsert
            | Opcode::EphInsertUnique
            | Opcode::EphRewind
            | Opcode::EphNext
            | Opcode::EphColumn
            | Opcode::EphFound
            | Opcode::EphNotFound
            | Opcode::EphRemove
            | Opcode::EphClear
            | Opcode::EphDedup
            | Opcode::EphSawNull
            | Opcode::EphSort
            | Opcode::Window
    );
    if !uses_store {
        return;
    }
    let count = program.ephemeral_count as i32;
    if instruction.p1 < 0 || instruction.p1 >= count {
        problems.push(VerifyError::at(
            address,
            format!(
                "{} uses store {}, which is outside the {count} declared",
                instruction.opcode.name(),
                instruction.p1
            ),
        ));
    }
}

/// Checks that every register and cursor operand is inside its frame.
fn check_operand_ranges(program: &Program, address: usize, problems: &mut Vec<VerifyError>) {
    let Some(instruction) = program.instructions.get(address) else {
        return;
    };
    let mut registers: Vec<(i32, &str)> = Vec::new();
    let mut blocks: Vec<(i32, i32)> = Vec::new();
    match instruction.opcode {
        Opcode::Column | Opcode::IdxColumn => registers.push((instruction.p3, "destination")),
        Opcode::Rowid | Opcode::IdxRowid | Opcode::Null | Opcode::Load => {
            registers.push((instruction.p2, "destination"))
        }
        Opcode::Copy
        | Opcode::Not
        | Opcode::Negate
        | Opcode::BitNot
        | Opcode::Cast
        | Opcode::IsNull => {
            registers.push((instruction.p1, "source"));
            registers.push((instruction.p2, "destination"));
        }
        Opcode::Arithmetic | Opcode::Compare | Opcode::Is | Opcode::And | Opcode::Or => {
            registers.push((instruction.p1, "left"));
            registers.push((instruction.p2, "right"));
            registers.push((instruction.p3, "destination"));
        }
        Opcode::If
        | Opcode::IfNot
        | Opcode::IfNull
        | Opcode::IfNotNull
        | Opcode::IfPos
        | Opcode::DecrJumpZero
        | Opcode::Gosub
        | Opcode::Return => registers.push((instruction.p1, "operand")),
        Opcode::SeekRowid => registers.push((instruction.p3, "key")),
        Opcode::SeekGe
        | Opcode::SeekGt
        | Opcode::SeekLe
        | Opcode::SeekLt
        | Opcode::IdxGe
        | Opcode::IdxGt
        | Opcode::IdxLe
        | Opcode::IdxLt => blocks.push((instruction.p3, i32::from(instruction.p5))),
        Opcode::ResultRow | Opcode::ApplyAffinity => blocks.push((instruction.p1, instruction.p2)),
        Opcode::VFilter => blocks.push((instruction.p3, i32::from(instruction.p5))),
        Opcode::VColumn => registers.push((instruction.p3, "destination")),
        Opcode::VAux => {
            registers.push((instruction.p3, "destination"));
            for offset in 0..i32::from(instruction.p5) {
                registers.push((instruction.p2 + offset, "argument"));
            }
        }
        Opcode::VRowid => registers.push((instruction.p2, "destination")),
        Opcode::VUpdate => {
            blocks.push((instruction.p1, instruction.p2));
            if instruction.p3 >= 0 {
                registers.push((instruction.p3, "destination"));
            }
        }
        Opcode::Function
        | Opcode::Pattern
        | Opcode::MathCall
        | Opcode::TimeCall
        | Opcode::JsonCall
        | Opcode::ExtCall => {
            blocks.push((instruction.p1, instruction.p2));
            registers.push((instruction.p3, "destination"));
        }
        Opcode::AggStep => blocks.push((instruction.p1, instruction.p2)),
        Opcode::InList => {
            registers.push((instruction.p1, "operand"));
            blocks.push((instruction.p2, i32::from(instruction.p5)));
            registers.push((instruction.p3, "destination"));
        }
        Opcode::AggFinal => registers.push((instruction.p2, "destination")),
        Opcode::SorterInsert => blocks.push((instruction.p2, instruction.p3)),
        Opcode::SorterColumn => registers.push((instruction.p3, "destination")),
        Opcode::DistinctCheck | Opcode::NoConflict => {
            blocks.push((instruction.p3, i32::from(instruction.p5)))
        }
        Opcode::EphInsert => blocks.push((instruction.p2, instruction.p3)),
        Opcode::EphInsertUnique | Opcode::EphFound | Opcode::EphNotFound | Opcode::EphRemove => {
            blocks.push((instruction.p3, i32::from(instruction.p5)))
        }
        Opcode::EphColumn => registers.push((instruction.p3, "destination")),
        Opcode::EphSawNull => registers.push((instruction.p2, "destination")),
        Opcode::NewRowid | Opcode::RowData | Opcode::CreateBtree => {
            registers.push((instruction.p2, "destination"))
        }
        Opcode::MakeRecord => {
            blocks.push((instruction.p1, instruction.p2));
            registers.push((instruction.p3, "destination"));
        }
        Opcode::InsertRow => {
            registers.push((instruction.p2, "record"));
            registers.push((instruction.p3, "rowid"));
        }
        Opcode::IdxInsert | Opcode::IdxDelete => registers.push((instruction.p2, "record")),
        Opcode::NotExists => registers.push((instruction.p3, "rowid")),
        Opcode::DestroyBtree | Opcode::ClearBtree | Opcode::CountChange | Opcode::LastRowid => {
            registers.push((instruction.p1, "operand"))
        }
        Opcode::SeqRowid => registers.push((instruction.p3, "rowid")),
        Opcode::SeqUpdate => registers.push((instruction.p1, "rowid")),
        _ => {}
    }
    let count = program.register_count as i32;
    for (value, what) in registers {
        if value < 0 || value >= count {
            problems.push(VerifyError::at(
                address,
                format!("{what} register {value} is outside the {count}-register frame"),
            ));
        }
    }
    for (first, length) in blocks {
        check_block(program, address, first, length, problems);
    }
}

/// Checks that a contiguous block of registers is inside the frame.
fn check_block(
    program: &Program,
    address: usize,
    first: i32,
    count: i32,
    problems: &mut Vec<VerifyError>,
) {
    let registers = program.register_count as i32;
    if count < 0 {
        problems.push(VerifyError::at(address, "a negative register count"));
        return;
    }
    if first < 0 || first.saturating_add(count) > registers {
        // The arithmetic saturates in the message as well as in the test: the
        // verifier is handed operands nobody sanitised, and a diagnostic that
        // overflows while describing a bad program is the verifier having the
        // bug it was written to catch.
        problems.push(VerifyError::at(
            address,
            format!(
                "register block {first}..{} leaves the frame",
                first.saturating_add(count)
            ),
        ));
    }
}

/// Checks that every cursor is opened once and used as the kind it was opened.
fn check_cursors(program: &Program, problems: &mut Vec<VerifyError>) {
    let mut kinds: Vec<Option<CursorKind>> = vec![None; program.cursor_count as usize];
    for (address, instruction) in program.instructions.iter().enumerate() {
        let kind = match instruction.opcode {
            Opcode::OpenRead | Opcode::OpenWrite => Some(CursorKind::Table),
            Opcode::OpenIndex | Opcode::OpenWriteIndex => Some(CursorKind::Index),
            _ => None,
        };
        let Some(kind) = kind else {
            continue;
        };
        let index = instruction.p1;
        if index < 0 || index as usize >= kinds.len() {
            problems.push(VerifyError::at(
                address,
                format!("cursor {index} is outside the {} declared", kinds.len()),
            ));
            continue;
        }
        if let Some(slot) = kinds.get_mut(index as usize) {
            if slot.is_some() {
                problems.push(VerifyError::at(address, format!("cursor {index} reopened")));
            }
            *slot = Some(kind);
        }
    }
    for (address, instruction) in program.instructions.iter().enumerate() {
        let expected = match instruction.opcode {
            Opcode::SeekRowid
            | Opcode::Column
            | Opcode::NewRowid
            | Opcode::InsertRow
            | Opcode::NotExists => Some(CursorKind::Table),
            Opcode::IdxRowid
            | Opcode::IdxGe
            | Opcode::IdxGt
            | Opcode::IdxLe
            | Opcode::IdxLt
            | Opcode::IdxColumn
            | Opcode::IdxInsert
            | Opcode::IdxDelete
            | Opcode::NoConflict => Some(CursorKind::Index),
            _ => None,
        };
        let Some(expected) = expected else {
            continue;
        };
        let index = instruction.p1;
        let actual = kinds.get(index.max(0) as usize).copied().flatten();
        match actual {
            Some(actual) if actual == expected => {}
            Some(actual) => problems.push(VerifyError::at(
                address,
                format!(
                    "{} uses cursor {index}, which was opened as {actual:?}",
                    instruction.opcode.name()
                ),
            )),
            None => problems.push(VerifyError::at(
                address,
                format!(
                    "{} uses cursor {index}, which was never opened",
                    instruction.opcode.name()
                ),
            )),
        }
    }
}

/// A set of register numbers, as a bitmap over the frame.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RegisterSet {
    words: Vec<u64>,
}

impl RegisterSet {
    /// Returns an empty set able to hold `registers` members.
    fn empty(registers: usize) -> RegisterSet {
        RegisterSet {
            words: vec![0u64; registers.saturating_add(63) / 64],
        }
    }

    /// Adds a register, ignoring one outside the frame.
    ///
    /// A register outside the frame is already reported by the range check, and
    /// growing the bitmap to hold it would let a bad operand choose an
    /// allocation size.
    fn insert(&mut self, register: u32) {
        let index = (register / 64) as usize;
        if let Some(word) = self.words.get_mut(index) {
            *word |= 1u64 << (register % 64);
        }
    }

    /// Returns whether a register is in the set.
    fn contains(&self, register: u32) -> bool {
        let index = (register / 64) as usize;
        self.words
            .get(index)
            .is_some_and(|word| word & (1u64 << (register % 64)) != 0)
    }

    /// Replaces this set's members with another's, without allocating.
    fn copy_from(&mut self, other: &RegisterSet) {
        self.words.clear();
        self.words.extend_from_slice(&other.words);
    }

    /// Keeps only the members both sets have, reporting whether it changed.
    fn intersect_with(&mut self, other: &RegisterSet) -> bool {
        let mut changed = false;
        for (index, word) in self.words.iter_mut().enumerate() {
            let merged = *word & other.words.get(index).copied().unwrap_or(0);
            if merged != *word {
                *word = merged;
                changed = true;
            }
        }
        changed
    }
}

/// Checks that no instruction reads a register no path has written.
fn check_registers(program: &Program, problems: &mut Vec<VerifyError>) {
    let length = program.instructions.len();
    let registers = program.register_count as usize;
    let mut written: Vec<Option<RegisterSet>> = vec![None; length.saturating_add(1)];
    if let Some(entry) = written.first_mut() {
        *entry = Some(RegisterSet::empty(registers));
    }
    // A fixed-point pass: propagate the definitely-written set forward until it
    // stops shrinking. The set only ever shrinks at a join, so this terminates
    // in at most one pass per instruction.
    let mut changed = true;
    let mut rounds = 0usize;
    // Scratch reused across the whole analysis. Every statement an application
    // prepares runs this, and allocating a bitmap and a vector per instruction
    // per round made verifying a four-instruction `SELECT 1` cost more than
    // parsing, binding and compiling it put together.
    let mut outgoing = RegisterSet::empty(registers);
    while changed && rounds <= length.saturating_add(2) {
        changed = false;
        rounds = rounds.saturating_add(1);
        for address in 0..length {
            let Some(Some(incoming)) = written.get(address) else {
                continue;
            };
            let Some(instruction) = program.instructions.get(address) else {
                continue;
            };
            outgoing.copy_from(incoming);
            if let Some(register) = writes_of(program, address) {
                outgoing.insert(register);
            }
            let mut targets: [usize; 2] = [0, 0];
            let mut target_count = 0usize;
            if instruction.opcode != Opcode::Goto
                && instruction.opcode != Opcode::Init
                && instruction.opcode != Opcode::Halt
                && instruction.opcode != Opcode::Return
            {
                targets[0] = address.saturating_add(1);
                target_count = 1;
            }
            if instruction.opcode.jumps() {
                if let Some(slot) = targets.get_mut(target_count) {
                    *slot = instruction.p2.max(0) as usize;
                }
                target_count = target_count.saturating_add(1);
            }
            if instruction.opcode == Opcode::Return {
                // A return goes wherever its caller was, which the verifier
                // models as "everywhere a Gosub could have come from"; the
                // conservative answer is to stop propagating here, and the
                // Gosub already propagated into the subroutine.
                continue;
            }
            for target in targets.iter().copied().take(target_count) {
                if target > length {
                    continue;
                }
                let Some(slot) = written.get_mut(target) else {
                    continue;
                };
                match slot {
                    Some(existing) => {
                        if existing.intersect_with(&outgoing) {
                            changed = true;
                        }
                    }
                    None => {
                        *slot = Some(outgoing.clone());
                        changed = true;
                    }
                }
            }
        }
    }
    for address in 0..length {
        let Some(Some(available)) = written.get(address) else {
            // Unreachable code is not a correctness problem, and the compiler
            // emits none; nothing to check.
            continue;
        };
        for register in reads_of(program, address) {
            if !available.contains(register) {
                problems.push(VerifyError::at(
                    address,
                    format!("register {register} is read before it is written"),
                ));
            }
        }
    }
}

/// Returns the register an instruction writes, when it writes one.
///
/// No opcode writes more than one, which is what lets this be an `Option`
/// rather than a vector - and a vector here was an allocation per instruction
/// per round of the dataflow pass.
pub(crate) fn writes_of(program: &Program, address: usize) -> Option<u32> {
    let instruction = program.instructions.get(address)?;
    let single = |value: i32| Some(value.max(0) as u32);
    match instruction.opcode {
        // The save direction fills its register; the restore direction reads it.
        Opcode::LastRowid if instruction.p2 == 0 => single(instruction.p1),
        Opcode::SeqRowid => single(instruction.p3),
        Opcode::Column | Opcode::IdxColumn | Opcode::SorterColumn | Opcode::EphColumn => {
            single(instruction.p3)
        }
        Opcode::EphSawNull => single(instruction.p2),
        Opcode::Rowid | Opcode::IdxRowid | Opcode::Null | Opcode::Load => single(instruction.p2),
        Opcode::Copy
        | Opcode::Not
        | Opcode::Negate
        | Opcode::BitNot
        | Opcode::Cast
        | Opcode::IsNull => single(instruction.p2),
        Opcode::Arithmetic
        | Opcode::Compare
        | Opcode::Is
        | Opcode::And
        | Opcode::Or
        | Opcode::InList => single(instruction.p3),
        Opcode::Function
        | Opcode::Pattern
        | Opcode::MathCall
        | Opcode::TimeCall
        | Opcode::JsonCall
        | Opcode::ExtCall => single(instruction.p3),
        Opcode::AggFinal | Opcode::VRowid => single(instruction.p2),
        Opcode::VColumn => single(instruction.p3),
        Opcode::VAux => single(instruction.p3),
        Opcode::VUpdate => {
            if instruction.p3 >= 0 {
                single(instruction.p3)
            } else {
                None
            }
        }
        Opcode::Gosub => single(instruction.p1),
        Opcode::NewRowid | Opcode::RowData | Opcode::CreateBtree => single(instruction.p2),
        Opcode::MakeRecord => single(instruction.p3),
        _ => None,
    }
}

/// Returns the registers an instruction reads.
pub(crate) fn reads_of(program: &Program, address: usize) -> Vec<u32> {
    let Some(instruction) = program.instructions.get(address) else {
        return Vec::new();
    };
    // The count is clamped to the frame: these operands come from a program
    // that has not been proved yet, and enumerating a block of two billion
    // registers to report that it is too big is the verifier hanging on the
    // input it exists to reject. `check_operand_ranges` reports the block
    // itself; this pass only needs the registers inside the frame.
    let registers = program.register_count as i32;
    let block = move |first: i32, count: i32| -> Vec<u32> {
        (0..count.clamp(0, registers))
            .map(|offset| first.saturating_add(offset).max(0) as u32)
            .collect()
    };
    match instruction.opcode {
        Opcode::Copy
        | Opcode::Not
        | Opcode::Negate
        | Opcode::BitNot
        | Opcode::Cast
        | Opcode::IsNull => vec![instruction.p1.max(0) as u32],
        Opcode::Arithmetic | Opcode::Compare | Opcode::Is | Opcode::And | Opcode::Or => {
            vec![instruction.p1.max(0) as u32, instruction.p2.max(0) as u32]
        }
        Opcode::If
        | Opcode::IfNot
        | Opcode::IfNull
        | Opcode::IfNotNull
        | Opcode::IfPos
        | Opcode::DecrJumpZero
        | Opcode::Return => vec![instruction.p1.max(0) as u32],
        Opcode::SeekRowid => vec![instruction.p3.max(0) as u32],
        Opcode::SeekGe
        | Opcode::SeekGt
        | Opcode::SeekLe
        | Opcode::SeekLt
        | Opcode::IdxGe
        | Opcode::IdxGt
        | Opcode::IdxLe
        | Opcode::IdxLt => block(instruction.p3, instruction.p5 as i32),
        Opcode::ResultRow | Opcode::ApplyAffinity => block(instruction.p1, instruction.p2),
        Opcode::Function
        | Opcode::Pattern
        | Opcode::MathCall
        | Opcode::TimeCall
        | Opcode::JsonCall
        | Opcode::ExtCall
        | Opcode::AggStep => block(instruction.p1, instruction.p2),
        Opcode::VFilter => block(instruction.p3, i32::from(instruction.p5)),
        Opcode::VUpdate => block(instruction.p1, instruction.p2),
        Opcode::InList => {
            let mut reads = vec![instruction.p1.max(0) as u32];
            reads.extend(block(instruction.p2, instruction.p5 as i32));
            reads
        }
        Opcode::MakeRecord => block(instruction.p1, instruction.p2),
        Opcode::InsertRow => vec![instruction.p2.max(0) as u32, instruction.p3.max(0) as u32],
        Opcode::IdxInsert | Opcode::IdxDelete => vec![instruction.p2.max(0) as u32],
        Opcode::NotExists => vec![instruction.p3.max(0) as u32],
        Opcode::NoConflict => block(instruction.p3, instruction.p5 as i32),
        Opcode::DestroyBtree | Opcode::ClearBtree | Opcode::CountChange => {
            vec![instruction.p1.max(0) as u32]
        }
        // Only the restore direction reads its register; the save direction
        // writes it, and listing it as a read would flag the save as reading an
        // uninitialised register.
        Opcode::LastRowid if instruction.p2 == 1 => vec![instruction.p1.max(0) as u32],
        Opcode::SeqUpdate => vec![instruction.p1.max(0) as u32],
        Opcode::SorterInsert => block(instruction.p2, instruction.p3),
        Opcode::DistinctCheck => block(instruction.p3, instruction.p5 as i32),
        Opcode::EphInsert => block(instruction.p2, instruction.p3),
        Opcode::EphInsertUnique | Opcode::EphFound | Opcode::EphNotFound | Opcode::EphRemove => {
            block(instruction.p3, instruction.p5 as i32)
        }
        _ => Vec::new(),
    }
}

/// Returns whether an operand is the kind an opcode requires.
///
/// The compiler always attaches the right operand; a program built by hand or
/// by a future compiler might not, and the machine's behaviour on a mismatched
/// operand would be to take a default rather than to fail.
pub fn operand_matches(opcode: Opcode, operand: &Operand) -> bool {
    match opcode {
        Opcode::Compare | Opcode::Is | Opcode::InList => {
            matches!(operand, Operand::Comparison(_))
        }
        Opcode::Arithmetic => matches!(operand, Operand::Arithmetic(_)),
        Opcode::Cast | Opcode::ApplyAffinity => matches!(operand, Operand::Affinity(_)),
        Opcode::Function => matches!(operand, Operand::Scalar(_, _)),
        Opcode::JsonCall => matches!(operand, Operand::Json(_)),
        Opcode::VOpen
        | Opcode::VUpdate
        | Opcode::VBegin
        | Opcode::VSync
        | Opcode::VCommit
        | Opcode::VRollback
        | Opcode::VSavepoint => matches!(operand, Operand::Virtual(_)),
        Opcode::VFilter => matches!(operand, Operand::VirtualPlan(_)),
        Opcode::Pattern => matches!(operand, Operand::Pattern(_)),
        Opcode::AggStep | Opcode::AggFinal | Opcode::AggReset => {
            matches!(operand, Operand::Aggregate(_))
        }
        Opcode::SorterOpen => matches!(operand, Operand::SortKey(_)),
        Opcode::OpenIndex => matches!(operand, Operand::IndexKey(_)),
        _ => true,
    }
}

/// Checks that every instruction carries the operand its opcode needs.
pub fn verify_operands(program: &Program) -> Vec<VerifyError> {
    let mut problems = Vec::new();
    for (address, instruction) in program.instructions.iter().enumerate() {
        if !operand_matches(instruction.opcode, &instruction.p4) {
            problems.push(VerifyError::at(
                address,
                format!(
                    "{} carries the wrong kind of operand",
                    instruction.opcode.name()
                ),
            ));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::{Instruction, ProgramDependencies};

    /// Builds a minimal well-formed program for the tests to damage.
    fn program(instructions: Vec<Instruction>, registers: u32, cursors: u32) -> Program {
        Program {
            ephemeral_count: 0,
            instructions,
            register_count: registers,
            cursor_count: cursors,
            sorter_count: 0,
            distinct_count: 0,
            aggregate_count: 0,
            result_columns: Vec::new(),
            dependencies: ProgramDependencies::default(),
            readonly: true,
            optimizations_used: 0,
            parameter_count: 0,
        }
    }

    /// A well-formed program is accepted.
    #[test]
    fn a_well_formed_program_verifies() {
        let found = verify(&program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::Null, 0, 1, 0),
                Instruction::new(Opcode::ResultRow, 1, 1, 0),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            2,
            0,
        ));
        assert!(found.is_empty(), "{found:#?}");
    }

    /// A jump past the end is rejected, which is the failure that would make
    /// the machine read an instruction that does not exist.
    #[test]
    fn a_jump_outside_the_program_is_rejected() {
        let found = verify(&program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::Goto, 0, 99, 0),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            1,
            0,
        ));
        assert!(found
            .iter()
            .any(|problem| problem.reason.contains("outside the program")));
    }

    /// A register outside the frame is rejected.
    #[test]
    fn a_register_outside_the_frame_is_rejected() {
        let found = verify(&program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::Null, 0, 40, 0),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            2,
            0,
        ));
        assert!(found
            .iter()
            .any(|problem| problem.reason.contains("outside the")));
    }

    /// Reading a register nothing wrote is rejected, which is the failure that
    /// would silently read a NULL that should have been a value.
    #[test]
    fn reading_an_unwritten_register_is_rejected() {
        let found = verify(&program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::Copy, 1, 2, 0),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            3,
            0,
        ));
        assert!(found
            .iter()
            .any(|problem| problem.reason.contains("read before it is written")));
    }

    /// Using an index cursor where a table cursor was opened is rejected.
    #[test]
    fn a_cursor_kind_mismatch_is_rejected() {
        let found = verify(&program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::OpenRead, 0, 2, 0),
                Instruction::new(Opcode::IdxRowid, 0, 1, 0),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            2,
            1,
        ));
        assert!(found
            .iter()
            .any(|problem| problem.reason.contains("opened as Table")));
    }

    /// A cursor nobody opened is rejected.
    #[test]
    fn an_unopened_cursor_is_rejected() {
        let found = verify(&program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::Column, 3, 0, 1),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            2,
            1,
        ));
        assert!(found
            .iter()
            .any(|problem| problem.reason.contains("never opened")));
    }

    /// A program with no Halt cannot terminate and is rejected.
    #[test]
    fn a_program_without_halt_is_rejected() {
        let found = verify(&program(
            vec![Instruction::new(Opcode::Init, 0, 1, 0)],
            1,
            0,
        ));
        assert!(found
            .iter()
            .any(|problem| problem.reason.contains("no Halt")));
    }

    /// An operand of the wrong kind is rejected.
    #[test]
    fn a_mismatched_operand_is_rejected() {
        let found = verify_operands(&program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::Compare, 1, 1, 1).with_p4(Operand::Integer(0)),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            2,
            0,
        ));
        assert_eq!(found.len(), 1);
    }
}
