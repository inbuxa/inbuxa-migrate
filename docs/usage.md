<!--
SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
SPDX-FileCopyrightText: 2026 John Coffey <johnellis@linux.com>

SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Using inbuxa-migrate

The full command reference. The [README](../README.md) says what the tool is
and how to install it.

## Commands and global flags

```
inbuxa-migrate <import|export|inspect> [args...]
```

Every command takes a few global flags -- verbosity, worker pool size, retry
policy, TLS handling -- besides its own. The ones that matter most:

| Flag | Purpose |
| --- | --- |
| `-j, --threads <N>` | Worker pool size (default: logical CPUs). |
| `--dry-run` | Compute the full plan; perform no writes. |
| `-v`, `-vv`, `-vvv` | Increase log verbosity. |
| `-q, --quiet` | Warnings and errors only. |
| `--max-retries <N>` | Max retries per request on transient failures (default 5). |
| `--allow-invalid-certs` | Accept a self-signed or otherwise invalid certificate from the server named by `--url` (see below). |

### Invalid certificates

`--allow-invalid-certs` is for a server with a self-signed certificate, and
it applies to that server only: the host in `--url`, whether that is the
source of an import or the target of an export. For an Exchange import with
no `--url`, it covers the mailbox's own domain and the hosts under it, which
is where on-premises Autodiscover looks, and then only the EWS endpoint
Autodiscover finds.

Every other host is verified as usual, including a host the server
redirects to or names for its API, uploads or downloads. The Microsoft and
Google sign-in and cloud endpoints are always verified, with or without the
flag: a certificate that fails there is an attack or a broken network, never
a server to trust.

Connections time out rather than wait forever: 30 seconds to connect, 5
minutes for the server's first byte, and 30 minutes to read a whole
response. A timed-out request is retried like any other transient failure.

