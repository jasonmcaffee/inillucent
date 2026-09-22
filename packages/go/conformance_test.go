// The shared conformance suite, run through the Go wrapper.
//
// **One specification, four runners.** `drivers/conformance/suite.json` is what
// "the binding is correct" means, and until task-2036 only two things read it:
// the Rust driver and the Python binding. npm, Go and PHP each had a round trip
// of their own - three to eight cases, written by hand, agreeing with nothing.
// A binding graded against a suite it does not run is a binding graded against
// itself.
//
// **What this wrapper cannot do.** The Go package spawns the command line:
// every call is a process, so a transaction, a savepoint or a temporary table
// cannot outlive one step. That is the `session` capability, and
// `skipped_by.go` in the suite names it with the reason. Five of the
// thirty-two cases need it; the other twenty-seven run here.
//
// Rows *do* outlive a step - they are in the file - and so do bytes: the
// command line binds a blob as `{"blob":"<hex>"}` and renders one back as the
// text `x'<hex>'`.
//
// `tooling::every_binding_runs_the_whole_suite` reads
// `_agent_output/conformance/go.json`, which this writes.
package inillucent_test

import (
	"strings"
	"context"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"testing"

	inillucent "github.com/Black-Rainbow-Labs/Inillucent/packages/go"
)

// language is how the suite's `skipped_by` names this runner.
const language = "go"

// suiteFile is the specification, as a file.
type suiteFile struct {
	Version   int `json:"version"`
	SkippedBy map[string]struct {
		Lacks []string `json:"lacks"`
		Why   string   `json:"why"`
	} `json:"skipped_by"`
	Cases []suiteCase `json:"cases"`
}

// suiteCase is one case: a schema, then steps against it.
type suiteCase struct {
	Name  string     `json:"name"`
	Group string     `json:"group"`
	Needs []string   `json:"needs"`
	Setup []string   `json:"setup"`
	Steps []suiteStep `json:"steps"`
}

// suiteStep asserts whatever keys it carries and ignores the rest.
type suiteStep struct {
	SQL             string              `json:"sql"`
	Params          []map[string]any    `json:"params"`
	Limit           *int                `json:"limit"`
	Columns         []string            `json:"columns"`
	Rows            [][]map[string]any  `json:"rows"`
	Affected        *json.RawMessage    `json:"affected"`
	Total           *int                `json:"total"`
	More            *bool               `json:"more"`
	Status          string              `json:"status"`
	MessageContains string              `json:"message_contains"`
	FeatureContains string              `json:"feature_contains"`
}

// record is what this runner writes out for the guard to read.
type record struct {
	Language string        `json:"language"`
	Lacks    []string      `json:"lacks"`
	Ran      []string      `json:"ran"`
	Skipped  []skippedCase `json:"skipped"`
	Failures []string      `json:"failures"`
}

// skippedCase names a case this runner cannot do, and which capability it
// needed.
type skippedCase struct {
	Name  string   `json:"name"`
	Group string   `json:"group"`
	Needs []string `json:"needs"`
}

// repositoryRoot walks up from this package to the workspace root.
func repositoryRoot(t *testing.T) string {
	t.Helper()
	here, err := os.Getwd()
	if err != nil {
		t.Fatalf("no working directory: %v", err)
	}
	return filepath.Dir(filepath.Dir(here))
}

// readSuite reads the specification.
func readSuite(t *testing.T, root string) suiteFile {
	t.Helper()
	raw, err := os.ReadFile(filepath.Join(root, "drivers", "conformance", "suite.json"))
	if err != nil {
		t.Fatalf("the conformance suite is not readable: %v", err)
	}
	var suite suiteFile
	if err := json.Unmarshal(raw, &suite); err != nil {
		t.Fatalf("the conformance suite is not JSON this runner can read: %v", err)
	}
	if len(suite.Cases) == 0 {
		t.Fatal("the conformance suite holds no cases, so this runner would grade nothing")
	}
	return suite
}

// bound converts the suite's one-key value form into what `params` takes.
//
// A blob becomes `{"blob":"<hex>"}`, which is how bytes reach a wrapper that
// can only put text on a command line.
func bound(described map[string]any) any {
	if _, ok := described["null"]; ok {
		return nil
	}
	if value, ok := described["int"]; ok {
		return value
	}
	if value, ok := described["real"]; ok {
		return value
	}
	if value, ok := described["text"]; ok {
		return value
	}
	if value, ok := described["blob"]; ok {
		return map[string]string{"blob": hexOf(value)}
	}
	return nil
}

// hexOf renders a byte array as lowercase hex, whichever shape it arrives in.
//
// The suite writes a blob as a JSON array of numbers, which is []any after the
// decoder; a step that wrote it as hexadecimal is a string; and Maps() hands
// back []byte once the envelope has been decoded. All three are the same bytes
// and all three have to compare (task-2066 section 4.1.15).
func hexOf(value any) string {
	switch held := value.(type) {
	case []any:
		out := make([]byte, 0, len(held))
		for _, one := range held {
			number, ok := one.(float64)
			if !ok {
				continue
			}
			out = append(out, byte(int(number)))
		}
		return hex.EncodeToString(out)
	case []byte:
		return hex.EncodeToString(held)
	case string:
		return strings.ToLower(held)
	default:
		return ""
	}
}

