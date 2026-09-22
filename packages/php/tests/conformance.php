<?php

declare(strict_types=1);

/**
 * The shared conformance suite, run through the PHP wrapper.
 *
 * **One specification, four runners.** `drivers/conformance/suite.json` is what
 * "the binding is correct" means, and until task-2036 only two things read it:
 * the Rust driver and the Python binding. npm, Go and PHP each had a round trip
 * of their own - three to eight cases, written by hand, agreeing with nothing.
 * A binding graded against a suite it does not run is a binding graded against
 * itself.
 *
 * **What this wrapper cannot do.** The PHP package spawns the command line:
 * every call is a process, so a transaction, a savepoint or a temporary table
 * cannot outlive one step. That is the `session` capability, and
 * `skipped_by.php` in the suite names it with the reason. Five of the
 * thirty-two cases need it; the other twenty-seven run here.
 *
 * Rows *do* outlive a step - they are in the file - and so do bytes: this
 * wrapper already binds a binary string as `{"blob":"<hex>"}` through
 * `--params-file`, which is `Inillucent::encodeParam`'s own answer to the same
 * problem.
 *
 * `tooling::every_binding_runs_the_whole_suite` reads
 * `_agent_output/conformance/php.json`, which this writes.
 *
 * Run it:
 *
 *     php packages/php/tests/conformance.php
 *
 * It exits 0 when every case this wrapper can run passed, 1 otherwise, and
 * prints a `; skipping` line and exits 0 when there is no binary - unless
 * `INILLUCENT_STRICT=1`, which turns that into a non-zero exit so a run with
 * nothing provisioned cannot read as a pass.
 */

require __DIR__ . '/../src/Error.php';
require __DIR__ . '/../src/Process.php';
require __DIR__ . '/../src/Target.php';
require __DIR__ . '/../src/Locator.php';
require __DIR__ . '/../src/Inillucent.php';

use Inillucent\Error;
use Inillucent\Inillucent;
use Inillucent\Locator;

/** How the suite's `skipped_by` names this runner. */
const LANGUAGE = 'php';

/** The workspace root, three directories up from this file. */
$root = dirname(__DIR__, 3);

/**
 * Makes a scratch directory of this run's own.
 */
function scratch(): string
{
    $directory = sys_get_temp_dir() . '/inillucent-conformance-' . bin2hex(random_bytes(6));
    mkdir($directory, 0o777, true);

    return $directory;
}

/**
 * Removes a directory and everything in it.
 *
 * @param string $directory the directory this run made
 */
function clean(string $directory): void
{
    if (!is_dir($directory)) {
        return;
    }
    foreach (scandir($directory) ?: [] as $entry) {
        if ($entry === '.' || $entry === '..') {
            continue;
        }
        $path = $directory . '/' . $entry;
        is_dir($path) ? clean($path) : @unlink($path);
    }
    @rmdir($directory);
}

/**
 * Reads a value out of the suite's one-key object form.
 *
 * A blob becomes a PHP binary string, which `Inillucent::encodeParam` already
 * knows to send as `{"blob":"<hex>"}`.
 *
 * @param array<string,mixed> $described the one-key object
 * @return mixed
 */
function bound(array $described)
{
    if (array_key_exists('null', $described)) {
        return null;
    }
    if (array_key_exists('int', $described)) {
        return (int) $described['int'];
    }
    if (array_key_exists('real', $described)) {
        return (float) $described['real'];
    }
    if (array_key_exists('text', $described)) {
        return (string) $described['text'];
    }
    if (array_key_exists('blob', $described)) {
        return pack('C*', ...array_map('intval', $described['blob']));
    }
    throw new RuntimeException(json_encode($described) . ' names no value kind');
}

/**
 * Compares an expected value to what the command line answered.
 *
 * @param array<string,mixed> $want the suite's one-key object
 * @param mixed $got what came back
 */
