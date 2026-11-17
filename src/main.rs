/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::{env, process};

use inbuxa_migrate::cli::{Action, Cli};
use inbuxa_migrate::console::{ColorChoice, Console};
use inbuxa_migrate::error::ExitStatus;

fn main() {
    let choice = ColorChoice::from_args(env::args_os());
    let console = Console::detect(choice);
    let status = match Cli::try_parse_styled(choice, console) {
        Ok(cli) => match cli.resolve() {
            Ok(action) => action.with_console(console).run(),
            Err(err) => Action::fail(console, &err),
        },
        Err(err) => {
            let _ = err.print();
            if err.use_stderr() {
                ExitStatus::Usage
            } else {
                ExitStatus::Ok
            }
        }
    };
    process::exit(status.code());
}
