//! What the hot types cost to move.
//!
//! Invariant: this reports sizes, it changes nothing. A `Result` returned once
//! per row is memcpy'd once per row, so how big its error half is belongs in a
//! profile next to the timings.
//!
//! **The old VM's `Instruction`/`Opcode` sizes were reported here and are not
//! any more.** The new engine compiles to an operator tree rather than
//! bytecode, so there is no per-instruction `Result` for those two types'
//! sizes to matter against - the question this file answers stopped applying
//! to them when `inillucent-vm` was deleted, rather than having an answer that
//! moved somewhere else.
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
}
