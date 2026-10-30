/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Mail as a Maildir++ tree, the layout Dovecot and Courier serve and
//! `import maildir` reads: the inbox at the root, every other folder a
//! `.Parent.Child` directory beside it, names in modified UTF-7, flags in
//! the file name and other keywords in each folder's `dovecot-keywords`.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use encodify::utf7;
use rusqlite::Connection;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::{Rfc2822, Rfc3339};

use crate::db::blobs;
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::TypeCounts;

use super::paths::Names;

/// Keywords with a Maildir flag letter of their own.
const FLAG_LETTERS: [(&str, char); 6] = [
    ("$draft", 'D'),
    ("$flagged", 'F'),
    ("$forwarded", 'P'),
    ("$answered", 'R'),
    ("$seen", 'S'),
    ("$deleted", 'T'),
];

/// Dovecot gives each folder 26 keyword letters, `a` to `z`.
const MAX_CUSTOM_KEYWORDS: usize = 26;

struct Mailbox {
    id: i64,
    name: String,
    parent_id: Option<i64>,
    role: Option<String>,
    is_subscribed: bool,
}

struct Folder {
    /// The Maildir++ name, `Parent.Child`; empty for the inbox.
    maildir_name: String,
    /// The folder's real name and those of its parents.
    display_path: Vec<String>,
    dir: PathBuf,
    role: Option<String>,
    is_subscribed: bool,
    messages: u64,
    custom_keywords: Vec<String>,
    keywords_dropped: u64,
}

pub struct MailOutcome {
    pub mailbox: TypeCounts,
    pub email: TypeCounts,
    pub manifest: Value,
}

