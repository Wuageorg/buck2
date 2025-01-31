/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::Context as _;
use async_trait::async_trait;
use buck2_common::dice::file_ops::FileChangeTracker;
use buck2_common::ignores::ignore_set::IgnoreSet;
use buck2_common::legacy_configs::key::BuckconfigKeyRef;
use buck2_common::legacy_configs::LegacyBuckConfig;
use buck2_core::cells::name::CellName;
use buck2_core::cells::CellResolver;
use buck2_core::fs::paths::abs_norm_path::AbsNormPath;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::rollout_percentage::RolloutPercentage;
use buck2_events::dispatch::span_async;
use buck2_util::process::async_background_command;
use dice::DiceTransactionUpdater;
use tracing::info;
use tracing::warn;
use watchman_client::expr::Expr;
use watchman_client::prelude::Connector;
use watchman_client::prelude::FileType;

use crate::file_watcher::FileWatcher;
use crate::mergebase::Mergebase;
use crate::stats::FileWatcherStats;
use crate::watchman::core::SyncableQuery;
use crate::watchman::core::SyncableQueryProcessor;
use crate::watchman::core::WatchmanEvent;
use crate::watchman::core::WatchmanEventType;
use crate::watchman::core::WatchmanKind;

struct WatchmanQueryProcessor {
    // FIXME(JakobDegen): Storing these values statically is completely broken. See
    // `tests/e2e/cells/test_file_watcher_resolution:test_changing_cell_location_bug` for a repro of
    // a bug.
    cells: CellResolver,
    ignore_specs: HashMap<CellName, IgnoreSet>,
    retain_dep_files_on_watchman_fresh_instance: bool,
    report_global_rev: bool,
    last_mergebase: Option<String>,
    last_mergebase_global_rev: Option<u64>,
}

/// Used in process_one_change
#[derive(Debug)]
enum ChangeEvent<'a> {
    Watchman(&'a WatchmanEvent),
    SyntheticDirectoryChange,
}

impl WatchmanQueryProcessor {
    async fn process_events_impl(
        &self,
        mut ctx: DiceTransactionUpdater,
        events: Vec<WatchmanEvent>,
        watchman_version: Option<String>,
    ) -> anyhow::Result<(buck2_data::FileWatcherStats, DiceTransactionUpdater)> {
        let mut handler = FileChangeTracker::new();
        let mut stats = FileWatcherStats::new(
            events.len(),
            self.last_mergebase.as_deref(),
            self.last_mergebase_global_rev,
            watchman_version,
        );

        for ev in events {
            // If the path is invalid, then walk up all the way until you find a valid dir to
            // invalidate listings. We don't need to invalidate the file itself, as we can't
            // read invalid files.

            let (path, event) = match ProjectRelativePath::new(&ev.path) {
                Ok(path) => (path, ChangeEvent::Watchman(&ev)),
                Err(_) => {
                    // If we error out here then we might miss other changes. This seems like
                    // it shouldn't happen, since the empty path should always be a valid path.
                    let path = find_first_valid_parent(&ev.path)
                        .with_context(|| {
                            format!("Invalid path had no valid parent: `{}`", ev.path.display())
                        })
                        .unwrap();

                    (path, ChangeEvent::SyntheticDirectoryChange)
                }
            };

            self.process_one_change(path, event, &mut handler, &mut stats)?;
        }

        let stats = stats.finish();
        handler.write_to_dice(&mut ctx)?;

        Ok((stats, ctx))
    }

