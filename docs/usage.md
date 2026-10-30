<!--
SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>

SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Using inbuxa-migrate

The full command reference. The [README](../README.md) says what the tool is
and how to install it.

## Commands and global flags

```
inbuxa-migrate <import|export|export-imap|export-managesieve|export-files|export-directory|inspect> [args...]
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

Messages go in batches, as many to an `Email/import` as the target's
`maxObjectsInSet` allows, up to 50, with their blobs uploaded several at a
time -- the target's `maxConcurrentUpload`, and no more than `--threads`.
Each message is still counted on its own: one the target rejects fails
alone, and the rest of its batch lands. A batch that ends without a clear
answer -- a dropped connection, a gateway timeout -- is never sent again as
it was. The target is read first, the messages that arrived are counted as
created, and only the rest are imported again, so none is ever doubled.

While it runs, export prints a line every few seconds for mail, contacts and
events: how many of how many, how fast, and about how long is left. Each
type ends with a line of what was created, updated, left unchanged and
failed.

`--prune` also deletes what is on the target and not in the archive. It asks
first; `--yes` answers for it, for scripts. Export speaks JMAP only; for
other servers, see [Export to IMAP](#export-to-imap) and
[Export to files](#export-to-files).

With `--dry-run`, export reads the target and writes nothing to the account.
It prints the plan in plain words: for each type, how much would be
created, updated, left unchanged or deleted, and what would fail. It
catches in advance the failures a real run would hit:

- a message larger than the target's `maxSizeUpload`;
- a contact, event or other object too large for one request under its
  `maxSizeRequest`, as happens when a photo is carried inline;
- a Sieve script the target would reject. Each script that would be written
  is checked with `SieveScript/validate`, after any `vnd.stalwart.*` names
  are renamed, so what is checked is what would be uploaded. The check needs
  the script as a blob, so the dry run uploads Sieve scripts, and only them;
  a blob that nothing uses is discarded by the server. A target without
  `SieveScript/validate` gets one warning, and its scripts are not checked.

The plan lists each predicted failure and its reason. When anything would
fail, the dry run exits 5, as the real run would, so a script can stop
before it starts.

## Export to IMAP

```
inbuxa-migrate export-imap \
  --url <imap://host[:port]|imaps://host[:port]> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN] --auth-user <USER>) \
  [--allow-cleartext] [--dry-run] \
  <ARCHIVE>
```

Writes an archive's mail into an account on any IMAP server, for a target
that doesn't speak JMAP. It signs in the way `import imap` does, with the
same flags. Contacts, calendars and filters don't go over IMAP; write the
filters with `export-managesieve`, and contacts and calendars with
`export-files`.

Like `export`, it keeps no state. Each run reads the target folder first,
matching messages by Message-ID, or without one by sender, subject, date and
recipients, with size deciding between equals. What's missing is appended,
with its flags and the time it arrived; what's there gets the archive's
flags if they differ. So a second run writes nothing, and an interrupted run
is finished by running it again: a message that landed before the
connection dropped is found, not doubled.

- **Folders.** The inbox goes to INBOX. A sent, drafts, trash, junk or
  archive folder goes to the one the server marks for it with SPECIAL-USE,
  such as Gmail's `[Gmail]/Sent Mail`, and is created with that mark when the
  server allows it. Every other folder is created under the account's
  personal namespace with its path, using the server's separator; that
  separator inside a name becomes `_`. Names go as UTF-8 to a server that
  accepts it, in modified UTF-7 otherwise.
- **Flags.** Read, answered, flagged and draft are IMAP's own. Other keywords
  (Junk, labels) go along where the folder keeps keywords, and the run says
  how many were left off where it doesn't. Deleted isn't carried, so the
  next client to expunge removes nothing that was written.
- A message the server refuses (too large, over quota) is counted as failed
  and the run goes on; a dropped connection ends the run.

With `--dry-run` it signs in, reads the target with `EXAMINE`, and counts
what it would create, append and update, without writing anything.

## Export to ManageSieve

```
inbuxa-migrate export-managesieve \
  --url <sieve://host[:port]|sieves://host[:port]> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN] --auth-user <USER>) \
  [--allow-cleartext] [--dry-run] \
  <ARCHIVE>
