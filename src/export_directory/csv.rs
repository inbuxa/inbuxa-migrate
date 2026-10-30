/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! CSV as RFC 4180 writes it: CRLF line ends, and a field quoted when it
//! holds a comma, a quote or a line break. The text comes from the server,
//! and these files are meant to be opened in a spreadsheet, so a field that
//! one would run as a formula is written with a leading `'`.
//! `directory.json` keeps every value exactly.

use std::fs;
use std::path::Path;

use super::model::Table;

pub fn field(value: &str) -> String {
    let guarded;
    let value = if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        guarded = format!("'{value}");
        guarded.as_str()
    } else {
        value
    };
    if value.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

pub fn render((header, rows): &Table) -> String {
    let mut out = String::new();
    let mut line = |cells: &mut dyn Iterator<Item = &str>| {
        let cells: Vec<String> = cells.map(field).collect();
        out.push_str(&cells.join(","));
        out.push_str("\r\n");
    };
    line(&mut header.iter().copied());
    for row in rows {
        line(&mut row.iter().map(String::as_str));
    }
    out
}

pub fn write(path: &Path, table: &Table) -> std::io::Result<()> {
    fs::write(path, render(table))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_fields_are_left_alone() {
        assert_eq!(field("alice@example.org"), "alice@example.org");
        assert_eq!(field("Alice Example"), "Alice Example");
        assert_eq!(field(""), "");
    }

    #[test]
    fn commas_quotes_and_breaks_are_quoted() {
        assert_eq!(field("Smith, Jane"), "\"Smith, Jane\"");
        assert_eq!(field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(field("two\nlines"), "\"two\nlines\"");
    }

    #[test]
    fn formulas_are_not_left_to_run() {
        assert_eq!(field("=HYPERLINK(\"x\")"), "\"'=HYPERLINK(\"\"x\"\")\"");
        assert_eq!(field("+1"), "'+1");
        assert_eq!(field("-note"), "'-note");
        assert_eq!(field("@SUM(A1)"), "'@SUM(A1)");
    }

    #[test]
    fn a_table_is_crlf_lines() {
        let table: Table = (
            vec!["email", "description"],
            vec![vec!["a@example.org".to_owned(), "x, y".to_owned()]],
        );
        assert_eq!(
            render(&table),
            "email,description\r\na@example.org,\"x, y\"\r\n"
        );
    }
}
