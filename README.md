# inbuxa-migrate

<p align="center">
  <a href="LICENSES"><img alt="License: Apache-2.0 OR MIT" src="https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-2dd4bf?style=flat-square"></a>
  <a href="https://git.coffeylabs.org/inbuxa/inbuxa-migrate/releases/latest"><img alt="Latest release" src="https://img.shields.io/gitea/v/release/inbuxa/inbuxa-migrate?gitea_url=https%3A%2F%2Fgit.coffeylabs.org&label=release&color=2dd4bf&style=flat-square"></a>
  <a href="https://community.coffeylabs.org/c/inbuxa/5"><img alt="Forum: community.coffeylabs.org" src="https://img.shields.io/badge/forum-community.coffeylabs.org-0f766e?style=flat-square"></a>
  <a href="https://discord.gg/nqcY4TKfAn"><img alt="Chat on Discord" src="https://img.shields.io/discord/1523538164084637797?label=discord&logo=discord&logoColor=white&color=5865f2&style=flat-square"></a>
</p>

> [!NOTE]
> Development happens on [git.coffeylabs.org/inbuxa/inbuxa-migrate](https://git.coffeylabs.org/inbuxa/inbuxa-migrate); the copy on GitHub is a read-only mirror.
> Report issues at **[git.coffeylabs.org/inbuxa/inbuxa-migrate/issues](https://git.coffeylabs.org/inbuxa/inbuxa-migrate/issues)**, join discussions at **[community.coffeylabs.org](https://community.coffeylabs.org)**, or chat on **[Discord](https://discord.gg/nqcY4TKfAn)**.

One program that moves an account into **inbuxa**: its mail, calendars,
contacts, identities, Sieve scripts and files.

    inbuxa-migrate import imap … alice.sqlite    an account into an archive
    inbuxa-migrate inspect alice.sqlite          what landed
    inbuxa-migrate reconcile alice.sqlite        folder by folder, is it all there
    inbuxa-migrate export … alice.sqlite         the archive into inbuxa
    inbuxa-migrate export-files alice.sqlite out the archive as plain files
    inbuxa-migrate import files out alice.sqlite those files read back, manifest and all
    inbuxa-migrate export-imap … alice.sqlite    its mail into any IMAP server
    inbuxa-migrate export-managesieve … alice.sqlite  its filters, likewise
    inbuxa-migrate export-caldav … alice.sqlite  its calendars into any CalDAV server
    inbuxa-migrate export-carddav … alice.sqlite  its contacts into any CardDAV server
    inbuxa-migrate export-directory … out        a server's accounts, no secrets
    inbuxa-migrate import-directory … out        those accounts, set up on inbuxa
    inbuxa-migrate migrate --plan plan.json work  many accounts, by a plan
    inbuxa-migrate guide work                    the same, walked through in the browser

Between the two sides sits an archive: one SQLite file holding one account.
`import` fills it from the old server; `export` writes it into the new one.
Import and export never talk to each other, only to the archive, so the old
server can be gone before the new one exists.

Both commands converge. An interrupted run picks up where it stopped, and a
later `import` into the same archive adds what arrived since. Every command
takes `--dry-run` and then only reports the plan. An `import` also takes
`--estimate`, which connects to the source and reports how much is new to
fetch — counts, bytes and a rough time — before committing, and writes
nothing. It reports sizes for `imap`, `maildir`, `caldav`, `carddav`, Exchange
(`ews`) and a `takeout` (whose files it reads to count the mail, events and
contacts that are new). For `jmap` and Microsoft Graph it reports counts only:
the enumeration gives the new-object count for free, but a size there would
cost a body fetch per object. The remaining sources fall back to a plain
dry-run. `inspect` reads an archive and changes nothing, and `reconcile`
reads it folder by folder: a count and a content checksum for each, a checksum
for the whole, and a check that every message resolves to stored content and a
real folder. Compare the per-folder counts against the old mailbox to prove the
move was complete, and the checksum shows two archives hold the very same mail.
It exits non-zero if a message is missing its content or its folder.

The archive is also a backup. Import on a schedule, keep the file, and
restore it later with `export`. An archive remembers which account filled it
and refuses a different one unless told otherwise.

Export speaks JMAP only. It writes into inbuxa, or into any JMAP server that
advertises the types being written. `export-imap` writes the mail into any
IMAP server, `export-managesieve` the Sieve filters into any ManageSieve
server, `export-caldav` and `export-carddav` the calendars and contacts into
any CalDAV and CardDAV server, and `export-files` is the way out to everything else: it writes
the archive as a Maildir++ tree, vCard, iCalendar and Sieve files that other
servers and mail, calendar and contacts apps open, with no network and no
store format in the way, and `import files` reads such a tree back whole.
For a whole server, `export-directory` writes the
list of domains, accounts, groups, lists and aliases to set up again
elsewhere, without passwords, and `import-directory` sets them up on an
inbuxa server, with a new password for each user. `migrate` moves many
accounts with one plan file: the same steps for each account, several at
once, resuming where it stopped, with one report at the end. With
`--repeat` it keeps syncing, each pass moving only what arrived since, so
the new server stays close behind the old one until the cutover. `guide`
walks through the same move in the browser: it asks where the mail is,
which accounts and which inbuxa server, previews, and runs the plan it
writes. The full
command reference is in [docs/usage.md](docs/usage.md).

## Sources

- **JMAP** -- any JMAP account, including an inbuxa or Stalwart server.
- **Fastmail** -- JMAP, with its server and token filled in: just an API
  token. Mail always; calendars and contacts where the account advertises
  them.
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
    INBUXA_MIGRATE_API_KEY                an API key or bearer token for --auth-bearer
                                          (INBUXA_MIGRATE_TOKEN still works)
    INBUXA_MIGRATE_EWS_CLIENT_SECRET      the OAuth client secret for app-only EWS
    INBUXA_MIGRATE_GRAPH_TOKEN            a Microsoft Graph access token
    INBUXA_MIGRATE_GOOGLE_KEY             a Google service account's JSON key,
                                          for --google-key-from-env

Any of these can instead be read from a file, by setting the same name with
`_FILE` appended to the file's path -- the Docker, Compose and systemd secrets
convention:

    INBUXA_MIGRATE_PASSWORD_FILE=/run/secrets/imap-password
    INBUXA_MIGRATE_API_KEY_FILE=/run/secrets/token

The file holds the secret and nothing else; one trailing newline is ignored.
Setting both a variable and its `_FILE` form is an error. (A Google key is
already a file: `--google-key <path>`.)

The command line takes them too, but a secret there ends up in the shell's
history and in the process list. Use a file, the environment, or the prompt.

### A shared profile for the inbuxa server

`import` (into a JMAP server) and `import-directory` write to an inbuxa
server. Instead of giving its address and key each time, name it once in
`~/.config/inbuxa/config.toml`, which `inbuxa-cli` reads too:

    [profile.default]
    url = "https://mail.example.org"
    api-key = "API_…"

    [profile.staging]
    url = "https://staging.example.org"
    api-key-file = "~/.secrets/staging.key"

A section gives `url` and either `api-key` or `api-key-file` (a path to a file
holding the key, so the key stays out of the config). Keep the file to
yourself -- `chmod 600 ~/.config/inbuxa/config.toml`. Without `--profile` or
`$INBUXA_PROFILE`, the `default` section is used; `--profile staging` picks
another. A `--url`, or any `INBUXA_MIGRATE_*` variable, still wins, so the
profile is only the fallback and `--url` becomes optional on these two
commands when it is set. The other commands connect to IMAP, DAV and the like,
not to the inbuxa server, so they don't read the profile.

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

`SHA256SUMS` is signed with [minisign](https://jedisct1.github.io/minisign/),
so you can confirm it came from us before trusting the checksums.
`SHA256SUMS.minisig` is the signature:

    curl -fLO "$base/SHA256SUMS.minisig"
    minisign -Vm SHA256SUMS -P RWS1yFYwVuJ7MGJTZXLbhv9+hrEsjrC8Zg+7WmPlM86Sjy0qF/xkoCKq

The same public key signs every inbuxa release (installer, migrate and cli).

## Shell completions and a man page

`inbuxa-migrate completions <shell>` prints a completion script to standard
output, for `bash`, `zsh`, `fish`, `elvish` or `powershell`. Install it where
the shell looks:

    inbuxa-migrate completions bash > /etc/bash_completion.d/inbuxa-migrate
    inbuxa-migrate completions zsh  > "${fpath[1]}/_inbuxa-migrate"
    inbuxa-migrate completions fish > ~/.config/fish/completions/inbuxa-migrate.fish

`inbuxa-migrate man` prints a man page, in roff, to standard output:

    inbuxa-migrate man > /usr/share/man/man1/inbuxa-migrate.1

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
