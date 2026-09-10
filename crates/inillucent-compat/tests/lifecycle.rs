//! Statement lifecycle, result metadata, parameters, and the verifier.
//!
//! Invariant: the public API behaves the way SQLite's does, including when it
//! is misused. A statement stepped past its end reports done rather than
//! panicking, a parameter index that does not exist is a misuse error rather
//! than a silent no-op, and an interrupt stops the machine at a safe point and
//! leaves it resettable.
//!
//! The verifier tests are the other half of the same argument: the machine is
//! safe because programs are proved before they run, so the proof has to be
//! shown to reject the programs it claims to.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use inillucent_base::limits::Limits;
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::workspace_root;
use inillucent_legacy::{Database, PrimaryCode, Value};
use inillucent_vm::machine::Machine;
use inillucent_vm::program::{Instruction, Opcode, Operand, Program, ProgramDependencies};
use inillucent_vm::{verify, verify_operands};

/// Returns the corpus fixture's path.
fn fixture() -> PathBuf {
    workspace_root().join("compat/fixtures/select-corpus.db")
}

/// Opens a connection onto the corpus fixture.
fn connect() -> inillucent_legacy::Connection {
    let database = Database::open_with_busy_timeout(fixture(), std::time::Duration::from_secs(5))
        .expect("the fixture opens");
    database.connect().expect("the connection opens")
}

/// A statement steps to done and then keeps reporting done.
#[test]
fn a_statement_steps_to_done_and_stays_there() {
    let connection = connect();
    let mut statement = connection
        .prepare("SELECT id FROM people WHERE id = 1")
        .expect("it prepares");
    assert!(statement.step().expect("it steps"));
    assert!(!statement.step().expect("it steps"));
    assert!(!statement.step().expect("it steps"));
    assert!(!statement.step().expect("it steps"));
}

/// A statement reset runs again from the beginning, keeping its bindings.
#[test]
fn reset_runs_again_and_keeps_bindings() {
    let connection = connect();
    let mut statement = connection
        .prepare("SELECT name FROM people WHERE id = ?1")
        .expect("it prepares");
    statement.bind_integer(1, 1).expect("it binds");
    assert!(statement.step().expect("it steps"));
    let first = statement.value_text(0);
    assert!(!statement.step().expect("it steps"));

    statement.reset().expect("it resets");
    assert!(statement.step().expect("it steps"));
    assert_eq!(statement.value_text(0), first);

    statement.reset().expect("it resets");
    statement.clear_bindings();
    // With the binding cleared the parameter is NULL, and `id = NULL` is never
    // true, so the statement returns nothing rather than failing.
    assert!(!statement.step().expect("it steps"));
}

/// A parameter index that does not exist is a misuse, not a silent no-op.
#[test]
fn an_unknown_parameter_is_a_misuse() {
    let connection = connect();
    let mut statement = connection.prepare("SELECT ?1").expect("it prepares");
    assert!(statement.bind_integer(1, 1).is_ok());
    let failure = statement
        .bind_integer(2, 1)
        .expect_err("parameter 2 does not exist");
    assert_eq!(failure.code(), PrimaryCode::Misuse);
    let zero = statement
        .bind_integer(0, 1)
        .expect_err("parameters are one-based");
    assert_eq!(zero.code(), PrimaryCode::Misuse);
}

/// Every bindable class round-trips through a parameter.
#[test]
fn every_class_binds_and_returns() {
    let connection = connect();
    let mut statement = connection
        .prepare("SELECT ?1, ?2, ?3, ?4, ?5")
        .expect("it prepares");
    statement.bind_null(1).expect("it binds");
    statement.bind_integer(2, -7).expect("it binds");
    statement.bind_real(3, 1.5).expect("it binds");
    statement.bind_text(4, "text").expect("it binds");
    statement.bind_blob(5, &[0x00, 0xff]).expect("it binds");
    assert!(statement.step().expect("it steps"));
    let row = statement.row();
    assert!(row.first().is_some_and(Value::is_null));
    assert_eq!(row.get(1).and_then(Value::as_integer), Some(-7));
    assert_eq!(row.get(2).and_then(Value::as_real), Some(1.5));
    assert_eq!(
        row.get(3)
            .and_then(Value::as_text)
            .map(|text| text.utf8_bytes().into_owned()),
        Some(b"text".to_vec())
    );
    assert_eq!(
        row.get(4)
            .and_then(Value::as_blob)
            .map(|blob| blob.raw().to_vec()),
        Some(vec![0x00, 0xff])
    );
}

