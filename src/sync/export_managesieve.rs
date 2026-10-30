/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! `export-managesieve`: an archive's Sieve scripts written into any
//! ManageSieve server (RFC 5804). Like `export-imap`, it keeps no state:
//! each run lists the target's scripts, writes the ones that are missing or
//! differ, and makes the archive's active script the active one, so a
//! second run writes nothing. It never deletes a script.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::db::{blobs, init};
use crate::error::Error;
use crate::imap::transport::Connector;
use crate::logging::{LEVEL_PROGRESS, Logger};
use crate::managesieve::error::SieveError;
use crate::managesieve::response::parse_listscripts;
use crate::managesieve::{ConnectMode, SieveClient, parse_getscript};
use crate::sync::import_managesieve::ManageSieveAuth;
use crate::sync::import_managesieve::coordinator::{authenticate, parse_endpoint};
use crate::sync::{CommonConfig, Summary, TypeCounts};

pub struct ExportManageSieveConfig {
    pub url: String,
    pub auth: ManageSieveAuth,
    pub allow_cleartext: bool,
}

struct Script {
    name: String,
    active: bool,
    bytes: Vec<u8>,
}

/// The archive's scripts by name. A script with no name is given one.
fn read_scripts(conn: &Connection) -> Result<Vec<Script>, Error> {
    let mut stmt =
        conn.prepare("SELECT id, name, is_active, blob_id FROM sieve_scripts ORDER BY name, id")?;
    let rows: Vec<(i64, Option<String>, bool, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, name, active, blob_id) in rows {
        let Some(bytes) = blobs::blob_bytes(conn, blob_id)? else {
            return Err(Error::Partial(format!(
                "the archive is missing the blob for Sieve script {id}"
            )));
        };
        out.push(Script {
            name: name
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("script-{id}")),
            active,
            bytes,
        });
    }
    Ok(out)
}

fn wire(context: &str) -> impl Fn(SieveError) -> Error + '_ {
    move |e| Error::Connection(format!("{context}: {e}"))
}

pub fn run(common: CommonConfig, config: ExportManageSieveConfig) -> Result<Summary, Error> {
    if !common.archive.is_file() {
        return Err(Error::Usage(format!(
            "no archive at {}",
            common.archive.display()
        )));
    }
    let conn = init::open(&common.archive)?;
    let scripts = read_scripts(&conn)?;
    let logger = common.logger;
    let dry_run = common.dry_run;

    let endpoint = parse_endpoint(&config.url)?;
    let connector =
        Connector::new(common.allow_invalid_certs).map_err(|e| Error::Connection(e.to_string()))?;
    let mode = if endpoint.implicit_tls {
        ConnectMode::ImplicitTls
    } else {
        ConnectMode::StartTls
    };
    let mut client = SieveClient::connect(
        &connector,
        &endpoint.host,
        endpoint.port,
        mode,
        config.allow_cleartext,
        logger,
    )
    .map_err(|e| Error::Connection(e.to_string()))?;
    authenticate(&mut client, &config.auth)?;
    if !client.had_fresh_post_auth_caps()
        && let Err(e) = client.refresh_capabilities()
    {
        logger.warn(&format!(
            "post-auth CAPABILITY refresh failed: {e}; continuing"
        ));
    }

    let listing = client.listscripts().map_err(wire("LISTSCRIPTS"))?;
    let present: HashMap<String, bool> = parse_listscripts(&listing.data)
        .map_err(wire("LISTSCRIPTS"))?
        .into_iter()
        .map(|s| (s.name, s.active))
        .collect();
    // CHECKSCRIPT is RFC 5804's; a server that predates it has no VERSION.
    let can_check = client.capabilities().version.is_some();

    let mut counts = TypeCounts::default();
    let mut written: Vec<&str> = Vec::new();
    for script in &scripts {
        let name = script.name.as_str();
        let exists = present.contains_key(name);
        if exists {
            let block = client
                .getscript(name)
                .map_err(wire(&format!("GETSCRIPT {name:?}")))?;
            let theirs = parse_getscript(&block.data).map_err(wire("GETSCRIPT"))?;
            if theirs == script.bytes {
                counts.skipped += 1;
                written.push(name);
                continue;
            }
        }
        let outcome = if dry_run {
            if can_check {
                client.checkscript(&script.bytes)
            } else {
                Ok(())
            }
        } else {
            client.putscript(name, &script.bytes)
        };
        match outcome {
            Ok(()) => {
                if exists {
                    counts.updated += 1;
                } else {
                    counts.created += 1;
                }
                written.push(name);
                if logger.enabled(LEVEL_PROGRESS) {
                    let verb = if exists { "updated" } else { "created" };
                    eprintln!("sievescript {name:?}: {verb}");
                }
            }
            // The server refused this script: say why and go on.
            Err(SieveError::No(no)) => {
                logger.warn(&format!("script {name:?} was refused: {no}"));
                counts.failed += 1;
            }
            Err(e) => return Err(wire(&format!("PUTSCRIPT {name:?}"))(e)),
        }
    }

    // The archive's active script becomes the target's. With none active in
    // the archive, the target's choice is left as it is.
    if let Some(active) = scripts.iter().find(|s| s.active) {
        let name = active.name.as_str();
        let already = present.get(name).copied().unwrap_or(false);
        if !already && written.contains(&name) {
            if dry_run {
                log_activation(logger, name, true);
            } else {
                match client.setactive(name) {
                    Ok(()) => log_activation(logger, name, false),
                    Err(SieveError::No(no)) => {
                        logger.warn(&format!("SETACTIVE {name:?} was refused: {no}"));
                        counts.failed += 1;
                    }
                    Err(e) => return Err(wire(&format!("SETACTIVE {name:?}"))(e)),
                }
            }
        }
    }
    let _ = client.logout();

    if dry_run {
        logger.warn(&format!(
            "dry run: nothing was written{}",
            if can_check {
                "; each script to write was checked with CHECKSCRIPT"
            } else {
                "; the server has no CHECKSCRIPT, so scripts were not checked"
            }
        ));
    }
    Ok(Summary {
        per_type: vec![("sievescript", counts)],
        ..Summary::default()
    })
}

fn log_activation(logger: Logger, name: &str, dry_run: bool) {
    if logger.enabled(LEVEL_PROGRESS) {
        let verb = if dry_run { "would be made" } else { "made" };
        eprintln!("sievescript {name:?}: {verb} active");
    }
}
