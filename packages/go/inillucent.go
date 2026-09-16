// Package inillucent drives the inillucent command line from Go and returns
// its results as typed values.
//
// # What this is, and what it is not
//
// This is a wrapper over the `inillucent` binary, not a database driver. Each
// call is a process. That makes it the right tool for the handful of calls a
// build step, a migration script or an agent harness makes, and the wrong tool
// for a loop over a million rows.
//
// It is pure Go with no cgo, which is what lets it build for any target
// without a C toolchain and cross-compile the way the rest of a Go project
// does. A real in-process binding is a worthwhile thing to have and it is a
// different piece of work: it goes through the C ABI in
// drivers/inillucent-driver-capi/include/inillucent_driver.h, with cgo or with
// purego, and drivers/README.md is written to be followed by somebody doing
// exactly that. Writing an unverified cgo binding here and calling it a driver
// would be worse than not having one.
//
// # Getting the binary
//
//	go install github.com/Black-Rainbow-Labs/Inillucent/packages/go/cmd/inillucent-install@latest
//
// installs a small program that downloads the inillucent release for this
// machine, verifies its SHA-256 and puts the four binaries in GOBIN. After that
// this package finds them on PATH.
//
// # Errors that are answers
//
// A refusal comes back as an [Error] carrying a [Status]. StatusUnsupported
// means the engine has not built that construct - it is not a syntax error and
// rewording the statement will not help. Handling the two together throws away
// the distinction this engine's driver was designed around.
package inillucent

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"strings"
)

// Status is the class of a failure, as inillucent's driver names them. The same
// thirteen words a C binding reads out of inillucent_error_status.
type Status string

// The statuses. StatusUnsupported is the one worth branching on: it is the
// engine saying it has not built a construct, which is a different thing from
// the statement being wrong.
const (
	StatusUnsupported  Status = "unsupported"
	StatusSyntax       Status = "syntax"
	StatusNotFound     Status = "not_found"
	StatusConstraint   Status = "constraint"
	StatusReadOnly     Status = "readonly"
	StatusBusy         Status = "busy"
	StatusInterrupted  Status = "interrupted"
	StatusCorrupt      Status = "corrupt"
	StatusIO           Status = "io"
	StatusFull         Status = "full"
	StatusTooBig       Status = "too_big"
	StatusInvalidState Status = "invalid_state"
	StatusInternal     Status = "internal"
)

// Column is one result column: the name the statement gave it, and the storage
// class its values turned out to have.
//
// Observed rather than declared, because SQLite's typing is dynamic: a column's
// declared affinity says what it will probably store, and this says what it did.
type Column struct {
	Name string `json:"name"`
	Type string `json:"type"`
}

// Result is what a command produced. It is the command line's `--output json`
// contract, unchanged, so the fields here are the fields a shell script or a
// Python caller sees.
type Result struct {
	OK              bool            `json:"ok"`
	Command         string          `json:"command"`
	Columns         []Column        `json:"columns"`
	Rows            [][]any         `json:"rows"`
	RowCount        int             `json:"row_count"`
	Total           int             `json:"total"`
	More            bool            `json:"more"`
	Changes         int64           `json:"changes"`
	LastInsertRowID int64           `json:"last_insert_rowid"`
	ElapsedMS       float64         `json:"elapsed_ms"`
	Text            string          `json:"text"`
	Status          Status          `json:"status"`
	Message         string          `json:"message"`
	Feature         string          `json:"feature"`
	raw             json.RawMessage `json:"-"`
}

// Field returns a member of the result object that this struct does not name.
//
// Some commands add their own: describe carries `ddl`, `indexes` and
// `row_count_in_table`; stats carries the cache counters. Naming every one of
// them here would make this struct a list that goes stale, so the raw document
// is kept and this reaches into it.
func (result Result) Field(name string, into any) error {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(result.raw, &members); err != nil {
		return err
	}
	held, ok := members[name]
	if !ok {
		return fmt.Errorf("inillucent: %s did not report a %q", result.Command, name)
	}
	return json.Unmarshal(held, into)
}

