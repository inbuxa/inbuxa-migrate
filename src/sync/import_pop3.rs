/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Read a POP3 mailbox into an archive.
//!
//! POP3 is the simplest source: one flat maildrop, no folders, no flags. Every
//! message lands in a single INBOX. Messages are keyed by their UIDL -- the
//! stable unique-id POP3 gives each -- so a later run adds only what arrived
//! since, exactly like the other sources.
//!
//! It is **additive**: a message that is in the archive but no longer on the
//! server is kept, not deleted. POP3 mailboxes are routinely emptied after
//! download (that is the protocol's own model), so a message vanishing from
//! the server is expected and says nothing about whether the archive should
//! still hold it. This is the one place an importer does not mirror deletions.
//!
//! It never deletes anything on the server: no `DELE`, so a run is safe to
//! repeat and leaves the source mailbox untouched.

use std::collections::HashMap;

use rusqlite::{Connection, params};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::db;
use crate::db::sources::SourceKey;
use crate::error::Error;
use crate::imap::transport::Connector;
use crate::logging::{LEVEL_DEFAULT, LEVEL_PROGRESS, Logger};
use crate::pop3::{MessageListing, Pop3Client, Pop3Mode};
use crate::sync::emailmeta::email_meta_from_blob;
use crate::sync::keys::index_to_json;
use crate::sync::{CommonConfig, RunOutcome, Summary, TypeCounts};

/// New messages are committed in groups of this many, so an interrupted run of
/// a large mailbox keeps what it already fetched.
const COMMIT_EVERY: u64 = 200;
const PROGRESS_TICK: u64 = 500;

pub struct Pop3ImportConfig {
    /// `pop3://host[:port]`, `pop3s://host[:port]`, or a bare `host[:port]`
    /// (treated as POP3S).
    pub url: String,
    pub user: String,
    pub password: String,
    /// Permit a plain `pop3://` connection with no STLS, sending the password
    /// in the clear. Ignored for `pop3s://`.
    pub allow_cleartext: bool,
    pub allow_source_change: bool,
}

pub fn run(common: CommonConfig, config: Pop3ImportConfig) -> Result<Summary, Error> {
    run_reporting(common, config).into_result()
}

pub fn run_reporting(common: CommonConfig, config: Pop3ImportConfig) -> RunOutcome {
    let mut summary = Summary::default();
    let error = run_into(common, config, &mut summary).err();
    RunOutcome { summary, error }
}

struct Endpoint {
    host: String,
    port: u16,
    implicit_tls: bool,
}

fn parse_endpoint(url: &str) -> Result<Endpoint, Error> {
    let (implicit_tls, rest) = if let Some(rest) = url.strip_prefix("pop3s://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("pop3://") {
        (false, rest)
    } else if url.contains("://") {
        return Err(Error::Usage(format!(
            "unsupported scheme in {url:?}: use pop3:// or pop3s://"
        )));
    } else {
        // No scheme: POP3S is the safe reading.
        (true, url)
    };
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        return Err(Error::Usage(format!("no host in {url:?}")));
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) => {
            let port = p
                .parse::<u16>()
                .map_err(|_| Error::Usage(format!("invalid port in {url:?}")))?;
            (h, port)
        }
        None => (rest, if implicit_tls { 995 } else { 110 }),
    };
    if host.is_empty() {
        return Err(Error::Usage(format!("no host in {url:?}")));
    }
    Ok(Endpoint {
        host: host.to_owned(),
        port,
        implicit_tls,
    })
}

