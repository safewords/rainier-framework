# Rainier Documentation

Rainier is an MVC framework for Rust: a service container, providers that
wire it, a router with named routes and middleware groups, form requests,
guards, jobs, mailables, facades, events. It is designed to provide a smooth
transition for developers familiar with Laravel — each piece sits where an
MVC developer expects to find it.

These pages assume you have used an MVC framework before and are new to
Rainier. Where Rainier's answer **differs** from the conventional one, the
difference is explained rather than glossed — those are the places you will
otherwise waste an afternoon.

Rainier is an independent project. It is not affiliated with, sponsored by,
or endorsed by any other framework or its trademark holders, and it claims no
rights in their names or marks.

> **New here?** Read [Installation](installation.md), then
> [The Request Lifecycle](lifecycle.md), then [Routing](routing.md). That is
> enough to be productive; come back for the rest.

---

## Getting Started

| Page | What it covers |
|---|---|
| [Installation](installation.md) | Cloning the starter, prerequisites, and the two dependency rules that bite everyone once |
| [Directory Structure](directory-structure.md) | Every directory, and what lives in each |
| [Configuration](configuration.md) | `.env`, the config repository, typed keys and driver enums |
| [Deployment](deployment.md) | What to change before this faces the internet |

## Architecture Concepts

| Page | What it covers |
|---|---|
| [Architecture Overview](architecture.md) | The crates, why they are separate, and the rules they obey |
| [The Request Lifecycle](lifecycle.md) | Socket to response, with every seam named |
| [Service Container](container.md) | Binding, resolving, singletons, and what happens on a cycle |
| [Service Providers](providers.md) | `register` and `boot`, and why they are two phases |
| [Facades](facades.md) | Static proxies, how they resolve, and when not to use one |

## The Basics

| Page | What it covers |
|---|---|
| [Routing](routing.md) | Verbs, parameters, constraints, groups, resources, named routes |
| [Middleware](middleware.md) | The pipeline, attaching middleware by value, groups, controller middleware |
| [Controllers](controllers.md) | Actions, extractors, and resource controllers |
| [Requests](requests.md) | Input, headers, cookies, uploads, and why bodies are buffered |
| [Responses](responses.md) | Builders, `IntoResponse`, streaming, cookies |
| [Views](views.md) | The template syntax, the engine, and what it deliberately cannot do |
| [URL Generation](urls.md) | Named routes to URLs, signed links, and the escaping that keeps them safe |
| [Validation](validation.md) | Rules, the validator, and request contracts |
| [Error Handling](errors.md) | `Error`, `ErrorKind`, the exception renderer, 4xx versus 5xx |

## Digging Deeper

| Page | What it covers |
|---|---|
| [Console](console.md) | Built-in commands and writing your own |
| [Events](events.md) | The dispatcher, listeners, subscribers, and the fake |
| [Mail](mail.md) | Mailables, envelopes, transports, and `always_to` |
| [Notifications](notifications.md) | A message to a recipient, over their channels — and how it differs from an event |
| [Broadcasting](broadcasting.md) | Events pushed to WebSocket channels, and who may subscribe |
| [WebSockets](websockets.md) | Sockets your own process holds, on the same port as HTTP |
| [HTTP Client](http-client.md) | Calling somebody else's API, and the fake that makes it assertable |
| [Kafka](kafka.md) | A partitioned log behind broadcasting, sockets and the queue — and where it disagrees with each |
| [Scenarios](scenarios.md) | Worked designs composing the pages above — starting with a Twitter-shaped feed |
| [Queues](queues.md) | Jobs, drivers, the worker, retries, and the reservation protocol |
| [Cache](cache.md) | The cache port, Redis, Redis Cluster, Memcached, Workers KV, atomic locks |
| [Task Scheduling](scheduling.md) | Cron expressions, `without_overlapping`, `on_one_server` |
| [Observability](observability.md) | Prometheus metrics, an OpenAPI document, OpenTelemetry, log format, health checks |
| [Helpers](helpers.md) | `Str` inflection, type-maps, the error type, and `build_info!()` |

