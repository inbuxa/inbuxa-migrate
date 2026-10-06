/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

#[derive(Debug, Clone, Copy)]
pub struct MailboxSpec {
    pub key: &'static str,
    pub name: &'static str,
    pub parent: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub struct FileSpec {
    pub key: &'static str,
    pub name: &'static str,
    pub parent: Option<&'static str>,
    pub directory: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SieveScript {
    pub name: &'static str,
    pub body: &'static str,
    pub active: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub mailboxes: &'static [MailboxSpec],
    pub files: &'static [FileSpec],
    pub calendars: &'static [&'static str],
    pub address_books: &'static [&'static str],
    pub sieve_scripts: &'static [SieveScript],
    pub email_count: usize,
    pub contact_count: usize,
    pub event_count: usize,
}

macro_rules! mailbox {
    ($k:expr, $n:expr, $p:expr) => {
        MailboxSpec {
            key: $k,
            name: $n,
            parent: $p,
        }
    };
}

macro_rules! node {
    ($k:expr, $n:expr, $p:expr, $d:expr) => {
        FileSpec {
            key: $k,
            name: $n,
            parent: $p,
            directory: $d,
        }
    };
}

pub const ACCOUNT1: &str = "user1";
pub const ACCOUNT2: &str = "user2";
pub const ACCOUNT3: &str = "user3";

pub const PASSWORD: &str = "InbuxaUser#2026";

pub static MAILBOXES_DEEP: &[MailboxSpec] = &[
    mailbox!("proj", "Projects", None),
    mailbox!("y2024", "2024", Some("proj")),
    mailbox!("q1", "Q1", Some("y2024")),
    mailbox!("acme", "ClientAcme", Some("q1")),
    mailbox!("acme_in", "Internal", Some("acme")),
    mailbox!("q2", "Q2", Some("y2024")),
    mailbox!("globex", "ClientGlobex", Some("q2")),
    mailbox!("y2025", "2025", Some("proj")),
    mailbox!("alpha", "Alpha", Some("y2025")),
    mailbox!("beta", "Beta", Some("y2025")),
    mailbox!("travel", "Travel", None),
    mailbox!("asia", "Asia", Some("travel")),
    mailbox!("jp", "Japan", Some("asia")),
    mailbox!("tokyo", "Tokyo", Some("jp")),
    mailbox!("eu", "Europe", Some("travel")),
    mailbox!("it", "Italy", Some("eu")),
    mailbox!("rome", "Rome", Some("it")),
    mailbox!("arch", "Archive", None),
    mailbox!("arch23", "2023", Some("arch")),
    mailbox!("arch24", "2024", Some("arch")),
];

pub static MAILBOXES_MEDIUM: &[MailboxSpec] = &[
    mailbox!("work", "Work", None),
    mailbox!("clients", "Clients", Some("work")),
    mailbox!("acme", "AcmeCorp", Some("clients")),
    mailbox!("inv", "Invoices", Some("acme")),
    mailbox!("globex", "Globex", Some("clients")),
    mailbox!("internal", "Internal", Some("work")),
    mailbox!("pers", "Personal", None),
    mailbox!("rcpt", "Receipts", Some("pers")),
];

pub static MAILBOXES_FLAT: &[MailboxSpec] = &[
    mailbox!("lists", "Lists", None),
    mailbox!("news", "Newsletters", Some("lists")),
    mailbox!("misc", "Misc", None),
];

pub static FILES_DEEP: &[FileSpec] = &[
    node!("docs", "Documents", None, true),
    node!("rep", "Reports", Some("docs"), true),
    node!("d24", "2024", Some("rep"), true),
    node!("q1f", "q1-results.bin", Some("d24"), false),
    node!("q2f", "q2-results.bin", Some("d24"), false),
    node!("d25", "2025", Some("rep"), true),
    node!("q1f25", "q1-results.bin", Some("d25"), false),
    node!("spec", "Specs", Some("docs"), true),
    node!("ovr", "overview.bin", Some("spec"), false),
    node!("notes", "notes.bin", Some("docs"), false),
    node!("photos", "Photos", None, true),
    node!("vac", "Vacations", Some("photos"), true),
    node!("vit", "Italy", Some("vac"), true),
    node!("vrome", "rome.bin", Some("vit"), false),
    node!("vjp", "Japan", Some("vac"), true),
    node!("vtokyo", "tokyo.bin", Some("vjp"), false),
    node!("readme", "README.bin", None, false),
];

