<?php

declare(strict_types=1);

namespace Inillucent;

/**
 * Finds the inillucent binaries.
 *
 * Composer cannot install a Rust program, so this package looks for one in
 * three places, in the order that respects what the machine already has:
 *
 * 1. `INILLUCENT_BIN`, when somebody has said exactly which one to use;
 * 2. the package's own `runtime/bin`, where `vendor/bin/inillucent-install`
 *    puts a downloaded release;
 * 3. `PATH`, which is where an install by Homebrew, npm, pip, the installer
 *    script or `cargo install` will have put it.
 *
 * Looking at PATH *last* is deliberate: a project that ran the installer chose
 * a version, and a different one on PATH should not silently win.
 */
final class Locator
{
    /**
     * Returns the path of one of the four programs.
     *
     * @param string $program which program to find
     * @throws Error when it is not installed anywhere this looks
     */
    public static function find(string $program = 'inillucent'): string
    {
        if (!in_array($program, Inillucent::PROGRAMS, true)) {
            throw new Error(sprintf('inillucent has no program called "%s"', $program), 'invalid_state');
        }
        $suffix = self::onWindows() ? '.exe' : '';

        $named = getenv('INILLUCENT_BIN');
        if (is_string($named) && $named !== '' && is_file($named)) {
            return $named;
        }

        $bundled = self::runtimeDirectory() . DIRECTORY_SEPARATOR . $program . $suffix;
        if (is_file($bundled)) {
            return $bundled;
        }

        $found = self::onPath($program . $suffix);
        if ($found !== null) {
            return $found;
        }

        throw new Error(
            sprintf(
                "%s is not installed.\n" .
                "  Run:  vendor/bin/inillucent-install\n" .
                "  Or install it another way: https://github.com/jasonmcaffee/inillucent#install\n" .
                "  Or point INILLUCENT_BIN at a binary you already have.",
                $program
            ),
            'not_found'
        );
    }

    /**
     * Returns where a downloaded release is unpacked to.
     */
    public static function runtimeDirectory(): string
    {
        return dirname(__DIR__) . DIRECTORY_SEPARATOR . 'runtime' . DIRECTORY_SEPARATOR . 'bin';
    }

    /**
     * Returns whether this is Windows, which decides the executable suffix.
     *
     * `PHP_OS_FAMILY` rather than a comparison against DIRECTORY_SEPARATOR:
     * the two answer the same question and only one of them needs an escaped
     * backslash in the source.
     */
    public static function onWindows(): bool
    {
        return PHP_OS_FAMILY === 'Windows';
    }

    /**
     * Searches PATH for a program.
     *
     * @param string $name the file name to look for
     */
    private static function onPath(string $name): ?string
    {
        $path = getenv('PATH');
        if (!is_string($path) || $path === '') {
            return null;
        }
        $separator = self::onWindows() ? ';' : ':';
        foreach (explode($separator, $path) as $directory) {
            if ($directory === '') {
                continue;
            }
            $candidate = rtrim($directory, '\/') . DIRECTORY_SEPARATOR . $name;
            if (is_file($candidate)) {
                return $candidate;
            }
        }
        return null;
    }
}