function same(array $want, $got): bool
{
    if (array_key_exists('null', $want)) {
        return $got === null;
    }
    if (array_key_exists('int', $want)) {
        return (string) $want['int'] === (string) $got;
    }
    if (array_key_exists('real', $want)) {
        return (float) $want['real'] === (float) $got;
    }
    if (array_key_exists('text', $want)) {
        return (string) $want['text'] === (string) $got;
    }
    if (array_key_exists('blob', $want)) {
        // **The envelope, not the old x'..' string** (task-2066 section
        // 4.1.15). A blob used to come back as text, so bytes could be written
        // and not read - and this comparison agreed with it, which is why the
        // round-trip case passed throughout. Both sides are compared as
        // hexadecimal, which is exact whichever way either spells a byte array.
        $hex = is_array($want['blob'])
            ? bin2hex(pack('C*', ...array_map('intval', $want['blob'])))
            : strtolower((string) $want['blob']);
        if (is_array($got) && array_key_exists('blob', $got) && is_string($got['blob'])) {
            return strtolower($got['blob']) === $hex;
        }
        // The wrapper's own `query` decodes the envelope to a binary string, so
        // a step read through it arrives as bytes rather than as an array.
        if (is_string($got)) {
            return bin2hex($got) === $hex;
        }

        return false;
    }

    return false;
}

/**
 * Runs one step and returns the command's report.
 *
 * `query` when the step names a limit, because `--limit` is that verb's and the
 * exact total beside a cut result is what such a case is about; `exec`
 * otherwise, because it answers reads and writes alike.
 *
 * @param array<string,mixed> $step the step out of the suite
 * @return array<string,mixed>
 */
function runStep(Inillucent $db, array $step): array
{
    $arguments = ['sql' => $step['sql']];
    if (isset($step['params']) && $step['params'] !== []) {
        $arguments['params'] = array_map('bound', $step['params']);
    }
    if (array_key_exists('limit', $step)) {
        $arguments['limit'] = (int) $step['limit'];

        return $db->run('query', $arguments);
    }

    return $db->run('exec', $arguments);
}

/**
 * Grades one case and returns what did not hold.
 *
 * @param array<string,mixed> $case the case out of the suite
 * @return list<string>
 */
function grade(Inillucent $db, array $case): array
{
    $wrong = [];
    foreach ($case['setup'] ?? [] as $statement) {
        $report = $db->run('exec', ['sql' => $statement]);
        if (($report['ok'] ?? false) !== true) {
            $wrong[] = sprintf('%s: the setup `%s` failed: %s', $case['name'], $statement,
                $report['message'] ?? '');

            return $wrong;
        }
    }

    foreach ($case['steps'] ?? [] as $step) {
        $report = runStep($db, $step);
        $ok = ($report['ok'] ?? false) === true;

        if (array_key_exists('status', $step)) {
            if ($ok) {
                $wrong[] = sprintf('%s: `%s` succeeded and should have failed with `%s`',
                    $case['name'], $step['sql'], $step['status']);
                continue;
            }
            if (($report['status'] ?? '') !== $step['status']) {
                $wrong[] = sprintf('%s: `%s` failed with `%s` and should have failed with `%s`',
                    $case['name'], $step['sql'], $report['status'] ?? '', $step['status']);
            }
            if (isset($step['message_contains'])
                && !str_contains((string) ($report['message'] ?? ''), $step['message_contains'])) {
                $wrong[] = sprintf('%s: `%s` said `%s`, which does not contain `%s`',
                    $case['name'], $step['sql'], $report['message'] ?? '', $step['message_contains']);
            }
            if (isset($step['feature_contains'])
                && !str_contains((string) ($report['feature'] ?? ''), $step['feature_contains'])) {
                $wrong[] = sprintf('%s: `%s` named the feature `%s`, which does not contain `%s`',
                    $case['name'], $step['sql'], $report['feature'] ?? '', $step['feature_contains']);
            }
            continue;
        }

        if (!$ok) {
            $wrong[] = sprintf('%s: `%s` failed with `%s`: %s', $case['name'], $step['sql'],
                $report['status'] ?? '', $report['message'] ?? '');
            continue;
        }
        if (array_key_exists('columns', $step)) {
            $names = array_map(static fn (array $column): string => $column['name'],
                $report['columns'] ?? []);
            if ($names !== $step['columns']) {
                $wrong[] = sprintf('%s: `%s` answered columns %s, wanted %s', $case['name'],
                    $step['sql'], json_encode($names), json_encode($step['columns']));
            }
        }
        if (array_key_exists('rows', $step)) {
            $rows = $report['rows'] ?? [];
            if (count($rows) !== count($step['rows'])) {
                $wrong[] = sprintf('%s: `%s` answered %d rows, wanted %d', $case['name'],
                    $step['sql'], count($rows), count($step['rows']));
            } else {
                foreach ($step['rows'] as $at => $wantRow) {
                    foreach ($wantRow as $column => $wantCell) {
                        $got = $rows[$at][$column] ?? null;
                        if (!same($wantCell, $got)) {
                            $wrong[] = sprintf('%s: `%s`: row %d column %d is %s and should be %s',
                                $case['name'], $step['sql'], $at, $column, json_encode($got),
                                json_encode($wantCell));
                        }
                    }
                }
            }
        }
        if (array_key_exists('total', $step) && ($report['total'] ?? null) !== $step['total']) {
            $wrong[] = sprintf('%s: `%s` reported total %s, wanted %s', $case['name'],
                $step['sql'], json_encode($report['total'] ?? null), json_encode($step['total']));
        }
        if (array_key_exists('more', $step) && ($report['more'] ?? null) !== $step['more']) {
            $wrong[] = sprintf('%s: `%s` reported more=%s, wanted %s', $case['name'],
                $step['sql'], json_encode($report['more'] ?? null), json_encode($step['more']));
        }
        if (array_key_exists('affected', $step) && $step['affected'] !== null
            && ($report['changes'] ?? null) !== $step['affected']) {
            $wrong[] = sprintf('%s: `%s` reported %s changed, wanted %s', $case['name'],
                $step['sql'], json_encode($report['changes'] ?? null), json_encode($step['affected']));
        }
    }

    return $wrong;
}