pub fn write(
    conn: &Connection,
    root: &Path,
    dry_run: bool,
    logger: Logger,
) -> Result<MailOutcome, Error> {
    let mailboxes = read_mailboxes(conn)?;
    let mut folders = plan_folders(&mailboxes, root);
    let mut mailbox_counts = TypeCounts::default();
    if !dry_run {
        for folder in folders.values() {
            make_maildir(&folder.dir, !folder.maildir_name.is_empty())?;
        }
    }
    mailbox_counts.created = mailboxes.len() as u64;

    let root_folder = folders
        .iter()
        .find(|(_, f)| f.maildir_name.is_empty())
        .map(|(id, _)| *id)
        .expect("plan_folders always makes a root");
    let mut email_counts = TypeCounts::default();
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
    for row in rows {
        let (id, blob_id, received_at, mailbox_ids, keywords) = row?;
        let Some(bytes) = blobs::blob_bytes(conn, blob_id)? else {
            logger.warn(&format!(
                "email {id}: its message is missing from the archive"
            ));
            email_counts.failed += 1;
            continue;
        };
        let mut targets: Vec<i64> = serde_json::from_str::<Vec<i64>>(&mailbox_ids)
            .unwrap_or_default()
            .into_iter()
            .filter(|m| folders.contains_key(m))
            .collect();
        if targets.is_empty() {
            logger.warn(&format!(
                "email {id}: in no folder that was written; put in the inbox"
            ));
            targets.push(root_folder);
        }
        let keywords: Vec<String> = serde_json::from_str(&keywords).unwrap_or_default();
        let secs = delivery_secs(&received_at, &bytes);
        for mailbox_id in targets {
            let folder = folders.get_mut(&mailbox_id).expect("filtered above");
            let flags = flags_for(&keywords, folder, logger);
            let name = format!("{secs}.M{id}.inbuxa-migrate,S={}:2,{flags}", bytes.len());
            if !dry_run {
                write_message(&folder.dir.join("cur").join(name), &bytes, secs)?;
            }
            folder.messages += 1;
        }
        email_counts.created += 1;
    }

    let mut entries: Vec<&Folder> = folders.values().collect();
    entries.sort_by(|a, b| a.maildir_name.cmp(&b.maildir_name));
    if !dry_run {
        for folder in &entries {
            write_keywords_file(folder)?;
        }
    }
    let manifest = json!({
        "layout": "maildir++",
        "path": "mail",
        "folders": entries.iter().map(|f| folder_manifest(f, root)).collect::<Vec<_>>(),
    });
    Ok(MailOutcome {
        mailbox: mailbox_counts,
        email: email_counts,
        manifest,
    })
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

/// Every mailbox's directory, parents before children so a renamed parent
/// takes its children with it.
fn plan_folders(mailboxes: &[Mailbox], root: &Path) -> HashMap<i64, Folder> {
    let by_id: HashMap<i64, &Mailbox> = mailboxes.iter().map(|m| (m.id, m)).collect();
    let inbox = mailboxes
        .iter()
        .find(|m| m.role.as_deref() == Some("inbox"))
        .map(|m| m.id);
    let mut names = Names::default();
    names.reserve("INBOX");
    let mut folders: HashMap<i64, Folder> = HashMap::new();
    let mut order: Vec<&Mailbox> = mailboxes.iter().collect();
    order.sort_by_key(|m| depth(m, &by_id));
    for mailbox in order {
        let parent = mailbox
            .parent_id
            .filter(|p| by_id.contains_key(p) && *p != mailbox.id);
        let (maildir_name, display_path) = if Some(mailbox.id) == inbox {
            (String::new(), vec![mailbox.name.clone()])
        } else {
            let (prefix, mut display) = match parent.and_then(|p| folders.get(&p)) {
                Some(p) if p.maildir_name.is_empty() => {
                    ("INBOX.".to_owned(), p.display_path.clone())
                }
                Some(p) => (format!("{}.", p.maildir_name), p.display_path.clone()),
                None => (String::new(), Vec::new()),
            };
            display.push(mailbox.name.clone());
            let wanted = format!("{prefix}{}", encode_segment(&mailbox.name));
            (names.unique(&wanted, ""), display)
        };
        let dir = if maildir_name.is_empty() {
            root.to_path_buf()
        } else {
            root.join(format!(".{maildir_name}"))
        };
        folders.insert(
            mailbox.id,
            Folder {
                maildir_name,
                display_path,
                dir,
                role: mailbox.role.clone(),
                is_subscribed: mailbox.is_subscribed,
                messages: 0,
                custom_keywords: Vec::new(),
                keywords_dropped: 0,
            },
        );
    }
    if !folders.values().any(|f| f.maildir_name.is_empty()) {
        // Maildir++ needs its root to be a maildir, even with no inbox.
        folders.insert(
            i64::MIN,
            Folder {
                maildir_name: String::new(),
                display_path: vec!["INBOX".to_owned()],
                dir: root.to_path_buf(),
                role: Some("inbox".to_owned()),
                is_subscribed: true,
                messages: 0,
                custom_keywords: Vec::new(),
                keywords_dropped: 0,
            },
        );
    }
    folders
}

fn depth(mailbox: &Mailbox, by_id: &HashMap<i64, &Mailbox>) -> usize {
    let mut depth = 0;
    let mut current = mailbox.parent_id;
    while let Some(id) = current {
        depth += 1;
        if depth > by_id.len() {
            break;
        }
        current = by_id.get(&id).and_then(|m| m.parent_id);
    }
    depth
}

/// One level of a Maildir++ name: modified UTF-7, with the hierarchy
/// separator and anything a directory name can't hold turned into `_`. The
/// manifest keeps the real name.
fn encode_segment(name: &str) -> String {
    let encoded: String = utf7::IMAP
        .encode(name)
        .chars()
        .map(|c| match c {
            '.' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect();
    let trimmed = encoded.trim_end_matches(' ');
    if trimmed.is_empty() {
        "_".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn flags_for(keywords: &[String], folder: &mut Folder, logger: Logger) -> String {
    let mut letters: Vec<char> = Vec::new();
    for keyword in keywords {
        let lower = keyword.to_ascii_lowercase();
        if let Some((_, letter)) = FLAG_LETTERS.iter().find(|(k, _)| *k == lower) {
            letters.push(*letter);
            continue;
        }
        let index = match folder
            .custom_keywords
            .iter()
            .position(|k| k.eq_ignore_ascii_case(keyword))
        {
            Some(i) => i,
            None if folder.custom_keywords.len() < MAX_CUSTOM_KEYWORDS => {
                folder.custom_keywords.push(keyword.clone());
                folder.custom_keywords.len() - 1
            }
            None => {
                if folder.keywords_dropped == 0 {
                    logger.warn(&format!(
                        "folder {:?}: more than {MAX_CUSTOM_KEYWORDS} keywords; \
                         the rest are left off its messages",
                        folder.display_path.join("/")
                    ));
                }
                folder.keywords_dropped += 1;
                continue;
            }
        };
        letters.push((b'a' + index as u8) as char);
    }
    letters.sort_unstable();
    letters.dedup();
    letters.into_iter().collect()
}

/// When the message arrived, as Unix seconds: the archive's received time,
/// else its Date header, else now.
pub(crate) fn delivery_secs(received_at: &str, message: &[u8]) -> i64 {
    let parsed = OffsetDateTime::parse(received_at, &Rfc3339)
        .or_else(|_| OffsetDateTime::parse(received_at, &Rfc2822))
        .map(|t| t.unix_timestamp())
        .ok()
        .or_else(|| {
            mail_parser::MessageParser::default()
                .parse_headers(message)
                .and_then(|m| m.date().map(|d| d.to_timestamp()))
        });
    parsed
        .filter(|s| *s > 0)
        .unwrap_or_else(|| OffsetDateTime::now_utc().unix_timestamp())
}

fn make_maildir(dir: &Path, is_subfolder: bool) -> Result<(), Error> {
    for sub in ["cur", "new", "tmp"] {
        fs::create_dir_all(dir.join(sub))?;
    }
    if is_subfolder {
        fs::File::create(dir.join("maildirfolder"))?;
    }
    Ok(())
}

fn write_message(path: &Path, bytes: &[u8], secs: i64) -> Result<(), Error> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    if secs > 0 {
        file.set_modified(UNIX_EPOCH + Duration::from_secs(secs as u64))
            .or_else(|_| file.set_modified(SystemTime::now()))?;
    }
    Ok(())
}

fn write_keywords_file(folder: &Folder) -> Result<(), Error> {
    if folder.custom_keywords.is_empty() {
        return Ok(());
    }
    let mut text = String::new();
    for (i, keyword) in folder.custom_keywords.iter().enumerate() {
        text.push_str(&format!("{i} {keyword}\n"));
    }
    fs::write(folder.dir.join("dovecot-keywords"), text)?;
    Ok(())
}

fn folder_manifest(folder: &Folder, root: &Path) -> Value {
    let dir = match folder.dir.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => Path::new("mail").join(rel),
        _ => PathBuf::from("mail"),
    };
    let mut entry = json!({
        "name": folder.display_path.last().cloned().unwrap_or_default(),
        "path": folder.display_path,
        "dir": dir.to_string_lossy().replace('\\', "/"),
        "role": folder.role,
        "subscribed": folder.is_subscribed,
        "messages": folder.messages,
    });
    if folder.keywords_dropped > 0 {
        entry["keywordsDropped"] = json!(folder.keywords_dropped);
    }
    entry
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

    #[test]
    fn folders_follow_maildir_plus_plus() {
        let root = Path::new("/x");
        let folders = plan_folders(
            &[
                mailbox(1, "Inbox", None, Some("inbox")),
                mailbox(2, "Clients", Some(1), None),
                mailbox(3, "Archive", None, Some("archive")),
                mailbox(4, "2024", Some(3), None),
                mailbox(5, "v1.0 notes", None, None),
                mailbox(6, "Équipe & co", None, None),
                mailbox(7, "INBOX", None, None),
            ],
            root,
        );
        let name = |id| folders[&id].maildir_name.as_str();
        assert_eq!(name(1), "");
        assert_eq!(folders[&1].dir, root);
        assert_eq!(name(2), "INBOX.Clients");
        assert_eq!(name(3), "Archive");
        assert_eq!(name(4), "Archive.2024");
        assert_eq!(folders[&4].dir, root.join(".Archive.2024"));
        assert_eq!(name(5), "v1_0 notes");
        assert_eq!(folders[&5].display_path, vec!["v1.0 notes"]);
        assert_eq!(name(6), "&AMk-quipe &- co");
        assert_eq!(name(7), "INBOX (2)");
    }

    #[test]
    fn a_store_with_no_inbox_still_gets_a_root() {
        let folders = plan_folders(&[mailbox(1, "Sent", None, Some("sent"))], Path::new("/x"));
        assert!(folders.values().any(|f| f.maildir_name.is_empty()));
        assert_eq!(folders[&1].maildir_name, "Sent");
    }

    #[test]
    fn keywords_become_letters() {
        let mut folder = Folder {
            maildir_name: "Work".to_owned(),
            display_path: vec!["Work".to_owned()],
            dir: PathBuf::from("/x/.Work"),
            role: None,
            is_subscribed: true,
            messages: 0,
            custom_keywords: Vec::new(),
            keywords_dropped: 0,
        };
        let kw = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let logger = Logger::new(0);
        assert_eq!(
            flags_for(
                &kw(&["$seen", "$flagged", "$answered"]),
                &mut folder,
                logger
            ),
            "FRS"
        );
        assert_eq!(
            flags_for(&kw(&["$Junk", "$seen"]), &mut folder, logger),
            "Sa"
        );
        assert_eq!(
            flags_for(&kw(&["project-x", "$junk"]), &mut folder, logger),
            "ab"
        );
        assert_eq!(folder.custom_keywords, vec!["$Junk", "project-x"]);

        let many: Vec<String> = (0..30).map(|i| format!("k{i}")).collect();
        let flags = flags_for(&many, &mut folder, logger);
        // Two letters were already taken above.
        assert_eq!(flags.len(), MAX_CUSTOM_KEYWORDS - 2);
        assert_eq!(folder.keywords_dropped, 6);
    }

    #[test]
    fn delivery_time_prefers_the_archive_then_the_date_header() {
        assert_eq!(delivery_secs("2024-03-01T12:00:00Z", b""), 1_709_294_400);
        assert_eq!(
            delivery_secs("Fri, 01 Mar 2024 12:00:00 +0000", b""),
            1_709_294_400
        );
        assert_eq!(
            delivery_secs("", b"Date: Fri, 01 Mar 2024 12:00:00 +0000\r\n\r\nbody"),
            1_709_294_400
        );
        assert!(delivery_secs("", b"\r\n") > 1_709_294_400);
    }
}