    fn process_one_change(
        &self,
        path: &ProjectRelativePath,
        ev: ChangeEvent<'_>,
        handler: &mut FileChangeTracker,
        stats: &mut FileWatcherStats,
    ) -> anyhow::Result<()> {
        let cell_path = self.cells.get_cell_path(path)?;

        let ignore = self
            .ignore_specs
            .get(&cell_path.cell())
            .map_or(false, |ignore| ignore.is_match(cell_path.path()));

        info!("Watchman: {:?} (ignore = {})", ev, ignore);

        if ignore {
            stats.add_ignored(1);
        } else {
            let cell_path_str = cell_path.to_string();
            let log_kind;
            let log_event;

            match ev {
                ChangeEvent::Watchman(ev) => {
                    match (&ev.kind, &ev.event) {
                        (WatchmanKind::File, typ) => {
                            log_kind = buck2_data::FileWatcherKind::File;
                            match typ {
                                WatchmanEventType::Modify => {
                                    handler.file_changed(cell_path);
                                    log_event = buck2_data::FileWatcherEventType::Modify;
                                }
                                WatchmanEventType::Create => {
                                    handler.file_added(cell_path);
                                    log_event = buck2_data::FileWatcherEventType::Create;
                                }
                                WatchmanEventType::Delete => {
                                    handler.file_removed(cell_path);
                                    log_event = buck2_data::FileWatcherEventType::Delete;
                                }
                            }
                        }
                        (WatchmanKind::Directory, typ) => {
                            log_kind = buck2_data::FileWatcherKind::Directory;
                            match typ {
                                WatchmanEventType::Modify => {
                                    // We can safely ignore this, as it corresponds to files being added or removed,
                                    // but there are always file add/remove notifications sent too.
                                    // See https://fb.workplace.com/groups/watchman.users/permalink/2858842194433249
                                    return Ok(());
                                }
                                WatchmanEventType::Create => {
                                    handler.dir_added(cell_path);
                                    log_event = buck2_data::FileWatcherEventType::Create;
                                }
                                WatchmanEventType::Delete => {
                                    handler.dir_removed(cell_path);
                                    log_event = buck2_data::FileWatcherEventType::Delete;
                                }
                            }
                        }
                        (WatchmanKind::Symlink, typ) => {
                            log_kind = buck2_data::FileWatcherKind::Symlink;
                            match typ {
                                WatchmanEventType::Modify => {
                                    handler.file_changed(cell_path);
                                    log_event = buck2_data::FileWatcherEventType::Modify;
                                }
                                WatchmanEventType::Create => {
                                    warn!(
                                        "New symlink detected (source symlinks are not supported): {}",
                                        cell_path
                                    );
                                    handler.file_added(cell_path);
                                    log_event = buck2_data::FileWatcherEventType::Create;
                                }
                                WatchmanEventType::Delete => {
                                    handler.file_removed(cell_path);
                                    log_event = buck2_data::FileWatcherEventType::Delete;
                                }
                            }
                        }
                    }
                }
                ChangeEvent::SyntheticDirectoryChange => {
                    log_kind = buck2_data::FileWatcherKind::Directory;
                    log_event = buck2_data::FileWatcherEventType::Modify;
                    handler.dir_changed(cell_path);
                }
            };

            stats.add(cell_path_str, log_event, log_kind);
        }

        Ok(())
    }
}

fn find_first_valid_parent(mut path: &Path) -> Option<&ProjectRelativePath> {
    loop {
        path = path.parent()?;

        match ProjectRelativePath::new(path) {
            Ok(path) => return Some(path),
            Err(_) => {}
        }
    }
}

async fn try_fetch_global_rev(hash: &str) -> Option<u64> {
    // There's a variety of ways in which this might go wrong: `PATH` is messed up, this somehow got
    // turned on in a non-`hg` repo, etc. To make sure we don't fail any builds from this, ignore
    // all errors.
    let command = async_background_command("hg")
        .args(["log", "-r", hash, "-T", "{get(extras, \"global_rev\")}"])
        .env("HPGPLAIN", "1")
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_millis(500), command)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout.trim().parse::<u64>().ok()
}

#[async_trait]
impl SyncableQueryProcessor for WatchmanQueryProcessor {
    type Output = buck2_data::FileWatcherStats;
    type Payload = DiceTransactionUpdater;

    async fn process_events(
        &mut self,
        dice: DiceTransactionUpdater,
        events: Vec<WatchmanEvent>,
        mergebase: &Option<String>,
        watchman_version: Option<String>,
    ) -> anyhow::Result<(Self::Output, DiceTransactionUpdater)> {
        self.last_mergebase = mergebase.clone();
        self.process_events_impl(dice, events, watchman_version)
            .await
    }

