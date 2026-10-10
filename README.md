# inbuxa-migrate

> [!NOTE]
> Development happens on [git.coffeylabs.org/inbuxa/inbuxa-migrate](https://git.coffeylabs.org/inbuxa/inbuxa-migrate); the copy on GitHub is a read-only mirror.
> Report issues at **[git.coffeylabs.org/inbuxa/inbuxa-migrate/issues](https://git.coffeylabs.org/inbuxa/inbuxa-migrate/issues)**, join discussions at **[community.coffeylabs.org](https://community.coffeylabs.org)**, or chat on **[Discord](https://discord.gg/nqcY4TKfAn)**.

One program that moves an account into **inbuxa**: its mail, calendars,
contacts, identities, Sieve scripts and files.

    inbuxa-migrate import imap … alice.sqlite    an account into an archive
    inbuxa-migrate inspect alice.sqlite          what landed
    inbuxa-migrate export … alice.sqlite         the archive into inbuxa

Between the two sides sits an archive: one SQLite file holding one account.
`import` fills it from the old server; `export` writes it into the new one.
Import and export never talk to each other, only to the archive, so the old
server can be gone before the new one exists.

Both commands converge. An interrupted run picks up where it stopped, and a
later `import` into the same archive adds what arrived since. Every command
takes `--dry-run` and then only reports the plan. `inspect` reads an archive
and changes nothing.

The archive is also a backup. Import on a schedule, keep the file, and
restore it later with `export`. An archive remembers which account filled it
and refuses a different one unless told otherwise.

Export speaks JMAP only. It writes into inbuxa, or into any JMAP server that
advertises the types being written. The full command reference is in
[docs/usage.md](docs/usage.md).

## Sources

- **JMAP** -- any JMAP account, including an inbuxa or Stalwart server.
- **IMAP** -- mail only. Folders are chosen by name or by pattern.
- **CalDAV, CardDAV** -- calendars and events, address books and contacts.
- **WebDAV** -- a plain file collection, kept as a file tree.
- **ManageSieve** -- Sieve scripts only, with the active one recorded.
- **Maildir** -- a local Maildir++ tree. No network.
- **Google Takeout** -- the `.mbox`, `.ics` and `.vcf` files in a Takeout
  export, or in any directory tree laid out that way.
- **Exchange Server** -- an on-premises mailbox, through EWS.
- **Exchange Online** -- through Microsoft Graph. EWS is being retired in
  Exchange Online: blocked from October 1, 2026 unless an administrator allows
  the client, and switched off on April 1, 2027. Public folders are only
  reachable through EWS.

## Credentials

Secrets are read from the environment, or asked for at a prompt:

    INBUXA_MIGRATE_PASSWORD               a password for --auth-basic
    INBUXA_MIGRATE_TOKEN                  a bearer token for --auth-bearer
    INBUXA_MIGRATE_EWS_CLIENT_SECRET      the OAuth client secret for app-only EWS
    INBUXA_MIGRATE_GRAPH_TOKEN            a Microsoft Graph access token

The command line takes them too, but a secret there ends up in the shell's
history and in the process list. Use the environment or the prompt.

## Installing

Linux, on amd64 or arm64. Each release carries one archive per architecture
and a `SHA256SUMS` file:

    base=https://git.coffeylabs.org/inbuxa/inbuxa-migrate/releases/latest/download
    curl -fLO "$base/inbuxa-migrate-linux-amd64.tar.gz"
    curl -fLO "$base/SHA256SUMS"
    sha256sum --check --ignore-missing SHA256SUMS
    tar -xzf inbuxa-migrate-linux-amd64.tar.gz
    ./inbuxa-migrate --version

On arm64, fetch `inbuxa-migrate-linux-arm64.tar.gz` instead. The binary is
the whole program; put it anywhere on the path.

## Building and testing

    cargo build --release          the binary is target/release/inbuxa-migrate
    cargo test                     the default suite

The default suite needs no network and no Docker. Unit tests and scripted
mocks stand in for JMAP, DAV, EWS and Graph servers.

The live tests are marked `#[ignore]`. Each boots a throwaway container --
Stalwart, Dovecot, Cyrus, Radicale, Baikal or Apache `mod_dav` -- so they
need a working Docker daemon. Run them one binary at a time, and always with
one test thread:

    cargo test --test sync_jmap -- --ignored --test-threads=1
    cargo test --test integration_dovecot -- --ignored --test-threads=1

The tests in a binary share one container and one disposable domain, so a
second thread would trip over the first. [docs/usage.md](docs/usage.md) lists
every live test binary.

## License

Apache-2.0 OR MIT, at your option. The texts are in [LICENSES](LICENSES).

Copyright (C) 2020, Stalwart Labs LLC<br>
Copyright (C) 2026 Coffey Labs LLC

Forked from Vandelay, originally developed by Stalwart Labs, and distributed
under the same Apache-2.0 OR MIT terms.