// ---------------------------------------------------------------------------

$strict = getenv('INILLUCENT_STRICT') === '1';

try {
    $binary = Locator::find();
} catch (Error) {
    fwrite(STDERR, "no inillucent binary: set INILLUCENT_BIN or install one "
        . "(`cargo build --release -p inillucent-cli`); skipping\n");
    exit($strict ? 1 : 0);
}

$suiteFile = $root . '/drivers/conformance/suite.json';
$suite = json_decode((string) file_get_contents($suiteFile), true, 512, JSON_THROW_ON_ERROR);
if (!is_array($suite) || ($suite['cases'] ?? []) === []) {
    fwrite(STDERR, "$suiteFile holds no cases, so this runner would grade nothing\n");
    exit(1);
}
$lacks = $suite['skipped_by'][LANGUAGE]['lacks'] ?? [];

$ran = [];
$skipped = [];
$failures = [];

foreach ($suite['cases'] as $case) {
    $missing = array_values(array_intersect($case['needs'] ?? [], $lacks));
    if ($missing !== []) {
        $skipped[] = ['name' => $case['name'], 'group' => $case['group'] ?? '', 'needs' => $missing];
        continue;
    }
    $directory = scratch();
    try {
        // No `create`: the first setup statement makes the file, because a
        // write verb on a path that is not there creates it. `create` also
        // refuses `--db`, which this wrapper always passes.
        $database = $directory . '/case.rdb';
        $failures = array_merge($failures, grade(new Inillucent($database, $binary), $case));
        $ran[] = $case['name'];
    } finally {
        clean($directory);
    }
}

$recorded = $root . '/_agent_output/conformance';
if (!is_dir($recorded)) {
    mkdir($recorded, 0o777, true);
}
file_put_contents($recorded . '/' . LANGUAGE . '.json', json_encode([
    'language' => LANGUAGE,
    'lacks' => $lacks,
    'ran' => $ran,
    'skipped' => $skipped,
    'failures' => $failures,
], JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES) . "\n");

if ($ran === []) {
    fwrite(STDERR, "no case ran, so this runner graded nothing at all\n");
    exit(1);
}
if ($failures === []) {
    printf("the PHP wrapper runs %d of the conformance suite's %d cases, and %d are skipped for "
        . "`session`\n", count($ran), count($suite['cases']), count($skipped));
    exit(0);
}

fwrite(STDERR, sprintf("%d of the conformance suite's assertions did not hold through the PHP "
    . "wrapper:\n", count($failures)));
foreach ($failures as $failure) {
    fwrite(STDERR, '  ' . $failure . "\n");
}
exit(1);