fn run_into(
    common: CommonConfig,
    config: Pop3ImportConfig,
    summary: &mut Summary,
) -> Result<(), Error> {
    let logger = common.logger;
    if common.threads > 1 {
        log_at(
            logger,
            LEVEL_PROGRESS,
            "pop3 importer is single-threaded; --threads value will be ignored",
        );
    }

    let endpoint = parse_endpoint(&config.url)?;
    let scheme = if endpoint.implicit_tls {
        "pop3s"
    } else {
        "pop3"
    };
    let session_url = format!("{scheme}://{}:{}", endpoint.host, endpoint.port);
    let mode = if endpoint.implicit_tls {
        Pop3Mode::ImplicitTls
    } else if config.allow_cleartext {
        Pop3Mode::Plain
    } else {
        Pop3Mode::StartTls
    };

    let mut conn = db::init::open(&common.archive)?;

    let source_key = SourceKey {
        kind: "pop3".to_owned(),
        session_url: session_url.clone(),
        account_id: config.user.clone(),
    };
    if let Some((existing_url, existing_account)) =
        db::sources::conflicting_source(&conn, "pop3", &session_url, &config.user)?
        && !config.allow_source_change
    {
        return Err(Error::SourceChange(format!(
            "archive already records pop3 source {existing_url} / {existing_account}; \
             re-run with --allow-source-change or use a fresh archive"
        )));
    }

    let connector =
        Connector::new(common.allow_invalid_certs).map_err(|e| Error::Connection(e.to_string()))?;
    let mut client = Pop3Client::connect(&connector, &endpoint.host, endpoint.port, mode)
        .map_err(|e| Error::Connection(format!("pop3 connect {session_url}: {e}")))?;
    client
        .login(&config.user, &config.password)
        .map_err(|e| Error::Connection(format!("pop3 sign-in as {}: {e}", config.user)))?;
    log_at(
        logger,
        LEVEL_PROGRESS,
        &format!("signed in as {} on {session_url}", config.user),
    );

    let listing = client
        .listing()
        .map_err(|e| Error::Connection(format!("pop3 listing: {e}")))?;

    let source_id = if common.dry_run {
        db::sources::find_source(&conn, &source_key)?.unwrap_or(-1)
    } else {
        db::sources::upsert_source(&conn, &source_key, Some(&config.user), &config.user)?
    };

    let local: HashMap<String, i64> = if source_id < 0 {
        HashMap::new()
    } else {
        db::pop3_ids::email_ids(&conn, source_id)?
    };
    let mailbox_exists =
        source_id >= 0 && db::pop3_ids::local_for_mailbox(&conn, source_id)?.is_some();

    let new: Vec<&MessageListing> = listing
        .iter()
        .filter(|m| !local.contains_key(&m.uid))
        .collect();
    let present = listing.len() - new.len();

    if common.dry_run {
        client.quit();
        *summary = dry_run_summary(mailbox_exists, &new, present, common.estimate);
        return Ok(());
    }

    let mut mailbox_counts = TypeCounts::default();
    let mut email_counts = TypeCounts::default();
    email_counts.skipped += present as u64;

    let mailbox_local = ensure_inbox(&mut conn, source_id, mailbox_exists, &mut mailbox_counts)?;

    let total_new = new.len() as u64;
    let mut inserted: u64 = 0;
    let mut tx = conn.transaction()?;
    for msg in &new {
        let sp = tx.savepoint()?;
        match fetch_and_store(&sp, &mut client, source_id, mailbox_local, msg) {
            Ok(()) => {
                sp.commit()?;
                email_counts.created += 1;
                email_counts.fetched += 1;
                inserted += 1;
                if inserted.is_multiple_of(COMMIT_EVERY) {
                    tx.commit()?;
                    tx = conn.transaction()?;
                }
                if inserted.is_multiple_of(PROGRESS_TICK) && logger.enabled(LEVEL_PROGRESS) {
                    eprintln!("pop3: fetched {inserted}/{total_new}");
                }
            }
            Err(e) => {
                drop(sp);
                logger.warn(&format!("pop3 message {} ({}): {e}", msg.number, msg.uid));
                email_counts.failed += 1;
            }
        }
    }
    tx.commit()?;
    client.quit();

    if email_counts.failed == 0 && mailbox_counts.failed == 0 {
        let tx = conn.unchecked_transaction()?;
        db::blobs::gc_orphan_blobs(&tx)?;
        tx.commit()?;
    }

    log_at(
        logger,
        LEVEL_DEFAULT,
        &format!(
            "pop3: mailbox created={}; email created={} skipped={} failed={}",
            mailbox_counts.created, email_counts.created, email_counts.skipped, email_counts.failed
        ),
    );

    *summary = Summary {
        per_type: vec![("mailbox", mailbox_counts), ("email", email_counts)],
        retries_observed: 0,
        retry_after_sleeps: 0,
    };
    Ok(())
}

/// The INBOX for this source, created and mapped on the first run.
fn ensure_inbox(
    conn: &mut Connection,
    source_id: i64,
    mailbox_exists: bool,
    counts: &mut TypeCounts,
) -> Result<i64, Error> {
    if mailbox_exists && let Some(id) = db::pop3_ids::local_for_mailbox(conn, source_id)? {
        return Ok(id);
    }
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO mailboxes (name, parent_id, role, sort_order, is_subscribed)
         VALUES ('INBOX', NULL, 'inbox', 0, 1)",
        [],
    )?;
    let id = tx.last_insert_rowid();
    db::pop3_ids::insert_mailbox(&tx, source_id, id)?;
    tx.commit()?;
    counts.created += 1;
    counts.fetched += 1;
    Ok(id)
}