    async fn on_fresh_instance(
        &mut self,
        ctx: DiceTransactionUpdater,
        mergebase: &Option<String>,
        watchman_version: Option<String>,
    ) -> anyhow::Result<(Self::Output, DiceTransactionUpdater)> {
        let has_new_mergebase = self.last_mergebase.as_ref() != mergebase.as_ref();

        let clear_dep_files =
            has_new_mergebase || !self.retain_dep_files_on_watchman_fresh_instance;

        // We'll clear dep files if we're configured to do so on all fresh instances. Otherwise,
        // we'll drop them if the mergebase has changed, which means our dep files are likely
        // irrelevant.
        //
        // This is imperfect. If the user rebased from yesterday's stable to today's stable, then
        // flushing is the right thing to do. In contrast, if they rebased from X to X's parent,
        // then it probably isn't. The consequences of flushing in the latter case aren't as bad
        // (where we'll rebuild things our dep files *could* have avoided) as not flushing in the
        // former (where we'll fetch loads of dep files that all miss), so we err on the side of
        // being safe and drop them when the mergebase changes.
        if clear_dep_files {
            crate::dep_files::flush_dep_files();
        }

        self.last_mergebase = mergebase.clone();

        if let Some(hash) = self.last_mergebase.as_ref() {
            if self.report_global_rev {
                self.last_mergebase_global_rev = try_fetch_global_rev(hash).await;
            }
        }

        // TODO(cjhopman): could probably get away with just invalidating all fs things, but that's not supported.
        // Dropping the entire DICE map can be somewhat computationally expensive as there
        // are a lot of destructors to run. On the other hand, we don't have to wait for
        // it. So, we just send it off to its own thread.
        let ctx = ctx.unstable_take();

        Ok((
            buck2_data::FileWatcherStats {
                fresh_instance: true,
                branched_from_revision: mergebase.clone(),
                branched_from_global_rev: self.last_mergebase_global_rev,
                incomplete_events_reason: Some("Fresh instance".to_owned()),
                watchman_version,
                fresh_instance_data: Some(buck2_data::FreshInstance {
                    new_mergebase: has_new_mergebase,
                    cleared_dice: true,
                    cleared_dep_files: clear_dep_files,
                }),
                ..Default::default()
            },
            ctx,
        ))
    }
}

#[derive(Allocative)]
pub(crate) struct WatchmanFileWatcher {
    #[allocative(skip)]
    query: SyncableQuery<buck2_data::FileWatcherStats, DiceTransactionUpdater>,
}

/// The watchman query is constructed once on daemon startup. It is an unfiltered watchman query
/// over the entire project (though, like all watchman queries, still filtered by the
/// .watchmanconfig itself). Before any new computation request is started, it will be synced to
/// ensure that any recent changes are flushed and visible to the computation.
impl WatchmanFileWatcher {
    pub(crate) fn new(
        project_root: &AbsNormPath,
        root_config: &LegacyBuckConfig,
        cells: CellResolver,
        ignore_specs: HashMap<CellName, IgnoreSet>,
    ) -> anyhow::Result<Self> {
        let watchman_merge_base = root_config
            .get(BuckconfigKeyRef {
                section: "project",
                property: "watchman_merge_base",
            })
            .map(|s| s.to_owned());

        let retain_dep_files_on_watchman_fresh_instance = root_config
            .parse::<RolloutPercentage>(BuckconfigKeyRef {
                section: "buck2",
                property: "retain_dep_files_on_watchman_fresh_instance",
            })?
            .unwrap_or_else(RolloutPercentage::always)
            .roll();

        let report_global_rev = root_config
            .parse::<bool>(BuckconfigKeyRef {
                section: "buck2",
                property: "watchman_report_global_rev",
            })?
            .unwrap_or(false);

        let query = SyncableQuery::new(
            Connector::new(),
            project_root,
            Expr::Any(vec![
                Expr::FileType(FileType::Regular),
                Expr::FileType(FileType::Directory),
                Expr::FileType(FileType::Symlink),
            ]),
            Box::new(WatchmanQueryProcessor {
                cells,
                ignore_specs,
                retain_dep_files_on_watchman_fresh_instance,
                report_global_rev,
                last_mergebase: None,
                last_mergebase_global_rev: None,
            }),
            watchman_merge_base,
        )?;

        Ok(Self { query })
    }
}

#[async_trait]
impl FileWatcher for WatchmanFileWatcher {
    async fn sync(
        &self,
        dice: DiceTransactionUpdater,
    ) -> anyhow::Result<(DiceTransactionUpdater, Mergebase)> {
        span_async(
            buck2_data::FileWatcherStart {
                provider: buck2_data::FileWatcherProvider::Watchman as i32,
            },
            async {
                let (stats, res) = match self.query.sync(dice).await {
                    Ok((stats, dice)) => {
                        let mergebase = Mergebase(Arc::new(stats.branched_from_revision.clone()));
                        ((Some(stats)), Ok((dice, mergebase)))
                    }
                    Err(e) => (None, Err(e)),
                };
                (res, buck2_data::FileWatcherEnd { stats })
            },
        )
        .await
    }
}
