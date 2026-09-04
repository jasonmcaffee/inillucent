//! What the hot types cost to move.
//!
//! Invariant: this reports sizes, it changes nothing. A `Result` returned once
//! per bytecode instruction is memcpy'd once per bytecode instruction, so how
//! big its error half is belongs in a profile next to the timings.
//!
//! Usage: `cargo run --release -p rustdb-compat --bin rustdb-sizecheck`

use std::mem::size_of;

fn main() {
    println!("{:<44} {:>6}", "type", "bytes");
    println!(
        "{:<44} {:>6}",
        "rustdb_base::DbError",
        size_of::<rustdb_base::DbError>()
    );
    println!(
        "{:<44} {:>6}",
        "DbResult<()>",
        size_of::<rustdb_base::DbResult<()>>()
    );
    println!(
        "{:<44} {:>6}",
        "DbResult<bool>",
        size_of::<rustdb_base::DbResult<bool>>()
    );
    println!(
        "{:<44} {:>6}",
        "Value",
        size_of::<rustdb_value::Value<'static>>()
    );
    println!(
        "{:<44} {:>6}",
        "Instruction",
        size_of::<rustdb_vm::Instruction>()
    );
    println!("{:<44} {:>6}", "Opcode", size_of::<rustdb_vm::Opcode>());
}
