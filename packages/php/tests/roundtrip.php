<?php

declare(strict_types=1);

/**
 * The PHP wrapper against a real binary: write, read back, bind, and classify.
 *
 * Invariant: this file calls `query`, `exec` and `batch` and asserts on what
 * came back. `target.php` beside it reads the installer's platform table as
 * text, which is worth having and is not a test of the wrapper: nothing in it
 * runs a statement, so a release could ship a wrapper that cannot open a
 * database and both of this package's assertions would still pass
 * (task-1969, 5.7).
 *
 * The three cases are the Go suite's - a round trip, a bound hostile string,
 * and a classified status - because that suite was the one already doing the
 * job, and a fourth for a `VECTOR` column.
 *
 * Plain PHP rather than PHPUnit, for the reason `target.php` gives: this
 * package has no development dependencies and adding a test framework to run a
 * dozen assertions would be a larger change than the thing being asserted. Run
 * it with
 *
 *     php packages/php/tests/roundtrip.php
 *
 * It needs a built binary. `INILLUCENT_BIN` names one and `tools/validate`'s
 * `wrappers` stage sets it to `target/release/inillucent`; otherwise the
 * locator looks where an install would have put it. With neither, this exits 0
 * and says what to run, rather than failing on a machine that has never built
 * the workspace.
 */

require_once __DIR__ . '/../src/Error.php';
require_once __DIR__ . '/../src/Locator.php';
require_once __DIR__ . '/../src/Process.php';
require_once __DIR__ . '/../src/Inillucent.php';

use Inillucent\Error;
use Inillucent\Inillucent;
use Inillucent\Locator;

$failures = [];

/**
 * Records a failure when two values differ.
 *
 * @param string $what what was being checked
 * @param mixed $got what it answered
 * @param mixed $want what it should have answered
 */
function same(string $what, $got, $want): void
{
    global $failures;
    if ($got !== $want) {
        $failures[] = sprintf('%s: got %s, want %s', $what, var_export($got, true), var_export($want, true));
    }
}

/**
 * Records a failure when a condition does not hold.
 *
 * @param string $what what was being checked
 * @param bool $held whether it held
 */
function holds(string $what, bool $held): void
{
    global $failures;
    if (!$held) {
        $failures[] = $what;
    }
}

/**
 * Returns a directory of this run's own, inside the system's temporary one.
 *
 * A directory rather than a file, because a database is a file plus its log
 * segments: a run that left them beside each other would be a run the next one
 * reopened.
 */
function scratch(): string
{
    $path = sys_get_temp_dir() . DIRECTORY_SEPARATOR . 'inillucent-roundtrip-' . getmypid() . '-' . uniqid();
    mkdir($path, 0o777, true);
    return $path;
}

/**
 * Removes a directory and everything in it.
 *
 * @param string $path the directory this run made
 */
function clean(string $path): void
{
    foreach (glob($path . DIRECTORY_SEPARATOR . '*') ?: [] as $file) {
        @unlink($file);
    }
    @rmdir($path);
}

$binary = null;
try {
    $binary = Locator::find();
} catch (Error $why) {
    // **A skip exits 0 on a fresh clone and non-zero under INILLUCENT_STRICT
    // (task-2036).** This used to be a bare `exit(0)`, so a machine with no
    // binary printed that it was skipping and then reported success having run
    // nothing at all - and because this file had no row in
    // `tests/selection.toml` either, `inillucent-testrun --strict` could not
    // count it. That is rule 1.2's exact shape: a test that cannot fail is
    // worse than no test.
    fwrite(
        STDERR,
        "no inillucent binary: set INILLUCENT_BIN or install one "
            . "(`cargo build --release -p inillucent-cli`); skipping\n"
    );
    exit(getenv('INILLUCENT_STRICT') === '1' ? 1 : 0);
}

$directory = scratch();
$database = $directory . DIRECTORY_SEPARATOR . 'probe.rdb';
$db = new Inillucent($database, $binary);

// --- a round trip -----------------------------------------------------------
$db->batch(
    'CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);'
    . " INSERT INTO people VALUES (1, 'Ada', 36), (2, 'Grace', 45), (3, 'Alan', 41)"
);

