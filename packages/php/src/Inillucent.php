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
     * Encodes the bound values as the JSON text the command line reads.
     *
     * **`json_encode` cannot carry three things a parameter can be
     * (task-1979, D13, D15 and D16).** A binary string is not valid UTF-8, so
     * encoding one threw and PHP could not bind a BLOB at all; a NAN or an INF
     * throws too, where the honest answer is to name the value; and a negative
     * zero is written `-0`, which reads back as a positive zero.
     *
     * What is written instead: bytes become `{"blob":"<hex>"}`, a non-finite
     * number is refused here with the value in the message, and a negative zero
     * is written `-0.0`, which JSON's own grammar carries.
     *
     * @param list<mixed> $values the bound values, in order
     * @throws Error when a value has no SQL spelling
     */
    private static function encodeParams(array $values): string
    {
        $parts = [];
        foreach ($values as $value) {
            $parts[] = self::encodeParam($value);
        }

        return '[' . implode(',', $parts) . ']';
    }

    /**
     * Encodes one bound value. See {@see self::encodeParams}.
     *
     * @param mixed $value one element of the parameter list
     * @throws Error when the value has no SQL spelling
     */
    private static function encodeParam($value): string
    {
        if (is_float($value)) {
            if (is_nan($value) || is_infinite($value)) {
                throw new Error(
                    'a parameter cannot be NAN or INF: SQL has no spelling for either.',
                    'invalid_state'
                );
            }
            // `1 / $value` is a DivisionByZeroError in PHP 8, so the sign bit
            // is read off the bytes instead: `pack('e', ...)` is little-endian
            // IEEE 754, and the top bit of its last byte is the sign.
            if ($value === 0.0 && (ord(pack('e', $value)[7]) & 0x80) !== 0) {
                return '-0.0';
            }

            return json_encode($value, JSON_THROW_ON_ERROR | JSON_PRESERVE_ZERO_FRACTION);
        }
        // `preg_match('//u', ...)` rather than `mb_check_encoding`, because
        // mbstring is an extension a PHP build may not have and PCRE is not.
        if (is_string($value) && preg_match('//u', $value) !== 1) {
            return json_encode(['blob' => bin2hex($value)], JSON_THROW_ON_ERROR);
        }
        if (is_array($value)) {
            return self::encodeParams(array_values($value));
        }

        return json_encode($value, JSON_THROW_ON_ERROR);
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
        $stdin = null;
        foreach ($arguments as $name => $value) {
            if ($value === null || $value === false) {
                continue;
            }
            $flag = '--' . str_replace('_', '-', (string) $name);
            if ($value === true) {
                $argv[] = $flag;
                continue;
            }
            // **`params` travels on standard input.** It carries what a command
            // line cannot: a value larger than the argument ceiling, and a
            // binary string, which had no spelling at all before (task-1979,
            // D15 and D17).
            if ($name === 'params' && is_array($value)) {
                $stdin = self::encodeParams(array_values($value));
                $argv[] = '--params-file';
                $argv[] = '-';
                continue;
            }
            $argv[] = $flag;
            // `vector` is a JSON argument and the command line reads it as
            // JSON, so an array is encoded rather than joined.
            $argv[] = is_array($value)
                ? json_encode(array_values($value), JSON_THROW_ON_ERROR)
                : (string) $value;
        }

        $output = Process::run($argv, $stdin);
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
            static fn (array $row): array => array_combine(
                $names,
                array_map(static fn (mixed $value): mixed => self::decodeValue($value), $row)
            ),
            $result['rows']
        );
    }

    /**
     * Turns one cell of a result into the PHP value it stands for.
     *
     * **Bytes went in and could not come back** (task-2066 section 4.1.15). A
     * blob left as the string `x'00ff'`, typed `text` in the column list, so
     * nothing told it apart from a TEXT column holding that text - while the
     * encoder has always sent bytes as `{"blob": "<hex>"}`. The two halves of
     * the same grammar now agree, and a byte string read out of one query binds
     * straight into the next.
     *
     * Anything that is not an envelope is passed through untouched.
     *
     * @param mixed $value one cell, as --output json rendered it
     * @return mixed
     */
    private static function decodeValue(mixed $value): mixed
    {
        if (is_array($value) && array_keys($value) === ['blob'] && is_string($value['blob'])) {
            $bytes = hex2bin($value['blob']);
            return $bytes === false ? $value : $bytes;
        }
        return $value;
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
