/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export-imap`: an archive's mail written into any IMAP server, for a
//! target that speaks neither JMAP nor takes files. Like `export`, it keeps
//! no state: every run reads the target first, appends only what isn't
//! there and brings flags up to date on what is, so an interrupted run is
//! finished by running it again.

use std::collections::{BTreeSet, HashMap};

use encodify::utf7;
use rusqlite::Connection;
use time::OffsetDateTime;
use time::macros::format_description;

use crate::db::{blobs, init};
use crate::error::Error;
use crate::export_files::mail::delivery_secs;
use crate::export_files::paths::Names;
use crate::imap::client::{ConnectMode, ImapClient};
use crate::imap::command;
use crate::imap::error::ImapError;
use crate::imap::name::decode_mailbox_name_with;
use crate::imap::response::{Untagged, Value};
use crate::imap::transport::Connector;
use crate::logging::{LEVEL_PROGRESS, Logger};
use crate::sync::emailmeta::email_index_from_blob;
use crate::sync::import_imap::ImapAuth;
use crate::sync::import_imap::coordinator::{authenticate, discover_namespace, parse_endpoint};
use crate::sync::keys::EmailIndex;
use crate::sync::{CommonConfig, Summary, TypeCounts};

pub struct ExportImapConfig {
    pub url: String,
    pub auth: ImapAuth,
    pub allow_cleartext: bool,
}

/// The header fields matching reads from what the target already holds:
/// the same ones the archive's messages are matched by.
const MATCH_HEADERS: &str = "BODY.PEEK[HEADER.FIELDS (MESSAGE-ID FROM TO SUBJECT DATE)]";

/// Archive roles and the SPECIAL-USE attribute a target marks the same
/// folder with.
const SPECIAL_USE: [(&str, &str); 5] = [
    ("sent", "\\Sent"),
    ("drafts", "\\Drafts"),
    ("trash", "\\Trash"),
    ("junk", "\\Junk"),
    ("archive", "\\Archive"),
];

pub fn run(common: CommonConfig, config: ExportImapConfig) -> Result<Summary, Error> {
    if !common.archive.is_file() {
        return Err(Error::Usage(format!(
            "no archive at {}",
            common.archive.display()
        )));
    }
    let conn = init::open(&common.archive)?;
    let logger = common.logger;
    let dry_run = common.dry_run;

    let endpoint = parse_endpoint(&config.url)?;
    let connector =
        Connector::new(common.allow_invalid_certs).map_err(|e| Error::Connection(e.to_string()))?;
    let mode = if endpoint.implicit_tls {
        ConnectMode::ImplicitTls
    } else if config.allow_cleartext {
        ConnectMode::Plain
    } else {
        ConnectMode::StartTls
    };
    let mut client = ImapClient::connect(&connector, &endpoint.host, endpoint.port, mode, logger)
        .map_err(|e| Error::Connection(e.to_string()))?;
    let user = authenticate(&mut client, &config.auth)?;
    client
        .refresh_capabilities()
        .map_err(|e| Error::Connection(format!("post-auth CAPABILITY: {e}")))?;
    if client.has_capability("ENABLE") && client.has_capability("UTF8=ACCEPT") {
        let _ = client.enable(&["UTF8=ACCEPT"]);
    }
    let utf8 = client.utf8_accept();
    log_at(
        logger,
        &format!("signed in as {user} on {}:{}", endpoint.host, endpoint.port),
    );

    let prefix = discover_namespace(&mut client, logger)?;
    let (delimiter, existing) = list_target(&mut client, utf8)?;
    let mailboxes = read_mailboxes(&conn)?;
    let mut folders = plan_folders(&mailboxes, &existing, &prefix, delimiter);

    let mut mailbox_counts = TypeCounts::default();
    let can_mark_use = client.has_capability("CREATE-SPECIAL-USE");
    let mut order: Vec<&mut Planned> = folders.values_mut().collect();
    order.sort_by(|a, b| a.name.cmp(&b.name));
    for folder in order {
        if folder.exists {
            mailbox_counts.skipped += 1;
            continue;
        }
        if !dry_run {
            let wire = wire_name(&folder.name, utf8);
            let use_attr = folder.special_use.filter(|_| can_mark_use);
            if let Err(e) = client.run_collect(&command::create(&wire, use_attr)) {
                if use_attr.is_some() {
                    // A server can refuse the attribute and still take the name.
                    client
                        .run_collect(&command::create(&wire, None))
                        .map_err(|e| folder_error(&folder.name, "CREATE", e))?;
                } else {
                    return Err(folder_error(&folder.name, "CREATE", e));
                }
            }
            if folder.subscribed {
                let _ = client.run_collect(&command::subscribe(&wire));
            }
        }
        folder.exists = !dry_run;
        mailbox_counts.created += 1;
    }

    let by_folder = read_emails(&conn)?;
    let mut email_counts = TypeCounts::default();
    let mut names: Vec<(i64, String)> = folders
        .iter()
        .map(|(id, f)| (*id, f.name.clone()))
        .collect();
    names.sort_by(|a, b| a.1.cmp(&b.1));
    for (mailbox_id, _) in names {
        let folder = &folders[&mailbox_id];
        let Some(emails) = by_folder.get(&mailbox_id) else {
            continue;
        };
        sync_folder(
            &mut client,
            &conn,
            folder,
            emails,
            utf8,
            dry_run,
            logger,
            &mut email_counts,
        )?;
    }
    let _ = client.logout();
    if dry_run {
        logger.warn("dry run: nothing was written to the server");
    }
    Ok(Summary {
        per_type: vec![("mailbox", mailbox_counts), ("email", email_counts)],
        ..Default::default()
    })
}

struct Mailbox {
    id: i64,
    name: String,
    parent_id: Option<i64>,
    role: Option<String>,
    is_subscribed: bool,
}

struct TargetFolder {
    name: String,
    attributes: Vec<String>,
}

#[derive(Debug)]
struct Planned {
    name: String,
    exists: bool,
    special_use: Option<&'static str>,
    subscribed: bool,
}

struct ArchiveEmail {
    id: i64,
    blob_id: i64,
    received_at: String,
    keywords: Vec<String>,
}

fn log_at(logger: Logger, message: &str) {
    if logger.enabled(LEVEL_PROGRESS) {
        eprintln!("{message}");
    }
}

fn folder_error(name: &str, what: &str, e: ImapError) -> Error {
    Error::Connection(format!("{what} {name:?}: {e}"))
}

fn wire_name(name: &str, utf8: bool) -> String {
    if utf8 {
        name.to_owned()
    } else {
        utf7::IMAP.encode(name)
    }
}

/// The target's hierarchy separator and the folders it already has.
fn list_target(client: &mut ImapClient, utf8: bool) -> Result<(char, Vec<TargetFolder>), Error> {
    let probe = client
        .run_collect(&command::list("", ""))
        .map_err(|e| Error::Connection(format!("LIST: {e}")))?;
    let delimiter = probe
        .untagged
        .iter()
        .find_map(|u| match u {
            Untagged::List { delimiter, .. } => *delimiter,
            _ => None,
        })
        .unwrap_or('/');
    let all = client
        .run_collect(&command::list("", "*"))
        .map_err(|e| Error::Connection(format!("LIST: {e}")))?;
    let folders = all
        .untagged
        .into_iter()
        .filter_map(|u| match u {
            Untagged::List {
                attributes, name, ..
            } => Some(TargetFolder {
                name: canonical_inbox(
                    &decode_mailbox_name_with(&name, utf8).unwrap_or(name.clone()),
                ),
                attributes,
            }),
            _ => None,
        })
        .collect();
    Ok((delimiter, folders))
}

fn canonical_inbox(name: &str) -> String {
    if name.eq_ignore_ascii_case("INBOX") {
        "INBOX".to_owned()
    } else {
        name.to_owned()
    }
}

fn read_mailboxes(conn: &Connection) -> Result<Vec<Mailbox>, Error> {
    let mut stmt = conn.prepare(
        "SELECT id, name, parent_id, role, is_subscribed FROM mailboxes ORDER BY sort_order, id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Mailbox {
            id: row.get(0)?,
            name: row.get(1)?,
            parent_id: row.get(2)?,
            role: row.get(3)?,
            is_subscribed: row.get::<_, i64>(4)? != 0,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Where each archive folder goes on the target: the inbox to INBOX, a
/// sent, drafts, trash, junk or archive folder to the one the target marks
/// for it, everything else under the personal namespace by its path.
fn plan_folders(
    mailboxes: &[Mailbox],
    existing: &[TargetFolder],
    prefix: &str,
    delimiter: char,
) -> HashMap<i64, Planned> {
    let by_id: HashMap<i64, &Mailbox> = mailboxes.iter().map(|m| (m.id, m)).collect();
    let depth = |m: &Mailbox| {
        let mut d = 0;
        let mut cur = m.parent_id;
        while let Some(p) = cur {
            d += 1;
            if d > by_id.len() {
                break;
            }
            cur = by_id.get(&p).and_then(|m| m.parent_id);
        }
        d
    };
    let mut order: Vec<&Mailbox> = mailboxes.iter().collect();
    order.sort_by_key(|m| depth(m));

    let mut names = Names::default();
    names.reserve("INBOX");
    let mut taken_use: BTreeSet<&str> = BTreeSet::new();
    let mut inbox_done = false;
    let mut out: HashMap<i64, Planned> = HashMap::new();
    for mailbox in order {
        let role = mailbox.role.as_deref();
        let special = SPECIAL_USE
            .iter()
            .find(|(r, _)| Some(*r) == role)
            .map(|(_, a)| *a);
        let name = if role == Some("inbox") && !inbox_done {
            inbox_done = true;
            "INBOX".to_owned()
        } else if let Some(found) = special.filter(|a| !taken_use.contains(a)).and_then(|attr| {
            existing
                .iter()
                .find(|f| f.attributes.iter().any(|x| x.eq_ignore_ascii_case(attr)))
        }) {
            taken_use.insert(special.expect("found by it"));
            names.reserve(&found.name);
            found.name.clone()
        } else {
            let segment = segment(&mailbox.name, delimiter);
            let wanted = match mailbox
                .parent_id
                .filter(|p| *p != mailbox.id)
                .and_then(|p| out.get(&p))
            {
                Some(parent) => format!("{}{delimiter}{segment}", parent.name),
                None => format!("{prefix}{segment}"),
            };
            names.unique(&wanted, "")
        };
        let exists = existing.iter().any(|f| f.name == name);
        out.insert(
            mailbox.id,
            Planned {
                name,
                exists,
                special_use: special,
                subscribed: mailbox.is_subscribed,
            },
        );
    }
    out
}

/// One level of a folder's name; the target's separator inside it would
/// split it in two.
fn segment(name: &str, delimiter: char) -> String {
    let out: String = name
        .chars()
        .map(|c| if c == delimiter { '_' } else { c })
        .collect();
    if out.trim().is_empty() {
        "_".to_owned()
    } else {
        out
    }
}

fn read_emails(conn: &Connection) -> Result<HashMap<i64, Vec<ArchiveEmail>>, Error> {
    let mut stmt = conn.prepare(
        "SELECT id, blob_id, received_at, mailbox_ids, keywords FROM emails ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut out: HashMap<i64, Vec<ArchiveEmail>> = HashMap::new();
    for row in rows {
        let (id, blob_id, received_at, mailbox_ids, keywords) = row?;
        let keywords: Vec<String> = serde_json::from_str(&keywords).unwrap_or_default();
        for mailbox in serde_json::from_str::<Vec<i64>>(&mailbox_ids).unwrap_or_default() {
            out.entry(mailbox).or_default().push(ArchiveEmail {
                id,
                blob_id,
                received_at: received_at.clone(),
                keywords: keywords.clone(),
            });
        }
    }
    Ok(out)
}

struct OnTarget {
    uid: u32,
    flags: BTreeSet<String>,
    size: u64,
    index: EmailIndex,
    taken: bool,
}

/// What the target lets a folder's messages keep: from PERMANENTFLAGS, or
/// everything when the server doesn't say.
struct Allowed {
    any_keyword: bool,
    listed: BTreeSet<String>,
}

#[allow(clippy::too_many_arguments)]
fn sync_folder(
    client: &mut ImapClient,
    conn: &Connection,
    folder: &Planned,
    emails: &[ArchiveEmail],
    utf8: bool,
    dry_run: bool,
    logger: Logger,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let wire = wire_name(&folder.name, utf8);
    let (mut present, allowed) = if folder.exists {
        read_folder(client, &wire, dry_run).map_err(|e| folder_error(&folder.name, "SELECT", e))?
    } else {
        (
            Vec::new(),
            Allowed {
                any_keyword: true,
                listed: BTreeSet::new(),
            },
        )
    };
    let mut by_mid: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_fallback: HashMap<[u8; 32], Vec<usize>> = HashMap::new();
    for (i, t) in present.iter().enumerate() {
        if t.index.mids.is_empty() {
            by_fallback.entry(t.index.fb).or_default().push(i);
        }
        for mid in &t.index.mids {
            by_mid.entry(mid.clone()).or_default().push(i);
        }
    }

    let (mut created, mut updated, mut unchanged, mut dropped) = (0u64, 0u64, 0u64, 0u64);
    for email in emails {
        let Some(bytes) = blobs::blob_bytes(conn, email.blob_id)? else {
            logger.warn(&format!(
                "email {}: its message is missing from the archive",
                email.id
            ));
            counts.failed += 1;
            continue;
        };
        let index = email_index_from_blob(&bytes);
        let (flags, left_off) = imap_flags(&email.keywords, &allowed);
        dropped += left_off;
        let candidates: Vec<usize> = if index.mids.is_empty() {
            by_fallback.get(&index.fb).cloned().unwrap_or_default()
        } else {
            index
                .mids
                .iter()
                .flat_map(|m| by_mid.get(m).cloned().unwrap_or_default())
                .collect()
        };
        let size = bytes.len() as u64;
        let found = candidates
            .iter()
            .copied()
            .filter(|i| !present[*i].taken)
            .min_by_key(|i| present[*i].size != size);
        match found {
            Some(i) => {
                present[i].taken = true;
                let wanted: BTreeSet<String> = flags.iter().map(|f| f.to_lowercase()).collect();
                let have: BTreeSet<String> = present[i]
                    .flags
                    .iter()
                    .filter(|f| *f != "\\recent")
                    .cloned()
                    .collect();
                if wanted == have {
                    counts.skipped += 1;
                    unchanged += 1;
                    continue;
                }
                if !dry_run {
                    client
                        .run_collect(&command::uid_store_flags(present[i].uid, &flags))
                        .map_err(|e| folder_error(&folder.name, "STORE", e))?;
                }
                counts.updated += 1;
                updated += 1;
            }
            None => {
                if !dry_run {
                    let date = internal_date(delivery_secs(&email.received_at, &bytes));
                    match client.append(&wire, &flags, Some(&date), &bytes) {
                        Ok(_) => {}
                        Err(ImapError::No(no)) => {
                            logger.warn(&format!(
                                "email {} into {:?}: refused: {}",
                                email.id, folder.name, no.text
                            ));
                            counts.failed += 1;
                            continue;
                        }
                        Err(e) => return Err(folder_error(&folder.name, "APPEND", e)),
                    }
                }
                counts.created += 1;
                created += 1;
            }
        }
    }
    if dropped > 0 {
        logger.warn(&format!(
            "folder {:?}: the server keeps no keywords of its own here; \
             {dropped} were left off",
            folder.name
        ));
    }
    log_at(
        logger,
        &format!(
            "{}: {created} appended, {updated} flags updated, {unchanged} already there",
            folder.name
        ),
    );
    Ok(())
}

/// Selects the folder (read-only in a dry run) and reads what it holds:
/// each message's UID, flags, size and match headers.
fn read_folder(
    client: &mut ImapClient,
    wire: &str,
    dry_run: bool,
) -> Result<(Vec<OnTarget>, Allowed), ImapError> {
    let selected = if dry_run {
        client.run_collect(&command::examine(wire))?
    } else {
        client.run_collect(&command::select(wire))?
    };
    let mut exists = 0;
    let mut allowed = Allowed {
        any_keyword: true,
        listed: BTreeSet::new(),
    };
    for u in &selected.untagged {
        match u {
            Untagged::Exists(n) => exists = *n,
            Untagged::StatusLine(line) if line.code.as_deref() == Some("PERMANENTFLAGS") => {
                let listed: BTreeSet<String> = line
                    .code_args
                    .as_deref()
                    .unwrap_or("")
                    .trim_matches(['(', ')'])
                    .split_whitespace()
                    .map(str::to_lowercase)
                    .collect();
                allowed = Allowed {
                    any_keyword: listed.contains("\\*"),
                    listed,
                };
            }
            _ => {}
        }
    }
    if exists == 0 {
        return Ok((Vec::new(), allowed));
    }
    let fetched = client.run_collect(&command::uid_fetch(
        "1:*",
        &["UID", "FLAGS", "RFC822.SIZE", MATCH_HEADERS],
    ))?;
    let present = fetched
        .untagged
        .iter()
        .filter_map(|u| match u {
            Untagged::Fetch { items, .. } => on_target(items),
            _ => None,
        })
        .collect();
    Ok((present, allowed))
}

fn on_target(items: &[(String, Value)]) -> Option<OnTarget> {
    let mut uid = None;
    let mut flags = BTreeSet::new();
    let mut size = 0;
    let mut headers: Vec<u8> = Vec::new();
    for (name, value) in items {
        match (name.as_str(), value) {
            ("UID", Value::Number(n)) => uid = u32::try_from(*n).ok(),
            ("FLAGS", Value::List(list)) => {
                flags = list
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_lowercase))
                    .collect();
            }
            ("RFC822.SIZE", Value::Number(n)) => size = *n,
            (n, Value::Bytes(b)) if n.starts_with("BODY[HEADER") => headers = b.clone(),
            (n, Value::Str(s)) if n.starts_with("BODY[HEADER") => headers = s.clone().into_bytes(),
            _ => {}
        }
    }
    if !headers.ends_with(b"\r\n\r\n") {
        headers.extend_from_slice(b"\r\n");
    }
    Some(OnTarget {
        uid: uid?,
        flags,
        size,
        index: email_index_from_blob(&headers),
        taken: false,
    })
}

/// The archive's keywords as IMAP flags, and how many the folder can't keep.
/// `$deleted` is left off, so nothing written is expunged by the next
/// client that expunges.
fn imap_flags(keywords: &[String], allowed: &Allowed) -> (Vec<String>, u64) {
    let mut out: Vec<String> = Vec::new();
    let mut dropped = 0;
    for keyword in keywords {
        let flag = match keyword.to_ascii_lowercase().as_str() {
            "$seen" => "\\Seen".to_owned(),
            "$answered" => "\\Answered".to_owned(),
            "$flagged" => "\\Flagged".to_owned(),
            "$draft" => "\\Draft".to_owned(),
            "$deleted" | "$recent" => continue,
            _ if !is_atom(keyword) => {
                dropped += 1;
                continue;
            }
            lower if allowed.any_keyword || allowed.listed.contains(lower) => keyword.clone(),
            _ => {
                dropped += 1;
                continue;
            }
        };
        if !out.iter().any(|f| f.eq_ignore_ascii_case(&flag)) {
            out.push(flag);
        }
    }
    (out, dropped)
}

/// A flag keyword must be an IMAP atom: printable ASCII without the
/// characters IMAP gives a meaning.
fn is_atom(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('\\')
        && s.bytes().all(|b| {
            (0x21..0x7f).contains(&b)
                && !matches!(b, b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']')
        })
}

fn internal_date(secs: i64) -> String {
    let format = format_description!(
        "[day padding:space]-[month repr:short]-[year] [hour]:[minute]:[second] +0000"
    );
    OffsetDateTime::from_unix_timestamp(secs)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .format(&format)
        .unwrap_or_else(|_| " 1-Jan-1970 00:00:00 +0000".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox(id: i64, name: &str, parent_id: Option<i64>, role: Option<&str>) -> Mailbox {
        Mailbox {
            id,
            name: name.to_owned(),
            parent_id,
            role: role.map(str::to_owned),
            is_subscribed: true,
        }
    }

    fn target(name: &str, attributes: &[&str]) -> TargetFolder {
        TargetFolder {
            name: name.to_owned(),
            attributes: attributes.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn folders_land_where_the_target_keeps_them() {
        let plan = plan_folders(
            &[
                mailbox(1, "Inbox", None, Some("inbox")),
                mailbox(2, "Clients", Some(1), None),
                mailbox(3, "Sent Items", None, Some("sent")),
                mailbox(4, "Archive", None, Some("archive")),
                mailbox(5, "2024", Some(4), None),
                mailbox(6, "a/b", None, None),
            ],
            &[
                target("INBOX", &[]),
                target("[Gmail]/Sent Mail", &["\\HasNoChildren", "\\Sent"]),
            ],
            "",
            '/',
        );
        let name = |id| plan[&id].name.as_str();
        assert_eq!(name(1), "INBOX");
        assert!(plan[&1].exists);
        assert_eq!(name(2), "INBOX/Clients");
        assert_eq!(name(3), "[Gmail]/Sent Mail");
        assert!(plan[&3].exists);
        assert_eq!(name(4), "Archive");
        assert!(!plan[&4].exists);
        assert_eq!(plan[&4].special_use, Some("\\Archive"));
        assert_eq!(name(5), "Archive/2024");
        assert_eq!(name(6), "a_b");
    }

    #[test]
    fn a_namespace_prefix_holds_everything_but_the_inbox() {
        let plan = plan_folders(
            &[
                mailbox(1, "Inbox", None, Some("inbox")),
                mailbox(2, "Work", None, None),
                mailbox(3, "Q1", Some(2), None),
            ],
            &[target("INBOX", &[])],
            "INBOX.",
            '.',
        );
        assert_eq!(plan[&1].name, "INBOX");
        assert_eq!(plan[&2].name, "INBOX.Work");
        assert_eq!(plan[&3].name, "INBOX.Work.Q1");
    }

    #[test]
    fn keywords_become_flags_the_folder_can_keep() {
        let kw = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let any = Allowed {
            any_keyword: true,
            listed: BTreeSet::new(),
        };
        let (flags, dropped) = imap_flags(
            &kw(&[
                "$seen",
                "$Flagged",
                "$deleted",
                "$Junk",
                "bad word",
                "$answered",
            ]),
            &any,
        );
        assert_eq!(flags, ["\\Seen", "\\Flagged", "$Junk", "\\Answered"]);
        assert_eq!(dropped, 1);
        let system_only = Allowed {
            any_keyword: false,
            listed: ["\\seen", "$forwarded"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        let (flags, dropped) = imap_flags(&kw(&["$seen", "$junk", "$Forwarded"]), &system_only);
        assert_eq!(flags, ["\\Seen", "$Forwarded"]);
        assert_eq!(dropped, 1);
    }

    #[test]
    fn internal_dates_are_imap_shaped() {
        assert_eq!(internal_date(1_709_294_400), " 1-Mar-2024 12:00:00 +0000");
        assert_eq!(internal_date(1_734_567_890), "19-Dec-2024 00:24:50 +0000");
    }

    #[test]
    fn a_target_message_is_read_from_its_fetch() {
        let items = vec![
            ("UID".to_owned(), Value::Number(12)),
            (
                "FLAGS".to_owned(),
                Value::List(vec![
                    Value::Atom("\\Seen".into()),
                    Value::Atom("$Junk".into()),
                ]),
            ),
            ("RFC822.SIZE".to_owned(), Value::Number(321)),
            (
                "BODY[HEADER.FIELDS (MESSAGE-ID FROM TO SUBJECT DATE)]".to_owned(),
                Value::Bytes(b"Message-ID: <x@y>\r\nSubject: hi\r\n\r\n".to_vec()),
            ),
        ];
        let t = on_target(&items).unwrap();
        assert_eq!(t.uid, 12);
        assert_eq!(t.size, 321);
        assert!(t.flags.contains("\\seen") && t.flags.contains("$junk"));
        assert_eq!(t.index.mids, ["x@y"]);
    }
}
