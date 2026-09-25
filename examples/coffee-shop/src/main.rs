//! `coffee-server`: orders, stock and bookkeeping for a coffee shop, as a REST
//! API on one inillucent database.
//!
//! Two commands:
//!
//! | Command | What it does |
//! |---|---|
//! | `serve` | opens the database, creating it if needed, and answers HTTP requests |
//! | `seed` | fills a new database with a menu, stock, staff, customers and a week of trading, and prints a summary |
//!
//! The modules, in the order a request travels through them:
//!
//! ```text
//! routes/*.rs   reads the request and calls one Store method
//! store/*.rs    runs the SQL against inillucent
//! error.rs      turns a failure into an HTTP status and a JSON body
//! schema.rs     the tables, indexes, triggers and views the SQL runs against
//! ```

mod error;
mod routes;
mod schema;
mod seed;
mod store;

use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::store::Store;

/// The command line.
#[derive(Parser)]
#[command(name = "coffee-server", version, about = "Orders, stock and bookkeeping for a coffee shop, backed by an inillucent database")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The two commands.
#[derive(Subcommand)]
enum Command {
    /// Answer HTTP requests.
    Serve {
        /// The database file. Created, with its schema, when it does not exist.
        #[arg(long, default_value = "data/coffee.rdb")]
        db: PathBuf,
        /// The address to listen on. Port 0 picks a free port, and the line
        /// printed at start names it.
        #[arg(long, default_value = "127.0.0.1:3000")]
        addr: String,
    },
    /// Fill a new database with demo data: a menu, stock, staff, customers and
    /// a week of orders, purchases and closed days.
    Seed {
        /// The database file. It must not have any orders yet.
        #[arg(long, default_value = "data/coffee.rdb")]
        db: PathBuf,
        /// How many days of trading to make up, ending the day before `--until`.
        #[arg(long, default_value_t = 7)]
        days: i64,
        /// The day after the last day of trading. The seed data is the same
        /// every time for the same `--until` and `--days`.
        #[arg(long, default_value = "2026-09-28")]
        until: String,
    },
}

/// Parses the command line and runs the command.
fn main() {
    let cli = Cli::parse();
    let outcome = match cli.command {
        Command::Serve { db, addr } => serve(db, addr),
        Command::Seed { db, days, until } => run_seed(db, days, until),
    };
    if let Err(message) = outcome {
        eprintln!("coffee-server: {message}");
        std::process::exit(1);
    }
}

/// Opens the database and serves the API until the process is stopped.
///
/// The first line on stdout is `listening on http://<address>`, printed once
/// the socket is bound. The end to end tests start the server on port 0 and
/// read that line to learn which port it got.
///
/// @param db - the database file
/// @param addr - the address to listen on
fn serve(db: PathBuf, addr: String) -> Result<(), String> {
    let store = Store::open(&db)?;
    let runtime = tokio::runtime::Runtime::new().map_err(|error| format!("cannot start the async runtime: {error}"))?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|error| format!("cannot listen on {addr}: {error}"))?;
        let bound = listener.local_addr().map_err(|error| error.to_string())?;
        println!("listening on http://{bound}");
        std::io::stdout().flush().map_err(|error| error.to_string())?;
        axum::serve(listener, routes::router(store))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .map_err(|error| format!("the server stopped: {error}"))
    })
}

/// Adds the demo data and prints a summary of it, as JSON.
///
/// @param db - the database file
/// @param days - how many days of trading to make up
/// @param until - the day after the last day of trading
fn run_seed(db: PathBuf, days: i64, until: String) -> Result<(), String> {
    let store = Store::open(&db)?;
    let seeded = seed::seed(&store, days, &until).map_err(|error| error.message)?;
    println!("{}", serde_json::to_string_pretty(&seeded).map_err(|error| error.to_string())?);
    Ok(())
}
