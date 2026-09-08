<?php

declare(strict_types=1);

namespace Inillucent;

/**
 * Runs a program and collects both of its streams.
 *
 * `proc_open` with an argument *array* rather than `exec` with a string, and
 * that is the whole reason this class exists: a statement passed through a
 * shell would be re-quoted by it, and SQL is full of the characters a shell
 * cares about. PHP 7.4 and later pass an array straight to the operating system
 * with no shell in between, so a quote in a statement is a quote in the
 * statement.
 */
final class Process
{
    /**
     * Runs a program and returns its output, its errors and its exit code.
     *
     * @param list<string> $argv the program and its arguments
     * @return array{stdout:string,stderr:string,code:int}
     * @throws Error when the program could not be started at all
     */
    public static function run(array $argv): array
    {
        $descriptors = [
            0 => ['pipe', 'r'],
            1 => ['pipe', 'w'],
            2 => ['pipe', 'w'],
        ];
        $pipes = [];
        $handle = @proc_open($argv, $descriptors, $pipes);
        if ($handle === false) {
            throw new Error(
                sprintf('could not run %s. Is it installed and on PATH?', $argv[0] ?? 'inillucent'),
                'io'
            );
        }
        fclose($pipes[0]);
        $stdout = stream_get_contents($pipes[1]) ?: '';
        $stderr = stream_get_contents($pipes[2]) ?: '';
        fclose($pipes[1]);
        fclose($pipes[2]);
        $code = proc_close($handle);

        return ['stdout' => $stdout, 'stderr' => $stderr, 'code' => $code];
    }
}
