//! Phase 1 performance instrumentation.
//!
//! Invariant: this measures, it does not optimise. Phase 1's performance work
//! is a baseline and a set of counters, so that a later phase which changes a
//! hot path has something to change it against. Nothing here is a comparison
//! against SQLite, and nothing here should be read as one.
//!
//! Three quantities are recorded, because they are the three the storage layers
//! above will spend their time in:
//!
//! - **VFS call overhead**, per operation, for each implementation. The
//!   in-memory and simulated numbers are the cost of the contract itself: the
//!   virtual call, the bounds checks, the lock acquisition. The real numbers
//!   are that plus the operating system.
//! - **Lock latency**, for each protocol transition, same-process and
//!   cross-process. A pager that takes a lock per statement pays this per
//!   statement.
//! - **Allocation counters**, from the buffer module, for a fixed workload. A
//!   later change that allocates twice per page instead of once will show here
//!   without anyone having to notice it.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-perf -- [--out <dir>]`

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use inillucent_base::buffer;
use inillucent_compat::report::json_string;
use inillucent_compat::{platform_name, workspace_root};
use inillucent_sim::sim_vfs::{SimConfig, SimVfs};
use inillucent_vfs::contract::{FileLock, OpenOptions, SyncMode, Vfs};
use inillucent_vfs::memory::MemoryVfs;
use inillucent_vfs::os::OsVfs;
use inillucent_vfs::path::DbPath;

/// How many iterations each measurement runs.
const ITERATIONS: u32 = 20_000;

/// The page size the measurements use, which is the engine's default.
const PAGE: usize = 4096;

/// One recorded measurement.
#[derive(Clone, Debug)]
struct Measurement {
    vfs: String,
    operation: String,
    iterations: u32,
    nanos_per_call: f64,
}

/// Runs the measurements and writes the artifact.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let out = flag(&arguments, "--out")
        .unwrap_or_else(|| workspace_root().join("_agent_output/engine-foundation/perf"));
    match run(&out) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the value of a `--flag value` argument.
fn flag(arguments: &[String], name: &str) -> Option<PathBuf> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).map(PathBuf::from)
}

/// Measures every VFS, then writes the artifact.
fn run(out: &Path) -> Result<String, String> {
    let mut measurements = Vec::new();
    let mut workspace = std::env::temp_dir();
    workspace.push(format!("inillucent-perf-{}", std::process::id()));
    std::fs::create_dir_all(&workspace)
        .map_err(|error| format!("cannot create a workspace: {error}"))?;

    let memory = MemoryVfs::new();
    measurements.extend(measure(&memory, &DbPath::from("/memory"))?);
    let os = OsVfs::new();
    measurements.extend(measure(&os, &DbPath::new(workspace.clone()))?);
    let simulator = SimVfs::new(SimConfig {
        seed: 1782,
        ..SimConfig::default()
    });
    measurements.extend(measure(&simulator, &DbPath::from("/sim"))?);

    let allocations = measure_allocations(&os, &DbPath::new(workspace.clone()))?;
    let _ = std::fs::remove_dir_all(&workspace);

    std::fs::create_dir_all(out)
        .map_err(|error| format!("cannot create {}: {error}", out.display()))?;
    std::fs::write(
        out.join("phase1-perf.json"),
        render_json(&measurements, &allocations),
    )
    .map_err(|error| format!("cannot write the measurements: {error}"))?;
    std::fs::write(
        out.join("phase1-perf.md"),
        render_markdown(&measurements, &allocations),
    )
    .map_err(|error| format!("cannot write the measurements: {error}"))?;
    Ok(format!(
        "{} measurements written to {}",
        measurements.len(),
        out.display()
    ))
}

/// Times one closure over `ITERATIONS` calls and returns nanoseconds per call.
fn time<F: FnMut(u32)>(mut body: F) -> f64 {
    // One warm pass first, so the measurement is not dominated by the first
    // page fault or the first allocation.
    for index in 0..64 {
        body(index);
    }
    let start = Instant::now();
    for index in 0..ITERATIONS {
        body(index);
    }
    let elapsed: Duration = start.elapsed();
    elapsed.as_nanos() as f64 / f64::from(ITERATIONS)
}

