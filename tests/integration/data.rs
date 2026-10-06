/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use mail_parser::mailbox::mbox::MessageIterator;

use super::error::{ContainerError, ContainerResult};

fn resources_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("resources")
}

#[derive(Debug, Clone)]
pub struct MboxMessage {
    pub raw: Vec<u8>,
    pub received_at: i64,
}

pub fn load_mbox(limit: usize) -> ContainerResult<Vec<MboxMessage>> {
    let path = resources_dir().join("mailbox.gz");
    if path.exists() {
        let file = std::fs::File::open(&path)?;
        let mut decoder = GzDecoder::new(file);
        let mut bytes = Vec::new();
        decoder.read_to_end(&mut bytes)?;
        let reader = BufReader::new(std::io::Cursor::new(bytes));
        let mut out = Vec::new();
        for item in MessageIterator::new(reader) {
            let message = item.map_err(|e| ContainerError::Resource(format!("mbox parse: {e}")))?;
            let received_at = message.internal_date() as i64;
            out.push(MboxMessage {
                raw: message.unwrap_contents(),
                received_at,
            });
            if out.len() >= limit {
                break;
            }
        }
        if !out.is_empty() {
            return Ok(out);
        }
    }
    if let Ok(takeout) = load_takeout_mbox("all_mail_including_spam_and_trash.mbox")
        && !takeout.is_empty()
    {
        return Ok(takeout.into_iter().take(limit).collect());
    }
    Ok(synth_messages(limit))
}

#[derive(Debug, Clone)]
pub struct RawFixture {
    pub name: String,
    pub bytes: Vec<u8>,
}

pub fn load_vcards() -> ContainerResult<Vec<RawFixture>> {
    Ok(synth_vcards(48))
}

pub fn load_icals() -> ContainerResult<Vec<RawFixture>> {
    Ok(synth_icals(48))
}

pub fn load_takeout_mbox(name: &str) -> ContainerResult<Vec<MboxMessage>> {
    let path = resources_dir().join("takeout").join(name);
    let bytes = std::fs::read(&path)
        .map_err(|e| ContainerError::Resource(format!("open {}: {e}", path.display())))?;
    let reader = BufReader::new(std::io::Cursor::new(bytes));
    let mut out = Vec::new();
    for item in MessageIterator::new(reader) {
        let message = item.map_err(|e| ContainerError::Resource(format!("mbox parse: {e}")))?;
        let received_at = message.internal_date() as i64;
        out.push(MboxMessage {
            raw: message.unwrap_contents(),
            received_at,
        });
    }
    Ok(out)
}

fn synth_messages(n: usize) -> Vec<MboxMessage> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let body = format!(
            "From: sender{i}@inbuxa-migrate.test\r\n\
             To: user@inbuxa-migrate.test\r\n\
             Subject: Synthetic test message {i}\r\n\
             Message-ID: <synth-{i}@inbuxa-migrate.test>\r\n\
             Date: Wed, 01 Jan 2025 12:00:00 +0000\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             This is synthetic seed message #{i} for inbuxa-migrate container tests.\r\n",
        );
        out.push(MboxMessage {
            raw: body.into_bytes(),
            received_at: 0,
        });
    }
    out
}

const VCARD_VARIANTS: &[fn(usize) -> String] = &[
    vcard_v3_basic,
    vcard_v3_full,
    vcard_v3_multi_email,
    vcard_v3_org_and_title,
    vcard_v4_basic,
    vcard_v4_with_address,
    vcard_v4_nickname,
    vcard_v4_birthday,
    vcard_v3_apple_item_labels,
];

fn synth_vcards(n: usize) -> Vec<RawFixture> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let body = VCARD_VARIANTS[i % VCARD_VARIANTS.len()](i);
        out.push(RawFixture {
            name: format!("synth-{i:03}"),
            bytes: body.into_bytes(),
        });
    }
    out
}

fn vcard_v3_basic(i: usize) -> String {
    format!(
        "BEGIN:VCARD\r\n\
         VERSION:3.0\r\n\
         UID:inbuxa-migrate-card-basic-{i}\r\n\
         FN:Basic Contact {i}\r\n\
         N:Contact{i};Basic;;;\r\n\
         EMAIL;TYPE=INTERNET:basic-{i}@inbuxa-migrate.test\r\n\
         END:VCARD\r\n"
    )
}

