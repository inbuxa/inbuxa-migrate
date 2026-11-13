/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod client;
pub mod error;

pub use client::{MessageListing, Pop3Client, Pop3Mode};
pub use error::Pop3Error;
