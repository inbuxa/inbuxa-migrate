# Change Log

All notable changes to this project are recorded here. Versions follow the
inbuxa suite: a series (`27.1`), its patches (`27.1.3`) and release
candidates before a series ships (`27.1-rc.1`). The tag is the version with a
`v`. Releases before 27.1 were dates (`v2026.9.30`).

## [Unreleased]

### Added
- `import-directory` sets up on an inbuxa server what `export-directory`
  wrote: tenants, roles, domains, groups, users and mailing lists, with
  their aliases, quotas, roles and memberships. What the server already has
  is left as it is, so running it again creates nothing. Each user created
  gets a generated password, written to a new file readable by its owner
  only.

### Fixed
- `inspect` opens the archive read-only. It used to create an empty archive
  at a mistyped path and bring an older archive's layout up to date; now
  the first is an error and the second is refused, and reading a closed
  archive leaves no files beside it.
- `import carddav` takes a card that is in two address books into both.
  It used to fail on that card on every run. Taking it out of one book, or
  deleting that book, now leaves it in the other.
- `import caldav` keeps every occurrence of a series someone was invited to
  one occurrence at a time, with no master event. It used to keep the first
  and drop the rest. `export` now tells such occurrences apart instead of
  writing them all onto one event, and `export-caldav` writes them as one
  resource, as CalDAV keeps them.

## [27.1-rc.3]

The third release candidate of the 27.1 series: contacts, calendars and
filters go straight into another server, as mail already could.

### Added
- `export-managesieve` writes an archive's Sieve scripts into any
  ManageSieve server and makes the archive's active script the active one.
  Like `export-imap`, it writes only what's missing or differs, so running
  it again writes nothing. A script the server refuses is counted and the
  rest go on; `--dry-run` checks each one with `CHECKSCRIPT`.
- `export-caldav` and `export-carddav` write an archive's calendars and
  address books into any CalDAV or CardDAV server. The account's home is
  found from the server's root, collections are matched by name or made,
  and each event or card is written only when it's missing or differs, under
  its etag, so running it again writes nothing. Nothing is deleted.

### Changed
- `export-files` names its layout in `manifest.json` by the address of its
  specification, `https://coffeylabs.org/specs/account-export/`, instead of
  `inbuxa-migrate-files`. The layout itself is unchanged (`formatVersion` 1).

## [27.1-rc.2]

The second release candidate of the 27.1 series: three ways out of an
archive or a server that need no vendor's store format.

### Added
- `export-files` writes an archive out as plain files in open formats: mail
  as a Maildir++ tree, each address book as a `.vcf`, each calendar as an
  `.ics`, Sieve scripts, the file tree, and a `manifest.json` with what those
  formats don't hold. It needs no network, writes only into a new or empty
  directory, and keeps the directory private to its owner.
- `export-imap` writes an archive's mail into any IMAP server: folders in the
  server's own layout (SPECIAL-USE folders for sent, drafts, trash, junk and
  archive), flags, keywords where the folder keeps them, and arrival times.
  Like `export`, it matches what is already there, so running it again
  writes only what changed.
- `export-directory` writes who is on a server: its domains, users, groups,
  mailing lists, aliases, quotas, tenants and roles, read from the registry
  with an administrator's login, as `directory.json` and CSV files that
  another server can be set up from. Credentials, OTP seeds, API keys and
  DKIM keys are never asked for. `--domain` keeps it to some domains.

### Changed
- `import maildir` reads folder names in modified UTF-7, as Dovecot and
  Courier write them, and the custom keywords in each folder's
  `dovecot-keywords` file (Junk, labels).
- A FETCH section with a header list, such as
  `BODY[HEADER.FIELDS (MESSAGE-ID)]`, is read as one item.
- `import takeout` unfolds and unescapes a calendar's `X-WR-CALNAME` and
  `X-WR-CALDESC`, so a long name or one with a comma reads back whole.

## [27.1-rc.1]

The first release candidate of the 27.1 series, with the rest of the suite.

### Changed
- `--version` and the User-Agent show the version as tags name it
  (`27.1-rc.1`).

## [2026.10.26] -- 2026-10-26

### Changed
- `--auth-bearer` reads `INBUXA_MIGRATE_API_KEY`, the name inbuxa-cli and the
  console use. `INBUXA_MIGRATE_TOKEN` still works; when both are set, the API
  key wins.

## [2026.9.30] -- 2026-09-30

The first release of inbuxa-migrate.

### Changed
- Renamed to inbuxa-migrate: the binary, the crate, and the credential
  variables, now `INBUXA_MIGRATE_PASSWORD`, `INBUXA_MIGRATE_TOKEN`,
  `INBUXA_MIGRATE_EWS_CLIENT_SECRET` and `INBUXA_MIGRATE_GRAPH_TOKEN`. The old
  names are not read. Archives written by Vandelay are not read either.
- Registry calls (`x:Account`, `x:Domain` and the other `x:` types) use the
  capability the server advertises: `urn:inbuxa:jmap:registry` on inbuxa,
  `urn:stalwart:jmap` on a Stalwart source. A server that advertises neither
  gets a clear error instead of a request it never offered to accept.
- Released as Linux archives for amd64 and arm64 on the Gitea release page,
  with `SHA256SUMS`. The npm, Homebrew, shell and PowerShell installers and
  the MSI are gone.
- A second export brings matched items up to date instead of skipping them:
  flags and folders on mail, changed contacts and events, and edited Sieve
  scripts.
- A message filed in several folders is written once, in every one of them,
  and a resumed export adds any folder it was still missing.