fn vcard_v3_full(i: usize) -> String {
    format!(
        "BEGIN:VCARD\r\n\
         VERSION:3.0\r\n\
         UID:inbuxa-migrate-card-full-{i}\r\n\
         FN:Full Contact {i}\r\n\
         N:Contact{i};Full;Middle;;\r\n\
         EMAIL;TYPE=INTERNET;TYPE=WORK:full-{i}@inbuxa-migrate.test\r\n\
         TEL;TYPE=CELL:+15550010{i:03}\r\n\
         ORG:Inbuxa Test\r\n\
         TITLE:Senior Test Engineer\r\n\
         URL:https://inbuxa-migrate.test/{i}\r\n\
         NOTE:Synthetic full v3 contact #{i}\r\n\
         END:VCARD\r\n"
    )
}

fn vcard_v3_multi_email(i: usize) -> String {
    format!(
        "BEGIN:VCARD\r\n\
         VERSION:3.0\r\n\
         UID:inbuxa-migrate-card-multi-{i}\r\n\
         FN:Multi Email {i}\r\n\
         N:Contact{i};MultiEmail;;;\r\n\
         EMAIL;TYPE=INTERNET;TYPE=HOME:multi-{i}-home@inbuxa-migrate.test\r\n\
         EMAIL;TYPE=INTERNET;TYPE=WORK:multi-{i}-work@inbuxa-migrate.test\r\n\
         END:VCARD\r\n"
    )
}

fn vcard_v3_org_and_title(i: usize) -> String {
    format!(
        "BEGIN:VCARD\r\n\
         VERSION:3.0\r\n\
         UID:inbuxa-migrate-card-org-{i}\r\n\
         FN:Org Contact {i}\r\n\
         N:Contact{i};Org;;Dr.;PhD\r\n\
         ORG:Inbuxa Industries;Engineering;Test Group\r\n\
         TITLE:Principal Researcher\r\n\
         EMAIL;TYPE=INTERNET:org-{i}@inbuxa-migrate.test\r\n\
         END:VCARD\r\n"
    )
}

fn vcard_v4_basic(i: usize) -> String {
    format!(
        "BEGIN:VCARD\r\n\
         VERSION:4.0\r\n\
         UID:urn:uuid:00000000-0000-4000-a000-0000{i:08x}\r\n\
         FN:V4 Basic {i}\r\n\
         N:Contact{i};V4Basic;;;\r\n\
         EMAIL:v4-basic-{i}@inbuxa-migrate.test\r\n\
         END:VCARD\r\n"
    )
}

fn vcard_v4_with_address(i: usize) -> String {
    format!(
        "BEGIN:VCARD\r\n\
         VERSION:4.0\r\n\
         UID:urn:uuid:00000000-0000-4000-b000-0000{i:08x}\r\n\
         FN:V4 Address {i}\r\n\
         N:Contact{i};V4Address;;;\r\n\
         EMAIL:v4-address-{i}@inbuxa-migrate.test\r\n\
         ADR;TYPE=home:;;{i} Test Street;Springfield;OR;97477;US\r\n\
         END:VCARD\r\n"
    )
}

fn vcard_v4_nickname(i: usize) -> String {
    format!(
        "BEGIN:VCARD\r\n\
         VERSION:4.0\r\n\
         UID:urn:uuid:00000000-0000-4000-c000-0000{i:08x}\r\n\
         FN:V4 Nick {i}\r\n\
         N:Contact{i};V4Nick;;;\r\n\
         NICKNAME:Nicky{i}\r\n\
         EMAIL:v4-nick-{i}@inbuxa-migrate.test\r\n\
         END:VCARD\r\n"
    )
}

fn vcard_v4_birthday(i: usize) -> String {
    let year = 1970 + (i % 30);
    let month = (i % 12) + 1;
    let day = (i % 28) + 1;
    format!(
        "BEGIN:VCARD\r\n\
         VERSION:4.0\r\n\
         UID:urn:uuid:00000000-0000-4000-d000-0000{i:08x}\r\n\
         FN:V4 Birthday {i}\r\n\
         N:Contact{i};V4Birthday;;;\r\n\
         BDAY:{year:04}{month:02}{day:02}\r\n\
         EMAIL:v4-bday-{i}@inbuxa-migrate.test\r\n\
         END:VCARD\r\n"
    )
}

