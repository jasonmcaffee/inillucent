package inillucent_test

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"testing"

	"github.com/Black-Rainbow-Labs/Inillucent/packages/go"
)

// skipWithoutBinary skips a test when inillucent is not installed, and says how
// to get it rather than failing on a machine that never had it.
//
// A test that fails because a tool is missing teaches somebody to ignore the
// test. A test that skips with an instruction teaches them to install the tool.
func skipWithoutBinary(t *testing.T) {
	t.Helper()
	if named := os.Getenv("INILLUCENT_BIN"); named != "" {
		if _, err := os.Stat(named); err == nil {
			return
		}
	}
	if _, err := exec.LookPath("inillucent"); err != nil {
		t.Skip("inillucent is not on PATH; install it with " +
			"`go run ./cmd/inillucent` or see https://github.com/Black-Rainbow-Labs/Inillucent#install")
	}
}

// A database can be created, written to and read back.
func TestRoundTrip(t *testing.T) {
	skipWithoutBinary(t)
	ctx := context.Background()
	db := inillucent.Open(filepath.Join(t.TempDir(), "probe.rdb"))

	if err := db.Batch(ctx, `
		CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
		INSERT INTO people VALUES (1, 'Ada', 36), (2, 'Grace', 45), (3, 'Alan', 41)
	`); err != nil {
		t.Fatalf("batch: %v", err)
	}

	result, err := db.Query(ctx, "SELECT name FROM people WHERE age > ?1 ORDER BY age", 40)
	if err != nil {
		t.Fatalf("query: %v", err)
	}
	if result.Total != 2 {
		t.Fatalf("expected 2 rows, got %d", result.Total)
	}
	rows := result.Maps()
	if rows[0]["name"] != "Alan" || rows[1]["name"] != "Grace" {
		t.Fatalf("wrong rows or wrong order: %v", rows)
	}
}

// A parameter is bound rather than pasted, which is what makes a quote safe.
func TestBindingIsNotInterpolation(t *testing.T) {
	skipWithoutBinary(t)
	ctx := context.Background()
	db := inillucent.Open(filepath.Join(t.TempDir(), "probe.rdb"))
	if err := db.Batch(ctx, "CREATE TABLE t (a TEXT)"); err != nil {
		t.Fatalf("create: %v", err)
	}
	hostile := "o'brien'; DROP TABLE t; --"
	if _, err := db.Exec(ctx, "INSERT INTO t VALUES (?1)", hostile); err != nil {
		t.Fatalf("insert: %v", err)
	}
	result, err := db.Query(ctx, "SELECT a FROM t")
	if err != nil {
		t.Fatalf("query: %v", err)
	}
	rows := result.Maps()
	if len(rows) != 1 || rows[0]["a"] != hostile {
		t.Fatalf("the value did not survive binding: %v", rows)
	}
}

// A missing table is not_found, which is a different thing from a bad statement.
func TestStatusIsClassified(t *testing.T) {
	skipWithoutBinary(t)
	ctx := context.Background()
	db := inillucent.Open(filepath.Join(t.TempDir(), "probe.rdb"))
	if err := db.Batch(ctx, "CREATE TABLE t (a)"); err != nil {
		t.Fatalf("create: %v", err)
	}
	_, err := db.Query(ctx, "SELECT * FROM absent")
	if err == nil {
		t.Fatal("a query against a missing table should have failed")
	}
	held, ok := err.(*inillucent.Error)
	if !ok {
		t.Fatalf("expected an *inillucent.Error, got %T", err)
	}
	if held.Status != inillucent.StatusNotFound {
		t.Fatalf("expected not_found, got %q (%s)", held.Status, held.Message)
	}
	if inillucent.Unsupported(err) {
		t.Fatal("a missing table is not an unsupported construct")
	}
}

// Describe answers in one call, including the fields this package does not name.
func TestDescribeCarriesItsExtraFields(t *testing.T) {
	skipWithoutBinary(t)
	ctx := context.Background()
	db := inillucent.Open(filepath.Join(t.TempDir(), "probe.rdb"))
	if err := db.Batch(ctx, "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)"); err != nil {
		t.Fatalf("create: %v", err)
	}
	result, err := db.Describe(ctx, "notes")
	if err != nil {
		t.Fatalf("describe: %v", err)
	}
	var ddl string
	if err := result.Field("ddl", &ddl); err != nil {
		t.Fatalf("describe did not carry its ddl: %v", err)
	}
	if ddl == "" {
		t.Fatal("the ddl is empty")
	}
}

// A read-only database refuses a write, classified by the binder.
func TestReadOnlyRefusesAWrite(t *testing.T) {
	skipWithoutBinary(t)
	ctx := context.Background()
	path := filepath.Join(t.TempDir(), "probe.rdb")
	if err := inillucent.Open(path).Batch(ctx, "CREATE TABLE t (a)"); err != nil {
		t.Fatalf("create: %v", err)
	}
	locked := &inillucent.DB{Path: path, ReadOnly: true}
	if _, err := locked.Exec(ctx, "INSERT INTO t VALUES (1)"); err == nil {
		t.Fatal("a read-only database accepted a write")
	} else if held, ok := err.(*inillucent.Error); !ok || held.Status != inillucent.StatusReadOnly {
		t.Fatalf("expected readonly, got %v", err)
	}
	// And a read still works, which is what makes it read-only rather than shut.
	if _, err := locked.Query(ctx, "SELECT count(*) FROM t"); err != nil {
		t.Fatalf("a read-only database refused a read: %v", err)
	}
}