pub static FILES_MEDIUM: &[FileSpec] = &[
    node!("media", "Media", None, true),
    node!("audio", "Audio", Some("media"), true),
    node!("song", "song.bin", Some("audio"), false),
    node!("video", "Video", Some("media"), true),
    node!("clip", "clip.bin", Some("video"), false),
    node!("proj", "Projects", None, true),
    node!("src", "src", Some("proj"), true),
    node!("main", "main.bin", Some("src"), false),
];

pub static FILES_FLAT: &[FileSpec] = &[
    node!("bak", "Backup", None, true),
    node!("snap", "snapshot.bin", Some("bak"), false),
    node!("scratch", "scratch.bin", None, false),
];

pub static CALENDARS_DEEP: &[&str] = &["Personal", "Work", "Birthdays", "Travel"];
pub static CALENDARS_MEDIUM: &[&str] = &["Personal", "Work"];
pub static CALENDARS_FLAT: &[&str] = &["Personal", "Holidays"];

pub static ADDRESS_BOOKS_DEEP: &[&str] = &["Personal", "Work", "Friends"];
pub static ADDRESS_BOOKS_MEDIUM: &[&str] = &["Personal", "Work"];
pub static ADDRESS_BOOKS_FLAT: &[&str] = &["Personal", "Family"];

pub static SIEVE_DEEP: &[SieveScript] = &[
    SieveScript {
        name: "spam",
        body: "require [\"fileinto\"];\nif header :contains \"X-Spam\" \"YES\" {\n  fileinto \"Junk\";\n  stop;\n}\n",
        active: true,
    },
    SieveScript {
        name: "vacation",
        body: "require [\"vacation\"];\nvacation :days 7 :subject \"Out of office\" \"I am away, will reply later.\";\n",
        active: false,
    },
    SieveScript {
        name: "organize",
        body: "require [\"fileinto\",\"mailbox\"];\nif address :is \"from\" \"newsletter@example.com\" {\n  fileinto :create \"Projects/2025/Beta\";\n}\n",
        active: false,
    },
];

pub static SIEVE_MEDIUM: &[SieveScript] = &[
    SieveScript {
        name: "filter",
        body: "require [\"fileinto\"];\nif address :is \"to\" \"work@example.com\" {\n  fileinto \"Work\";\n}\n",
        active: true,
    },
    SieveScript {
        name: "draft",
        body: "require [\"reject\"];\nif address :is \"from\" \"blocked@example.com\" {\n  reject \"go away\";\n}\n",
        active: false,
    },
];

pub static SIEVE_FLAT: &[SieveScript] = &[];

pub fn layout_for(username: &str) -> Layout {
    match username {
        s if s == ACCOUNT1 => Layout {
            mailboxes: MAILBOXES_DEEP,
            files: FILES_DEEP,
            calendars: CALENDARS_DEEP,
            address_books: ADDRESS_BOOKS_DEEP,
            sieve_scripts: SIEVE_DEEP,
            email_count: 200,
            contact_count: 40,
            event_count: 40,
        },
        s if s == ACCOUNT2 => Layout {
            mailboxes: MAILBOXES_MEDIUM,
            files: FILES_MEDIUM,
            calendars: CALENDARS_MEDIUM,
            address_books: ADDRESS_BOOKS_MEDIUM,
            sieve_scripts: SIEVE_MEDIUM,
            email_count: 80,
            contact_count: 15,
            event_count: 15,
        },
        _ => Layout {
            mailboxes: MAILBOXES_FLAT,
            files: FILES_FLAT,
            calendars: CALENDARS_FLAT,
            address_books: ADDRESS_BOOKS_FLAT,
            sieve_scripts: SIEVE_FLAT,
            email_count: 20,
            contact_count: 10,
            event_count: 10,
        },
    }
}

pub fn accounts() -> [&'static str; 3] {
    [ACCOUNT1, ACCOUNT2, ACCOUNT3]
}