## Security

| Page | What it covers |
|---|---|
| [Authentication](authentication.md) | Guards, user providers, the `auth` middleware, token abilities, challenges |
| [Authorization](authorization.md) | Gates, policies, actors that are not people, and failing closed |
| [Hashing](hashing.md) | Argon2id, the timing side channel a login has, and reading hashes you inherited |
| [Encryption](encryption.md) | `Crypt`, signing, key rotation, JWTs and a JWKS |
| [Sessions](sessions.md) | The bag, flash data, drivers, and session fixation |

## Database

| Page | What it covers |
|---|---|
| [Getting Started](database.md) | Connections, dialects, executors, raw SQL, and the `Send` story |
| [Models](models.md) | `#[derive(Entity)]`, the `Model` trait, route keys, lifecycle hooks |
| [Repositories](repositories.md) | The `Repository` contract, `EntityRepository`, `Criteria` |
| [Relationships](relationships.md) | `has_many`, `belongs_to`, pivots, and why loading is a call |
| [Migrations](migrations.md) | The migrator, the schema builder, DDL from model metadata, ordering |
| [Pagination](pagination.md) | `Paginated<T>` and what it computes for you |

## Testing

[Testing](testing.md) — `TestApp`, factories, the fakes, feature tests against
the real kernel, and the one rule every Rainier double follows.

---

## The concept map

The fastest way in, if you already think in another MVC framework: the
concept as you know it, and where it lives here.