// same compares an expected value to what the command line answered.
//
// Every number arrives as a float64 out of Go's JSON reader, so an integer is
// compared by its text - which is exact for the values past 2^53 this suite
// deliberately carries.
func same(want map[string]any, got any) bool {
	if _, ok := want["null"]; ok {
		return got == nil
	}
	if value, ok := want["int"]; ok {
		return rendered(value) == rendered(got)
	}
	if value, ok := want["real"]; ok {
		wanted, one := value.(float64)
		answered, other := got.(float64)
		if one && other {
			return wanted == answered || (math.IsNaN(wanted) && math.IsNaN(answered))
		}
		return rendered(value) == rendered(got)
	}
	if value, ok := want["text"]; ok {
		return value == got
	}
	if value, ok := want["blob"]; ok {
		// The envelope, not the old x'..' string (task-2066 section 4.1.15). A
		// blob used to come back as text, so bytes could be written and not
		// read - and this comparison agreed with it, which is why the
		// round-trip case passed throughout. Both sides are compared as
		// hexadecimal, which is exact.
		wanted := strings.ToLower(hexOf(value))
		if held, ok := got.(map[string]any); ok {
			if hex, ok := held["blob"].(string); ok {
				return strings.ToLower(hex) == wanted
			}
		}
		// Maps() decodes the envelope to []byte, so a step read through the
		// wrapper arrives as bytes rather than as a map.
		if bytes, ok := got.([]byte); ok {
			return strings.ToLower(hexOf(bytes)) == wanted
		}
		return false
	}
	return false
}

// rendered turns a number into the text that compares exactly.
func rendered(value any) string {
	if number, ok := value.(float64); ok && number == math.Trunc(number) && math.Abs(number) < 1e18 {
		return fmt.Sprintf("%d", int64(number))
	}
	return fmt.Sprintf("%v", value)
}

// step runs one statement and returns the command's report.
//
// `query` when the step names a limit, because `--limit` is that verb's and the
// exact total beside a cut result is what such a case is about; `exec`
// otherwise, because it answers reads and writes alike.
func step(ctx context.Context, db *inillucent.DB, one suiteStep) (inillucent.Result, error) {
	arguments := inillucent.Args{"sql": one.SQL}
	if len(one.Params) > 0 {
		values := make([]any, 0, len(one.Params))
		for _, described := range one.Params {
			values = append(values, bound(described))
		}
		arguments["params"] = values
	}
	if one.Limit != nil {
		arguments["limit"] = *one.Limit
		return db.Run(ctx, "query", arguments)
	}
	return db.Run(ctx, "exec", arguments)
}

// grade runs one case and returns what did not hold.
func grade(ctx context.Context, db *inillucent.DB, one suiteCase) []string {
	wrong := []string{}
	for _, statement := range one.Setup {
		if _, err := db.Run(ctx, "exec", inillucent.Args{"sql": statement}); err != nil {
			return append(wrong, fmt.Sprintf("%s: the setup `%s` failed: %v", one.Name, statement, err))
		}
	}
	for _, current := range one.Steps {
		result, err := step(ctx, db, current)

		if current.Status != "" {
			if err == nil {
				wrong = append(wrong, fmt.Sprintf(
					"%s: `%s` succeeded and should have failed with `%s`", one.Name, current.SQL, current.Status))
				continue
			}
			if string(result.Status) != current.Status {
				wrong = append(wrong, fmt.Sprintf(
					"%s: `%s` failed with `%s` and should have failed with `%s`",
					one.Name, current.SQL, result.Status, current.Status))
			}
			if current.MessageContains != "" && !contains(result.Message, current.MessageContains) {
				wrong = append(wrong, fmt.Sprintf(
					"%s: `%s` said `%s`, which does not contain `%s`",
					one.Name, current.SQL, result.Message, current.MessageContains))
			}
			if current.FeatureContains != "" && !contains(result.Feature, current.FeatureContains) {
				wrong = append(wrong, fmt.Sprintf(
					"%s: `%s` named the feature `%s`, which does not contain `%s`",
					one.Name, current.SQL, result.Feature, current.FeatureContains))
			}
			continue
		}

		if err != nil {
			wrong = append(wrong, fmt.Sprintf("%s: `%s` failed: %v", one.Name, current.SQL, err))
			continue
		}
		if current.Columns != nil {
			names := make([]string, 0, len(result.Columns))
			for _, column := range result.Columns {
				names = append(names, column.Name)
			}
			if fmt.Sprint(names) != fmt.Sprint(current.Columns) {
				wrong = append(wrong, fmt.Sprintf(
					"%s: `%s` answered columns %v, wanted %v", one.Name, current.SQL, names, current.Columns))
			}
		}
		if current.Rows != nil {
			if len(result.Rows) != len(current.Rows) {
				wrong = append(wrong, fmt.Sprintf(
					"%s: `%s` answered %d rows, wanted %d",
					one.Name, current.SQL, len(result.Rows), len(current.Rows)))
			} else {
				for at, wantRow := range current.Rows {
					for column, wantCell := range wantRow {
						if column >= len(result.Rows[at]) {
							wrong = append(wrong, fmt.Sprintf(
								"%s: `%s`: row %d has no column %d", one.Name, current.SQL, at, column))
							continue
						}
						if !same(wantCell, result.Rows[at][column]) {
							wrong = append(wrong, fmt.Sprintf(
								"%s: `%s`: row %d column %d is %v and should be %v",
								one.Name, current.SQL, at, column, result.Rows[at][column], wantCell))
						}
					}
				}
			}
		}
		if current.Total != nil && result.Total != *current.Total {
			wrong = append(wrong, fmt.Sprintf(
				"%s: `%s` reported total %d, wanted %d", one.Name, current.SQL, result.Total, *current.Total))
		}
		if current.More != nil && result.More != *current.More {
			wrong = append(wrong, fmt.Sprintf(
				"%s: `%s` reported more=%v, wanted %v", one.Name, current.SQL, result.More, *current.More))
		}
		if current.Affected != nil {
			var affected any
			if err := json.Unmarshal(*current.Affected, &affected); err == nil && affected != nil {
				if wanted, ok := affected.(float64); ok && result.Changes != int64(wanted) {
					wrong = append(wrong, fmt.Sprintf(
						"%s: `%s` reported %d changed, wanted %d",
						one.Name, current.SQL, result.Changes, int64(wanted)))
				}
			}
		}
	}
	return wrong
}

