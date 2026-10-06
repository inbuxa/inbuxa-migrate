/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::imap::client::{ConnectMode, ImapClient};
use crate::imap::command;
use crate::imap::error::ImapError;
use crate::imap::response::Untagged;
use crate::imap::retry::{BackoffState, Disposition, RetryPolicy, classify};
use crate::imap::transport::Connector;
use crate::logging::Logger;

use super::coordinator::{Endpoint, ImapAuth, authenticate_client};
use super::fetch::FetchAttrs;

pub const HARD_CAP: usize = 8;

/// A job or event from an older folder generation than the one the
/// coordinator is working on belongs to a folder it has already given up
/// on. Workers skip such jobs without fetching, and the coordinator drops
/// such events, so nothing from one folder can be filed into the next.
pub struct FetchJob {
    pub generation: u64,
    pub folder: String,
    pub wire_name: String,
    pub uidvalidity: u32,
    pub uids: Vec<u32>,
}

pub enum FetchEvent {
    Item {
        generation: u64,
        folder: String,
        uidvalidity: u32,
        attrs: FetchAttrs,
    },
    ChunkDone {
        generation: u64,
        folder: String,
        uidvalidity: u32,
        uids_requested: Vec<u32>,
        outcome: Result<(), ImapError>,
    },
}

pub struct WorkerArgs {
    pub connector: Arc<Connector>,
    pub endpoint: Arc<Endpoint>,
    pub mode: ConnectMode,
    pub auth: ImapAuth,
    pub compress: bool,
    pub policy: RetryPolicy,
    pub backoff: BackoffState,
    pub logger: Logger,
}

pub struct WorkerPool {
    job_tx: Sender<FetchJob>,
    event_rx: Receiver<FetchEvent>,
    handles: Vec<thread::JoinHandle<()>>,
    cancel_below: Arc<AtomicU64>,
}

impl WorkerPool {
    pub fn start(args: WorkerArgs, pool_size: usize) -> Result<WorkerPool, ImapError> {
        let size = pool_size.clamp(1, HARD_CAP);
        let (job_tx, job_rx) = unbounded::<FetchJob>();
        // Jobs are only lists of UIDs, but each event carries a whole message:
        // bounding the events stops fast workers from running ahead of the
        // single archive writer, so memory holds at most a couple of messages
        // per worker rather than whole folders.
        let (event_tx, event_rx) = bounded::<FetchEvent>(size * 2);
        let mut handles = Vec::with_capacity(size);
        let args = Arc::new(args);
        let cancel_below = Arc::new(AtomicU64::new(0));

        for _ in 0..size {
            let args = args.clone();
            let job_rx = job_rx.clone();
            let event_tx = event_tx.clone();
            let cancel_below = cancel_below.clone();
            let handle = thread::spawn(move || {
                worker_loop(args, job_rx, event_tx, cancel_below);
            });
            handles.push(handle);
        }

        Ok(WorkerPool {
            job_tx,
            event_rx,
            handles,
            cancel_below,
        })
    }

    /// Jobs of any generation below `generation` are skipped from now on:
    /// called when the coordinator moves to a new folder, so work still
    /// queued for one it abandoned is not fetched.
    pub fn cancel_before(&self, generation: u64) {
        self.cancel_below.fetch_max(generation, Ordering::SeqCst);
    }

    pub fn submit(&self, job: FetchJob) {
        let _ = self.job_tx.send(job);
    }

    pub fn recv(&self) -> Result<FetchEvent, crossbeam_channel::RecvError> {
        self.event_rx.recv()
    }

    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<FetchEvent, crossbeam_channel::RecvTimeoutError> {
        self.event_rx.recv_timeout(timeout)
    }

    /// Stops the workers. Events still in flight are drained and dropped
    /// first: a worker blocked handing over an event the coordinator will
    /// never read (after a folder was abandoned) would otherwise never
    /// finish, and joining it would hang.
    pub fn shutdown(self) {
        self.cancel_before(u64::MAX);
        drop(self.job_tx);
        while self.event_rx.recv().is_ok() {}
        for h in self.handles {
            let _ = h.join();
        }
    }
}

fn worker_loop(
    args: Arc<WorkerArgs>,
    job_rx: Receiver<FetchJob>,
    event_tx: Sender<FetchEvent>,
    cancel_below: Arc<AtomicU64>,
) {
    let mut client: Option<ImapClient> = None;
    let mut current_folder: Option<String> = None;
    while let Ok(job) = job_rx.recv() {
        let job_gen = job.generation;
        let job_folder = job.folder.clone();
        let job_uv = job.uidvalidity;
        let job_uids = job.uids.clone();
        if job_gen < cancel_below.load(Ordering::SeqCst) {
            let _ = event_tx.send(FetchEvent::ChunkDone {
                generation: job_gen,
                folder: job_folder,
                uidvalidity: job_uv,
                uids_requested: job_uids,
                outcome: Err(ImapError::Protocol("cancelled: folder abandoned".into())),
            });
            continue;
        }
        let event_tx_for_job = event_tx.clone();
        let outcome = match catch_unwind(AssertUnwindSafe(|| {
            run_job_with_retry(
                &args,
                &mut client,
                &mut current_folder,
                &job,
                &event_tx_for_job,
            )
        })) {
            Ok(r) => r,
            Err(_) => {
                client = None;
                current_folder = None;
                Err(ImapError::Protocol("worker thread panicked".into()))
            }
        };
        let _ = event_tx.send(FetchEvent::ChunkDone {
            generation: job_gen,
            folder: job_folder,
            uidvalidity: job_uv,
            uids_requested: job_uids,
            outcome,
        });
    }
}

