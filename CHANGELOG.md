# Change Log

All notable changes to this project are recorded here. Versions are dates:
release `v2026.9.30` is version `2026.9.30`.

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
