//! Folding a value written into a scratch register and immediately copied.
//!
//! Invariant: this rewrites a program into one that computes the same thing,
//! and the verifier runs *after* it. That order is the whole safety argument.
//! A peephole pass is exactly the kind of code that is right for a year and
//! then wrong for one opcode nobody thought about, so it is not trusted: the
//! program it produces is proved the same way the compiler's own output is,
//! against the same rules, before anything runs it.
//!
//! What it folds is the shape the compiler emits everywhere. `compile_expr`
//! returns whichever register an expression landed in, and the caller copies
//! that into wherever it wanted the value - a result block, an aggregate's
//! argument block, a sorter record. For a bare column that is two instructions
//! where one would do, and it is in the innermost loop of every scan:
//!
//! ```text
//!     IdxColumn  cursor 1, column 1 -> r5
//!     Copy       r5 -> r3
//! ```
//!
//! becomes
//!
//! ```text
//!     IdxColumn  cursor 1, column 1 -> r3
//! ```
//!
//! The conditions are deliberately narrow, because the cost of being wrong is
//! a wrong answer rather than a slow one:
//!
//! - the first instruction writes exactly one register and has no other effect
//!   (an allowlist, not "everything that is not on a denylist");
//! - it does not jump, so control always reaches the copy;
//! - nothing jumps *to* the copy, so removing it removes no entry point;
//! - the scratch register is written only there and read only by the copy, so
//!   no later instruction and no other iteration can see it.
//!
//! The last one is checked over the whole program rather than by a liveness
//! analysis. That is conservative - it declines folds that would be safe - and
//! it cannot be fooled by a loop.

use crate::program::{Instruction, Opcode, Program};

/// Rewrites a program to fold scratch copies away, returning how many it took.
///
/// The addresses move, so every jump target moves with them. A jump whose
/// target is a folded copy is retargeted to the instruction that replaced it,
/// which is the same instruction the copy would have fallen through to.
/// @param program - the program to rewrite in place
pub fn fold_scratch_copies(program: &mut Program) -> usize {
    let length = program.instructions.len();
    if length < 2 {
        return 0;
    }
    let targets = jump_targets(program);
    let (writes, reads) = register_uses(program);

    let mut folded = vec![false; length];
    for address in 0..length.saturating_sub(1) {
        // An instruction already folded away is not a source for the next
        // fold. Without this, `Rowid -> r8; Copy r8 -> r9; Copy r9 -> r2`
        // folded twice: the second fold retargeted an instruction that was
        // being deleted, and the write to `r2` vanished with it.
        if folded.get(address).copied().unwrap_or(false) {
            continue;
        }
        let Some(instruction) = program.instructions.get(address) else {
            continue;
        };
        if !produces_a_value(instruction.opcode) || instruction.opcode.jumps() {
            continue;
        }
        let Some(scratch) = destination(instruction) else {
            continue;
        };
        let next = address.saturating_add(1);
        if targets.contains(&next) || folded.get(next).copied().unwrap_or(false) {
            continue;
        }
        let Some(copy) = program.instructions.get(next) else {
            continue;
        };
        if copy.opcode != Opcode::Copy || copy.p1 != scratch as i32 || copy.p2 == scratch as i32 {
            continue;
        }
        // A plain copy only moves the value. `Copy` also has two normalising
        // forms - a `LIMIT` and an `OFFSET` are clamped as they are copied -
        // and folding one of those away takes the clamp with it: `LIMIT -1`
        // means no limit, and without the clamp it meant no rows.
        if copy.p5 != 0 || copy.p2 < 0 {
            continue;
        }
        // The scratch register belongs to this pair and to nothing else.
        if writes.get(scratch as usize).copied().unwrap_or(0) != 1
            || reads.get(scratch as usize).copied().unwrap_or(0) != 1
        {
            continue;
        }
        let destination_register = copy.p2;
        if let Some(instruction) = program.instructions.get_mut(address) {
            set_destination(instruction, destination_register);
        }
        if let Some(slot) = folded.get_mut(next) {
            *slot = true;
        }
    }

    let removed = folded.iter().filter(|folded| **folded).count();
    if removed == 0 {
        return 0;
    }
    compact(program, &folded);
    removed
}

/// Returns every address something can jump to.
///
/// `Return` jumps to an address a `Gosub` stored at run time, which is always
/// the instruction after the `Gosub` - so that address is a target too, even
/// though no operand names it.
fn jump_targets(program: &Program) -> Vec<usize> {
    let mut targets = vec![0usize];
    for (address, instruction) in program.instructions.iter().enumerate() {
        if instruction.opcode.jumps() && instruction.p2 >= 0 {
            targets.push(instruction.p2 as usize);
        }
        if instruction.opcode == Opcode::Gosub {
            targets.push(address.saturating_add(1));
        }
    }
    targets
}