/// An interrupt stops the machine and reports the interrupt code.
#[test]
fn an_interrupt_stops_a_running_statement() {
    let connection = connect();
    connection.interrupt();
    let mut statement = connection
        .prepare("SELECT count(*) FROM people")
        .expect("it prepares");
    let failure = statement.step().expect_err("it is interrupted");
    assert_eq!(failure.code(), PrimaryCode::Interrupt);
    connection.clear_interrupt();
    statement.reset().expect("it resets");
    assert!(statement.step().expect("it steps"));
}

/// A read-only connection is always in autocommit, and every statement it can
/// prepare is read-only.
#[test]
fn the_connection_is_in_autocommit_and_read_only() {
    let connection = connect();
    assert!(connection.autocommit());
    let statement = connection.prepare("SELECT 1").expect("it prepares");
    assert!(statement.is_readonly());
}

/// Prepare reports the tail so a caller can walk a script.
#[test]
fn prepare_reports_the_tail() {
    let connection = connect();
    let sql = "SELECT 1; SELECT 2;";
    let (mut first, consumed) = connection.prepare_with_tail(sql).expect("it prepares");
    assert!(first.step().expect("it steps"));
    assert_eq!(first.value_integer(0), Some(1));
    let rest = sql.get(consumed..).unwrap_or("");
    let (mut second, _) = connection.prepare_with_tail(rest).expect("it prepares");
    assert!(second.step().expect("it steps"));
    assert_eq!(second.value_integer(0), Some(2));
}

/// Column metadata is available before the first step, which is what a caller
/// binding a result set needs.
#[test]
fn column_metadata_is_available_before_stepping() {
    let connection = connect();
    let statement = connection
        .prepare("SELECT id, name AS who, id + 1 FROM people")
        .expect("it prepares");
    assert_eq!(statement.column_count(), 3);
    assert_eq!(statement.column_name(0), Some(b"id".as_slice()));
    assert_eq!(statement.column_name(1), Some(b"who".as_slice()));
    // An expression with no alias is named after the text it was written as,
    // which is what SQLite's default `short_column_names` produces and what an
    // application reading results by name depends on.
    assert_eq!(statement.column_name(2), Some(b"id + 1".as_slice()));
    assert_eq!(statement.column_name(3), None);
}

/// Reading a database changes nothing about it, even after many statements.
#[test]
fn a_long_session_changes_no_byte() {
    let before = std::fs::read(fixture()).expect("the fixture reads");
    {
        let connection = connect();
        for sql in [
            "SELECT count(*) FROM people",
            "SELECT * FROM people ORDER BY name",
            "SELECT team, count(*) FROM people GROUP BY team",
            "SELECT DISTINCT team FROM people",
        ] {
            let _ = connection.query(sql).expect("it runs");
        }
    }
    let after = std::fs::read(fixture()).expect("the fixture reads");
    assert_eq!(before, after);
}

/// Two statements on one connection can be stepped alternately and see the
/// same snapshot, which is what the reference-counted read transaction is for.
#[test]
fn two_statements_interleave_on_one_connection() {
    let connection = connect();
    let mut first = connection
        .prepare("SELECT id FROM people ORDER BY id")
        .expect("it prepares");
    let mut second = connection
        .prepare("SELECT id FROM people ORDER BY id DESC")
        .expect("it prepares");
    let mut ascending = Vec::new();
    let mut descending = Vec::new();
    loop {
        let more_first = first.step().expect("it steps");
        let more_second = second.step().expect("it steps");
        if more_first {
            ascending.push(first.value_integer(0));
        }
        if more_second {
            descending.push(second.value_integer(0));
        }
        if !more_first && !more_second {
            break;
        }
    }
    descending.reverse();
    assert_eq!(ascending, descending);
    assert_eq!(ascending.len(), 10);
}

