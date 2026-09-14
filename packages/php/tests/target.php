<?php

declare(strict_types=1);

/**
 * The installer's platform table, checked against the archives the release
 * publishes.
 *
 * Invariant: every platform this package claims to support names a file
 * `packaging/macos/release-macos.sh` or `packaging/release-all.ps1` writes. A
 * wrapper that asks for a file nobody publishes fails at the download with a
 * 404, which reads to whoever ran it as "there is no build for my machine"
 * rather than as a bug in the installer.
 *
 * The defect this was written for (task-1932, H12): macOS resolved to
 * `aarch64-apple-darwin` and `x86_64-apple-darwin`, and the release publishes
 * neither.
 *
 * Plain PHP rather than PHPUnit: this package has no development dependencies
 * and adding a test framework to run four assertions would be a larger change
 * than the thing being asserted. Run it with
 *
 *     php packages/php/tests/target.php
 */

require_once __DIR__ . '/../src/Target.php';

use Inillucent\Target;

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

// Every platform resolves to a triple the release publishes.
$cases = [
    ['Windows', 'AMD64', 'x86_64-pc-windows-msvc'],
    ['Darwin', 'arm64', 'universal-apple-darwin'],
    ['Darwin', 'x86_64', 'universal-apple-darwin'],
    ['Linux', 'x86_64', 'x86_64-unknown-linux-gnu'],
    ['Linux', 'aarch64', 'aarch64-unknown-linux-gnu'],
    ['Linux', 'armv7l', 'aarch64-unknown-linux-gnu'],
];
foreach ($cases as [$family, $machine, $want]) {
    $got = Target::triple($family, $machine);
    same("$family/$machine", $got, $want);
    holds(
        "$family/$machine asks for $got, which the release does not publish",
        in_array($got, Target::PUBLISHED, true)
    );
}

// A macOS install asks for the file release-macos.sh writes. The name is
// asserted whole, because the defect was in the middle of it: the version and
// the extension were right and the triple was a file that has never existed.
//
// `packaging/macos/release-macos.sh` line 88:
//     name="inillucent-$version-universal-apple-darwin"
// and line 144 appends `.tar.gz`.
same(
    'the macOS archive name',
    Target::archiveName('0.1.1', Target::triple('Darwin', 'arm64'), false),
    'inillucent-0.1.1-universal-apple-darwin.tar.gz'
);

// Windows gets the zip and everything else the tarball.
same(
    'the Windows archive name',
    Target::archiveName('0.1.1', 'x86_64-pc-windows-msvc', true),
    'inillucent-0.1.1-x86_64-pc-windows-msvc.zip'
);
same(
    'the Linux archive name',
    Target::archiveName('0.1.1', 'x86_64-unknown-linux-gnu', false),
    'inillucent-0.1.1-x86_64-unknown-linux-gnu.tar.gz'
);

// The installer pins the version it was tested against, and `latest` is the
// argument that asks the server for the newest. `go install ...@v0.1.1`
// installing 0.1.2 is the thing this prevents.
$installer = (string) file_get_contents(__DIR__ . '/../bin/inillucent-install');
holds(
    'the installer declares NATIVE_VERSION',
    (bool) preg_match("/const NATIVE_VERSION = '\\d+(\\.\\d+)*';/", $installer)
);
holds(
    'the installer defaults to its pinned version rather than to the newest',
    str_contains($installer, '$version = $argv[1] ?? NATIVE_VERSION;')
);
holds(
    'the installer reads the downloads host rather than a private release asset',
    str_contains($installer, "const DOWNLOADS = 'https://inillucent.com/downloads';")
        && !str_contains($installer, 'releases/download')
);

if ($failures !== []) {
    fwrite(STDERR, "the PHP installer's platform table is wrong:\n");
    foreach ($failures as $failure) {
        fwrite(STDERR, "  $failure\n");
    }
    exit(1);
}
echo "the PHP installer's platform table agrees with the release\n";
