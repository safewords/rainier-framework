# Filesystem

One port — `Filesystem` — over a local directory, an in-process map, and
anything that speaks S3. An application writes the same six calls whichever disk
is behind them.

```rust
use rainier_framework::facades::Storage;

Storage::instance().put("avatars/7.png", bytes).await?;
let avatar = Storage::instance().get("avatars/7.png").await?;

Storage::disk("archive").unwrap().put("2026/report.pdf", bytes).await?;
```

| | |
|---|---|
| `get` / `put` / `delete` / `exists` | the four that need no explanation |
| `metadata` / `size` / `last_modified` | one stat call, or the two fields off it |
| `list(prefix)` | the files under a prefix |
| `directories(prefix)` | the **directories** directly under it, not recursing |
| `copy` / `rename` | read-then-write by default; S3 does it server-side |
| `url` / `temporary_url` | a public link, and a signed one — [not interchangeable](#a-public-url-is-never-a-substitute-for-a-signed-one) |
| `get_string` / `put_string` / `append` | the text conveniences |

Every path is normalised before it reaches a driver, and a traversal is
**refused** rather than resolved: `../../etc/passwd` is an error, not a file
outside the disk.

## Declaring disks

A disk is not one setting — it names its own driver, and its own bucket,
endpoint, region and credentials. Two disks on two services have nothing to
share, so they are declared as a **section**: a `default` naming one entry, and
each entry naming its own driver.

```rust
use rainier_framework::filesystem::{DiskConfig, Disks, S3Disk};

Rainier::new(".").with_disks(
    Disks::new("uploads")
        .with("uploads", DiskConfig::local("storage/app"))
        .with("archive", S3Disk::new("archive-bucket").region("us-east-1")),
)
```

which writes it to the `filesystems` key, so the same thing can come from the
configuration tree instead:

```json
{
  "filesystems": {
    "default": "uploads",
    "disks": {
      "uploads": { "driver": "local", "root": "storage/app" },
      "archive": { "driver": "s3", "bucket": "archive-bucket", "region": "auto",
                   "endpoint": "https://account.r2.cloudflarestorage.com",
                   "key": "…", "secret": "…" }
    }
  }
}
```

Note the spelling: the entries are `disks` here, where the [queue](queues.md)
and [database](database.md) sections call theirs `connections`. That asymmetry
is deliberate — the shapes mirror the framework these sections are ported from,
including its own inconsistency, so a section carried across does not have to be
re-learned.

The framework seeds one `local` disk under `storage/app`, so a fresh clone has
working storage with no section at all. `FILESYSTEM_DISK` names which declared
disk is the default, because which disk a deployment writes to is a
deployment's decision — and naming one it never declared is a **boot failure**
rather than a fallback to the seeded one.

Each disk is built from **its own** declaration. There is no shared connector to
inherit: the version of this that built them all from one produced a disk with
the right bucket name pointed at the wrong service, and that failure raises
nothing — the bucket resolves, the prefix is empty, and a listing reports
nothing, which reads exactly like a bucket that is genuinely empty.

`with_storage` hands over a `Storage` you built yourself and wins over anything
declared, the way `with_database` does.

### What a declaration refuses

Each is a case where accepting it would give a working-looking disk reading or
writing somewhere other than the one intended, so each is a boot failure:

| Declaration | Why |
|---|---|
| no `driver` | an assumed driver is a disk pointed at whatever the default happens to be |
| `bucket` on a `local` disk | somebody believes these files reach object storage; they reach a directory |
| `key` without `secret` | falls back to the ambient chain, and reads a **different account's** bucket of the same name |
| `key` and `secret` with no `region` | a signed request has to name one, and a guess is a wrong one |
| `default` naming an undeclared disk | the fallback would be silent, and the wrong disk |
| a `driver` nothing answers to | the fallback would be a disk on whichever backend the default happens to be |

A disk whose cargo feature is not enabled fails at build time too: an `s3` disk
without the `s3` feature is an error naming the feature, never a quiet
substitution of a local directory.

## A driver the framework does not ship

The `driver` field is not limited to the drivers in this crate. An application
registers its own and then declares it by name like any other:

```rust
use rainier_framework::filesystem::{CustomDisk, Filesystem, FilesystemDriver};

FilesystemDriver::extend("my-store", |disk: CustomDisk| async move {
    let endpoint = disk.string("endpoint").unwrap_or_default();
    Ok(Arc::new(MyStore::connect(endpoint).await?) as Arc<dyn Filesystem>)
})?;
```

```json
{ "driver": "my-store", "endpoint": "https://example.invalid", "namespace": "uploads" }
```

Those settings arrive at the factory as a `CustomDisk`. They are **not** checked
against the built-in field list — the framework has no idea what a driver it
does not ship needs — so a custom driver validates its own, which
`CustomDisk::settings_as::<T>()` makes one `?`.

**An unknown driver never becomes a working default.** The name still has to
resolve, and it is checked twice, with two different messages because they have
two different fixes:

| When | What happened | What the error says |
|---|---|---|
| a declaration is read | the name is neither built in nor registered | `` `x` is not a valid filesystem driver ``, listing the built-ins *and* everything registered |
| a declaration is built | it names a driver nobody registered | `` no filesystem driver is registered under `x` ``, and to register it first |

The second is the one somebody will hit: the declaration was assembled in code
rather than read from configuration, so nothing checked the name until the disk
was built. "Register it before the disk that names it is built" is a different
fix from "you spelled it wrong".

## Signed URLs

`url(path)` is a public, permanent link, and `None` for a driver with no public
face — a local disk behind an application is not reachable by URL, and
pretending otherwise produces a link that 404s.

`temporary_url(path, expires_in)` is what restricted content needs: the object
is not publicly readable, so the link carries its own proof of authorisation and
that proof runs out.

```rust
let link = Storage::disk("content")
    .unwrap()
    .temporary_url("paid/film.mp4", Duration::from_secs(300))
    .await?;
```

### A public URL is never a substitute for a signed one

This is the one rule that must not bend, and it is why `temporary_url` answers
`Result<String>` rather than `Result<Option<String>>`.

`url` answers with a link anyone who sees it can keep and pass on, **for ever** —
there is nothing in it to expire. A driver that cannot sign and quietly answered
with that instead would ship every paywalled object with a permanent,
redistributable link, and nothing at the call site would look wrong: it asked
for a temporary URL and got a URL.

`Option` would be an invitation. `unwrap_or_else(|| public_url)` is a natural
line to write, reads as a graceful fallback, and is exactly that paywall bypass.
`Result` makes the same mistake require deliberately discarding an error, and
makes the correct handling a `?`.

A driver that cannot sign therefore **fails**, naming itself, and it renders as
**501** — because from outside that is what it is: the deployment put restricted
content somewhere with no way to sign for it, and no request the client could
have made differently would work.

## Listing a subtree's shape

`list(prefix)` answers with files and says nothing about what is below them, so
"how many variants are stored beside this one" has no answer through it. The
alternative — listing every key in the subtree and cutting each at the first
separator — downloads a whole subtree to learn its shape.

```rust
let sub = disk.directories("renditions/7").await?;   // ["renditions/7/1080p", …]
let files = disk.list(&sub[0]).await?;
```

What comes back is a **prefix `list` accepts**, not a bare segment, so
descending is passing the answer back in rather than rebuilding a path at every
call site. `""` enumerates the root, and a prefix with nothing under it is an
empty `Vec` rather than an error.

## Drivers

| `FilesystemDriver` | Feature | Shared | Durable | Signs |
|---|---|---|---|---|
| `Local` | — | no | as durable as the volume | no |
| `Memory` | — | no | no | no |
| `S3` | `s3` | yes | yes | yes |
| anything [registered](#a-driver-the-framework-does-not-ship) | — | its own answer | | |

`S3` covers Cloudflare R2, MinIO and anything else speaking the protocol — set
`endpoint`, and `path_style` where the service wants it.

## Testing

```rust
use rainier_framework::filesystem::MemoryFilesystem;

let disk = MemoryFilesystem::new();
```

No directory to clean up and no server. It refuses `temporary_url` like any
other driver that cannot sign, which is what makes a test asserting on a signed
link fail rather than pass against a permanent one.
