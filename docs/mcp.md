# MCP

Rainier can serve a [Model Context Protocol](https://modelcontextprotocol.io)
server from the application itself: **tools** an AI client may call,
**resources** it may read, and **prompts** a person may pick, declared in Rust
next to the code they use. With [Lookout](lookout.md) on as well, Lookout's
own tools and prompts join it, so an assistant can read what the dashboard
shows — the slowest routes, a request's waterfall, the open cases — and
explain it.

It is served over Streamable HTTP at one path, `/_mcp`, from the router the
application already runs. It keeps no session and never opens a stream of its
own, so any replica answers any request and nothing more needs deploying.

---

## Turning it on

Add `mcp` to the framework's features:

```toml
[dependencies]
rainier-framework = { version = "2", features = ["mcp"] }
```

and switch it on in `.env` or the process environment:

```env
MCP_ENABLED=true
MCP_TOKENS=a-random-string-of-at-least-32-characters
```

| Variable | Default | What |
|---|---|---|
| `MCP_ENABLED` | `false` | Serve it at all |
| `MCP_PATH` | `/_mcp` | Where |
| `MCP_TOKENS` | — | Bearer tokens that are let in, comma-separated, each at least 32 characters — two at once to rotate one without a gap |
| `MCP_READ_ONLY_CLIENTS` | — | [Named clients that may only read](#read-only-clients), as `name=token` pairs, comma-separated |
| `MCP_OPEN_LOCALLY` | `true` | Let everyone in when `APP_ENV=local` |
| `MCP_ALLOWED_ORIGINS` | — | Browser origins allowed besides the server's own host, comma-separated |
| `MCP_LOOKOUT` | `true` | Offer [Lookout's tools](#lookout) when Lookout is on |
| `APP_VERSION` | the framework's | The version the server reports |

A token shorter than 32 characters, or an `MCP_PATH` without a leading `/`,
fails the boot. Off, nothing is bound and nothing is mounted.

The same settings can be adjusted in code, over what the environment says:

```rust
Rainier::new(".").with_mcp_settings(|settings| McpSettings { enabled: true, ..settings })
```

---

## Tools, resources and prompts

`Rainier::with_mcp` is handed the server and returns it; call it as often as
you like. The server is named after `APP_NAME`.

```rust
use rainier_framework::mcp::{Prompt, Resource, Tool, ToolResult};
use serde_json::json;

Rainier::new(".").with_mcp(|server| {
    server
        .instructions("Tools for this blog: its posts and its authors.")
        .tool(
            Tool::new("count_posts", "How many posts are published, per author.", |_call| async {
                let counts = posts().published_per_author().await?;
                Ok(ToolResult::json(&counts))
            })
            .read_only(),
        )
        .tool(
            Tool::new("find_post", "One post by its slug, with its body.", |call| async move {
                let Some(slug) = call.str("slug") else {
                    return Ok(ToolResult::error("Give the post's slug."));
                };
                match posts().by_slug(slug).await? {
                    Some(post) => Ok(ToolResult::json(&post)),
                    None => Ok(ToolResult::error(format!("No post {slug}."))),
                }
            })
            .input_schema(json!({
                "type": "object",
                "properties": { "slug": { "type": "string" } },
                "required": ["slug"],
            }))
            .read_only(),
        )
        .resource(
            Resource::text("blog://style", "style guide", include_str!("../STYLE.md"))
                .mime_type("text/markdown"),
        )
        .prompt(
            Prompt::template(
                "review_post",
                "Review a draft against the style guide.",
                "Read blog://style, then review the post {{slug}} against it.",
            )
            .argument("slug", "The post's slug", true),
        )
})
```

- **A tool** is a name, a description — what the model decides by, so say
  what it does, when to use it and what it answers — a JSON Schema for its
  arguments, and an async handler given a `Call`: the arguments, and the HTTP
  request that carried them (`call.request()`), for a tool that decides by
  who is asking. `read_only()` and `annotations(...)` are hints a client uses
  to decide whether to ask before running it.
- **What a tool answers**: `ToolResult::text`, `ToolResult::json` (compact
  JSON text), or `ToolResult::error` for a failure the model should see and
  can correct. A handler that returns `Err` is answered the same way, with the
  error's message and, for a validation error, its details; a **server
  error's** message is logged instead, and the model is told only that the
  tool failed, so a connection string never reaches it.
- **A resource** is a URI and a reader. `Resource::text` is one that never
  changes.
- **A prompt** renders into a user message; a client shows prompts as
  commands. `Prompt::template` replaces `{{argument}}` with what was given
  (nothing, for an optional argument left out), and refuses to render without
  a required one.

The server is bound in the container as `Arc<McpServer>`. It is the protocol
without the transport: `McpServer::handle` answers one JSON-RPC message or a
batch, which is also how to test a tool without HTTP; `handle_as` does the
same for a caller with a given `Access`, such as a read-only client.

---

## Who may use it

| Where | Who |
|---|---|
| `APP_ENV=local` (with `MCP_OPEN_LOCALLY`, the default) | everyone |
| anywhere | a request with `Authorization: Bearer <one of MCP_TOKENS>`, compared in constant time |
| anywhere | a [read-only client](#read-only-clients), with its own token from `MCP_READ_ONLY_CLIENTS` |
| anywhere | whoever the application's own check lets in |

The application's check sees the request's headers:

```rust
Rainier::new(".").with_mcp_access(|request| {
    let from_gateway = request.header("x-internal-gateway") == Some("1");
    Box::pin(async move { from_gateway })
})
```

Anyone else gets **the router's own 404**, on every method, the same bytes as
a path that never existed. The endpoint is mounted outside every middleware
group, like Lookout's dashboard, so a refused request carries no session
cookie either.

A request with an `Origin` header — one a browser sent — is refused with
`403` unless the origin is the server's own host or is listed in
`MCP_ALLOWED_ORIGINS`: that is the protocol's defence against a web page
reaching a local server by rebinding a name. Clients that are not browsers
send no `Origin`.

Treat a token as you would a database password: with Lookout on, it reads
everything Lookout recorded.

### Read-only clients

A token in `MCP_TOKENS` can do everything the server offers, and every holder
of it looks the same. When an assistant — or anything you would rather not
trust with writes — needs the server, give it a **read-only client** instead:
a name and a token of its own.

```env
MCP_READ_ONLY_CLIENTS=reviewer=a-random-string-of-at-least-32-characters,triage-bot=another-random-string-of-32-or-more
```

A read-only client:

- is shown, by `tools/list`, only the tools marked
  [`read_only()`](#tools-resources-and-prompts), and is refused the rest by
  `tools/call` with an `invalid params` error naming the tool. With Lookout,
  that leaves out `lookout_update_case`. Resources and prompts read, so it
  keeps them;
- is **logged by name** on every method it calls, at `info` on the
  `rainier_mcp::access` target, with the tool, prompt or resource it named —
  so what one client read can be told from what another did;
- is handed to every handler: `call.client()` is its name (`None` for full
  access), and `call.access()` says how it got in. A tool that should let such
  a client change something is better as its own endpoint that records the
  name as the actor than as a write tool it can call;
- is taken away on its own: remove its entry and restart. Nobody else's token
  changes.

Each entry is `name=token`. The name is 1–64 letters, digits, `.`, `_` or `-`;
the token at least 32 characters. A name given twice, a token given twice, or
a token that is also in `MCP_TOKENS` fails the boot — two clients on one token
could not be told apart, and a read-only client on a full token would not be
read-only. In code, `McpGate::read_only_client(name, token)` does the same.

"Read-only" is the tool's own `read_only()` claim. A tool marked read-only
that writes anyway is a bug in the tool, and this cannot catch it: mark a tool
read-only only when it is.

---

## Connecting a client

With Lookout on, its dashboard has an **MCP** page (`/_lookout/mcp`, under
"Lookout" in the menu) that says all of this for the deployment in front of
you: the server's address, whether a token is needed here (it never shows
one), the Claude Code command, a project `.mcp.json`, a `curl` that lists the
tools, and the tools and prompts the server offers. Without an MCP server it
says how to turn one on.

Any client that speaks Streamable HTTP. For Claude Code:

```sh
claude mcp add --transport http blog https://app.example.com/_mcp \
  --header "Authorization: Bearer $MCP_TOKEN"
```

and locally, where it is open:

```sh
claude mcp add --transport http blog-local http://localhost:8000/_mcp
```

What it speaks:

- protocol revisions `2025-11-25`, `2025-06-18`, `2025-03-26` and
  `2024-11-05`, answering a client that asks for another with the newest;
- `initialize`, `ping`, `tools/list`, `tools/call`, `resources/list`,
  `resources/templates/list`, `resources/read`, `prompts/list`,
  `prompts/get`, and `logging/setLevel` (accepted, and ignored);
- a `POST` per message or batch, answered with `application/json` — or
  `202 Accepted` for notifications. `GET` and `DELETE` answer `405`: there is
  no server-initiated stream and no session to end. An `MCP-Protocol-Version`
  header naming a revision it does not speak is a `400`.

---

## Lookout

With the `lookout` feature on and `LOOKOUT_ENABLED=true`, boot adds Lookout's
tools and prompts to the server, and a paragraph to its instructions saying
what batches, families, aggregates and cases are. `MCP_LOOKOUT=false` leaves
them out. They read the same store as the dashboard and run the same code as
its [JSON API](lookout.md#the-json-api), so the parameters and the validation
are the API's, and an invalid one comes back as a tool error naming it.

| Tool | What |
|---|---|
| `lookout_status` | the store, watchers, sample rates, retention — what can be found |
| `lookout_batches` | kept batches, newest first, filtered by kind, route, user, tag, trace, exception, slow or text |
| `lookout_batch` | one batch with its entries — the waterfall — cut to `max_entries` (300) |
| `lookout_entries` | entries of one type across batches, one family, or matching text |
| `lookout_aggregates` | the slowest or busiest of a type over a window, with percentiles |
| `lookout_series` | a type or a family over time, with a forecast |
| `lookout_family` | the key a family id names |
| `lookout_insights` | what is new, rising, falling, slower or faster |
| `lookout_cases` | cases by status, severity or text |
| `lookout_case` | one case |
| `lookout_update_case` | resolve, reopen, ignore, or set the severity — the one tool that writes |
| `lookout_models` | every model written, with its writes per action |
| `lookout_model_changes` | one model's recorded changes |

An assistant reading Lookout is not the application's work, any more than
browsing the dashboard is: while the server offers Lookout's tools, requests
to the `mcp` route are left out of Lookout's [scope](lookout.md#scope-and-sampling)
— a failure in one is still kept.

Times may be RFC 3339 or relative to now — `30m`, `6h`, `7d`. Numbers and
booleans may be JSON numbers and booleans.

The prompts are recipes — which tools to call, in what order, and what to
report:

| Prompt | Arguments | What |
|---|---|---|
| `lookout_health` | `since` | errors, latency and what changed, in five lines |
| `lookout_triage` | `severity` | the open cases, worst first, with likely causes |
| `lookout_slow_route` | `route`, `since` | where a route's time goes, and what to change |
| `lookout_investigate_case` | `case` (required) | one case: what fails, for whom, since when, the fix |

Entries hold what users sent — URLs, query text, exception messages. The
instructions tell the model to treat them as data, but a client that lets the
model act on what it reads should still ask before it does.

An application that is not on the framework's boot can add them itself:

```rust
let server = rainier_lookout::mcp::register(server, store, config, "production");
```