/// Measures the VFS call overhead and lock latency of one implementation.
fn measure(vfs: &dyn Vfs, root: &DbPath) -> Result<Vec<Measurement>, String> {
    let path = DbPath::new(root.as_path().join("perf.db"));
    let _ = vfs.delete(&path, false);
    let file = vfs
        .open(&path, OpenOptions::main_db())
        .map_err(|error| format!("cannot open the measurement file: {}", error.detail()))?;
    let payload = vec![0xa5u8; PAGE];
    let mut buffer = vec![0u8; PAGE];
    let pages = 64u64;
    for page in 0..pages {
        file.write_all_at(page * PAGE as u64, &payload)
            .map_err(|error| format!("cannot prepare the file: {}", error.detail()))?;
    }
    file.sync(SyncMode::Full)
        .map_err(|error| format!("cannot sync the file: {}", error.detail()))?;

    let name = vfs.name().to_string();
    let mut measurements = Vec::new();
    let mut record = |operation: &str, nanos: f64| {
        measurements.push(Measurement {
            vfs: name.clone(),
            operation: operation.to_string(),
            iterations: ITERATIONS,
            nanos_per_call: nanos,
        });
    };

    record(
        "read_exact_at(4096)",
        time(|index| {
            let page = u64::from(index % pages as u32);
            let _ = file.read_exact_at(page * PAGE as u64, &mut buffer);
        }),
    );
    record(
        "write_all_at(4096)",
        time(|index| {
            let page = u64::from(index % pages as u32);
            let _ = file.write_all_at(page * PAGE as u64, &payload);
        }),
    );
    record(
        "file_size",
        time(|_| {
            let _ = file.file_size();
        }),
    );
    record(
        "device_characteristics",
        time(|_| {
            let _ = file.device_characteristics();
        }),
    );
    record(
        "lock(SHARED)+unlock(NONE)",
        time(|_| {
            let _ = file.lock(FileLock::Shared);
            let _ = file.unlock(FileLock::None);
        }),
    );
    record(
        "lock(EXCLUSIVE)+unlock(NONE)",
        time(|_| {
            let _ = file.lock(FileLock::Shared);
            let _ = file.lock(FileLock::Exclusive);
            let _ = file.unlock(FileLock::None);
        }),
    );
    record(
        "check_reserved_lock",
        time(|_| {
            let _ = file.check_reserved_lock();
        }),
    );

    // Sync is measured over far fewer iterations: on real hardware it is
    // milliseconds, and twenty thousand of them would take an hour.
    let sync_iterations = 200u32;
    let start = Instant::now();
    for _ in 0..sync_iterations {
        let _ = file.write_all_at(0, &payload);
        let _ = file.sync(SyncMode::Full);
    }
    measurements.push(Measurement {
        vfs: name.clone(),
        operation: "write+sync(FULL)".to_string(),
        iterations: sync_iterations,
        nanos_per_call: start.elapsed().as_nanos() as f64 / f64::from(sync_iterations),
    });

    drop(file);
    let _ = vfs.delete(&path, false);
    Ok(measurements)
}

/// Measures how much the buffer layer allocates for a fixed page workload.
fn measure_allocations(vfs: &dyn Vfs, root: &DbPath) -> Result<(u64, u64, u64), String> {
    let path = DbPath::new(root.as_path().join("alloc.db"));
    let _ = vfs.delete(&path, false);
    let file = vfs
        .open(&path, OpenOptions::main_db())
        .map_err(|error| format!("cannot open the allocation file: {}", error.detail()))?;
    let size = inillucent_base::page::PageSize::DEFAULT;
    let pages = 1024u64;
    let before = buffer::allocation_stats();
    for page in 0..pages {
        let mut buffer = buffer::PageBuffer::zeroed(size)
            .map_err(|error| format!("cannot allocate a page: {}", error.message()))?;
        buffer
            .write_u32(0, page as u32)
            .map_err(|error| format!("cannot write a page: {}", error.message()))?;
        file.write_all_at(page * PAGE as u64, buffer.as_slice())
            .map_err(|error| format!("cannot write a page: {}", error.detail()))?;
    }
    let delta = buffer::allocation_stats().since(before);
    drop(file);
    let _ = vfs.delete(&path, false);
    Ok((pages, delta.buffers, delta.bytes))
}

/// Renders the measurements as JSON.
fn render_json(measurements: &[Measurement], allocations: &(u64, u64, u64)) -> String {
    let (pages, buffers, bytes) = *allocations;
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"platform\": {},\n",
        json_string(&platform_name())
    ));
    out.push_str(&format!("  \"page_size\": {PAGE},\n"));
    out.push_str("  \"allocations\": {\n");
    out.push_str(&format!("    \"pages_written\": {pages},\n"));
    out.push_str(&format!("    \"buffers_allocated\": {buffers},\n"));
    out.push_str(&format!("    \"bytes_allocated\": {bytes}\n"));
    out.push_str("  },\n  \"measurements\": [\n");
    for (index, measurement) in measurements.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        out.push_str(&format!(
            "    {{\"vfs\": {}, \"operation\": {}, \"iterations\": {}, \"nanos_per_call\": {:.1}}}",
            json_string(&measurement.vfs),
            json_string(&measurement.operation),
            measurement.iterations,
            measurement.nanos_per_call
        ));
    }
    out.push_str("\n  ]\n}\n");
    out
}

/// Renders the measurements as a table.
fn render_markdown(measurements: &[Measurement], allocations: &(u64, u64, u64)) -> String {
    let (pages, buffers, bytes) = *allocations;
    let mut out = String::new();
    out.push_str("# Phase 1 performance baseline\n\n");
    out.push_str(&format!(
        "Platform: `{}`. Page size: {PAGE} bytes.\n\n",
        platform_name()
    ));
    out.push_str(
        "This is instrumentation, not optimisation. Nothing here is a comparison against\n",
    );
    out.push_str("SQLite, and nothing here should be read as one: the engine has no pager yet.\n");
    out.push_str("What it establishes is what one VFS call costs today, so a later phase that\n");
    out.push_str("changes a hot path has a number to change it against.\n\n");
    out.push_str("## VFS call overhead and lock latency\n\n");
    out.push_str("| vfs | operation | iterations | ns/call |\n|---|---|---:|---:|\n");
    for measurement in measurements {
        out.push_str(&format!(
            "| {} | `{}` | {} | {:.1} |\n",
            measurement.vfs,
            measurement.operation,
            measurement.iterations,
            measurement.nanos_per_call
        ));
    }
    out.push_str("\n## Allocation counters\n\n");
    out.push_str(&format!(
        "Writing {pages} pages through `PageBuffer` allocated **{buffers} buffers** and\n**{bytes} bytes** - {:.1} bytes per page, against a {PAGE}-byte page.\n",
        bytes as f64 / pages as f64
    ));
    out.push_str("\nA later change that allocates twice per page, or that starts allocating a\n");
    out.push_str("scratch buffer per read, moves this number without anyone having to notice.\n");
    out
}