/// Returns how many instructions write and read each register.
fn register_uses(program: &Program) -> (Vec<u32>, Vec<u32>) {
    let width = program.register_count.saturating_add(1) as usize;
    let mut writes = vec![0u32; width];
    let mut reads = vec![0u32; width];
    for address in 0..program.instructions.len() {
        if let Some(register) = crate::verify::writes_of(program, address) {
            if let Some(count) = writes.get_mut(register as usize) {
                *count = count.saturating_add(1);
            }
        }
        for register in crate::verify::reads_of(program, address) {
            if let Some(count) = reads.get_mut(register as usize) {
                *count = count.saturating_add(1);
            }
        }
    }
    (writes, reads)
}

/// Removes the folded instructions and moves every jump target with them.
fn compact(program: &mut Program, folded: &[bool]) {
    let mut mapping = Vec::with_capacity(program.instructions.len().saturating_add(1));
    let mut next = 0usize;
    for address in 0..program.instructions.len() {
        mapping.push(next);
        if !folded.get(address).copied().unwrap_or(false) {
            next = next.saturating_add(1);
        }
    }
    // A jump past the last instruction is how a loop says "done", so the map
    // has to answer for one address beyond the program.
    mapping.push(next);

    let mut kept: Vec<Instruction> = Vec::with_capacity(next);
    for (address, instruction) in program.instructions.iter().enumerate() {
        if folded.get(address).copied().unwrap_or(false) {
            continue;
        }
        let mut instruction = instruction.clone();
        if instruction.opcode.jumps() && instruction.p2 >= 0 {
            let target = instruction.p2 as usize;
            instruction.p2 = mapping.get(target).copied().unwrap_or(next) as i32;
        }
        if instruction.opcode == Opcode::Gosub {
            // `Gosub`'s p2 is its subroutine and is remapped above; the return
            // address it stores is computed at run time from where it now is.
            let target = instruction.p2.max(0) as usize;
            let _ = target;
        }
        kept.push(instruction);
    }
    program.instructions = kept;
}

/// Returns whether an opcode's only effect is to write its destination.
///
/// An allowlist rather than a denylist: an opcode added later is not folded
/// until somebody has looked at it, which is the failure that costs nothing.
fn produces_a_value(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::Column
            | Opcode::IdxColumn
            | Opcode::SorterColumn
            | Opcode::EphColumn
            | Opcode::Rowid
            | Opcode::IdxRowid
            | Opcode::Null
            | Opcode::Load
            | Opcode::Copy
            | Opcode::Not
            | Opcode::Negate
            | Opcode::BitNot
            | Opcode::Cast
            | Opcode::IsNull
            | Opcode::Arithmetic
            | Opcode::Compare
            | Opcode::Is
            | Opcode::And
            | Opcode::Or
            | Opcode::Function
            | Opcode::Pattern
            | Opcode::MathCall
            | Opcode::TimeCall
            | Opcode::JsonCall
            | Opcode::ExtCall
            | Opcode::AggFinal
            | Opcode::VColumn
            | Opcode::VRowid
            | Opcode::MakeRecord
    )
}

/// Returns the register an instruction writes, for the folded opcodes only.
fn destination(instruction: &Instruction) -> Option<u32> {
    match instruction.opcode {
        Opcode::Column
        | Opcode::IdxColumn
        | Opcode::SorterColumn
        | Opcode::EphColumn
        | Opcode::Arithmetic
        | Opcode::Compare
        | Opcode::Is
        | Opcode::And
        | Opcode::Or
        | Opcode::Function
        | Opcode::Pattern
        | Opcode::MathCall
        | Opcode::TimeCall
        | Opcode::JsonCall
        | Opcode::ExtCall
        | Opcode::VColumn
        | Opcode::MakeRecord => (instruction.p3 >= 0).then_some(instruction.p3 as u32),
        Opcode::Rowid
        | Opcode::IdxRowid
        | Opcode::Null
        | Opcode::Load
        | Opcode::Copy
        | Opcode::Not
        | Opcode::Negate
        | Opcode::BitNot
        | Opcode::Cast
        | Opcode::IsNull
        | Opcode::AggFinal
        | Opcode::VRowid => (instruction.p2 >= 0).then_some(instruction.p2 as u32),
        _ => None,
    }
}