fn vcard_v3_apple_item_labels(i: usize) -> String {
    format!(
        "BEGIN:VCARD\r\n\
         VERSION:3.0\r\n\
         UID:inbuxa-migrate-card-ablabels-{i}\r\n\
         FN:Apple Labels {i}\r\n\
         N:Contact{i};AppleLabels;;;\r\n\
         EMAIL;TYPE=INTERNET:ablabels-{i}@inbuxa-migrate.test\r\n\
         ITEM1.X-ABLABEL:Name1\r\n\
         ITEM2.X-ABLABEL:Name2\r\n\
         ITEM1.X-ABDATE:20171111\r\n\
         ITEM2.X-ABDATE:20111111\r\n\
         END:VCARD\r\n"
    )
}

const ICAL_VARIANTS: &[fn(usize) -> String] = &[
    ical_simple_dt,
    ical_all_day,
    ical_multi_day_dt,
    ical_with_organizer_attendees,
    ical_with_categories_location,
    ical_recurring_daily,
    ical_recurring_weekly_until,
    ical_with_alarm,
];

fn synth_icals(n: usize) -> Vec<RawFixture> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let body = ICAL_VARIANTS[i % ICAL_VARIANTS.len()](i);
        out.push(RawFixture {
            name: format!("synth-{i:03}"),
            bytes: body.into_bytes(),
        });
    }
    out
}

fn ical_simple_dt(i: usize) -> String {
    let month = ((i % 12) + 1) as u32;
    let day = ((i % 28) + 1) as u32;
    let hour = (i % 24) as u32;
    format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//inbuxa-migrate//container-test//EN\r\n\
         BEGIN:VEVENT\r\n\
         UID:inbuxa-migrate-simple-{i}@inbuxa-migrate.test\r\n\
         DTSTAMP:20250101T120000Z\r\n\
         DTSTART:2025{month:02}{day:02}T{hour:02}0000Z\r\n\
         DTEND:2025{month:02}{day:02}T{hour:02}3000Z\r\n\
         SUMMARY:Simple datetime event {i}\r\n\
         DESCRIPTION:Simple synthetic event #{i}\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR\r\n"
    )
}

fn ical_all_day(i: usize) -> String {
    let month = ((i % 12) + 1) as u32;
    let day = ((i % 28) + 1) as u32;
    let next_day = day + 1;
    format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//inbuxa-migrate//container-test//EN\r\n\
         BEGIN:VEVENT\r\n\
         UID:inbuxa-migrate-allday-{i}@inbuxa-migrate.test\r\n\
         DTSTAMP:20250101T120000Z\r\n\
         DTSTART;VALUE=DATE:2025{month:02}{day:02}\r\n\
         DTEND;VALUE=DATE:2025{month:02}{next_day:02}\r\n\
         SUMMARY:All-day event {i}\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR\r\n"
    )
}

fn ical_multi_day_dt(i: usize) -> String {
    let month = ((i % 12) + 1) as u32;
    let day = ((i % 27) + 1) as u32;
    format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//inbuxa-migrate//container-test//EN\r\n\
         BEGIN:VEVENT\r\n\
         UID:inbuxa-migrate-multiday-{i}@inbuxa-migrate.test\r\n\
         DTSTAMP:20250101T120000Z\r\n\
         DTSTART:2025{month:02}{day:02}T090000Z\r\n\
         DTEND:2025{month:02}{day:02}T170000Z\r\n\
         SUMMARY:Workshop session {i}\r\n\
         DESCRIPTION:Multi-hour workshop with description.\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR\r\n"
    )
}

fn ical_with_organizer_attendees(i: usize) -> String {
    let month = ((i % 12) + 1) as u32;
    let day = ((i % 28) + 1) as u32;
    format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//inbuxa-migrate//container-test//EN\r\n\
         BEGIN:VEVENT\r\n\
         UID:inbuxa-migrate-meeting-{i}@inbuxa-migrate.test\r\n\
         DTSTAMP:20250101T120000Z\r\n\
         DTSTART:2025{month:02}{day:02}T140000Z\r\n\
         DTEND:2025{month:02}{day:02}T150000Z\r\n\
         SUMMARY:Sync meeting {i}\r\n\
         ORGANIZER;CN=Organiser:mailto:organiser-{i}@inbuxa-migrate.test\r\n\
         ATTENDEE;CN=Alice;PARTSTAT=ACCEPTED:mailto:alice-{i}@inbuxa-migrate.test\r\n\
         ATTENDEE;CN=Bob;PARTSTAT=NEEDS-ACTION:mailto:bob-{i}@inbuxa-migrate.test\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR\r\n"
    )
}

