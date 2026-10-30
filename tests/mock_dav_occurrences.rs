/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Invitations to single occurrences of a series, with no master event:
//! `import caldav` keeps every occurrence, follows the resource as it
//! changes, and `export-caldav` writes them back as one resource.

mod fake_dav;

use std::path::Path;
use std::sync::{Arc, Mutex};

use fake_dav::{Resource, Store, serve};
use inbuxa_migrate::export_files::{self, ExportFilesConfig};
use inbuxa_migrate::logging::Logger;
use inbuxa_migrate::sync::export_dav::{self, ExportDavConfig};
use inbuxa_migrate::sync::import_dav::{self, DavAuth, DavImportConfig, DavKindArg};
use inbuxa_migrate::sync::{CommonConfig, Summary, TypeCounts};
use rusqlite::Connection;
use tempfile::TempDir;

fn occurrence(week: u32, title: &str) -> String {
    format!(
        "BEGIN:VEVENT\r\nUID:series-1\r\nRECURRENCE-ID:202403{d:02}T090000Z\r\n\
         DTSTAMP:20240101T000000Z\r\nDTSTART:202403{d:02}T090000Z\r\nDURATION:PT30M\r\n\
         SUMMARY:{title}\r\nEND:VEVENT\r\n",
        d = 4 + 7 * (week - 1)
    )
}

fn calendar(components: &[String]) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:x\r\n{}END:VCALENDAR\r\n",
        components.concat()
    )
}

const LUNCH: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:x\r\nBEGIN:VEVENT\r\nUID:lunch\r\n\
DTSTAMP:20240101T000000Z\r\nDTSTART:20240305T120000Z\r\nDURATION:PT1H\r\nSUMMARY:Lunch\r\n\
END:VEVENT\r\nEND:VCALENDAR\r\n";

fn common(archive: &Path) -> CommonConfig {
    CommonConfig {
        archive: archive.to_path_buf(),
        threads: 1,
        dry_run: false,
        max_retries: 0,
        allow_invalid_certs: false,
        logger: Logger::from_flags(false, 0),
    }
}

fn auth() -> DavAuth {
    DavAuth::Basic {
        user: "u".into(),
        password: "p".into(),
    }
}

fn import(url: &str, archive: &Path) -> Summary {
    let summary = import_dav::run(
        common(archive),
        DavImportConfig {
            kind: DavKindArg::Caldav,
            url: url.to_owned(),
            auth: auth(),
            allow_cleartext: true,
            dav_connections: 1,
            multiget_batch: 25,
            source_change: import_dav::SourceChange::Reject,
        },
    )
    .expect("import");
    assert!(!summary.any_failed(), "{summary:?}");
    summary
}

fn events(summary: &Summary) -> &TypeCounts {
    &summary
        .per_type
        .iter()
        .find(|(t, _)| *t == "calendarevent")
        .unwrap()
        .1
}

/// Each event as (uid, occurrence, title).
fn rows(archive: &Path) -> Vec<(String, Option<String>, String)> {
    let conn = Connection::open(archive).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT json_extract(data, '$.uid'), json_extract(data, '$.recurrenceId'),
                    json_extract(data, '$.title')
             FROM calendar_events ORDER BY 1, 2",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn row(uid: &str, rid: Option<&str>, title: &str) -> (String, Option<String>, String) {
    (uid.to_owned(), rid.map(str::to_owned), title.to_owned())
}

#[test]
fn occurrences_without_a_master_are_each_kept_and_written_back_together() {
    let dir = TempDir::new().unwrap();
    let archive = dir.path().join("archive.sqlite");
    let store = Arc::new(Mutex::new(Store::default()));
    {
        let mut s = store.lock().unwrap();
        s.collection("/cal/team/", "Team", true);
        s.put_item(
            "/cal/team/series.ics",
            &calendar(&[occurrence(1, "Week 1"), occurrence(2, "Week 2")]),
        );
        s.put_item("/cal/team/lunch.ics", LUNCH);
    }
    let url = serve(store.clone());

    import(&url, &archive);
    let week1 = row("series-1", Some("2024-03-04T09:00:00"), "Week 1");
    let week2 = row("series-1", Some("2024-03-11T09:00:00"), "Week 2");
    let week3 = row("series-1", Some("2024-03-18T09:00:00"), "Week 3");
    let lunch = row("lunch", None, "Lunch");
    assert_eq!(
        rows(&archive),
        [lunch.clone(), week1.clone(), week2.clone()]
    );

    let again = import(&url, &archive);
    assert_eq!(events(&again).fetched, 0, "{again:?}");
    assert_eq!(
        rows(&archive),
        [lunch.clone(), week1.clone(), week2.clone()]
    );

    // A third invitation, then one withdrawn: the rows follow.
    let path = "/cal/team/series.ics";
    let three = calendar(&[
        occurrence(1, "Week 1"),
        occurrence(2, "Week 2"),
        occurrence(3, "Week 3"),
    ]);
    store.lock().unwrap().put_item(path, &three);
    import(&url, &archive);
    assert_eq!(
        rows(&archive),
        [lunch.clone(), week1.clone(), week2.clone(), week3.clone()]
    );
    store
        .lock()
        .unwrap()
        .put_item(path, &calendar(&[occurrence(2, "Week 2")]));
    import(&url, &archive);
    assert_eq!(rows(&archive), [lunch.clone(), week2.clone()]);

    store.lock().unwrap().put_item(path, &three);
    import(&url, &archive);

    // Out again, into an empty server: one resource for the series.
    let target = Arc::new(Mutex::new(Store::default()));
    let target_url = serve(target.clone());
    let export = |archive: &Path| {
        export_dav::run(
            common(archive),
            ExportDavConfig {
                kind: DavKindArg::Caldav,
                url: target_url.clone(),
                auth: auth(),
                allow_cleartext: true,
            },
        )
        .expect("export")
    };
    let first = export(&archive);
    assert_eq!(events(&first).created, 4, "counted per row: {first:?}");
    {
        let t = target.lock().unwrap();
        let Some(Resource::Item { body, .. }) = t.resources.get("/cal/team/series-1.ics") else {
            panic!("no series: {:?}", t.resources.keys().collect::<Vec<_>>());
        };
        assert_eq!(body.matches("BEGIN:VEVENT").count(), 3, "{body}");
        assert_eq!(body.matches("RECURRENCE-ID").count(), 3, "{body}");
    }
    let second = export(&archive);
    let c = events(&second);
    assert_eq!((c.created, c.updated, c.skipped), (0, 0, 4), "{second:?}");

    // As files: the calendar's .ics holds each occurrence.
    let out = dir.path().join("files");
    let files = export_files::run(ExportFilesConfig {
        archive: archive.clone(),
        dir: out.clone(),
        objects: None,
        dry_run: false,
        logger: Logger::from_flags(false, 0),
    })
    .expect("export-files");
    assert!(!files.any_failed(), "{files:?}");
    let ics = std::fs::read_to_string(out.join("calendars/Team.ics")).unwrap();
    assert_eq!(ics.matches("RECURRENCE-ID").count(), 3, "{ics}");
    assert_eq!(ics.matches("UID:series-1").count(), 3, "{ics}");

    // And back in, from the exported copy: the same rows.
    let back = dir.path().join("back.sqlite");
    import(&target_url, &back);
    assert_eq!(rows(&back), rows(&archive));

    // The resource deleted: every occurrence goes.
    store.lock().unwrap().remove(path);
    import(&url, &archive);
    assert_eq!(rows(&archive), [lunch]);
}
