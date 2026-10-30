/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export-caldav` and `export-carddav` into real Radicale and Baikal
//! servers. One account is seeded and imported; the archive is exported
//! into a second account that has nothing yet, and read back with
//! `import caldav` / `import carddav`: every collection and item arrives,
//! a second run writes nothing, and a changed event is all a third run
//! sends.

mod integration;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use integration::Account;
use integration::baikal::Baikal;
use integration::data::{load_icals, load_vcards};
use integration::radicale::Radicale;
use integration::validate::{cleanup, common, open_archive, tmp_archive};

use inbuxa_migrate::sync::export_dav::{self, ExportDavConfig};
use inbuxa_migrate::sync::import_dav::{self, DavAuth, DavImportConfig, DavKindArg};
use inbuxa_migrate::sync::{Summary, TypeCounts};
use rusqlite::Connection;

fn auth(account: &Account) -> DavAuth {
    DavAuth::Basic {
        user: account.username.clone(),
        password: account.password.clone(),
    }
}

fn import(kind: DavKindArg, url: String, account: &Account, archive: &Path) -> Summary {
    let summary = import_dav::run(
        common(archive),
        DavImportConfig {
            kind,
            url,
            auth: auth(account),
            allow_cleartext: true,
            dav_connections: 2,
            multiget_batch: 25,
            source_change: inbuxa_migrate::sync::import_dav::SourceChange::Reject,
        },
    )
    .expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    summary
}

fn export(kind: DavKindArg, url: &str, account: &Account, archive: &Path) -> Summary {
    export_dav::run(
        common(archive),
        ExportDavConfig {
            kind,
            url: url.to_owned(),
            auth: auth(account),
            allow_cleartext: true,
        },
    )
    .expect("export")
}

fn counts<'a>(summary: &'a Summary, ty: &str) -> &'a TypeCounts {
    &summary.per_type.iter().find(|(t, _)| *t == ty).unwrap().1
}

/// Each calendar or address book by name, with its items as (uid, title).
fn contents(conn: &Connection, kind: DavKindArg) -> BTreeMap<String, BTreeSet<(String, String)>> {
    let sql = match kind {
        DavKindArg::Caldav => {
            "SELECT c.name, coalesce(json_extract(e.data, '$.uid'), json_extract(e.data, '$.entries[0].uid')),
                    coalesce(json_extract(e.data, '$.title'), json_extract(e.data, '$.entries[0].title'), '')
             FROM calendar_events e, json_each(e.calendar_ids) j JOIN calendars c ON c.id = j.value"
        }
        _ => {
            "SELECT b.name, c.uid, coalesce(json_extract(c.data, '$.name.full'), '')
             FROM contact_cards c, json_each(c.address_book_ids) j JOIN address_books b ON b.id = j.value"
        }
    };
    let mut out: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    let mut stmt = conn.prepare(sql).unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap();
    for row in rows {
        let (name, uid, title) = row.unwrap();
        out.entry(name).or_default().insert((uid, title));
    }
    let empty: Vec<String> = conn
        .prepare(match kind {
            DavKindArg::Caldav => "SELECT name FROM calendars",
            _ => "SELECT name FROM address_books",
        })
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for name in empty {
        out.entry(name).or_default();
    }
    out
}

/// The whole round trip for one kind. `source` and `target` give each
/// account's URL; `target` is where discovery starts, not the home.
fn round_trip(
    kind: DavKindArg,
    label: &str,
    source: (&Account, String),
    target: (&Account, String),
    back_url: String,
) {
    let (collection_type, item_type) = match kind {
        DavKindArg::Caldav => ("calendar", "calendarevent"),
        _ => ("addressbook", "contactcard"),
    };
    let archive = tmp_archive(&format!("export-dav-{label}-src"));
    import(kind, source.1, source.0, &archive);
    let expected = contents(&open_archive(&archive), kind);
    assert!(!expected.is_empty(), "nothing was seeded");
    let items: u64 = expected.values().map(|s| s.len() as u64).sum();

    let first = export(kind, &target.1, target.0, &archive);
    assert!(!first.any_failed(), "{first:?}");
    assert_eq!(
        counts(&first, collection_type).created,
        expected.len() as u64
    );
    assert_eq!(counts(&first, item_type).created, items, "{first:?}");

    let again = export(kind, &target.1, target.0, &archive);
    let c = counts(&again, item_type);
    assert_eq!(
        (c.created, c.updated, c.skipped),
        (0, 0, items),
        "{again:?}"
    );
    assert_eq!(counts(&again, collection_type).created, 0);

    let back = tmp_archive(&format!("export-dav-{label}-back"));
    import(kind, back_url, target.0, &back);
    assert_eq!(contents(&open_archive(&back), kind), expected);

    if matches!(kind, DavKindArg::Caldav) {
        let changed = open_archive(&archive)
            .execute(
                "UPDATE calendar_events SET data = json_set(data, '$.title', 'Moved')
                 WHERE id = (SELECT min(id) FROM calendar_events WHERE json_extract(data, '$.title') IS NOT NULL)",
                [],
            )
            .unwrap();
        assert_eq!(changed, 1);
        let third = export(kind, &target.1, target.0, &archive);
        let c = counts(&third, item_type);
        assert_eq!((c.created, c.updated), (0, 1), "{third:?}");
    }

    cleanup(&archive);
    cleanup(&back);
}

#[test]
#[ignore = "requires Docker"]
fn radicale_export_dav_round_trip_converges() {
    let r = Radicale::start().expect("radicale start");
    let (from, to) = (&r.accounts[0], r.accounts.last().unwrap());
    assert_ne!(from.username, to.username);
    r.seed_account(from, &load_icals().unwrap(), &load_vcards().unwrap())
        .expect("seed");
    for kind in [DavKindArg::Caldav, DavKindArg::Carddav] {
        round_trip(
            kind,
            &format!("radicale-{kind:?}"),
            (from, r.account_url(from)),
            (to, r.base_url()),
            r.account_url(to),
        );
    }
    r.stop().expect("radicale stop");
}

#[test]
#[ignore = "requires Docker"]
fn baikal_export_dav_round_trip_converges() {
    let b = Baikal::start().expect("baikal start");
    let (from, to) = (&b.accounts[0], b.accounts.last().unwrap());
    assert_ne!(from.username, to.username);
    b.seed_account(from, &load_icals().unwrap(), &load_vcards().unwrap())
        .expect("seed");
    for kind in [DavKindArg::Caldav, DavKindArg::Carddav] {
        let home = |a: &Account| match kind {
            DavKindArg::Caldav => b.calendar_home(a),
            _ => b.addressbook_home(a),
        };
        round_trip(
            kind,
            &format!("baikal-{kind:?}"),
            (from, home(from)),
            (to, format!("{}/", b.dav_root())),
            home(to),
        );
    }
    b.stop().expect("baikal stop");
}