// contains is `strings.Contains` without the import, kept local so the reader
// sees exactly what a substring assertion means here.
func contains(haystack, needle string) bool {
	return len(needle) == 0 || len(haystack) >= len(needle) && indexOf(haystack, needle) >= 0
}

// indexOf finds a substring, or answers -1.
func indexOf(haystack, needle string) int {
	for at := 0; at+len(needle) <= len(haystack); at++ {
		if haystack[at:at+len(needle)] == needle {
			return at
		}
	}
	return -1
}

// binaryIsThere reports whether a built command line can be found.
func binaryIsThere() bool {
	if named := os.Getenv("INILLUCENT_BIN"); named != "" {
		if _, err := os.Stat(named); err == nil {
			return true
		}
	}
	_, err := exec.LookPath("inillucent")
	return err == nil
}

// TestConformanceSuite runs every case this wrapper's capabilities allow.
func TestConformanceSuite(t *testing.T) {
	if !binaryIsThere() {
		t.Skip("no inillucent binary: set INILLUCENT_BIN or put one on PATH " +
			"(`cargo build --release -p inillucent-cli`); skipping")
	}
	root := repositoryRoot(t)
	suite := readSuite(t, root)
	lacks := map[string]bool{}
	for _, capability := range suite.SkippedBy[language].Lacks {
		lacks[capability] = true
	}

	written := record{Language: language, Lacks: suite.SkippedBy[language].Lacks,
		Ran: []string{}, Skipped: []skippedCase{}, Failures: []string{}}
	ctx := context.Background()

	for _, one := range suite.Cases {
		missing := []string{}
		for _, needed := range one.Needs {
			if lacks[needed] {
				missing = append(missing, needed)
			}
		}
		if len(missing) > 0 {
			written.Skipped = append(written.Skipped,
				skippedCase{Name: one.Name, Group: one.Group, Needs: missing})
			continue
		}
		// No `create`: the first setup statement makes the file, because a
		// write verb on a path that is not there creates it. `create` also
		// refuses `--db`, which this wrapper always passes.
		directory := t.TempDir()
		database := filepath.Join(directory, "case.rdb")
		written.Failures = append(written.Failures, grade(ctx, inillucent.Open(database), one)...)
		written.Ran = append(written.Ran, one.Name)
	}

	recorded := filepath.Join(root, "_agent_output", "conformance")
	if err := os.MkdirAll(recorded, 0o755); err != nil {
		t.Fatalf("the record directory could not be made: %v", err)
	}
	body, err := json.MarshalIndent(written, "", "  ")
	if err != nil {
		t.Fatalf("the record could not be written: %v", err)
	}
	if err := os.WriteFile(filepath.Join(recorded, language+".json"), append(body, '\n'), 0o644); err != nil {
		t.Fatalf("the record could not be written: %v", err)
	}

	if len(written.Ran) == 0 {
		t.Fatal("no case ran, so this runner graded nothing at all")
	}
	if len(written.Failures) > 0 {
		t.Fatalf("%d of the conformance suite's assertions did not hold through the Go wrapper:\n  %v",
			len(written.Failures), written.Failures)
	}
}
