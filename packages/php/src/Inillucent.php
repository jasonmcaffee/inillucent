<?php

declare(strict_types=1);

namespace Inillucent;

/**
 * inillucent, from PHP.
 *
 * This drives the `inillucent` binary and returns its results as arrays. Each
 * call is a process, which makes it the right tool for the handful of calls a
 * migration, a console command or an agent harness makes, and the wrong tool
 * for a loop over a million rows.
 *
 * It is not an in-process driver, and saying so plainly is better than shipping
 * one that has never been run. A real binding goes through the C ABI in
 * `include/inillucent_driver.h` with `ext-ffi`, and `drivers/README.md` is
 * written to be followed by somebody doing exactly that - it names the five
 * rules a binding has to get right and why each one matters.
 *
 * ```php
 * $db = new Inillucent('app.rdb');
 * $db->exec('CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)');
 * $db->exec('INSERT INTO notes (body) VALUES (?1)', ['hello']);
 * foreach ($db->query('SELECT * FROM notes') as $row) {
 *     echo $row['id'], ' ', $row['body'], PHP_EOL;
 * }
 * ```
 */
final class Inillucent
{
    /** The four programs a release ships. */
    public const PROGRAMS = ['inillucent', 'inillucent-shell', 'inillucent-mcp', 'inillucent-migrate'];

    /**
     * @param string $path the database file, or ':memory:' for a scratch one
     * @param string|null $binary the inillucent program to run; found automatically when null
     * @param bool $readOnly refuse every statement that changes something
     * @param string|null $root refuse every path outside this directory
     */
    public function __construct(
        private string $path,
        private ?string $binary = null,
        private bool $readOnly = false,
        private ?string $root = null,
    ) {
        $this->binary ??= Locator::find();
    }

    /**
     * Runs one inillucent command and returns its whole result array.
     *
     * The command line's `--output json` contract, unchanged: `ok`, `command`,
     * `columns`, `rows`, `total`, `more`, `changes`, `last_insert_rowid`,
     * `elapsed_ms` and `text` on success, and `ok`, `status`, `message` and
     * sometimes `feature` on failure.
     *
     * @param string $command the verb, such as 'query' or 'describe'
     * @param array<string,mixed> $arguments the named arguments that verb takes
     * @return array<string,mixed>
     * @throws Error when the command could not be run at all
     */
    public function run(string $command, array $arguments = []): array
    {
        $argv = [$this->binary, $command, '--output', 'json', '--db', $this->path];
        if ($this->readOnly) {
            $argv[] = '--readonly';
        }
        if ($this->root !== null) {
            $argv[] = '--root';
            $argv[] = $this->root;
        }
        foreach ($arguments as $name => $value) {
            if ($value === null || $value === false) {
                continue;
            }
            $flag = '--' . str_replace('_', '-', (string) $name);
            if ($value === true) {
                $argv[] = $flag;
                continue;
            }
            $argv[] = $flag;
            // `params` and `vector` are JSON arguments and the command line
            // reads them as JSON, so an array is encoded rather than joined.
            $argv[] = is_array($value)
                ? json_encode(array_values($value), JSON_THROW_ON_ERROR)
                : (string) $value;
        }

        $output = Process::run($argv);
        $trimmed = trim($output['stdout']);
        // A refusal exits non-zero *and* prints the result object, because JSON
        // was asked for. So the document is what is read, and only an
        // invocation with nothing to parse is a real failure to run.
        if ($trimmed === '' || $trimmed[0] !== '{') {
            throw new Error(
                sprintf(
                    'inillucent %s could not be run (exit %d): %s',
                    $command,
                    $output['code'],
                    trim($output['stderr']) !== '' ? trim($output['stderr']) : $trimmed
                ),
                'internal'
            );
        }

        /** @var array<string,mixed> $result */
        $result = json_decode($trimmed, true, 512, JSON_THROW_ON_ERROR);
        return $result;
    }

    /**
     * Runs a query and returns its rows as associative arrays.
     *
     * @param string $sql the statement
     * @param list<mixed> $params the values for ?1, ?2, ...
     * @param int|null $limit how many rows to hand back
     * @return list<array<string,mixed>>
     * @throws Error when the engine refuses the statement
     */
    public function query(string $sql, array $params = [], ?int $limit = null): array
    {
        $result = $this->run('query', ['sql' => $sql, 'params' => $params ?: null, 'limit' => $limit]);
        $this->refuseFailure($result);
        $names = array_map(static fn (array $column): string => $column['name'], $result['columns']);
        return array_map(
            static fn (array $row): array => array_combine($names, $row),
            $result['rows']
        );
    }

    /**
     * Runs one statement for its effect and returns how many rows it changed.
     *
     * @param string $sql the statement
     * @param list<mixed> $params the values for ?1, ?2, ...
     * @throws Error when the engine refuses the statement
     */
    public function exec(string $sql, array $params = []): int
    {
        $result = $this->run('exec', ['sql' => $sql, 'params' => $params ?: null]);
        $this->refuseFailure($result);
        return (int) $result['changes'];
    }

    /**
     * Runs several statements separated by semicolons, as one transaction.
     *
     * Either all of them take effect or none of them do.
     *
     * @param string $sql the statements
     * @throws Error when the engine refuses any of them
     */
    public function batch(string $sql): void
    {
        $this->refuseFailure($this->run('batch', ['sql' => $sql]));
    }

    /**
     * Returns everything about one table: columns, indexes, DDL and row count.
     *
     * @param string $table the table name
     * @return array<string,mixed>
     * @throws Error when there is no such table
     */
    public function describe(string $table): array
    {
        $result = $this->run('describe', ['table' => $table]);
        $this->refuseFailure($result);
        return $result;
    }

    /**
     * Returns the tables and views in the database.
     *
     * @return list<array<string,mixed>>
     * @throws Error when the database cannot be read
     */
    public function tables(): array
    {
        $result = $this->run('tables');
        $this->refuseFailure($result);
        return array_map(
            static fn (array $row): array => ['name' => $row[0], 'type' => $row[1]],
            $result['rows']
        );
    }

    /**
     * Turns a refused result into an exception carrying its status.
     *
     * The status matters more than the message: `unsupported` means the engine
     * has not built the construct and rewording will not help, which is a
     * different thing from a statement being wrong. `Error::isUnsupported()`
     * is how a caller branches on it.
     *
     * @param array<string,mixed> $result what the command line answered
     * @throws Error when the result says it failed
     */
    private function refuseFailure(array $result): void
    {
        if (($result['ok'] ?? false) === true) {
            return;
        }
        throw new Error(
            (string) ($result['message'] ?? 'the command failed'),
            (string) ($result['status'] ?? 'internal'),
            isset($result['feature']) ? (string) $result['feature'] : null
        );
    }
}
