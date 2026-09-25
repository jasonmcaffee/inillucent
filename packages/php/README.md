# inillucent, from PHP

inillucent is an embedded SQL database. It speaks SQLite's dialect, and it has keyword search and
vector search built in. One `.rdb` file holds the tables and the search indexes.

This Composer package runs the `inillucent` command line program and returns its results as PHP
arrays. It needs PHP 8.1 or later and the `json` extension.

## Install

```sh
composer require black-rainbow-labs/inillucent
vendor/bin/inillucent-install
```

inillucent is written in Rust, and Composer installs only PHP code. Composer also does not run a
dependency's scripts for the project that requires it. So `vendor/bin/inillucent-install` is a
command you run once, after `composer require`.

`vendor/bin/inillucent-install` downloads the release archive for your machine from
`https://inillucent.com/downloads`. It checks the archive's `SHA-256` against the release's published
`SHA256SUMS` file, then unpacks four programs into the package's `runtime/bin` folder:
`inillucent`, `inillucent-shell`, `inillucent-mcp` and `inillucent-migrate`.

| Command | What it installs |
|---|---|
| `vendor/bin/inillucent-install` | the release this package was built with, which has the same version as the package |
| `vendor/bin/inillucent-install <version>` | that version, such as `1.0.28` |
| `vendor/bin/inillucent-install latest` | the newest release on the server |

There are builds for Windows on x64, Linux on x64 and arm64, and macOS on Apple silicon and Intel.

### Where the package finds the program

The package looks in three places, in this order:

1. `INILLUCENT_BIN`, when it names a file.
2. `runtime/bin` inside the package, where `vendor/bin/inillucent-install` puts the programs.
3. `PATH`, where Homebrew, npm, pip, the installer script and `cargo install` put them.

If inillucent is already on `PATH`, you can skip `vendor/bin/inillucent-install`. The copy in
`runtime/bin` comes before `PATH`, so a project that ran the installer keeps the version it chose.

`vendor/bin/inillucent` runs the `inillucent` program the same search finds, with the arguments you
give it.

## A first script

```php
<?php

require 'vendor/autoload.php';

use Inillucent\Inillucent;

$db = new Inillucent('app.rdb');

$db->batch("
    CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
    INSERT INTO people VALUES (1, 'Ada', 36), (2, 'Grace', 45)
");

foreach ($db->query('SELECT name FROM people WHERE age > ?1', [40]) as $row) {
    echo $row['name'], PHP_EOL; // Grace
}

echo $db->exec('UPDATE people SET age = age + 1 WHERE id = ?1', [1]), PHP_EOL; // 1
```

The first command that writes creates `app.rdb`. `batch()` runs its statements as one transaction,
so either every statement takes effect or none does.

`?1`, `?2` and so on are bound to the values in the array, in order. Bind values this way instead of
pasting them into the SQL text. A quote inside a bound value is part of the value.

## How the package runs

Each method call starts one `inillucent` process with `--output json` and decodes the JSON object it
prints. Bound values travel to the process on standard input.

Starting a process costs time on every call. This package suits a console command, a migration or an
agent harness that makes a few calls. It does not suit a loop over a million rows. For that, write a
binding over the C library with `ext-ffi`. The
[driver guide](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/drivers/README.md)
explains how.

The constructor takes three optional arguments after the path:

```php
$db = new Inillucent('app.rdb', binary: '/usr/local/bin/inillucent', readOnly: true, root: '/srv/data');
```

| Argument | What it does |
|---|---|
| `path` | the database file, or `':memory:'` for a database that is gone when the process ends |
| `binary` | the `inillucent` program to run. `null` means the search above |
| `readOnly` | adds `--readonly`, so every statement that changes data is refused |
| `root` | adds `--root`, so every path outside that directory is refused |

### Values

| PHP value | Bound as |
|---|---|
| `null` | `NULL` |
| an `int` | `INTEGER` |
| a `float` | `REAL`. `-0.0` keeps its sign |
| `NAN`, `INF` | refused with an `Error`, because SQL has no value for them |
| a string of valid UTF-8 | `TEXT` |
| any other string | `BLOB` |

`query()` returns a BLOB column as a PHP string of bytes, so bytes read by one query can be bound
into the next.

## Errors

```php
use Inillucent\Error;

try {
    $db->query('SELECT * FROM absent');
} catch (Error $why) {
    echo $why->status, PHP_EOL;          // not_found
    echo $why->getMessage(), PHP_EOL;    // no such table: absent
    if ($why->isUnsupported()) {
        // The engine has not built this feature. Rewording the SQL will not help.
    }
}
```

When the engine refuses a statement, `query()`, `exec()`, `batch()`, `describe()` and `tables()`
throw `Inillucent\Error`. `Error` extends `RuntimeException` and has two public read only properties:

- `status` is one of thirteen names: `unsupported`, `syntax`, `not_found`, `constraint`,
  `readonly`, `busy`, `interrupted`, `corrupt`, `io`, `full`, `too_big`, `invalid_state` and
  `internal`.
- `feature` names the missing feature when `status` is `unsupported`, and is `null` otherwise.

`unsupported` means the engine has not built that feature. The SQL is not wrong, and a different
spelling fails the same way. Run `vendor/bin/inillucent capabilities` to see what the engine
supports.

`run()` does not throw on a refusal. It returns the result array with `ok` set to `false`. `run()`
throws only when the program could not be run at all.

## The API

`cargo test -p inillucent-compat --test tooling documentation::` reads this table and fails if a name in the
first column is not declared in `Inillucent.php` or `Error.php`.

| what | one line |
|---|---|
| `new Inillucent(path, binary, readOnly, root)` | opens a database file. Only `path` is required. |
| `Inillucent::run(command, arguments)` | runs any `inillucent` command and returns its whole JSON result as an array. `['table' => 'people']` becomes `--table people`. |
| `Inillucent::query(sql, params, limit)` | runs one query and returns its rows as associative arrays keyed by column name. |
| `Inillucent::exec(sql, params)` | runs one statement for its effect and returns the number of rows it changed. |
| `Inillucent::batch(sql)` | runs several statements separated by semicolons as one transaction. |
| `Inillucent::describe(table)` | returns the `describe` result for one table: its columns, indexes, `CREATE` statement as `ddl`, and row count as `row_count_in_table`. |
| `Inillucent::tables()` | returns the tables and views, each as `['name' => ..., 'type' => ...]`. |
| `Inillucent::PROGRAMS` | the names of the four programs. |
| `Error::isUnsupported()` | returns whether `status` is `unsupported`. |

`query()` returns at most 200 rows, the default of the `query` command. Pass `limit` for more, or
`0` for every row. `run('query', ...)` returns `total`, the count of every row the statement
produced, and `more`, which is `true` when rows were left out.

## Where the manifest is

`composer.json` is at the root of the repository, because Packagist reads a whole repository. This
folder holds the source that `composer.json` autoloads.

## More

- [Getting started](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/getting-started.md)
- [SQL support](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/sql.md)
- [Vector and keyword search](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/vector-search.md)
- [Glossary](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/glossary.md)

MIT licence. Source: <https://github.com/Black-Rainbow-Labs/Inillucent>
