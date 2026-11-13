/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use clap::{CommandFactory, Parser};

use inbuxa_migrate::cli::{Action, Cli};
use inbuxa_migrate::error::Error;
use inbuxa_migrate::sync::{self, RunOutcome, Summary};
use inbuxa_migrate::{
    export_directory, export_files, guide, import_directory, inspect, migrate, reconcile,
};

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return if err.use_stderr() { 1 } else { 0 };
        }
    };

    let action = match cli.resolve() {
        Ok(action) => action,
        Err(err) => return fail(&err),
    };

    // Completions and the man page are generated from the command itself and
    // touch no network, archive or logger, so they come before everything else.
    match &action {
        Action::Completions(shell) => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(*shell, &mut cmd, name, &mut std::io::stdout());
            return 0;
        }
        Action::Man => {
            return match clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout()) {
                Ok(()) => 0,
                Err(err) => fail(&Error::from(err)),
            };
        }
        _ => {}
    }

    let do_estimate = action.estimate_available();
    if action.estimate_requested() && !do_estimate {
        eprintln!(
            "note: --estimate isn't available for this source yet; running --dry-run instead \
             (its plan is below). Estimates work for imap, maildir, caldav, carddav, exchange (EWS), takeout, jmap and Graph today."
        );
    }
    let mut quiet_report = false;
    let mut partial = "some objects failed; the archive is consistent and resumable";
    let (outcome, logger) = match action {
        Action::Import(common, config) => {
            let logger = common.logger;
            (sync::import_jmap::run_reporting(common, config), logger)
        }
        Action::ImportImap(common, config) => {
            let logger = common.logger;
            (sync::import_imap::run_reporting(common, config), logger)
        }
        Action::ImportPop3(common, config) => {
            let logger = common.logger;
            (sync::import_pop3::run_reporting(common, config), logger)
        }
        Action::ImportDav(common, config) => {
            let logger = common.logger;
            (sync::import_dav::run_reporting(common, config), logger)
        }
        Action::ImportManageSieve(common, config) => {
            let logger = common.logger;
            (
                RunOutcome::from_result(sync::import_managesieve::run(common, config)),
                logger,
            )
        }
        Action::ImportMaildir(common, config) => {
            let logger = common.logger;
            (sync::import_maildir::run_reporting(common, config), logger)
        }
        Action::ImportTakeout(common, config) => {
            let logger = common.logger;
            (sync::import_takeout::run_reporting(common, config), logger)
        }
        Action::ImportFiles(common, config) => {
            let logger = common.logger;
            (sync::import_files::run_reporting(common, config), logger)
        }
        Action::ImportExchangeEws(common, config) => {
            let logger = common.logger;
            (
                RunOutcome::from_result(sync::import_exchange_ews::run(common, config)),
                logger,
            )
        }
        Action::ImportExchangeGraph(common, config) => {
            let logger = common.logger;
            (
                RunOutcome::from_result(sync::import_exchange_graph::run(common, config)),
                logger,
            )
        }
        Action::Export(common, config) => {
            let logger = common.logger;
            // A dry run prints its own plan; the counts below would repeat it.
            quiet_report = common.dry_run;
            (
                RunOutcome::from_result(sync::export::run(common, config)),
                logger,
            )
        }
        Action::ExportFiles(config) => {
            let logger = config.logger;
            (RunOutcome::from_result(export_files::run(config)), logger)
        }
        Action::ExportDav(common, config) => {
            let logger = common.logger;
            (
                RunOutcome::from_result(sync::export_dav::run(common, config)),
                logger,
            )
        }
        Action::ExportManageSieve(common, config) => {
            let logger = common.logger;
            (
                RunOutcome::from_result(sync::export_managesieve::run(common, config)),
                logger,
            )
        }
        Action::ExportDirectory(config) => {
            let logger = config.logger;
            (
                RunOutcome::from_result(export_directory::run(config)),
                logger,
            )
        }
        Action::ImportDirectory(config) => {
            let logger = config.logger;
            partial = "some objects were not created; fix what the warnings name and run \
                       again, with a new --passwords file";
            (
                RunOutcome::from_result(import_directory::run(config)),
                logger,
            )
        }
        Action::ExportImap(common, config) => {
            let logger = common.logger;
            (
                RunOutcome::from_result(sync::export_imap::run(common, config)),
                logger,
            )
        }
        Action::Migrate(config) => {
            let json = config.progress_json;
            return match migrate::run(config) {
                Ok(outcome) => {
                    if !json {
                        let passes = if outcome.passes > 1 {
                            format!(" after {} passes", outcome.passes)
                        } else {
                            String::new()
                        };
                        println!(
                            "accounts: {} done, {} not finished{passes}; report: {}",
                            outcome.done,
                            outcome.unfinished,
                            outcome.report.display()
                        );
                    }
                    if outcome.unfinished > 0 {
                        eprintln!(
                            "error: some accounts did not finish; the report says why. Fix that \
                             and run the same command again: it resumes where each stopped"
                        );
                        5
                    } else {
                        0
                    }
                }
                Err(err) => fail(&err),
            };
        }
        Action::Guide(config) => {
            return match guide::run(config) {
                Ok(()) => 0,
                Err(err) => fail(&err),
            };
        }
        Action::Plan(config) => {
            return match guide::write_plans(config) {
                Ok(()) => 0,
                Err(err) => fail(&err),
            };
        }
        Action::Inspect(config) => {
            return match inspect::run(config) {
                Ok(()) => 0,
                Err(err) => fail(&err),
            };
        }
        Action::Reconcile(config) => {
            return match reconcile::run(config) {
                // A sound archive exits 0; one with integrity problems exits 8,
                // past the 1-7 the other errors use, so a script can tell a
                // reconciliation failure from any other.
                Ok(true) => 0,
                Ok(false) => 8,
                Err(err) => fail(&err),
            };
        }
        // Handled before the match, above.
        Action::Completions(_) | Action::Man => unreachable!(),
    };

    if do_estimate {
        if outcome.error.is_none() {
            print!("{}", sync::estimate::render(&outcome.summary));
        }
    } else if !quiet_report {
        report(&outcome.summary);
    }
    match outcome.error {
        Some(err) => fail(&err),
        None => {
            if outcome.summary.any_failed() {
                logger.error(partial);
                5
            } else {
                0
            }
        }
    }
}

fn fail(err: &Error) -> i32 {
    eprintln!("error: {err}");
    err.exit_code()
}

fn report(summary: &Summary) {
    for (type_name, counts) in &summary.per_type {
        println!(
            "{type_name}: created={} fetched={} updated={} deleted={} skipped={} failed={}",
            counts.created,
            counts.fetched,
            counts.updated,
            counts.deleted,
            counts.skipped,
            counts.failed
        );
    }
}