$result = $db->run('query', [
    'sql' => 'SELECT name FROM people WHERE age > ?1 ORDER BY age',
    'params' => [40],
]);
same('the query succeeded', $result['ok'], true);
same('total is exact', $result['total'], 2);

$rows = $db->query('SELECT name FROM people WHERE age > ?1 ORDER BY age', [40]);
same('the rows and their order', array_column($rows, 'name'), ['Alan', 'Grace']);

// `tables()` is the one method here that reshapes what the command line
// answered: it turns each `[name, type]` row into a map, so a caller reads
// `$row['name']` rather than `$row[0]`. `describe()` does not reshape anything
// - its doc comment says it returns columns, indexes, DDL and the row count,
// and those only fit in the whole envelope - so its column names are field 1 of
// each row of the `PRAGMA table_info` result it carries.
same(
    'the table list names the table',
    in_array('people', array_column($db->tables(), 'name'), true),
    true
);
$described = $db->describe('people');
same('describe names the columns', array_column($described['rows'], 1), ['id', 'name', 'age']);
same(
    'describe carries the DDL',
    $described['ddl'],
    'CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)'
);

// --- a parameter is bound rather than pasted --------------------------------
// The quote is the point. Pasted into the statement it ends the string literal
// and the rest is parsed as SQL; bound, it is one value with a quote in it.
// This is the case that tells a wrapper apart from a string join.
$hostile = "Robert'); DROP TABLE people; --";
same('the insert changed one row', $db->exec('INSERT INTO people (name) VALUES (?1)', [$hostile]), 1);
$all = $db->query('SELECT name FROM people ORDER BY id');
same('the table survived', count($all), 4);
same('the value survived as one string', $all[3]['name'], $hostile);

// --- a failure comes back classified ----------------------------------------
$missing = $db->run('query', ['sql' => 'SELECT * FROM absent']);
same('a missing table is not ok', $missing['ok'], false);
same('a missing table is not_found', $missing['status'], 'not_found');

// `unsupported` is its own status and its own exit code, which is the thing
// AGENTS.md asks a caller to branch on rather than reword their SQL over.
$unbuilt = $db->run('query', ['sql' => 'SELECT (SELECT 1, 2)']);
same('an unbuilt construct is not ok', $unbuilt['ok'], false);
same('an unbuilt construct is unsupported', $unbuilt['status'], 'unsupported');

// The typed refusal, which is what a caller catches.
$classified = null;
try {
    $db->query('SELECT * FROM absent');
} catch (Error $why) {
    $classified = $why;
}
holds('a refused query throws Inillucent\\Error', $classified instanceof Error);
holds('the thrown error is not an unsupported one', $classified !== null && !$classified->isUnsupported());

// --- a VECTOR column survives the wrapper ------------------------------------
// The value goes in as a blob literal of little-endian `f32` bits, which is what
// a `VECTOR(N)` column is: three floats, twelve bytes. `vector` is one of the
// two arguments this wrapper encodes as JSON rather than as a string, which is
// the part under test.
$db->batch(
    'CREATE TABLE point (id INTEGER PRIMARY KEY, at VECTOR(3));'
    . " INSERT INTO point (id, at) VALUES (1, x'0000803f0000000000000000')"
);
$width = $db->query('SELECT vector_dims(at) AS width FROM point');
same('the vector came back three wide', $width[0]['width'], 3);

$found = $db->run('vector-search', [
    'table' => 'point',
    'column' => 'at',
    'vector' => [1, 0, 0],
    'k' => 1,
]);
same('vector-search succeeded', $found['ok'], true);
holds(
    'vector-search answered with a distance column',
    in_array('distance', array_column($found['columns'], 'name'), true)
);

clean($directory);

if ($failures === []) {
    echo "the PHP wrapper round trips against a real binary\n";
    exit(0);
}

fwrite(STDERR, "the PHP wrapper does not do what this package says it does:\n");
foreach ($failures as $failure) {
    fwrite(STDERR, '  ' . $failure . "\n");
}
exit(1);
