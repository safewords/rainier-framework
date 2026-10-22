<?php

/*
 * Mint OAuth tokens with league/oauth2-server itself, for rainier-compat's
 * ignored `php::oauth` fixture tests.
 *
 * It uses the library directly — the same CryptTrait, BearerTokenResponse and
 * AccessTokenTrait every PHP OAuth server built on it goes through — so what
 * it writes is byte-for-byte what such a server hands its clients.
 *
 * Run it with PHP from the root of any project that has league/oauth2-server
 * and a PSR-7 implementation (guzzlehttp/psr7 or nyholm/psr7) installed:
 *
 *     php path/to/php-oauth-fixtures.php /tmp/rainier-compat-fixtures.json
 *
 * then, from the Rainier checkout:
 *
 *     RAINIER_COMPAT_FIXTURES=/tmp/rainier-compat-fixtures.json \
 *         cargo test -p rainier-compat --features php-oauth -- --ignored
 *
 * By default it generates a THROWAWAY application key and RSA key pair, so the
 * file holds nothing secret: it proves the formats, not your keys. To also
 * prove your keys, pass them — and then treat the output as a secret and
 * delete it afterwards:
 *
 *     APP_KEY=base64:… php php-oauth-fixtures.php out.json storage/oauth-private.key
 *
 * If the output file already exists (the general `php-fixtures.php` export),
 * the OAuth fixtures are merged into it under "oauth".
 */

declare(strict_types=1);

use League\OAuth2\Server\CryptKey;
use League\OAuth2\Server\CryptTrait;
use League\OAuth2\Server\Entities\AccessTokenEntityInterface;
use League\OAuth2\Server\Entities\ClientEntityInterface;
use League\OAuth2\Server\Entities\RefreshTokenEntityInterface;
use League\OAuth2\Server\Entities\ScopeEntityInterface;
use League\OAuth2\Server\Entities\Traits\AccessTokenTrait;
use League\OAuth2\Server\Entities\Traits\ClientTrait;
use League\OAuth2\Server\Entities\Traits\EntityTrait;
use League\OAuth2\Server\Entities\Traits\RefreshTokenTrait;
use League\OAuth2\Server\Entities\Traits\ScopeTrait;
use League\OAuth2\Server\Entities\Traits\TokenEntityTrait;
use League\OAuth2\Server\ResponseTypes\BearerTokenResponse;

require getcwd().'/vendor/autoload.php';

$out = $argv[1] ?? sys_get_temp_dir().'/rainier-compat-fixtures.json';
$privateKeyPath = $argv[2] ?? null;

// --- key material ---------------------------------------------------------------

$appKey = getenv('APP_KEY') ?: null;
$throwaway = $appKey === null || $privateKeyPath === null;

if ($appKey === null) {
    $appKey = 'base64:'.base64_encode(random_bytes(32));
}
$password = str_starts_with($appKey, 'base64:') ? base64_decode(substr($appKey, 7), true) : $appKey;

if ($privateKeyPath === null) {
    $pair = openssl_pkey_new(['private_key_bits' => 2048, 'private_key_type' => OPENSSL_KEYTYPE_RSA]);
    openssl_pkey_export($pair, $privatePem);
    $publicPem = openssl_pkey_get_details($pair)['key'];
} else {
    $privatePem = file_get_contents($privateKeyPath);
    $publicPem = openssl_pkey_get_details(openssl_pkey_get_private($privatePem))['key'];
}

// --- the entities league expects an integration to supply -------------------------

$client = new class implements ClientEntityInterface {
    use ClientTrait, EntityTrait;

    public function __construct()
    {
        $this->setIdentifier('9d5f3c1e-0000-4000-8000-000000000001');
        $this->name = 'Fixture client';
        $this->redirectUri = ['https://app.test/callback'];
        $this->isConfidential = true;
    }
};

$scope = fn (string $id) => new class($id) implements ScopeEntityInterface {
    use EntityTrait, ScopeTrait;

    public function __construct(string $id)
    {
        $this->setIdentifier($id);
    }
};

$accessToken = new class implements AccessTokenEntityInterface {
    use AccessTokenTrait, EntityTrait, TokenEntityTrait;
};
$accessToken->setIdentifier(bin2hex(random_bytes(40)));
$accessToken->setClient($client);
$accessToken->setUserIdentifier('42');
$accessToken->addScope($scope('account'));
$accessToken->addScope($scope('vehicles'));
$accessToken->setExpiryDateTime((new DateTimeImmutable())->add(new DateInterval('P1D')));
$accessToken->setPrivateKey(new CryptKey($privatePem, null, false));

$refreshToken = new class implements RefreshTokenEntityInterface {
    use EntityTrait, RefreshTokenTrait;
};
$refreshToken->setIdentifier(bin2hex(random_bytes(40)));
$refreshToken->setAccessToken($accessToken);
$refreshToken->setExpiryDateTime((new DateTimeImmutable())->add(new DateInterval('P1Y')));

// --- the bearer response: access token + sealed refresh token ----------------------

$response = new BearerTokenResponse();
$response->setPrivateKey(new CryptKey($privatePem, null, false));
$response->setEncryptionKey($password);
$response->setAccessToken($accessToken);
$response->setRefreshToken($refreshToken);

$psr = class_exists(\GuzzleHttp\Psr7\Response::class)
    ? new \GuzzleHttp\Psr7\Response()
    : new \Nyholm\Psr7\Response();
$bearer = json_decode((string) $response->generateHttpResponse($psr)->getBody(), true);

// --- an authorization code, sealed as the auth code grant seals one ----------------

$sealer = new class {
    use CryptTrait;

    public function seal(string $plain): string
    {
        return $this->encrypt($plain);
    }
};
$sealer->setEncryptionKey($password);

$verifier = rtrim(strtr(base64_encode(random_bytes(48)), '+/', '-_'), '=');
$authCodePayload = [
    'client_id' => $client->getIdentifier(),
    'redirect_uri' => 'https://app.test/callback',
    'auth_code_id' => bin2hex(random_bytes(40)),
    'scopes' => ['account'],
    'user_id' => '42',
    'expire_time' => (new DateTimeImmutable())->add(new DateInterval('PT10M'))->getTimestamp(),
    'code_challenge' => rtrim(strtr(base64_encode(hash('sha256', $verifier, true)), '+/', '-_'), '='),
    'code_challenge_method' => 'S256',
];

// --- write -------------------------------------------------------------------------

$fixtures = [
    'throwaway_keys' => $throwaway,
    'generated_at' => time(),
    'app_key' => $appKey,
    'public_key' => $publicPem,
    'client_id' => $client->getIdentifier(),
    'user_id' => '42',
    'access_token' => $bearer['access_token'],
    'access_token_id' => $accessToken->getIdentifier(),
    'refresh_token' => $bearer['refresh_token'],
    'refresh_token_id' => $refreshToken->getIdentifier(),
    'auth_code' => $sealer->seal(json_encode($authCodePayload)),
    'auth_code_payload' => $authCodePayload,
    'code_verifier' => $verifier,
];

$existing = is_file($out) ? json_decode((string) file_get_contents($out), true) : [];
$existing = is_array($existing) ? $existing : [];
$existing['oauth'] = $fixtures;

file_put_contents($out, json_encode($existing, JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES));
fwrite(STDERR, "wrote OAuth fixtures to $out".($throwaway ? ' (throwaway keys)' : ' (REAL KEYS — delete after use)')."\n");