fn run_job_with_retry(
    args: &WorkerArgs,
    client_slot: &mut Option<ImapClient>,
    current_folder: &mut Option<String>,
    job: &FetchJob,
    event_tx: &Sender<FetchEvent>,
) -> Result<(), ImapError> {
    let mut transient_attempts: u32 = 0;
    let mut transport_attempts: u32 = 0;
    loop {
        if client_slot.is_none() {
            match connect_and_auth(args) {
                Ok(c) => {
                    *client_slot = Some(c);
                    *current_folder = None;
                }
                Err(e) => {
                    let disp = classify(&e);
                    if disp == Disposition::TransportDrop
                        && transport_attempts < args.policy.max_retries
                    {
                        transport_attempts += 1;
                        std::thread::sleep(args.backoff.transport_delay(transport_attempts));
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        let Some(client) = client_slot.as_mut() else {
            return Err(ImapError::Protocol(
                "worker pool: client slot empty after connect".into(),
            ));
        };
        match run_one_job(client, current_folder, job, event_tx) {
            Ok(()) => {
                args.backoff.reset();
                return Ok(());
            }
            Err(e) => match classify(&e) {
                Disposition::TransportDrop => {
                    *client_slot = None;
                    *current_folder = None;
                    if transport_attempts >= args.policy.max_retries {
                        return Err(e);
                    }
                    transport_attempts += 1;
                    std::thread::sleep(args.backoff.transport_delay(transport_attempts));
                }
                Disposition::Transient => {
                    if transient_attempts >= args.policy.max_retries {
                        return Err(e);
                    }
                    transient_attempts += 1;
                    std::thread::sleep(args.backoff.next_shared_delay());
                }
                _ => return Err(e),
            },
        }
    }
}

fn connect_and_auth(args: &WorkerArgs) -> Result<ImapClient, ImapError> {
    let mut client = ImapClient::connect(
        &args.connector,
        &args.endpoint.host,
        args.endpoint.port,
        args.mode,
        args.logger,
    )?;
    authenticate_client(&mut client, &args.auth)
        .map_err(|e| ImapError::AuthFailed(e.to_string()))?;
    let _ = client.refresh_capabilities();
    if args.compress && client.has_capability("COMPRESS=DEFLATE") {
        client.compress_deflate()?;
    }
    if client.has_capability("ENABLE") && client.has_capability("UTF8=ACCEPT") {
        let _ = client.enable(&["UTF8=ACCEPT"]);
    }
    Ok(client)
}

fn run_one_job(
    client: &mut ImapClient,
    current_folder: &mut Option<String>,
    job: &FetchJob,
    event_tx: &Sender<FetchEvent>,
) -> Result<(), ImapError> {
    if current_folder.as_deref() != Some(job.folder.as_str()) {
        client.run_collect(&command::select(&job.wire_name))?;
        *current_folder = Some(job.folder.clone());
    }
    let set = command::format_uid_set(&job.uids, true);
    let generation = job.generation;
    let folder = job.folder.clone();
    let uv = job.uidvalidity;
    client.run_streamed(
        &command::uid_fetch(
            &set,
            &["UID", "FLAGS", "INTERNALDATE", "RFC822.SIZE", "BODY.PEEK[]"],
        ),
        |u| {
            if let Untagged::Fetch { .. } = &u
                && let Some(attrs) = super::fetch::extract(&u)
            {
                let _ = event_tx.send(FetchEvent::Item {
                    generation,
                    folder: folder.clone(),
                    uidvalidity: uv,
                    attrs,
                });
            }
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn unreachable_args() -> WorkerArgs {
        WorkerArgs {
            connector: Arc::new(Connector::new(false).expect("connector")),
            endpoint: Arc::new(Endpoint {
                host: "127.0.0.1".to_owned(),
                port: 1,
                implicit_tls: false,
            }),
            mode: ConnectMode::Plain,
            auth: ImapAuth::Basic {
                user: "u".to_owned(),
                password: "p".to_owned(),
            },
            compress: false,
            policy: RetryPolicy::new(0),
            backoff: BackoffState::new(),
            logger: Logger::from_flags(false, 0),
        }
    }

    #[test]
    fn a_job_from_an_abandoned_generation_is_skipped_without_fetching() {
        // Port 1 refuses connections: a job that were actually run would come
        // back as a connection error, not as a cancellation.
        let pool = WorkerPool::start(unreachable_args(), 1).expect("pool");
        pool.cancel_before(2);
        pool.submit(FetchJob {
            generation: 1,
            folder: "Old".to_owned(),
            wire_name: "Old".to_owned(),
            uidvalidity: 7,
            uids: vec![1, 2, 3],
        });
        match pool.recv_timeout(Duration::from_secs(5)).expect("event") {
            FetchEvent::ChunkDone {
                generation,
                folder,
                uids_requested,
                outcome,
                ..
            } => {
                assert_eq!(generation, 1);
                assert_eq!(folder, "Old");
                assert_eq!(uids_requested, vec![1, 2, 3]);
                let err = outcome.expect_err("cancelled");
                assert!(err.to_string().contains("cancelled"), "{err}");
            }
            FetchEvent::Item { .. } => panic!("a cancelled job fetched something"),
        }
        pool.shutdown();
    }

    #[test]
    fn shutdown_returns_with_work_still_queued() {
        let pool = WorkerPool::start(unreachable_args(), 2).expect("pool");
        for g in 0..20u64 {
            pool.submit(FetchJob {
                generation: g,
                folder: format!("F{g}"),
                wire_name: format!("F{g}"),
                uidvalidity: 1,
                uids: vec![1],
            });
        }
        pool.shutdown();
    }
}
