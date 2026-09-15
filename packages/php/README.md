# inillucent, from PHP

```sh
composer require black-rainbow-labs/inillucent
vendor/bin/inillucent-install          # downloads the binaries, checksum verified
```

```php
<?php

use Inillucent\Inillucent;
use Inillucent\Error;

require 'vendor/autoload.php';

$db = new Inillucent('app.rdb');

$db->batch('
    CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
    INSERT INTO people VALUES (1, \'Ada\', 36), (2, \'Grace\', 45)
');

foreach ($db->query('SELECT name FROM people WHERE age > ?1', [40]) as $row) {
    echo $row['name'], PHP_EOL;   // Grace
}
```

Values are **bound**, never interpolated, so a quote in a value is a quote in
the value rather than the start of a new clause.

## Why there is an install step

Composer installs PHP. inillucent is Rust. A composer package cannot ship a
compiled binary for every platform without becoming four packages and a
platform-detecting installer, and composer deliberately does not run a
dependency's scripts on behalf of the project that requires it - which is a good
rule, not a limitation to work around.

So `vendor/bin/inillucent-install` is a command a person runs once. It downloads
the release for this machine, **verifies its SHA-256 against the release's
published `SHA256SUMS`**, and unpacks the four programs into the package.

If inillucent is already installed some other way - Homebrew, npm, pip, the
installer script, `cargo install` - you can skip it: the package finds a binary
on `PATH` too, and `INILLUCENT_BIN` names one explicitly.

## Errors that are answers

```php
try {
    $db->query('SELECT * FROM absent');
} catch (Error $why) {
    echo $why->status;          // 'not_found'
    if ($why->isUnsupported()) {
        // The engine has not built that construct. Rewording will not help.
    }
}
```

`unsupported` is its own status rather than a flavour of syntax error. This
engine is deliberately incomplete in places and refuses what it has not built
rather than answering it wrongly, and an application that can tell the two apart
can say "this database cannot do that yet" instead of "check your spelling".

## What this is not

An in-process driver. Each call is a process, which makes this right for the
handful of calls a console command, a migration or an agent harness makes and
wrong for a loop over a million rows.

A real binding goes through the C ABI with `ext-ffi`, and
[`drivers/README.md`](../../drivers/README.md) is written to be followed by
somebody writing one - it names the five rules a binding has to get right, the
three ownership rules and nothing else, and a conformance suite that says
whether you got them right. Shipping an FFI binding that had never been run
would be worse than not shipping one.

## Where the manifest lives

`composer.json` is at the **repository root**, not in this directory, because
Packagist reads a repository rather than a subdirectory. This directory holds
the source it autoloads.

## The API

Every method this binding has. The worked example each one appears in is the link; nothing here is
a summary of a method that does not exist, because
`cargo test -p inillucent-compat --test documentation` reads this table and fails on a name the
binding source does not declare.

| what | one line |
|---|---|
| `new Inillucent(path)` | open a database. |
| `Inillucent::run(command, arguments)` | run one command of the command line and return its parsed JSON. |
| `Inillucent::query(sql, params, limit)` | run one `SELECT` and return its rows. |
| `Inillucent::exec(sql, params)` | run one statement for its effect and return how many rows it changed. |
| `Inillucent::batch(sql)` | run several statements separated by semicolons. |
| `Inillucent::describe(table)` | one table's columns. |
| `Inillucent::tables()` | every table in the database. |
| `Error::isUnsupported()` | whether the failure was "this engine has not built that" rather than "you typed it wrong". |

Every call goes through the command line, for the reason the npm package's table gives.
