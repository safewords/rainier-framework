<?php

/*
 * Export real values from a running PHP application, for rainier-compat's
 * ignored fixture tests.
 *
 * Load it in the application's interactive console (the REPL its framework
 * ships with), or `require` it from any script that has already booted the
 * application:
 *
 *     $out = '/tmp/rainier-compat-fixtures.json';
 *     require 'path/to/php-fixtures.php';
 *
 * then, from the Rainier checkout:
 *
 *     RAINIER_COMPAT_FIXTURES=/tmp/rainier-compat-fixtures.json \
 *         cargo test -p rainier-compat --features php -- --ignored
 *
 * The file contains the APPLICATION KEY. Do not commit it; delete it when the
 * tests have passed.
 *
 * Read-only against the application: it writes nothing to the cache, the
 * session store or the database. Every value is computed in memory with the
 * application's own encrypter, cache store and guard.
 */

$out = $out ?? sys_get_temp_dir().'/rainier-compat-fixtures.json';
$guard = $guard ?? 'web';

$encrypter = app('encrypter');
$key = $encrypter->getKey();

// The cookie value prefix, as the cookie middleware computes it.
$prefix = fn (string $name) => hash_hmac('sha1', $name.'v2', $key).'|';

// --- serialize() ------------------------------------------------------------

$samples = [];
$add = function ($value, $json = null, bool $identical = true) use (&$samples) {
    $samples[] = [
        'php' => base64_encode(serialize($value)),
        'json' => $json,
        'byte_identical' => $identical,
    ];
};
$add(null, null);
$add(true, true);
$add(-42, -42);
$add('héllo "quoted"; with semicolons', 'héllo "quoted"; with semicolons');
$add([1, 2, 3], [1, 2, 3]);
$add(['a' => 1, 7 => 'seven', 'nested' => ['x' => null]], ['a' => 1, '7' => 'seven', 'nested' => ['x' => null]]);
$add(0.1, 0.1, false);
$add(1.0e25, 1.0e25, false);
$object = new stdClass;
$object->public = 'yes';
$add($object);
$add(new ArrayObject(['a' => 1]));

// --- an encrypted cookie ----------------------------------------------------

$cookieName = config('session.cookie');
$cookiePlain = str_repeat('a', 40);
$cookie = [
    'name' => $cookieName,
    'plain' => $cookiePlain,
    'sealed' => $encrypter->encrypt($prefix($cookieName).$cookiePlain, false),
];

// --- a session payload --------------------------------------------------------

$loginKey = app('auth')->guard($guard)->getName();
$token = bin2hex(random_bytes(20));
$session = [
    'guard' => $guard,
    'user_id' => '42',
    'token' => $token,
    'payload' => base64_encode(serialize([
        '_token' => $token,
        $loginKey => 42,
        '_previous' => ['url' => url('/')],
        '_flash' => ['old' => [], 'new' => []],
    ])),
];

// --- a cache value and a tagged key --------------------------------------------

$store = app('cache')->store();
$cacheValue = ['soc' => 80, 'state' => 'online', 'list' => [1, 2]];
$cache = [
    'prefix' => $store->getPrefix(),
    'key' => 'rainier-compat-fixture',
    'stored' => base64_encode(serialize($cacheValue)),
    'json' => $cacheValue,
];

$tags = ['vehicle-telemetry', 'rainier-compat'];
$tagged = [
    'tags' => $tags,
    'key' => 'item',
    'item_key' => method_exists($store, 'tags') || method_exists(app('cache'), 'tags')
        ? app('cache')->tags($tags)->taggedItemKey('item')
        : null,
];

// --- the XSRF-TOKEN cookie ------------------------------------------------------

$xsrfToken = bin2hex(random_bytes(20));
$xsrf = [
    'token' => $xsrfToken,
    'sealed' => $encrypter->encrypt($prefix('XSRF-TOKEN').$xsrfToken, false),
];

// --- a remember-me cookie, if the guard has one ----------------------------------

$remember = null;
$sessionGuard = app('auth')->guard($guard);
if (method_exists($sessionGuard, 'getRecallerName')) {
    $passwordHash = password_hash('fixture', PASSWORD_BCRYPT);
    $mac = hash_hmac('sha256', $passwordHash, config('app.key'));
    $name = $sessionGuard->getRecallerName();
    $remember = [
        'name' => $name,
        'user_id' => '42',
        'password_hash' => $passwordHash,
        'sealed' => $encrypter->encrypt($prefix($name).'42|remember-token|'.$mac, false),
    ];
}

file_put_contents($out, json_encode([
    'app_key' => config('app.key'),
    'serialize' => $samples,
    'cookie' => $cookie,
    'session' => $session,
    'cache' => $cache,
    'tagged' => $tagged,
    'xsrf' => $xsrf,
    'remember' => $remember,
], JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES | JSON_PRESERVE_ZERO_FRACTION));

echo "wrote {$out} — it contains the application key; delete it after use\n";