```

Writes an archive's Sieve scripts into an account on any ManageSieve server
(RFC 5804), with the flags `import managesieve` takes. It keeps no state:
each run lists the target's scripts, writes each one that is missing or
differs with `PUTSCRIPT`, and leaves alone the ones that match, so a second
run writes nothing. A script with no name is written as `script-<id>`.

- The archive's active script is made active on the target. If no script
  in the archive is active, the target's active script stays as it is.
- A script the server refuses, for instance one using an extension it
  lacks, is counted as failed with the server's reason, and the run goes
  on. If the active script is refused, it is not made active.
- Scripts are never deleted from the target, and names that match are
  overwritten only when their content differs.

With `--dry-run` it signs in, lists and compares, and checks each script it
would write with `CHECKSCRIPT`, so a script the server would refuse is
counted as failed before anything is written. A server without
`CHECKSCRIPT` (one that advertises no `VERSION`) gets the comparison only.

## Export to files

```
inbuxa-migrate export-files [--objects <list>] [--dry-run] <ARCHIVE> <DIR>
```

Writes `ARCHIVE` out as plain files in open formats, for a server or app
that doesn't speak JMAP, or to keep a copy that needs no tool to read. It
never opens a network connection. `DIR` must be new or empty, and is made
readable by its owner only, like the archive.

```
DIR/
  mail/            Maildir++: the inbox here, other folders as .Parent.Child
  contacts/*.vcf   one file per address book
  calendars/*.ics  one file per calendar
  sieve/*.sieve    one file per script
  files/           the file tree
  manifest.json    what the formats above don't hold
```

- **Mail** is laid out as Dovecot and Courier keep it, so either can serve
  it as it is, and `import maildir` reads it back. Folder names are in
  modified UTF-7. A `.` or `/` inside a folder's name becomes `_` on disk,
  since Maildir++ uses the dot between levels. A message kept in several
  folders is written to each. Read, answered, flagged, draft, forwarded and
  deleted are the standard Maildir flags; other keywords (Junk, labels) are
  the letters `a` to `z`, named in each folder's `dovecot-keywords`, up to
  26 per folder. Each file is dated when the message arrived.
- **Contacts** are vCard 4.0, and a card in two address books is in both
  files. **Calendars** are iCalendar, one VCALENDAR each, named by
  `X-WR-CALNAME`, with each time zone once. Photos and attachments the
  archive holds go inside as `data:` URIs, as `export` sends them.
  `import takeout` reads both back.
- **Sieve scripts** are written as they are; the manifest says which is
  active.
- **Files** keep their names and dates. Links are listed in the manifest
  and never made, since the source chose where they point.

Every name comes from the source, so none is used as a path as it stands:
separators and names Windows refuses become `_`, and names that clash, even
only by case, are numbered.

`manifest.json` records the source account, every folder's real name,
path, role and message count, each address book and calendar with its
file, color and time zone, which Sieve script is active, the identities,
and the links. It is written last: a tree without one was interrupted, and
the way to finish it is to run again into an empty directory.

The layout and the manifest are specified at
<https://coffeylabs.org/specs/account-export/>, so other tools can read and
write them too.

`--objects` takes the same types as `export`; naming a folder or a message
type writes all of mail, and the same for contacts and calendars. With
`--dry-run` it counts what it would write and creates nothing.

## Export a directory

```
inbuxa-migrate export-directory \
  --url <URL> \
  (--auth-basic <USER> [--auth-password <PASS>] | --auth-bearer [TOKEN]) \
  [--domain <NAME>]... [--dry-run] \
  <DIR>
```

Writes who has an address on a server, so the same accounts can be set up
on another one: domains, users, groups, mailing lists, their aliases and
quotas, and the tenants and roles they belong to. It reads inbuxa's
registry, or Stalwart's, so the login must be an administrator's. Mail and
the rest of each account's data move with `import` and the exports above.

Nothing secret is written, because nothing secret is asked for: every read
names the fields it wants, and credentials, app passwords, OTP seeds, API
keys, encryption keys and DKIM keys are not among them. Each new server
sets its own passwords and DKIM keys.

```
DIR/
  directory.json   everything below, exactly, with roles and tenants
  domains.csv      name, enabled, aliases, catch-all, sub-addressing, tenant
  users.csv        address, description, roles, groups, aliases, disk quota
  groups.csv       address, members, aliases, disk quota
  lists.csv        address, recipients, aliases
  aliases.csv      each alias with the address it delivers to
```

- Accounts, groups and lists are named by address, and roles and tenants by
  name, never by the server's ids. A group lists its members, a user the
  groups it is in.
- Roles are `user`, `admin` or `default`, or the custom roles by name;
  `permissions` holds what an account adds to its roles (`merge`) or has in
  their place (`replace`).
- Quotas are by name, in bytes or counts (`maxDiskQuota` and the rest); the
  CSV files show `maxDiskQuota` as `disk_quota`.
- In the CSV files, a cell with several values joins them with `;`. The
  text comes from the server and these files are made to be opened in a
  spreadsheet, so a cell that one would run as a formula starts with `'`.
  `directory.json` keeps every value as it is.

`--domain` keeps the export to the domains named, with what they use:
their tenants and the roles their accounts hold. A group's members on
other domains are still listed by address. `DIR` must be new or empty, and
is made readable by its owner only. `directory.json` is written last, so a
directory without one was interrupted. With `--dry-run` it reads the
registry and counts what it would write, and creates nothing.

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
cargo test --test integration_export_imap        -- --ignored --test-threads=1
cargo test --test integration_export_managesieve -- --ignored --test-threads=1

# Slow tests
cargo test --test mock_jmap -- --ignored
```

One thread is required, not advised. The tests in a binary share its
container, and each one creates and removes the same disposable domain,
`inbuxa-migrate.org`, and opens its archive with SQLite's `EXCLUSIVE` lock.
Separate binaries each boot their own container on their own ports, so
running them one after another is safe.