// Maps returns the rows as maps keyed by column name, which is the shape most
// callers want.
func (result Result) Maps() []map[string]any {
	out := make([]map[string]any, 0, len(result.Rows))
	for _, row := range result.Rows {
		held := make(map[string]any, len(result.Columns))
		for at, column := range result.Columns {
			if at < len(row) {
				held[column.Name] = row[at]
			}
		}
		out = append(out, held)
	}
	return out
}

// Error is a refusal the engine or the command line reported.
type Error struct {
	Status  Status
	Message string
	Feature string
	Command string
}

// Error makes this an error, naming the class as well as the message so a log
// line says which kind of failure it was.
func (e *Error) Error() string {
	if e.Feature != "" {
		return fmt.Sprintf("inillucent %s [%s]: %s (not built: %s)", e.Command, e.Status, e.Message, e.Feature)
	}
	return fmt.Sprintf("inillucent %s [%s]: %s", e.Command, e.Status, e.Message)
}

// Unsupported reports whether an error is the engine saying it has not built
// the construct, rather than the statement being wrong.
//
//	if inillucent.Unsupported(err) {
//	        // do not offer that feature; rewording will not help
//	}
func Unsupported(err error) bool {
	held, ok := err.(*Error)
	return ok && held.Status == StatusUnsupported
}

// DB is a database, addressed by its path. It is not a connection and holds
// nothing open: every call is a process, so a DB is safe to share across
// goroutines and safe to keep for the life of a program.
type DB struct {
	// Path is the database file. Empty means an in-memory database, which is
	// discarded when the process running the command ends - so it is only
	// useful for a single Batch.
	Path string
	// Binary is the inillucent program to run. Empty means "inillucent", found
	// on PATH.
	Binary string
	// ReadOnly refuses every statement that changes something, classified by
	// the binder rather than by scanning the text.
	ReadOnly bool
	// Root, when set, refuses every path outside that directory.
	Root string
}

// Open returns a DB for a database file. It does no I/O: the file is opened by
// whichever command runs first, and a path that does not exist is created by
// the first command that writes.
func Open(path string) *DB {
	return &DB{Path: path}
}

// binary returns the program to run.
func (db *DB) binary() string {
	if db.Binary != "" {
		return db.Binary
	}
	if named := os.Getenv("INILLUCENT_BIN"); named != "" {
		return named
	}
	return "inillucent"
}

// Run executes one inillucent command and returns its result.
//
// The arguments are the named ones that verb takes: Run(ctx, "describe",
// Args{"table": "people"}). A refusal is returned as an *Error rather than in
// the Result, so the ordinary `if err != nil` shape works; the Result is
// returned too, for the callers that want the timing or the raw text.
func (db *DB) Run(ctx context.Context, command string, arguments Args) (Result, error) {
	argv := []string{command, "--output", "json"}
	if db.Path != "" {
		argv = append(argv, "--db", db.Path)
	}
	if db.ReadOnly {
		argv = append(argv, "--readonly")
	}
	if db.Root != "" {
		argv = append(argv, "--root", db.Root)
	}
	argv = append(argv, arguments.flags()...)

	run := exec.CommandContext(ctx, db.binary(), argv...)
	output, runErr := run.Output()
	// A refusal exits non-zero *and* prints the result object, because the
	// caller asked for JSON. So the exit status is not the thing to read - the
	// document is - and only an invocation with nothing to parse is a real
	// failure to run.
	trimmed := strings.TrimSpace(string(output))
	if !strings.HasPrefix(trimmed, "{") {
		if runErr != nil {
			var exited *exec.ExitError
			if ok := asExitError(runErr, &exited); ok && len(exited.Stderr) > 0 {
				return Result{}, fmt.Errorf("inillucent %s: %s", command, strings.TrimSpace(string(exited.Stderr)))
			}
			return Result{}, fmt.Errorf("inillucent %s: %w", command, runErr)
		}
		return Result{}, fmt.Errorf("inillucent %s produced no result object", command)
	}

	var result Result
	if err := json.Unmarshal([]byte(trimmed), &result); err != nil {
		return Result{}, fmt.Errorf("inillucent %s: could not read its result: %w", command, err)
	}
	result.raw = json.RawMessage(trimmed)
	if !result.OK {
		return result, &Error{
			Status:  result.Status,
			Message: result.Message,
			Feature: result.Feature,
			Command: command,
		}
	}
	return result, nil
}