fn fetch_and_store(
    tx: &Connection,
    client: &mut Pop3Client,
    source_id: i64,
    mailbox_local: i64,
    msg: &MessageListing,
) -> Result<(), Error> {
    let bytes = client
        .retr(msg.number)
        .map_err(|e| Error::Partial(format!("RETR {}: {e}", msg.number)))?;
    let (index, date_header) = email_meta_from_blob(&bytes);
    let received_at = date_header.unwrap_or_else(now_rfc3339);
    let blob_id = db::blobs::intern_blob(tx, &bytes)?;
    let message_match = index_to_json(&index);
    let mailbox_ids = Value::Array(vec![Value::from(mailbox_local)]).to_string();
    tx.execute(
        "INSERT INTO emails (blob_id, received_at, mailbox_ids, keywords, message_match)
         VALUES (?1, ?2, ?3, '[]', ?4)",
        params![blob_id, received_at, mailbox_ids, message_match],
    )?;
    let local_id = tx.last_insert_rowid();
    db::pop3_ids::insert_email(tx, source_id, &msg.uid, local_id)?;
    Ok(())
}

fn dry_run_summary(
    mailbox_exists: bool,
    new: &[&MessageListing],
    present: usize,
    want_sizes: bool,
) -> Summary {
    let mut mailbox = TypeCounts::default();
    let mut email = TypeCounts::default();
    if !mailbox_exists {
        mailbox.created += 1;
        mailbox.fetched += 1;
    }
    email.created += new.len() as u64;
    email.fetched += new.len() as u64;
    email.skipped += present as u64;
    if want_sizes {
        // POP3 LIST gives every new message's size for free -- no body fetch.
        email.bytes += new.iter().map(|m| m.octets).sum::<u64>();
    }
    Summary {
        per_type: vec![("mailbox", mailbox), ("email", email)],
        retries_observed: 0,
        retry_after_sleeps: 0,
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn log_at(logger: Logger, level: u8, msg: &str) {
    if logger.enabled(level) {
        println!("{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(entries: &[(&str, u64)]) -> Vec<MessageListing> {
        entries
            .iter()
            .enumerate()
            .map(|(i, (uid, octets))| MessageListing {
                number: i as u32 + 1,
                uid: (*uid).to_owned(),
                octets: *octets,
            })
            .collect()
    }

    #[test]
    fn parse_endpoint_defaults_ports_and_tls() {
        let e = parse_endpoint("pop3s://mail.example.org").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.implicit_tls),
            ("mail.example.org", 995, true)
        );
        let e = parse_endpoint("pop3://mail.example.org").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.implicit_tls),
            ("mail.example.org", 110, false)
        );
        // Bare host: POP3S.
        let e = parse_endpoint("mail.example.org").unwrap();
        assert_eq!((e.port, e.implicit_tls), (995, true));
        // Explicit port wins.
        let e = parse_endpoint("pop3://host:1100").unwrap();
        assert_eq!((e.port, e.implicit_tls), (1100, false));
    }

    #[test]
    fn parse_endpoint_rejects_junk() {
        assert!(parse_endpoint("imap://host").is_err());
        assert!(parse_endpoint("pop3://").is_err());
        assert!(parse_endpoint("pop3://host:notaport").is_err());
    }

    #[test]
    fn dry_run_counts_new_and_present_and_the_first_mailbox() {
        let msgs = listing(&[("a", 100), ("b", 200), ("c", 50)]);
        let new: Vec<&MessageListing> = msgs.iter().collect();
        let s = dry_run_summary(false, &new, 0, false);
        let mailbox = &s.per_type[0].1;
        let email = &s.per_type[1].1;
        assert_eq!(mailbox.created, 1, "first run creates INBOX");
        assert_eq!((email.created, email.fetched), (3, 3));
        assert_eq!(email.bytes, 0, "no sizes without --estimate");
    }

    #[test]
    fn estimate_sums_new_message_sizes_and_skips_the_mailbox_when_it_exists() {
        let msgs = listing(&[("a", 100), ("b", 200)]);
        let new: Vec<&MessageListing> = msgs.iter().collect();
        let s = dry_run_summary(true, &new, 4, true);
        let mailbox = &s.per_type[0].1;
        let email = &s.per_type[1].1;
        assert_eq!(mailbox.created, 0, "mailbox already exists on a later run");
        assert_eq!(email.bytes, 300);
        assert_eq!(email.skipped, 4);
    }
}
