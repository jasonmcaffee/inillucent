//! A diagnostic shell: run SQL against inillucent and print what it says.
//!
//! Invariant: it reports, and never interprets. Every failure is printed with
//! its whole `DbError` - the extended code, the safe message and the detail -
//! because the detail is where the reason lives and every other path in this
//! workspace deliberately hides it. When a differential comparison says only
//! that inillucent failed, this is how the reason is found.
//!
//! It is a harness tool rather than a shell. The real one is `inillucent-cli`; this
//! one exists so that a scenario a test found can be replayed by hand without
//! the test around it.

use inillucent_engine::connect::Database;

/// Reads SQL from standard input, runs each statement, and prints the outcome.
fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| ":memory:".into());
    // A second argument means "open what is there", which is how a file a
    // scenario already wrote is read back.
    if std::env::args().nth(2).is_none() && path != ":memory:" {
        let _ = std::fs::remove_file(&path);
    }
    let database = Database::open(&path).expect("opens");
    let connection = database.connect();
    let mut sql = String::new();
    for line in std::io::stdin().lines() {
        sql.push_str(&line.expect("reads"));
        sql.push('\n');
    }
    for statement in sql.split(";\n") {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }
        run(&connection, statement);
    }
}

/// Runs one statement and prints its rows, or the whole error it failed with.
fn run(connection: &inillucent_engine::connect::Connection<'_>, statement: &str) {
    match connection.prepare(statement) {
        Err(error) => println!("PREPARE FAILED {statement}\n  {error:?}"),
        Ok(mut prepared) => {
            let mut rows = Vec::new();
            loop {
                match prepared.step() {
                    Err(error) => {
                        println!("STEP FAILED {statement}\n  {error:?}");
                        return;
                    }
                    Ok(false) => break,
                    Ok(true) => rows.push(format!("{:?}", prepared.row())),
                }
            }
            println!("OK {statement}");
            for row in rows {
                println!("   {row}");
            }
        }
    }
}