- Sieve scripts from a Stalwart server keep working on inbuxa: their
  `vnd.stalwart.*` extension names are renamed to inbuxa's, each rename is
  logged, and a script that can't be activated fails the export instead of
  leaving filtering quietly off.
- `--allow-invalid-certs` covers only the server named with `--url`. Microsoft
  and Google sign-in are always verified.
- Export imports mail in batches, uploads in parallel within the target's
  limits, and prints progress: count, rate and time left.
- `export --dry-run` predicts what would fail -- messages over the target's
  upload limit, objects over its request limit, Sieve scripts it would reject
  -- lists what would be created, updated and skipped, and exits as the real
  run would.

### Fixed
- Every connection times out instead of waiting forever on a dropped link,
  and a timeout is retried like any other transient error.
- An interrupted IMAP or Maildir import keeps what it wrote, and a message
  that can't be read is recorded and skipped rather than ending the folder.
- IMAP and EWS imports hold a bounded amount in memory, in batches capped by
  size as well as count.
- The archive is created readable by its owner only, and an existing archive
  others can read is reported.
- A dry run no longer writes to the target's address books.

---

# Earlier releases (upstream Vandelay)

## [1.0.11] - 2026-09-28

### Added

### Changed
- Replaced `base64` with `encodify`.

### Fixed
- IMAP: import failed after authenticating to Dovecot when a capability contained `:`, such as `IMAPSIEVE=sieve://...` (fixes #43)

## [1.0.10] - 2026-08-27

### Added
- MS Exchange Graph: 
    - OneDrive files import.
    - Graph contact photos, categories and IM addresses.
    - Graph event file attachments.

### Changed

### Fixed
- MS Exchange Graph: 
    - Import the default Contacts folder, recover series exceptions (fixes #39)
    - Graph deleted recurrence occurrences were not excluded.
    - `bySetPosition` was emitted on every Graph recurrence rule.
    - Graph read, flagged and category state was dropped.
    - A contact folder reachable by two paths aborted the run.
    - IM addresses were dropped on export for lacking a `uri`.
    - A redirect warning logged a OneDrive download credential.

## [1.0.9] - 2026-08-22

### Added

### Changed

### Fixed
- IMAP mailbox names were decoded as Latin-1 instead of UTF-8, so non-ASCII folders failed to SELECT (#32 #37).
- CalDAV discovery aborted when the given URL answered PROPFIND with 501 instead of trying the next discovery step (#35).
- An unparsable `receivedAt` dropped the whole email, and a `FileNode` without `nodeType` was skipped (#36).
- Maildir import took `received_at` from the file mtime, which does not survive a copy or restore (#38).
- An EWS recurrence `EndDate` carrying a UTC offset produced a malformed `until` and an unbounded series (#33).
- Export took the default calendar or address book away from a target account that already had one, and an archive could hold more than one default per type.
- Exchange participant addresses were replaced by a synthetic URN whenever `RoutingType` was not SMTP, and a Graph X500 reference was emitted as a malformed `mailto:` (#33).

## [1.0.8] - 2026-08-15

### Added
- A warning when the session advertises `apiUrl`, `uploadUrl` or `downloadUrl` on a different origin than the one connected to, naming both origins (#29).
- The per-type report is printed when a run aborts.

### Changed

### Fixed
- A transient `serverUnavailable` from `Email/import` permanently dropped that message from the migration instead of retrying it.
- The Exchange Graph importer stored a whole converted event as a `recurrenceOverrides` value (#31).
- Exporting a ContactCard carrying a photo blob failed against Stalwart, and a CalendarEvent enclosure sent as a `blobId` was silently dropped by the server; the bytes of both are now inlined as a base64 `data:` URI (#30).
- A connection failure against the session-advertised `apiUrl` was reported as a per-type partial failure (exit 5, "consistent and resumable") instead of aborting (#29).
- Session `apiUrl`, `uploadUrl` and `downloadUrl` given as relative references were used unresolved (#29).

## [1.0.7] - 2026-07-26

### Added

### Changed

### Fixed
- Improve verbosity (#4 #19).
- WebDAV import materialised the account root collection as a directory named after the account displayname (#18).
- Report user friendly error message when `urn:ietf:params:jmap:principals` is not supported and no accountId is provided (#21).
- Report which email failed to import when the blob is too large (#22).
- IMAP import failed with "LIST mailbox name missing" when a mailbox name is a purely numeric unquoted atom (#26).

## [1.0.6] - 2026-07-12

### Added

### Changed

### Fixed
- Self heal on `blobNotFound` errors when exporting data (#13).
- Mapping existing special mailbox fails after `alreadyExists` response (#17).

## [1.0.5] - 2026-06-27

### Added

### Changed

### Fixed
- Strict `RFC822.SIZE` == `BODY[]` length check discards good mail.

## [1.0.4] - 2026-06-21

### Added

### Changed

### Fixed
- Include correct JMAP capabilities in `using`.
- Failures are double-counted.

## [1.0.3] - 2026-06-15

### Added

### Changed

### Fixed
- Mailbox roles must be unique per archive (#8).
- Google takeout: Decode MIME-encoded values in `X-Gmail-Labels` (#7).

## [1.0.2] - 2026-06-11

### Added

### Changed

### Fixed
- IMAP: Import fails with `BAD` on servers that advertise `LIST-EXTENDED` without `SPECIAL-USE`.
- MS Exchange EWS: add support for version negotiation and other fixes (#6).

## [1.0.1] - 2026-06-04

### Added

### Changed

### Fixed
- MS Exchange Graph: duplicate ids and incorrect JSCalendar mapping issues.

## [1.0.0] - 2026-05-29

### Added
- Initial release.

### Changed

### Fixed
