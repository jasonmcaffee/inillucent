<?php

declare(strict_types=1);

namespace Inillucent;

use RuntimeException;

/**
 * A refusal the engine or the command line reported.
 *
 * It carries the status as well as the message because the two answer different
 * questions. `unsupported` means the engine has not built the construct: it is
 * not a syntax error, rewording the statement will not help, and an application
 * that can tell the difference can say "this engine cannot do that yet" instead
 * of "check your spelling". `drivers/README.md` argues the case at length; this
 * is that argument arriving in PHP.
 */
final class Error extends RuntimeException
{
    /**
     * @param string $message what went wrong
     * @param string $status the failure class, as inillucent's driver names it
     * @param string|null $feature the construct that is not built, when the status is 'unsupported'
     */
    public function __construct(
        string $message,
        public readonly string $status = 'internal',
        public readonly ?string $feature = null,
    ) {
        parent::__construct($message);
    }

    /**
     * Returns whether this is the engine saying it has not built the construct.
     */
    public function isUnsupported(): bool
    {
        return $this->status === 'unsupported';
    }
}
