<?php

declare(strict_types=1);

namespace Inillucent;

/**
 * Which release archive a machine should download.
 *
 * A class rather than two functions inside `bin/inillucent-install`, because the
 * rule is the one thing in that script worth checking and a script that runs on
 * include cannot be checked at all. `tests/target.php` drives this; the
 * installer calls it.
 *
 * **The rule was wrong for macOS (task-1932, H12).** The installer resolved a
 * Mac to `aarch64-apple-darwin` or `x86_64-apple-darwin`, and the release
 * publishes neither: it builds both Apple targets and `lipo`s them into one
 * `universal-apple-darwin` archive, which is what `packaging/install.sh` has
 * always asked for. Every Mac running `vendor/bin/inillucent-install` was told
 * there was no build for it.
 */
final class Target
{
    /**
     * Every target the release publishes an archive for.
     *
     * Written out so that a triple this class returns can be checked against
     * the set rather than against itself.
     *
     * @var list<string>
     */
    public const PUBLISHED = [
        'x86_64-pc-windows-msvc',
        'universal-apple-darwin',
        'x86_64-unknown-linux-gnu',
        'aarch64-unknown-linux-gnu',
    ];

    /**
     * Returns the Rust target triple a platform's releases are built for.
     *
     * Takes the platform rather than reading it, so every row can be checked
     * from one machine. The row that was wrong could not be checked from the
     * machine the release is cut on, which is how it stayed wrong.
     *
     * @param string $family the PHP_OS_FAMILY value: Windows, Darwin or Linux
     * @param string $machine what php_uname('m') answers
     */
    public static function triple(string $family, string $machine): string
    {
        if ($family === 'Windows') {
            return 'x86_64-pc-windows-msvc';
        }
        $folded = strtolower($machine);
        $arm = str_contains($folded, 'arm') || str_contains($folded, 'aarch64');
        if ($family === 'Darwin') {
            return 'universal-apple-darwin';
        }
        return $arm ? 'aarch64-unknown-linux-gnu' : 'x86_64-unknown-linux-gnu';
    }

    /**
     * Returns the triple for the machine this is running on.
     */
    public static function current(): string
    {
        return self::triple(PHP_OS_FAMILY, (string) php_uname('m'));
    }

    /**
     * Returns the file a release publishes for one version and target.
     *
     * @param string $version the release
     * @param string $triple the Rust target triple
     * @param bool $windows whether the container is a zip rather than a tarball
     */
    public static function archiveName(string $version, string $triple, bool $windows): string
    {
        return sprintf('inillucent-%s-%s%s', $version, $triple, $windows ? '.zip' : '.tar.gz');
    }
}
