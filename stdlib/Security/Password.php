<?php

namespace Security;

/**
 * Password hashes that don't hold up the worker: Argon2id on a background thread, while the other
 * requests go on. The hashes are those of password_hash() with PASSWORD_ARGON2ID, so existing ones keep
 * working and password_verify() takes the new ones. Outside a fiber (the script itself, a command) they
 * run in place, like password_hash().
 */
final class Password
{
    /** A new hash of $password with a random salt. */
    public static function hash(string $password): string
    {
        if (\Fiber::getCurrent() === null) {
            return password_hash($password, PASSWORD_ARGON2ID);
        }
        return \Tenma\Internal\await(\tenma_op_password('hash', $password, ''));
    }

    /** Whether $password matches $hash. A hash of another algorithm (bcrypt) is checked by PHP in place. */
    public static function verify(string $password, string $hash): bool
    {
        if (\Fiber::getCurrent() === null || !str_starts_with($hash, '$argon2')) {
            return password_verify($password, $hash);
        }
        return \Tenma\Internal\await(\tenma_op_password('verify', $password, $hash)) === '1';
    }

    /** Whether $hash should be made again with hash(): another algorithm or other costs. */
    public static function needsRehash(string $hash): bool
    {
        return password_needs_rehash($hash, PASSWORD_ARGON2ID);
    }
}
