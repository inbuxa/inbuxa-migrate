#!/usr/bin/env python3
#
# SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
# SPDX-FileCopyrightText: 2026 Coffey Labs LLC
#
# SPDX-License-Identifier: Apache-2.0 OR MIT
#

import argparse
import json
import os
import sqlite3
import sys
from datetime import datetime


STANDARD_FLAGS = {
    "$seen": "S",
    "$answered": "R",
    "$flagged": "F",
    "$draft": "D",
    "$deleted": "T",
}


def fail(message):
    print(f"error: {message}", file=sys.stderr)
    sys.exit(1)


def sanitize(component):
    out = []
    for ch in component:
        if ch in ("/", "\\", "\x00") or ord(ch) < 0x20:
            out.append("_")
        else:
            out.append(ch)
    cleaned = "".join(out).strip().rstrip(".")
    return cleaned or "_unnamed"


def unique_dir(parent, label):
    base = sanitize(label)
    candidate = os.path.join(parent, base)
    suffix = 1
    while os.path.exists(candidate):
        candidate = os.path.join(parent, f"{base}-{suffix}")
        suffix += 1
    os.makedirs(candidate)
    return candidate


def blob_bytes(conn, blob_id):
    if blob_id is None:
        return None
    row = conn.execute("SELECT data FROM blobs WHERE id = ?", (blob_id,)).fetchone()
    return row[0] if row is not None else None


def epoch_of(received_at):
    text = (received_at or "").strip()
    if not text:
        return 0
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        return int(datetime.fromisoformat(text).timestamp())
    except ValueError:
        return 0


def keywords_to_info(keywords_json):
    try:
        keywords = json.loads(keywords_json)
    except (ValueError, TypeError):
        keywords = []
    letters = set()
    for keyword in keywords:
        mapped = STANDARD_FLAGS.get(str(keyword).lower())
        if mapped:
            letters.add(mapped)
    return "".join(sorted(letters))


def ensure_maildir(path):
    for sub in ("cur", "new", "tmp"):
        os.makedirs(os.path.join(path, sub), exist_ok=True)


def maildir_paths(conn, root):
    rows = conn.execute(
        "SELECT id, name, parent_id, role FROM mailboxes"
    ).fetchall()
    info = {
        row[0]: {"name": row[1], "parent": row[2], "role": row[3]} for row in rows
    }

    def components(mailbox_id):
        chain = []
        cursor = mailbox_id
        seen = set()
        while cursor is not None and cursor not in seen:
            seen.add(cursor)
            node = info.get(cursor)
            if node is None:
                break
            chain.append((node["name"], node["role"]))
            cursor = node["parent"]
        chain.reverse()
        if chain and chain[0][1] == "inbox":
            chain = chain[1:]
        return [sanitize(name) for name, _ in chain]

    paths = {}
    os.makedirs(root, exist_ok=True)
    ensure_maildir(root)
    for mailbox_id in info:
        comps = components(mailbox_id)
        path = root if not comps else os.path.join(root, "." + ".".join(comps))
        ensure_maildir(path)
        paths[mailbox_id] = path
    return paths


def export_mail(conn, root):
    paths = maildir_paths(conn, root)
    rows = conn.execute(
        "SELECT e.id, e.received_at, e.mailbox_ids, e.keywords, b.data "
        "FROM emails e JOIN blobs b ON b.id = e.blob_id"
    ).fetchall()
    sequence = 0
    written = 0
    for email_id, received_at, mailbox_ids_json, keywords_json, data in rows:
        try:
            mailbox_ids = json.loads(mailbox_ids_json)
        except (ValueError, TypeError):
            mailbox_ids = []
        info = keywords_to_info(keywords_json)
        stamp = epoch_of(received_at)
        for mailbox_id in mailbox_ids:
            directory = paths.get(mailbox_id)
            if directory is None:
                continue
            sequence += 1
            name = f"{stamp}.M{email_id}P{sequence}.inbuxa-migrate:2,{info}"
            with open(os.path.join(directory, "cur", name), "wb") as handle:
                handle.write(data)
            written += 1
    return written


def resolve_blob_sentinels(conn, value, attachments_dir):
    if isinstance(value, dict):
        if "@blob" in value and isinstance(value["@blob"], int):
            blob_id = value["@blob"]
            payload = blob_bytes(conn, blob_id)
            if payload is not None:
                os.makedirs(attachments_dir, exist_ok=True)
                filename = f"blob-{blob_id}.bin"
                with open(os.path.join(attachments_dir, filename), "wb") as handle:
                    handle.write(payload)
                value["@blob"] = os.path.join(
                    os.path.basename(attachments_dir), filename
                )
        for nested in value.values():
            resolve_blob_sentinels(conn, nested, attachments_dir)
    elif isinstance(value, list):
        for nested in value:
            resolve_blob_sentinels(conn, nested, attachments_dir)


def write_json_item(conn, directory, label, payload):
    attachments_dir = os.path.join(directory, "_attachments")
    resolve_blob_sentinels(conn, payload, attachments_dir)
    base = sanitize(label)
    candidate = os.path.join(directory, f"{base}.json")
    suffix = 1
    while os.path.exists(candidate):
        candidate = os.path.join(directory, f"{base}-{suffix}.json")
        suffix += 1
    with open(candidate, "w", encoding="utf-8") as handle:
        json.dump(payload, handle, indent=2, ensure_ascii=False)