Secrets come from the `INBUXA_MIGRATE_*` environment variables or a prompt;
see [Credentials](../README.md#credentials). The command line takes them too,
but should not be given them.

## Import

```
inbuxa-migrate import <source> [source-args...] <ARCHIVE>
```

Reads a source account into the SQLite `ARCHIVE`, creating it if it is absent.
An archive remembers which account filled it; every importer takes
`--allow-source-change` to fill it from a different one anyway.

### JMAP

```
inbuxa-migrate import jmap \
  --url <URL> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN]) \
  (--account-id <ID> | --account-name <NAME>) \
  [--objects <list>] \
  <ARCHIVE>
```

Imports one JMAP account, from inbuxa, Stalwart or any other JMAP server. `--objects` accepts a comma-separated list of object tokens (`mailbox,email,calendar,calendarevent,addressbook,contactcard,identity,sievescript,participantidentity,filenode`). The default is everything the server advertises.

### IMAP

```
inbuxa-migrate import imap \
  --url imap(s)://host[:port] \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN] --auth-user <USER>) \
  [--include <REGEX>...] [--exclude <REGEX>...] [--exclude-special <ROLE>...] \
  [--folder <NAME>...] [--subscribed-only] [--noautomap] \
  [--include-deleted] [--allow-cleartext] [--compress] \
  [--fetch-batch <N>] [--fetch-batch-mib <MIB>] [--imap-connections <1..8>] \
  <ARCHIVE>
```

Imports mail, and only mail, from any IMAP server. Folders are chosen with
`--include` and `--exclude` patterns, or by exact name with `--folder`, but
not both. `--exclude-special` drops folders by SPECIAL-USE role.

Messages are fetched in chunks of at most `--fetch-batch` messages and
`--fetch-batch-mib` MiB (32 by default), so a folder of large attachments is
fetched a little at a time, like any other. A single message larger than the
cap is fetched on its own. Each chunk is written to the archive as it
arrives: an interrupted import keeps what it fetched, and the next run picks
up from there. A message that can't be imported is reported with its folder
and UID, and the rest of the folder carries on.

### CalDAV

```
inbuxa-migrate import caldav \
  --url <http(s)://host[/path]> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN]) \
  [--allow-cleartext] [--dav-connections <1..8>] [--multiget-batch <N>] \
  <ARCHIVE>
```

Discovers the user's CalDAV principal (or accepts a URL pointing straight at a calendar-home or calendar), then imports calendars and events.

### CardDAV

```
inbuxa-migrate import carddav \
  --url <http(s)://host[/path]> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN]) \
  [--allow-cleartext] [--dav-connections <1..8>] [--multiget-batch <N>] \
  <ARCHIVE>
```

Same shape as `caldav`, but for address books and contacts.

### WebDAV

```
inbuxa-migrate import webdav \
  --url <http(s)://host[/path]> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN]) \
  [--allow-cleartext] [--dav-connections <1..8>] [--multiget-batch <N>] \
  <ARCHIVE>
```

Imports a plain WebDAV file collection as a JMAP `FileNode` tree.

### ManageSieve

```
inbuxa-migrate import managesieve \
  --url sieve(s)://host[:port] \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN] --auth-user <USER>) \
  [--allow-cleartext] \
  <ARCHIVE>
```

Imports Sieve scripts only. Each script is stored once by content, and the
active one is recorded.

### Maildir

```
inbuxa-migrate import maildir <MAILDIR> <ARCHIVE> \
  [--include <REGEX>...] [--exclude <REGEX>...] [--folder <NAME>...] \
  [--noautomap] [--include-deleted]
```

Reads a local Maildir++ tree: a directory with `cur/`, `new/` and `tmp/`. No
network. Folders are chosen as for IMAP.

### Google Takeout

```
inbuxa-migrate import takeout <PATH> <ARCHIVE> [--noautomap]
```

Finds every `.mbox`, `.ics` and `.vcf` file under a directory and imports it.
It is shaped for Google Takeout but reads any such tree. `--noautomap` stops
it giving Gmail's system labels their mailbox roles.

### Microsoft Exchange (EWS)

```
inbuxa-migrate import exchange-ews \
  [--url <EWS-ENDPOINT>] [--mailbox <SMTP>] \
  [--mailbox-kind primary|archive|public-folders] \
  (--auth-basic <USER> [--auth-password <PASS>] \
   | --auth-bearer [TOKEN] [--ews-tenant <T> --ews-client-id <ID> \
                            (--ews-device-code | --ews-client-secret <SECRET>)]) \
  [--ews-connections <1..8>] [--ews-getitem-batch <N>] [--ews-getitem-batch-mib <MIB>] \
  [--ews-attachment-batch <N>] \
  [--ews-no-syncfolderitems] \
  <ARCHIVE>
```

Imports a mailbox from an on-premises Exchange Server through EWS. Without
`--url` it uses Autodiscover, and then needs `--mailbox`. It signs in with
Basic, with a bearer token acquired beforehand, with OAuth's interactive
device-code flow, or with app-only client credentials.

Items are fetched in GetItem batches of at most `--ews-getitem-batch` items
and `--ews-getitem-batch-mib` MiB (32 by default), `--ews-connections` at a
time. The byte cap applies where Exchange reports each item's size, which it
does on a full listing; items found through an incremental sync are batched
by count.

For Exchange Online, use `exchange-graph` instead. Microsoft is retiring EWS in Exchange Online: from October 1, 2026 it is blocked unless a tenant administrator sets `EwsEnabled` to `True` and adds the client id to `EwsAllowedAppIDs`, and on April 1, 2027 it is switched off for every tenant. On-premises Exchange Server is not affected.

### Microsoft Exchange (Graph)

```
inbuxa-migrate import exchange-graph \
  (--client-id <UUID> [--tenant <ID>] | --access-token [TOKEN]) \
  [--user <UPN|UUID>] \
  [--mailbox-kind primary|archive] \
  [--objects mail,calendar,contacts] \
  [--event-body-format text|html] \
  [--graph-connections <1..16>] [--top <1..1000>] \
  <ARCHIVE>
```

Imports a mailbox from Exchange Online through Microsoft Graph. Without
`--access-token` it signs in with the interactive device-code flow. `public-folders` is rejected here: Graph does not expose public folders, so they can only be imported with `exchange-ews`, which for Exchange Online is subject to the retirement described above.

## Export

```
inbuxa-migrate export \
  --url <URL> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN]) \
  (--account-id <ID> | --account-name <NAME>) \
  [--objects <list>] [--prune [--yes]] \
  <ARCHIVE>
```

Writes `ARCHIVE` into an account on a JMAP server, usually inbuxa. It keeps no
state of its own: every run matches the archive against the target afresh.
By default it only adds and updates: what the target lacks is created, what
it has is brought up to date, and anything on the target that the archive
does not cover is left alone. So a second run after a later import carries
what changed at the source in between.

- **Email** is matched by Message-ID, or without one by sender, subject, date
  and recipients; where several messages share one, size decides. A message
  the source kept in several folders -- IMAP and Maildir copies, Gmail labels
  -- is written once, in all of them. On a match, its keywords (read,
  flagged and the rest) are set to the archive's, and so are its memberships
  of folders this run migrated. Folders that exist only on the target are
  left alone, and a message is never left in no folder.
- **Contacts and events** are matched by UID. When both copies carry an
  `updated` time, the archive's is written only if it is newer; otherwise
  the properties that differ are written.
- **Sieve scripts** are matched by name, and the target's is replaced when
  its content differs from the archive's.

`--prune` also deletes what is on the target and not in the archive. It asks
first; `--yes` answers for it, for scripts. Export speaks JMAP only.

## Inspect

```
inbuxa-migrate inspect <ARCHIVE> [TYPE] [--limit <N>] [--offset <N>]
```

Read-only dump of a local archive. This command never opens a network connection and never writes to the archive.

- Omit `TYPE` for a per-type summary (counts of every object kind plus blob storage stats).
- Pass an object type to dump it: `mailbox`, `email`, `identity`, `sievescript`, `addressbook`, `contactcard`, `calendar`, `calendarevent`, `participantidentity`, `filenode`.
- `mailbox` and `filenode` render as a tree (`--limit`/`--offset` are ignored); all other types use a paginated list and respect `--limit` and `--offset`.


## Live tests

The tests against real servers -- a Stalwart server as a JMAP source, and
Dovecot, Cyrus, Radicale, Baikal and Apache `mod_dav` -- are marked
`#[ignore]`. Each test binary boots its own throwaway container through
`testcontainers`, pulling the image on first use, so Docker must be running
(`docker info` succeeds) before they start.

Run them one binary at a time, always with one test thread:

```sh
cargo test --test sync_jmap          -- --ignored --test-threads=1   # live JMAP import/export/convergence/prune
cargo test --test sync_imap          -- --ignored --test-threads=1
cargo test --test sync_managesieve   -- --ignored --test-threads=1
cargo test --test sync_maildir       -- --ignored --test-threads=1
cargo test --test sync_caldav        -- --ignored --test-threads=1
cargo test --test sync_carddav       -- --ignored --test-threads=1
cargo test --test sync_webdav        -- --ignored --test-threads=1
cargo test --test live_stalwart      -- --ignored --test-threads=1
cargo test --test seed_smoke         -- --ignored --test-threads=1
cargo test --test seed_only          -- --ignored --test-threads=1

# Third-party-server tests (one container each):
cargo test --test integration_radicale -- --ignored --test-threads=1
cargo test --test integration_baikal   -- --ignored --test-threads=1
cargo test --test integration_webdav   -- --ignored --test-threads=1
cargo test --test integration_dovecot  -- --ignored --test-threads=1
cargo test --test integration_cyrus    -- --ignored --test-threads=1

# Slow tests
cargo test --test mock_jmap -- --ignored
```

One thread is required, not advised. The tests in a binary share its
container, and each one creates and removes the same disposable domain,
`inbuxa-migrate.org`, and opens its archive with SQLite's `EXCLUSIVE` lock.
Separate binaries each boot their own container on their own ports, so
running them one after another is safe.