/// Points an instruction's destination at another register.
///
/// The mirror of [`destination`], and it has to stay the mirror: writing the
/// new register into the wrong operand would change what the instruction reads
/// rather than where it puts the answer.
fn set_destination(instruction: &mut Instruction, register: i32) {
    match instruction.opcode {
        Opcode::Column
        | Opcode::IdxColumn
        | Opcode::SorterColumn
        | Opcode::EphColumn
        | Opcode::Arithmetic
        | Opcode::Compare
        | Opcode::Is
        | Opcode::And
        | Opcode::Or
        | Opcode::Function
        | Opcode::Pattern
        | Opcode::MathCall
        | Opcode::TimeCall
        | Opcode::JsonCall
        | Opcode::ExtCall
        | Opcode::VColumn
        | Opcode::MakeRecord => instruction.p3 = register,
        Opcode::Rowid
        | Opcode::IdxRowid
        | Opcode::Null
        | Opcode::Load
        | Opcode::Copy
        | Opcode::Not
        | Opcode::Negate
        | Opcode::BitNot
        | Opcode::Cast
        | Opcode::IsNull
        | Opcode::AggFinal
        | Opcode::VRowid => instruction.p2 = register,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::{Operand, ProgramDependencies};

    /// Builds a program from a list of instructions, with room for registers.
    fn program(instructions: Vec<Instruction>, registers: u32) -> Program {
        Program {
            instructions,
            register_count: registers,
            cursor_count: 1,
            sorter_count: 0,
            distinct_count: 0,
            ephemeral_count: 0,
            aggregate_count: 0,
            result_columns: Vec::new(),
            dependencies: ProgramDependencies::default(),
            readonly: true,
            optimizations_used: 0,
            parameter_count: 0,
        }
    }

    /// A column read into a scratch register and copied becomes one read.
    #[test]
    fn a_scratch_copy_folds() {
        let mut built = program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::Column, 0, 2, 5),
                Instruction::new(Opcode::Copy, 5, 3, 0),
                Instruction::new(Opcode::ResultRow, 3, 1, 0),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            8,
        );
        assert_eq!(fold_scratch_copies(&mut built), 1);
        assert_eq!(built.instructions.len(), 4);
        let column = built.instructions.get(1).expect("the column survives");
        assert_eq!(column.opcode, Opcode::Column);
        assert_eq!(column.p3, 3, "it writes where the copy was going to");
    }

    /// A copy that normalises rather than only moving is left alone.
    ///
    /// `Copy` with a flag clamps a `LIMIT` or an `OFFSET` on the way past.
    /// Folding one of those took the clamp with it, and `LIMIT -1` - which
    /// means no limit - returned no rows.
    #[test]
    fn a_normalising_copy_is_not_folded() {
        let mut built = program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::Negate, 1, 2, 0),
                Instruction::new(Opcode::Copy, 2, 3, 0).with_p5(1),
                Instruction::new(Opcode::IfPos, 3, 4, 0),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            8,
        );
        assert_eq!(fold_scratch_copies(&mut built), 0);
    }

    /// A chain of copies folds once, not twice over itself.
    ///
    /// `Rowid -> r8; Copy r8 -> r9; Copy r9 -> r2` folded the first pair and
    /// then used the instruction it had just deleted as the source of a second
    /// fold, which retargeted a deleted instruction and lost the write to `r2`.
    #[test]
    fn a_chain_of_copies_folds_once() {
        let mut built = program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::Rowid, 0, 8, 0),
                Instruction::new(Opcode::Copy, 8, 9, 0),
                Instruction::new(Opcode::Copy, 9, 2, 0),
                Instruction::new(Opcode::ResultRow, 2, 1, 0),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            12,
        );
        assert_eq!(fold_scratch_copies(&mut built), 1);
        let rowid = built.instructions.get(1).expect("the rowid survives");
        assert_eq!(rowid.p2, 9);
        let copy = built.instructions.get(2).expect("the second copy survives");
        assert_eq!(copy.opcode, Opcode::Copy);
        assert_eq!((copy.p1, copy.p2), (9, 2), "the write to r2 is still made");
    }

    /// A scratch register something else reads is left alone.
    #[test]
    fn a_register_read_twice_is_not_folded() {
        let mut built = program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::Column, 0, 2, 5),
                Instruction::new(Opcode::Copy, 5, 3, 0),
                Instruction::new(Opcode::Copy, 5, 4, 0),
                Instruction::new(Opcode::ResultRow, 3, 2, 0),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            8,
        );
        assert_eq!(fold_scratch_copies(&mut built), 0);
    }

    /// A copy something jumps to is an entry point, and stays.
    #[test]
    fn a_copy_that_is_a_jump_target_is_not_folded() {
        let mut built = program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::Column, 0, 2, 5),
                Instruction::new(Opcode::Copy, 5, 3, 0),
                Instruction::new(Opcode::Goto, 0, 2, 0),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            8,
        );
        assert_eq!(fold_scratch_copies(&mut built), 0);
    }

    /// Jump targets move with the instructions they name.
    #[test]
    fn jumps_are_retargeted() {
        let mut built = program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::Column, 0, 2, 5),
                Instruction::new(Opcode::Copy, 5, 3, 0),
                Instruction::new(Opcode::ResultRow, 3, 1, 0),
                Instruction::new(Opcode::Goto, 0, 3, 0),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            8,
        );
        assert_eq!(fold_scratch_copies(&mut built), 1);
        let goto = built.instructions.get(3).expect("the goto survives");
        assert_eq!(goto.opcode, Opcode::Goto);
        assert_eq!(goto.p2, 2, "it still names the ResultRow");
    }

    /// An opcode that does more than write its destination is left alone.
    #[test]
    fn an_opcode_with_another_effect_is_not_folded() {
        let mut built = program(
            vec![
                Instruction::new(Opcode::Init, 0, 1, 0),
                Instruction::new(Opcode::NewRowid, 0, 5, 0),
                Instruction::new(Opcode::Copy, 5, 3, 0),
                Instruction::new(Opcode::ResultRow, 3, 1, 0),
                Instruction::new(Opcode::Halt, 0, 0, 0),
            ],
            8,
        );
        let _ = Operand::None;
        assert_eq!(fold_scratch_copies(&mut built), 0);
    }
}