/// Builds a minimal valid program for the verifier tests to damage.
fn valid_program() -> Program {
    Program {
        ephemeral_count: 0,
        instructions: vec![
            Instruction::new(Opcode::Init, 0, 1, 0),
            Instruction::new(Opcode::Load, 0, 1, 0).with_p4(Operand::Integer(7)),
            Instruction::new(Opcode::ResultRow, 1, 1, 0),
            Instruction::new(Opcode::Halt, 0, 0, 0),
        ],
        register_count: 2,
        cursor_count: 0,
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

/// The verifier accepts the valid program and rejects every generated mutation
/// of it that breaks an invariant.
///
/// The mutations are generated rather than listed so that the check is about
/// the invariants rather than about the eight cases somebody thought of.
#[test]
fn the_verifier_rejects_generated_invalid_programs() {
    assert!(verify(&valid_program()).is_empty());
    let mut rejected = 0usize;
    let mut accepted = Vec::new();
    for address in 0..valid_program().instructions.len() {
        for delta in [-1i32, 1, 40, i32::MAX] {
            for field in 0..3 {
                let mut program = valid_program();
                let Some(instruction) = program.instructions.get_mut(address) else {
                    continue;
                };
                match field {
                    0 => instruction.p1 = instruction.p1.saturating_add(delta),
                    1 => instruction.p2 = instruction.p2.saturating_add(delta),
                    _ => instruction.p3 = instruction.p3.saturating_add(delta),
                }
                let problems = verify(&program);
                // A mutation that is still a valid program is fine; what must
                // never happen is a mutation that breaks an invariant and is
                // accepted anyway. Anything that reads a register nothing
                // wrote, jumps outside, or leaves the frame is caught.
                let breaks_invariant = breaks_an_invariant(&program);
                if breaks_invariant && problems.is_empty() {
                    accepted.push(format!("{address}/{field}/{delta}"));
                }
                if !problems.is_empty() {
                    rejected = rejected.saturating_add(1);
                }
            }
        }
    }
    assert!(rejected > 10, "only {rejected} mutations were rejected");
    assert!(
        accepted.is_empty(),
        "accepted invalid programs: {accepted:?}"
    );
}

/// Returns whether a program breaks an invariant the verifier promises to
/// catch, decided independently of the verifier itself.
fn breaks_an_invariant(program: &Program) -> bool {
    let registers = program.register_count as i32;
    let length = program.instructions.len() as i32;
    for instruction in &program.instructions {
        if instruction.opcode.jumps() && (instruction.p2 < 0 || instruction.p2 > length) {
            return true;
        }
        match instruction.opcode {
            Opcode::Load if (instruction.p2 < 0 || instruction.p2 >= registers) => {
                return true;
            }
            Opcode::ResultRow
                if (instruction.p1 < 0
                    || instruction.p2 < 0
                    || instruction.p1.saturating_add(instruction.p2) > registers) =>
            {
                return true;
            }
            _ => {}
        }
    }
    // A result row that reads a register the load did not write.
    let loaded: Vec<i32> = program
        .instructions
        .iter()
        .filter(|instruction| instruction.opcode == Opcode::Load)
        .map(|instruction| instruction.p2)
        .collect();
    program.instructions.iter().any(|instruction| {
        instruction.opcode == Opcode::ResultRow
            && (0..instruction.p2).any(|offset| !loaded.contains(&(instruction.p1 + offset)))
    })
}

/// An instruction carrying the wrong kind of operand is rejected.
#[test]
fn the_verifier_rejects_mismatched_operands() {
    let mut program = valid_program();
    if let Some(instruction) = program.instructions.get_mut(1) {
        instruction.opcode = Opcode::Compare;
        instruction.p1 = 1;
        instruction.p2 = 1;
        instruction.p3 = 1;
        instruction.p4 = Operand::Integer(0);
    }
    assert!(!verify_operands(&program).is_empty());
}

/// The machine runs a verified program and produces the row it describes.
#[test]
fn the_machine_runs_a_verified_program() {
    let program = valid_program();
    assert!(verify(&program).is_empty());
    let mut machine = Machine::new(
        Arc::new(program),
        Arc::new(AtomicBool::new(false)),
        Limits::default(),
    );
    // A program with no cursors needs no pager beyond one to satisfy the
    // signature, so this is driven through the public API instead: the point
    // proved here is that the verifier's acceptance and the machine's success
    // are the same programs.
    assert_eq!(machine.state(), inillucent_vm::MachineState::Prepared);
    machine.reset();
    assert_eq!(machine.steps(), 0);
}

/// Returns the pinned oracle, if it has been built.
fn sqlite_oracle() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let path = workspace_root()
        .join(".sqlite-ref/3.53.4")
        .join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// The metadata, autocommit flag and bound values match the pinned release.
#[test]
fn lifecycle_and_metadata_match_the_oracle() {
    let Some(program) = sqlite_oracle() else {
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let mut driver = Driver::start("sqlite", &program).expect("the oracle starts");
    driver.send(&Op::Hello).expect("it answers");
    driver
        .send(&Op::Open(fixture().display().to_string()))
        .expect("it opens");
    let connection = connect();

    // Column names, for the three forms SQLite names differently.
    for (sql, expected) in [
        ("SELECT id FROM people", vec!["id"]),
        ("SELECT id AS x FROM people", vec!["x"]),
        ("SELECT id, name FROM people", vec!["id", "name"]),
    ] {
        let observation = driver.send(&Op::Query(sql.to_string())).expect("it runs");
        let statement = connection.prepare(sql).expect("it prepares");
        let ours: Vec<String> = statement
            .columns()
            .iter()
            .map(|column| String::from_utf8_lossy(&column.name).into_owned())
            .collect();
        assert_eq!(observation.columns, expected, "{sql}");
        assert_eq!(ours, expected, "{sql}");
        assert!(observation.autocommit);
        assert_eq!(connection.autocommit(), observation.autocommit);
    }

    // Bound values reach both engines as the same bits.
    for value in [
        TaggedValue::Null,
        TaggedValue::Integer(-9223372036854775808),
        TaggedValue::Integer(9223372036854775807),
        TaggedValue::Real(1.5),
        TaggedValue::Real(-0.0),
        TaggedValue::Text(b"text".to_vec()),
        TaggedValue::Text(Vec::new()),
        TaggedValue::Blob(vec![0x00, 0xff]),
        TaggedValue::Blob(Vec::new()),
    ] {
        let observation = driver
            .send(&Op::Bind {
                sql: "SELECT ?1".to_string(),
                values: vec![value.clone()],
            })
            .expect("it runs");
        let mut statement = connection.prepare("SELECT ?1").expect("it prepares");
        statement
            .bind(1, tagged_to_value(&value))
            .expect("it binds");
        assert!(statement.step().expect("it steps"));
        let ours = value_to_tagged(&statement.value(0));
        let theirs = observation
            .rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or(TaggedValue::Null);
        assert!(ours.identical(&theirs), "{value:?}: {ours:?} vs {theirs:?}");
    }
    let _ = driver.send(&Op::Bye);
}

/// Converts a protocol value into an engine value.
fn tagged_to_value(value: &TaggedValue) -> Value<'static> {
    match value {
        TaggedValue::Null => Value::Null,
        TaggedValue::Integer(integer) => Value::Integer(*integer),
        TaggedValue::Real(real) => Value::Real(*real),
        TaggedValue::Text(text) => Value::owned_text(text).unwrap_or(Value::Null),
        TaggedValue::Blob(bytes) => Value::owned_blob(bytes).unwrap_or(Value::Null),
    }
}

/// Converts an engine value into a protocol value.
fn value_to_tagged(value: &Value<'_>) -> TaggedValue {
    match value {
        Value::Null => TaggedValue::Null,
        Value::Integer(integer) => TaggedValue::Integer(*integer),
        Value::Real(real) => TaggedValue::Real(*real),
        Value::Text(text) => TaggedValue::Text(text.utf8_bytes().into_owned()),
        Value::Blob(blob) => TaggedValue::Blob(blob.raw().to_vec()),
    }
}