// asExitError is errors.As, spelled out so this package imports nothing beyond
// the standard library's smallest useful set.
func asExitError(err error, into **exec.ExitError) bool {
	held, ok := err.(*exec.ExitError)
	if ok {
		*into = held
	}
	return ok
}

// Args are a command's named arguments. A []any is serialised as JSON, which is
// what `params` and `vector` expect; a bool becomes a bare flag when true and is
// left out when false.
type Args map[string]any

// flags renders the arguments as command-line words.
func (args Args) flags() []string {
	out := make([]string, 0, len(args)*2)
	for name, value := range args {
		flag := "--" + strings.ReplaceAll(name, "_", "-")
		switch held := value.(type) {
		case nil:
			continue
		case bool:
			if held {
				out = append(out, flag)
			}
		case string:
			out = append(out, flag, held)
		case int:
			out = append(out, flag, strconv.Itoa(held))
		case int64:
			out = append(out, flag, strconv.FormatInt(held, 10))
		case float64:
			out = append(out, flag, strconv.FormatFloat(held, 'g', -1, 64))
		default:
			// Everything else - a slice of values for `params`, a vector - is a
			// JSON argument, and the command line reads it as JSON.
			encoded, err := json.Marshal(held)
			if err != nil {
				continue
			}
			out = append(out, flag, string(encoded))
		}
	}
	return out
}

// Query runs a statement that returns rows.
//
//	rows, err := db.Query(ctx, "SELECT id, name FROM people WHERE id > ?1", 3)
func (db *DB) Query(ctx context.Context, sql string, params ...any) (Result, error) {
	arguments := Args{"sql": sql}
	if len(params) > 0 {
		arguments["params"] = params
	}
	return db.Run(ctx, "query", arguments)
}

// Exec runs one statement for its effect and returns how many rows it changed.
func (db *DB) Exec(ctx context.Context, sql string, params ...any) (int64, error) {
	arguments := Args{"sql": sql}
	if len(params) > 0 {
		arguments["params"] = params
	}
	result, err := db.Run(ctx, "exec", arguments)
	return result.Changes, err
}

// Batch runs several statements separated by semicolons, as one transaction.
// Either all of them take effect or none of them do.
func (db *DB) Batch(ctx context.Context, sql string) error {
	_, err := db.Run(ctx, "batch", Args{"sql": sql})
	return err
}

// Describe returns everything about one table in a single call: its columns,
// its indexes, its DDL and how many rows it holds.
func (db *DB) Describe(ctx context.Context, table string) (Result, error) {
	return db.Run(ctx, "describe", Args{"table": table})
}

// Tables lists the tables and views.
func (db *DB) Tables(ctx context.Context) (Result, error) {
	return db.Run(ctx, "tables", nil)
}

// Search runs a full-text query over an FTS5 or inillucent_search table.
func (db *DB) Search(ctx context.Context, table, query string, k int) (Result, error) {
	arguments := Args{"table": table, "query": query}
	if k > 0 {
		arguments["k"] = k
	}
	return db.Run(ctx, "search", arguments)
}

// Version returns what the installed inillucent calls itself, which is also the
// cheapest way to check that the binary is on PATH and runnable.
func Version(ctx context.Context) (string, error) {
	db := &DB{}
	result, err := db.Run(ctx, "version", nil)
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(result.Text), nil
}
