/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 John Coffey <johnellis@linux.com>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::common::{create_batch, jid, retry_if_blob_missing, target_get_all};
use super::sieve_names;
use super::{Maps, Net, Plan, Uploader};
use crate::error::Error;
use crate::jmap::request::{Request, check_method_error};
use crate::logging::{LEVEL_DEFAULT, Logger};
use crate::sync::import_jmap::mapping::BlobBytes;
use crate::sync::import_jmap::mapping::{SIEVE_SELECT, row_to_sieve_script};
use crate::sync::{Context, TypeCounts};
use crate::types::ObjectType;

pub fn reconcile(
    ctx: &Context,
    net: &Net,
    _maps: &mut Maps,
    counts: &mut TypeCounts,
    logger: &Logger,
) -> Result<Plan, Error> {
    let ty = ObjectType::SieveScript;
    let targets = target_get_all(net, ty).map_err(Error::from)?;

    let mut target_by_name: HashMap<String, String> = HashMap::new();
    for t in &targets {
        let (Some(id), Some(name)) = (jid(t), t.get("name").and_then(Value::as_str)) else {
            continue;
        };
        target_by_name.insert(name.to_owned(), id);
    }

    let locals: Vec<(i64, Option<String>, bool, i64)> = {
        let mut stmt = ctx
            .conn
            .prepare(SIEVE_SELECT)
            .map_err(|e| Error::Partial(e.to_string()))?;
        stmt.query_map([], |row| {
            let sr = row_to_sieve_script(row);
            Ok((row.get::<_, i64>(0)?, sr))
        })
        .and_then(|m| m.collect::<Result<Vec<_>, _>>())
        .map_err(|e| Error::Partial(e.to_string()))?
        .into_iter()
        .map(|(id, sr)| {
            let sr = sr.map_err(Error::from)?;
            Ok((id, sr.name, sr.is_active, sr.blob_local_id))
        })
        .collect::<Result<_, Error>>()?
    };

    let mut active_target: Option<String> = None;
    let mut deactivate = false;
    let mut uploader = Uploader::new(net, &ctx.conn);
    let rename_vendor = sieve_names::target_uses_inbuxa_names(&target_sieve_extensions(net));
    let wanted_active = locals
        .iter()
        .find(|(_, _, a, _)| *a)
        .map(|(_, n, _, _)| n.clone().unwrap_or_default());

    for (local, name, is_active, blob_local) in &locals {
        let matched = name.as_ref().and_then(|n| target_by_name.get(n)).cloned();
        let target_id = if let Some(id) = matched {
            counts.skipped += 1;
            id
        } else {
            let cid = format!("c{local}");
            let label = name.as_deref().unwrap_or("(unnamed)");
            let rewritten = if rename_vendor {
                renamed_script(&uploader, *blob_local, label, logger)?
            } else {
                None
            };
            let build = |up: &mut Uploader<'_>| -> Result<Value, Error> {
                let blob_id = match &rewritten {
                    Some(bytes) => up.upload_bytes_as(*blob_local, "application/sieve", bytes),
                    None => up.upload_with(*blob_local, "application/sieve"),
                }
                .map_err(Error::from)?;
                let mut obj = serde_json::Map::new();
                if let Some(n) = name {
                    obj.insert("name".to_owned(), Value::String(n.clone()));
                }
                obj.insert("blobId".to_owned(), Value::String(blob_id.0));
                Ok(Value::Object(obj))
            };
            let _ = uploader.take_touched();
            let wire = build(&mut uploader)?;
            let touched = uploader.take_touched();
            let outcome = create_batch(net, ty, vec![(cid.clone(), wire)]).map_err(Error::from)?;
            let outcome =
                retry_if_blob_missing(net, ty, &cid, &mut uploader, touched, outcome, build)?;
            match outcome.created.first().and_then(|(_, v)| jid(v)) {
                Some(id) => {
                    counts.created += 1;
                    if let Some(n) = name {
                        target_by_name.insert(n.clone(), id.clone());
                    }
                    id
                }
                None => {
                    for (cid, err) in &outcome.not_created {
                        logger.warn(&format!("SieveScript {label} ({cid}) not created: {err}"));
                    }
                    counts.failed += 1;
                    continue;
                }
            }
        };
        if *is_active {
            active_target = Some(target_id);
        }
    }

    if active_target.is_none() && locals.iter().all(|(_, _, a, _)| !*a) {
        deactivate = true;
    }
    if let (Some(name), None) = (&wanted_active, &active_target) {
        // The script that was active at the source never made it to the
        // target (its creation failure is already counted): say plainly that
        // the account now has no filtering, rather than leave it to a warning.
        logger.error(&format!(
            "the active Sieve script \"{name}\" could not be created on the target; \
             no filtering is active there"
        ));
    }

    if !net.dry_run {
        let mut req = Request::new();
        let args = if let Some(id) = &active_target {
            json!({ "accountId": net.account, "onSuccessActivateScript": id })
        } else if deactivate {
            json!({ "accountId": net.account, "onSuccessDeactivateScript": true })
        } else {
            json!({ "accountId": net.account })
        };
        req.call("SieveScript/set", args, "a");
        let result = req
            .send(&net.client, &net.api)
            .and_then(|resp| resp.by_call_id("a").cloned())
            .and_then(|mr| check_method_error(&mr));
        if let Err(e) = result {
            match (&active_target, &wanted_active) {
                (Some(_), Some(name)) => {
                    logger.error(&format!(
                        "the Sieve script \"{name}\" was created but could not be activated: {e}"
                    ));
                    counts.failed += 1;
                }
                _ => logger.warn(&format!("SieveScript activation failed: {e}")),
            }
        }
    }

    let local_names: HashSet<String> = locals.iter().filter_map(|(_, n, _, _)| n.clone()).collect();
    let mut prune_candidates: Vec<String> = target_by_name
        .iter()
        .filter(|(name, _)| !local_names.contains(*name))
        .map(|(_, id)| id.clone())
        .collect();
    prune_candidates.sort();

    Ok(Plan {
        prune_candidates,
        active_sieve_target: active_target,
    })
}

/// The target's `sieveExtensions`, from its Sieve account capability.
fn target_sieve_extensions(net: &Net) -> Vec<String> {
    net.session
        .account_capabilities(&net.account)
        .and_then(|caps| caps.get("urn:ietf:params:jmap:sieve"))
        .and_then(|c| c.get("sieveExtensions"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The script's bytes with Stalwart's vendor names renamed for an inbuxa
/// target, or `None` when it needs no change. Each rename is logged.
fn renamed_script(
    uploader: &Uploader<'_>,
    blob_local: i64,
    label: &str,
    logger: &Logger,
) -> Result<Option<Vec<u8>>, Error> {
    let bytes = uploader.bytes(blob_local).map_err(Error::from)?;
    Ok(sieve_names::rewrite(&bytes).map(|(out, renamed)| {
        for old in &renamed {
            let new = old.replacen("vnd.stalwart.", "vnd.inbuxa.", 1);
            if logger.enabled(LEVEL_DEFAULT) {
                eprintln!("export: SieveScript {label}: renamed {old} to {new}");
            }
        }
        out
    }))
}
