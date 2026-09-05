//! What the hot types cost to move.
//!
//! Invariant: this reports sizes, it changes nothing. A `Result` returned once
//! per bytecode instruction is memcpy'd once per bytecode instruction, so how
//! big its error half is belongs in a profile next to the timings.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-sizecheck`

use std::mem::size_of;

fn main() {
    println!("{:<44} {:>6}", "type", "bytes");
    println!(
        "{:<44} {:>6}",
        "inillucent_base::DbError",
        size_of::<inillucent_base::DbError>()
    );
    println!(
        "{:<44} {:>6}",
        "DbResult<()>",
        size_of::<inillucent_base::DbResult<()>>()
    );
    println!(
        "{:<44} {:>6}",
        "DbResult<bool>",
        size_of::<inillucent_base::DbResult<bool>>()
    );
    println!(
        "{:<44} {:>6}",
        "Value",
        size_of::<inillucent_value::Value<'static>>()
    );
    println!(
        "{:<44} {:>6}",
        "Instruction",
        size_of::<inillucent_vm::Instruction>()
    );
    println!("{:<44} {:>6}", "Opcode", size_of::<inillucent_vm::Opcode>());
}