fn ical_with_categories_location(i: usize) -> String {
    let month = ((i % 12) + 1) as u32;
    let day = ((i % 28) + 1) as u32;
    format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//inbuxa-migrate//container-test//EN\r\n\
         BEGIN:VEVENT\r\n\
         UID:inbuxa-migrate-cat-{i}@inbuxa-migrate.test\r\n\
         DTSTAMP:20250101T120000Z\r\n\
         DTSTART:2025{month:02}{day:02}T100000Z\r\n\
         DTEND:2025{month:02}{day:02}T110000Z\r\n\
         SUMMARY:Categorised event {i}\r\n\
         LOCATION:Test Lab\\, Room {i}\r\n\
         CATEGORIES:INBUXA,TEST,FIXTURE\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR\r\n"
    )
}

fn ical_recurring_daily(i: usize) -> String {
    let month = ((i % 12) + 1) as u32;
    let day = ((i % 28) + 1) as u32;
    format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//inbuxa-migrate//container-test//EN\r\n\
         BEGIN:VEVENT\r\n\
         UID:inbuxa-migrate-daily-{i}@inbuxa-migrate.test\r\n\
         DTSTAMP:20250101T120000Z\r\n\
         DTSTART:2025{month:02}{day:02}T070000Z\r\n\
         DTEND:2025{month:02}{day:02}T073000Z\r\n\
         RRULE:FREQ=DAILY;COUNT=10\r\n\
         SUMMARY:Daily standup {i}\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR\r\n"
    )
}

fn ical_recurring_weekly_until(i: usize) -> String {
    let month = ((i % 12) + 1) as u32;
    let day = ((i % 28) + 1) as u32;
    let until_month = (month % 12) + 1;
    format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//inbuxa-migrate//container-test//EN\r\n\
         BEGIN:VEVENT\r\n\
         UID:inbuxa-migrate-weekly-{i}@inbuxa-migrate.test\r\n\
         DTSTAMP:20250101T120000Z\r\n\
         DTSTART:2025{month:02}{day:02}T160000Z\r\n\
         DTEND:2025{month:02}{day:02}T170000Z\r\n\
         RRULE:FREQ=WEEKLY;UNTIL=2025{until_month:02}{day:02}T160000Z;BYDAY=MO\r\n\
         SUMMARY:Weekly retro {i}\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR\r\n"
    )
}

fn ical_with_alarm(i: usize) -> String {
    let month = ((i % 12) + 1) as u32;
    let day = ((i % 28) + 1) as u32;
    format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//inbuxa-migrate//container-test//EN\r\n\
         BEGIN:VEVENT\r\n\
         UID:inbuxa-migrate-alarm-{i}@inbuxa-migrate.test\r\n\
         DTSTAMP:20250101T120000Z\r\n\
         DTSTART:2025{month:02}{day:02}T080000Z\r\n\
         DTEND:2025{month:02}{day:02}T083000Z\r\n\
         SUMMARY:Event with alarm {i}\r\n\
         BEGIN:VALARM\r\n\
         ACTION:DISPLAY\r\n\
         DESCRIPTION:Reminder {i}\r\n\
         TRIGGER:-PT15M\r\n\
         END:VALARM\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR\r\n"
    )
}

pub fn malformed_ical(name: &str) -> RawFixture {
    let body = format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//inbuxa-migrate//broken//EN\r\n\
         BEGIN:VEVENT\r\n\
         UID:broken-{name}@inbuxa-migrate.test\r\n\
         DTSTAMP:20250101T120000Z\r\n\
         DTSTART:NOT-A-DATE\r\n\
         RRULE:FREQ=BOGUS;INTERVAL=oops\r\n\
         END:NOT-A-VEVENT\r\n",
    );
    RawFixture {
        name: format!("broken-{name}"),
        bytes: body.into_bytes(),
    }
}

pub fn rewrite_uid(bytes: &[u8], suffix: &str) -> Option<(Vec<u8>, String)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut out = String::with_capacity(text.len() + 32);
    let mut new_uid: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("UID:") {
            let combined = format!("{rest}-{suffix}");
            if new_uid.is_none() {
                new_uid = Some(combined.clone());
            }
            out.push_str("UID:");
            out.push_str(&combined);
        } else {
            out.push_str(line);
        }
        out.push_str("\r\n");
    }
    let uid = new_uid?;
    Some((out.into_bytes(), uid))
}

pub fn extract_uid(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    for line in text.lines() {
        let l = line.trim_end_matches('\r');
        if let Some(rest) = l.strip_prefix("UID:") {
            return Some(rest.to_owned());
        }
    }
    None
}