| The concept | In Rainier |
|---|---|
| the IoC container | [`Container`](container.md) |
| the application object | [`Application`](container.md#the-application) |
| service providers | [`ServiceProvider`](providers.md) |
| the application bootstrap file | your `bootstrap.rs`, built with [`Rainier`](lifecycle.md#the-builder) |
| the HTTP kernel | [`Kernel`](lifecycle.md#the-kernel) + your `app/http/kernel.rs` |
| the web routes file | `src/routes/web.rs` — see [Routing](routing.md) |
| declaring a named route | `router.get(…).name(…)` |
| route middleware by name | `.middleware(Authenticate::<User>::resolved())` — [by value, not by name](middleware.md#why-values-and-not-names) |
| resource routes | `router.resource` — [Controllers](controllers.md#resource-controllers) |
| a model in the action signature | [`Bound<Post>`](controllers.md) — implicit route-model binding |
| the authenticated user, injected | [`AuthenticatedUser<User>`](controllers.md) as a parameter |
| a form request | [`FormRequest`](validation.md#request-contracts) + `Validated<T>` |
| reading request input | `request.input("x")` — [Requests](requests.md) |
| the fillable allow-list | falls out of the rules — [Validation](validation.md#mass-assignment) |
| the template engine | [Rainier templates](views.md) |
| a URL from a named route | `urls.route("posts.show", &[("post", slug)])` — [URLs](urls.md) |
| an active-record model | [`#[derive(Entity)]` + `Model`](models.md) |
| creating / created model hooks | [typed hook events](models.md#lifecycle-hooks) |
| the query builder, paginated | [`Criteria` + a repository](repositories.md) |
| has-many / belongs-to | [`HasMany` / `BelongsTo`](relationships.md), loaded for a slice |
| many-to-many, with a pivot | [`BelongsToMany`](relationships.md#many-to-many) |
| eager loading | `Post::author().load(&posts, &*users)` — [one query](relationships.md) |
| relation counts without loading | [`User::posts().count(…)`](relationships.md#counting-without-loading) |
| lazy relation access | [not available, deliberately](relationships.md#why-loading-is-a-call-and-not-a-property) |
| migrations | [`Migrator`](migrations.md) |
| creating a table | [`Step::create`](migrations.md#the-schema-builder) |
| altering a table | [`Step::table`](migrations.md#altering-a-table), with a [derived rollback](migrations.md#the-rollback-is-derived) |
| a constrained foreign id | [`table.foreign_id(..).constrained_on(..)`](migrations.md#keys-and-indexes) |
| writing `down()` by hand | [derived from the change](migrations.md#the-rollback-is-derived) |
| the current user | [a guard](authentication.md) |
| gate checks | [`Gate`](authorization.md) |
| policies | [gate abilities over a subject](authorization.md#policies-as-a-convention) |
| password hashing | [`Argon2Hasher`](hashing.md) |
| jobs, and dispatching them | [`Job` + `QueueManager`](queues.md) |
| mailables | [`Mailable`](mail.md) |
| notifications | [`Notification`](notifications.md) |
| notifying a recipient | `Notify::instance().send(&user, &…)` — [Notifications](notifications.md) |
| `via()` returning channel names | [`Channels::with::<C>()`](notifications.md#channels-are-selected-by-type) — by type |
| one rendering method per channel | [three renderings](notifications.md#three-renderings-not-one-method-per-channel), open channels |
| per-channel recipient addresses | [`Notifiable::route_for`](notifications.md#notifiable) |
| marking an event broadcastable | [`Broadcastable`](broadcasting.md#broadcastable) + a listener |
| a private channel | [`Channel::private`](broadcasting.md#channels) |
| the channel authorisation file | [`ChannelRegistry`](broadcasting.md#authorising-subscriptions) — `routes/channels.rs` |
| the broadcast auth endpoint | [`broadcasting::authorize::<User>`](broadcasting.md#the-endpoint) |
| broadcasting to everyone but the sender | [`event_except`](broadcasting.md#to_others) |
| sockets held by the framework itself | [`WebSocketHandler`](websockets.md), served on the same port |
| authorising a socket route | [`authorize`](websockets.md#authorising), before the handshake |
| events / listeners | [`Dispatcher`](events.md) |
| facades | [facades](facades.md) |
| the console | [the console](console.md), via `cargo run --` |
| the schedule definition | [`Schedule`](scheduling.md) |
| preventing overlapping runs | [`.without_overlapping(ttl)`](scheduling.md#without_overlapping) |
| running on one server only | [`.on_one_server()`](scheduling.md#on_one_server) |
| an atomic cache lock | [`LockManager::lock`](cache.md#atomic-locks) |
| a Kafka integration | [Kafka](kafka.md) — as a broadcaster, a relay and a queue |
| a Kafka broadcast driver | [`KafkaBroadcaster`](kafka.md#broadcasting) |
| a Kafka consumer command | [`relay::spawn`](kafka.md#sockets-across-replicas), in the web process |
| unique jobs | [`Job::unique_id`](queues.md#unique-jobs) |
| the mail and queue fakes | [the fakes](testing.md) |
| feature-testing an endpoint | [`TestApp` + `TestResponse`](testing.md#feature-tests) |
| asserting into a JSON body | [`assert_json_path("a.b", "c")`](testing.md#testresponse) |
| cache remember | [`cache.remember(key, ttl, closure)`](cache.md#remember) |
| raw SQL with bindings | [`database.query(..).bind(id)`](database.md#databasequerysql) |
| a form method override | [`MethodOverride`](middleware.md#methodoverride) |
| a queued listener | [`listen_queued::<E, J>()`](events.md#queued-listeners) |
| an execution time limit | [`Timeout`](middleware.md#timeout), or `SERVER_REQUEST_TIMEOUT` |
| log channels and rotation | [`LOG_FORMAT`](observability.md#logs) and whatever collects stdout |
| the outbound HTTP client, and its fake | [the same shape](http-client.md) |
| a signed route | [`SignedUrls::route`](urls.md#signed-urls) |
| validating a signed link | [`ValidateSignature::resolved()`](urls.md#signed-urls) |
| password confirmation | [`ConfirmPassword::within(..)`](authentication.md#password-confirmation) |
| token abilities | [`Abilities` + `RequireAbility`](authentication.md#token-abilities) |
| throttling keyed by input | [`.keyed_by(..).named(..)`](middleware.md#throttlerequests) |
| a model factory | [`User::factory()`](testing.md#factories) |
| rehashing across hash schemes | [`HashManager`](hashing.md#selection-governs-writing-never-verification) |
| a JWKS document | [`Jwt` + `JwtKeyRing`](encryption.md#jwts-and-a-jwks-document) |
| reading rows a PHP application encrypted | [`APP_CIPHER=php`](encryption.md#reading-what-a-php-application-encrypted) |
| session access | [`request.session()`](sessions.md#the-session-is-on-the-request-not-on-the-facade) |
| flash data | [flash data](sessions.md#flash-data) |
| trusted proxies | [`groups::trust_local_proxies()`](middleware.md#trustproxies) |
| generating the application key | `cargo run -- key:generate` |
| an instrumentation dashboard | [metrics and tracing](observability.md), and your own dashboards |
| generated API documentation | [a document generated from the router and the rules](observability.md#openapi) |
| cache get / put | [the cache](cache.md) |
| encrypting a string | [ciphers](encryption.md#ciphers) — five of them |
| public-key signing and sealing | [Ed25519 and sealed boxes](encryption.md#public-key-cryptography) |
| per-section config files | one module per section in `config/` — [Configuration](configuration.md) |
| controller middleware with only/except | [`ControllerMiddleware`](middleware.md#controller-middleware) |
| the web middleware group | a function returning a `MiddlewareStack` — [Middleware](middleware.md#groups-are-functions) |
| reading config by key | `config.setting(keys::CACHE_DRIVER)?` — a [typed key](configuration.md#typed-keys), a [driver enum](configuration.md#settings-closed-sets-of-values) |
| encrypt / decrypt via a facade | [`Crypt::instance().encrypt`](encryption.md) |

## What Rainier does not have

Deliberate gaps, so you do not go looking:

| Feature | Status |
|---|---|
| Localization | not shipped |
| Clustered WebSocket rooms | [one process's memory](websockets.md#rooms); broadcast through Redis instead |
| Lazy relationship loading | [not possible without `__get`](relationships.md#why-loading-is-a-call-and-not-a-property) |
| `has_many_through`, polymorphic | [two loads and a `where_in`](relationships.md#what-is-not-here) |
| Queued notifications | [queue the job that sends one](notifications.md#what-is-not-here) |
| Password reset, email verification | build from [the auth pieces](authentication.md#what-is-not-here) |
| CSRF middleware | [see the note](authentication.md#what-is-not-here) |
| A fillable allow-list | falls out of the rules — [Validation](validation.md#mass-assignment) |

## Design positions

Every one of these is a place where the conventional MVC answer would have
produced worse Rust. Each links to the page that explains the trade.

- **Middleware is attached by value, never by name.** [Why](middleware.md#why-values-and-not-names)
- **The auth manager is generic over your user model.** [Why](authentication.md#why-generic)
- **Model hooks can veto but not mutate.** [Why](models.md#what-a-hook-can-and-cannot-do)
- **Request bodies are buffered; response bodies stream.** [Why](requests.md#why-bodies-are-buffered)
- **Validation distinguishes absent, null and empty.** [Why](validation.md#absent-null-and-empty)
- **A 5xx message never reaches the client outside debug.** [Why](errors.md#what-the-client-is-told)
- **A notification declares three renderings, not one method per channel.** [Why](notifications.md#three-renderings-not-one-method-per-channel)
- **A relationship is loaded for a slice, never for one model.** [Why](relationships.md#why-loading-is-a-call-and-not-a-property)
- **A migration's `down` is derived from its `up`, not written twice.** [Why](migrations.md#the-rollback-is-derived)
- **An OpenAPI request body is the validator's own rules.** [Why](observability.md#the-request-body-comes-from-the-validator)
- **Nothing is autoloaded and nothing is discovered by name.** [Why](directory-structure.md#two-things-rust-does-differently)