def export_collections(conn, root, container_table, container_select, item_query,
                       container_label, item_membership_field, augment):
    os.makedirs(root, exist_ok=True)
    containers = {}
    for row in conn.execute(container_select).fetchall():
        containers[row[0]] = unique_dir(root, container_label(row))
    orphans = None
    written = 0
    for row in conn.execute(item_query).fetchall():
        payload, member_ids, label = augment(row)
        targets = [containers[c] for c in member_ids if c in containers]
        if not targets:
            if orphans is None:
                orphans = unique_dir(root, "_orphans")
            targets = [orphans]
        for directory in targets:
            write_json_item(conn, directory, label, json.loads(json.dumps(payload)))
            written += 1
    return written


def export_calendars(conn, root):
    def container_label(row):
        return row[1] or f"calendar-{row[0]}"

    def augment(row):
        event_id, calendar_ids_json, is_draft, use_default_alerts, data_json, data_type = row
        payload = json.loads(data_json)
        if "@type" not in payload and data_type:
            payload["@type"] = data_type
        payload["isDraft"] = bool(is_draft)
        payload["useDefaultAlerts"] = bool(use_default_alerts)
        try:
            calendar_ids = json.loads(calendar_ids_json)
        except (ValueError, TypeError):
            calendar_ids = []
        payload["calendarIds"] = calendar_ids
        label = payload.get("uid") or f"event-{event_id}"
        return payload, calendar_ids, label

    return export_collections(
        conn,
        root,
        "calendars",
        "SELECT id, name FROM calendars",
        "SELECT id, calendar_ids, is_draft, use_default_alerts, data, data_type "
        "FROM calendar_events",
        container_label,
        "calendar_ids",
        augment,
    )


def export_contacts(conn, root):
    def container_label(row):
        return row[1] or f"addressbook-{row[0]}"

    def augment(row):
        card_id, uid, address_book_ids_json, data_json = row
        payload = json.loads(data_json)
        payload["uid"] = uid
        try:
            address_book_ids = json.loads(address_book_ids_json)
        except (ValueError, TypeError):
            address_book_ids = []
        label = uid or f"card-{card_id}"
        return payload, address_book_ids, label

    return export_collections(
        conn,
        root,
        "address_books",
        "SELECT id, name FROM address_books",
        "SELECT id, uid, address_book_ids, data FROM contact_cards",
        container_label,
        "address_book_ids",
        augment,
    )


def export_sieve(conn, root):
    rows = conn.execute(
        "SELECT s.id, s.name, s.is_active, b.data "
        "FROM sieve_scripts s JOIN blobs b ON b.id = s.blob_id"
    ).fetchall()
    if not rows:
        return 0
    os.makedirs(root, exist_ok=True)
    written = 0
    for script_id, name, is_active, data in rows:
        label = sanitize(name or f"script-{script_id}")
        with open(os.path.join(root, f"{label}.sieve"), "wb") as handle:
            handle.write(data)
        if is_active:
            with open(os.path.join(root, "active.sieve"), "wb") as handle:
                handle.write(data)
        written += 1
    return written


def export_files(conn, root):
    rows = conn.execute(
        "SELECT id, parent_id, node_type, blob_id, target, name FROM file_nodes"
    ).fetchall()
    if not rows:
        return 0
    children = {}
    for row in rows:
        children.setdefault(row[1], []).append(row)
    os.makedirs(root, exist_ok=True)
    written = 0

    def walk(parent_id, directory):
        nonlocal written
        for node_id, _parent, node_type, blob_id, target_json, name in children.get(
            parent_id, []
        ):
            path = os.path.join(directory, sanitize(name))
            if node_type == "directory":
                os.makedirs(path, exist_ok=True)
                walk(node_id, path)
            elif node_type == "symlink":
                try:
                    target = json.loads(target_json) if target_json else []
                except (ValueError, TypeError):
                    target = []
                link_target = "/".join(str(part) for part in target)
                try:
                    os.symlink(link_target, path)
                    written += 1
                except (OSError, NotImplementedError):
                    pass
            else:
                payload = blob_bytes(conn, blob_id)
                with open(path, "wb") as handle:
                    handle.write(payload if payload is not None else b"")
                written += 1

    walk(None, root)
    return written


def main():
    parser = argparse.ArgumentParser(
        description="Export an inbuxa-migrate SQLite archive to a directory tree."
    )
    parser.add_argument("archive")
    parser.add_argument("target")
    args = parser.parse_args()

    if not os.path.isfile(args.archive):
        fail(f"archive not found: {args.archive}")
    if os.path.exists(args.target):
        fail(f"target already exists: {args.target}")

    os.makedirs(args.target)

    uri = f"file:{os.path.abspath(args.archive)}?mode=ro&immutable=1"
    conn = sqlite3.connect(uri, uri=True)
    try:
        emails = export_mail(conn, os.path.join(args.target, "mail"))
        events = export_calendars(conn, os.path.join(args.target, "calendars"))
        cards = export_contacts(conn, os.path.join(args.target, "contacts"))
        scripts = export_sieve(conn, os.path.join(args.target, "sieve"))
        files = export_files(conn, os.path.join(args.target, "files"))
    finally:
        conn.close()

    print(f"emails:    {emails}")
    print(f"events:    {events}")
    print(f"contacts:  {cards}")
    print(f"sieve:     {scripts}")
    print(f"files:     {files}")


if __name__ == "__main__":
    main()
