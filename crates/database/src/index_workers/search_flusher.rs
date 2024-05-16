use std::{
    collections::Bound,
    future,
    iter,
    marker::PhantomData,
    num::NonZeroU32,
    path::PathBuf,
    sync::Arc,
};

use anyhow::Context;
use common::{
    knobs::{
        DATABASE_WORKERS_MAX_CHECKPOINT_AGE,
        DEFAULT_DOCUMENTS_PAGE_SIZE,
        VECTOR_INDEX_WORKER_PAGE_SIZE,
    },
    persistence::TimestampRange,
    runtime::{
        new_rate_limiter,
        Runtime,
    },
    types::{
        IndexId,
        RepeatableTimestamp,
        TabletIndexName,
    },
};
use futures::{
    channel::oneshot,
    StreamExt,
    TryStreamExt,
};
use governor::Quota;
use keybroker::Identity;
use storage::Storage;
use sync_types::Timestamp;
use tempfile::TempDir;
use value::{
    ResolvedDocumentId,
    TableIdentifier,
};

use crate::{
    bootstrap_model::index_workers::IndexWorkerMetadataModel,
    index_workers::{
        index_meta::{
            SearchIndex,
            SearchIndexConfig,
            SearchIndexConfigParser,
            SearchOnDiskState,
            SearchSnapshot,
            SegmentStatistics,
            SnapshotData,
        },
        BuildReason,
        MultiSegmentBackfillResult,
    },
    Database,
    IndexModel,
    Token,
};

pub struct SearchFlusher<RT: Runtime, T: SearchIndexConfigParser> {
    runtime: RT,
    database: Database<RT>,
    storage: Arc<dyn Storage>,
    index_size_soft_limit: usize,
    full_scan_threshold_kb: usize,
    // Used for constraining the part size of incremental multi segment builds
    incremental_multipart_threshold_bytes: usize,
    _config: PhantomData<T>,
}

impl<RT: Runtime, T: SearchIndexConfigParser + 'static> SearchFlusher<RT, T> {
    pub fn new(
        runtime: RT,
        database: Database<RT>,
        storage: Arc<dyn Storage>,
        index_size_soft_limit: usize,
        full_scan_threshold_kb: usize,
        incremental_multipart_threshold_bytes: usize,
    ) -> Self {
        Self {
            runtime,
            database,
            storage,
            index_size_soft_limit,
            full_scan_threshold_kb,
            incremental_multipart_threshold_bytes,
            _config: PhantomData,
        }
    }

    /// Compute the set of indexes that need to be backfilled.
    pub async fn needs_backfill(&self) -> anyhow::Result<(Vec<IndexBuild<T::IndexType>>, Token)> {
        let mut to_build = vec![];

        let mut tx = self.database.begin(Identity::system()).await?;
        let step_ts = tx.begin_timestamp();

        let snapshot = self.database.snapshot(step_ts)?;

        let ready_index_sizes = T::IndexType::get_index_sizes(snapshot)?;

        for index_doc in IndexModel::new(&mut tx).get_all_indexes().await? {
            let (index_id, index_metadata) = index_doc.into_id_and_value();
            let Some(config) = T::get_config(index_metadata.config) else {
                continue;
            };
            let name = index_metadata.name;

            let needs_backfill = match &config.on_disk_state {
                SearchOnDiskState::Backfilling(_) => Some(BuildReason::Backfilling),
                SearchOnDiskState::SnapshottedAt(snapshot)
                | SearchOnDiskState::Backfilled(snapshot)
                    if !T::IndexType::is_version_current(snapshot) =>
                {
                    Some(BuildReason::VersionMismatch)
                },
                SearchOnDiskState::SnapshottedAt(SearchSnapshot { ts, .. })
                | SearchOnDiskState::Backfilled(SearchSnapshot { ts, .. }) => {
                    let ts = IndexWorkerMetadataModel::new(&mut tx)
                        .get_fast_forward_ts(*ts, index_id.internal_id())
                        .await?;

                    let index_size = ready_index_sizes
                        .get(&index_id.internal_id())
                        .cloned()
                        .unwrap_or(0);

                    anyhow::ensure!(ts <= *step_ts);

                    let index_age = *step_ts - ts;
                    let too_old = (index_age >= *DATABASE_WORKERS_MAX_CHECKPOINT_AGE
                        && index_size > 0)
                        .then_some(BuildReason::TooOld);
                    if too_old.is_some() {
                        tracing::info!(
                            "Non-empty index is too old, age: {:?}, size: {index_size}",
                            index_age
                        );
                    }
                    let too_large =
                        (index_size > self.index_size_soft_limit).then_some(BuildReason::TooLarge);
                    // Order matters! Too large is more urgent than too old.
                    too_large.or(too_old)
                },
            };
            if let Some(build_reason) = needs_backfill {
                tracing::info!("Queueing vector index for rebuild: {name:?} ({build_reason:?})");
                let table_id = name.table();
                let by_id_metadata = IndexModel::new(&mut tx)
                    .by_id_index_metadata(*table_id)
                    .await?;
                let job = IndexBuild {
                    index_name: name.clone(),
                    index_id: index_id.internal_id(),
                    by_id: by_id_metadata.id().internal_id(),
                    index_config: config,
                    metadata_id: index_id,
                    build_reason,
                };
                to_build.push(job);
            }
        }
        Ok((to_build, tx.into_token()?))
    }

    pub async fn build_multipart_segment(
        &self,
        job: &IndexBuild<T::IndexType>,
    ) -> anyhow::Result<IndexBuildResult<T::IndexType>> {
        let index_path = TempDir::new()?;
        let mut tx = self.database.begin(Identity::system()).await?;
        let table_id = tx.table_mapping().inject_table_number()(*job.index_name.table())?;
        let mut new_ts = tx.begin_timestamp();
        let (previous_segments, build_type) = match job.index_config.on_disk_state {
            SearchOnDiskState::Backfilling(ref backfill_state) => {
                let backfill_snapshot_ts = backfill_state
                    .backfill_snapshot_ts
                    .map(|ts| new_ts.prior_ts(ts))
                    .transpose()?
                    .unwrap_or(new_ts);
                // For backfilling indexes, the snapshot timestamp we return is the backfill
                // snapshot timestamp
                new_ts = backfill_snapshot_ts;

                let cursor = backfill_state.cursor;

                (
                    backfill_state.segments.clone(),
                    MultipartBuildType::IncrementalComplete {
                        cursor: cursor.map(|cursor| table_id.id(cursor)),
                        backfill_snapshot_ts,
                    },
                )
            },
            SearchOnDiskState::Backfilled(ref snapshot)
            | SearchOnDiskState::SnapshottedAt(ref snapshot) => {
                match snapshot.data {
                    // If we're on an old or unrecognized version, rebuild everything. The formats
                    // are not compatible.
                    SnapshotData::Unknown => (
                        vec![],
                        MultipartBuildType::IncrementalComplete {
                            cursor: None,
                            backfill_snapshot_ts: new_ts,
                        },
                    ),
                    SnapshotData::MultiSegment(ref parts) => {
                        let ts = IndexWorkerMetadataModel::new(&mut tx)
                            .get_fast_forward_ts(snapshot.ts, job.index_id)
                            .await?;
                        (
                            parts.clone(),
                            MultipartBuildType::Partial(new_ts.prior_ts(ts)?),
                        )
                    },
                }
            },
        };

        let MultiSegmentBuildResult {
            new_segment,
            updated_previous_segments,
            backfill_result,
        } = self
            .build_multipart_segment_in_dir(job, &index_path, new_ts, build_type, previous_segments)
            .await?;

        let new_segment = if let Some(new_segment) = new_segment {
            Some(
                T::IndexType::upload_new_segment(&self.runtime, self.storage.clone(), new_segment)
                    .await?,
            )
        } else {
            None
        };
        let new_segment_id = new_segment.as_ref().map(T::IndexType::segment_id);
        let new_segment_stats = new_segment
            .as_ref()
            .map(T::IndexType::statistics)
            .transpose()?;

        let new_and_updated_parts = if let Some(new_segment) = new_segment {
            updated_previous_segments
                .into_iter()
                .chain(iter::once(new_segment))
                .collect()
        } else {
            updated_previous_segments
        };

        let total_stats = new_and_updated_parts
            .iter()
            .map(|segment| {
                let segment_stats = T::IndexType::statistics(segment)?;
                segment_stats.log();
                Ok(segment_stats)
            })
            .reduce(SegmentStatistics::add)
            .transpose()?
            .unwrap_or_default();
        let data = SnapshotData::MultiSegment(new_and_updated_parts);

        Ok(IndexBuildResult {
            snapshot_ts: *new_ts,
            data,
            total_stats,
            new_segment_stats,
            new_segment_id,
            backfill_result,
        })
    }

    async fn build_multipart_segment_in_dir(
        &self,
        job: &IndexBuild<T::IndexType>,
        index_path: &TempDir,
        snapshot_ts: RepeatableTimestamp,
        build_type: MultipartBuildType,
        previous_segments: Vec<<T::IndexType as SearchIndex>::Segment>,
    ) -> anyhow::Result<MultiSegmentBuildResult<T::IndexType>> {
        let database = self.database.clone();

        let (tx, rx) = oneshot::channel();
        let runtime = self.runtime.clone();
        let index_name = job.index_name.clone();
        let index_path = index_path.path().to_owned();
        let storage = self.storage.clone();
        let full_scan_threshold_kb = self.full_scan_threshold_kb;
        let incremental_multipart_threshold_bytes = self.incremental_multipart_threshold_bytes;
        let by_id = job.by_id;
        let rate_limit_pages_per_second = job.build_reason.read_max_pages_per_second();
        let developer_config = job.index_config.developer_config.clone();
        self.runtime.spawn_thread(move || async move {
            let result = Self::build_multipart_segment_on_thread(
                rate_limit_pages_per_second,
                index_name,
                by_id,
                build_type,
                snapshot_ts,
                runtime,
                database,
                developer_config,
                index_path,
                storage,
                previous_segments,
                full_scan_threshold_kb,
                incremental_multipart_threshold_bytes,
            )
            .await;
            _ = tx.send(result);
        });
        rx.await?
    }

    async fn build_multipart_segment_on_thread(
        rate_limit_pages_per_second: NonZeroU32,
        index_name: TabletIndexName,
        by_id: IndexId,
        build_type: MultipartBuildType,
        snapshot_ts: RepeatableTimestamp,
        runtime: RT,
        database: Database<RT>,
        developer_config: <T::IndexType as SearchIndex>::DeveloperConfig,
        index_path: PathBuf,
        storage: Arc<dyn Storage>,
        previous_segments: Vec<<T::IndexType as SearchIndex>::Segment>,
        full_scan_threshold_kb: usize,
        incremental_multipart_threshold_bytes: usize,
    ) -> anyhow::Result<MultiSegmentBuildResult<T::IndexType>> {
        let row_rate_limiter = new_rate_limiter(
            runtime,
            Quota::per_second(
                NonZeroU32::new(*DEFAULT_DOCUMENTS_PAGE_SIZE)
                    .and_then(|val| val.checked_mul(rate_limit_pages_per_second))
                    .context("Invalid row rate limit")?,
            ),
        );
        // Cursor and completion state for MultipartBuildType::IncrementalComplete
        let mut new_cursor = None;
        let mut is_backfill_complete = true;
        let qdrant_schema = T::IndexType::new_schema(&developer_config);

        let (documents, previous_segments) = match build_type {
            MultipartBuildType::Partial(last_ts) => (
                database.load_documents_in_table(
                    *index_name.table(),
                    TimestampRange::new((
                        Bound::Excluded(*last_ts),
                        Bound::Included(*snapshot_ts),
                    ))?,
                    &row_rate_limiter,
                ),
                previous_segments,
            ),
            MultipartBuildType::IncrementalComplete {
                cursor,
                backfill_snapshot_ts,
            } => {
                let documents = database
                    .table_iterator(backfill_snapshot_ts, *VECTOR_INDEX_WORKER_PAGE_SIZE, None)
                    .stream_documents_in_table(*index_name.table(), by_id, cursor)
                    .boxed()
                    .scan(0_u64, |total_size, res| {
                        let updated_cursor = if let Ok((doc, _)) = &res {
                            let size = T::IndexType::estimate_document_size(&qdrant_schema, doc);
                            *total_size += size;
                            Some(doc.id())
                        } else {
                            None
                        };
                        // Conditionally update cursor and proceed with iteration if
                        // we haven't exceeded incremental part size threshold.
                        future::ready(
                            if *total_size <= incremental_multipart_threshold_bytes as u64 {
                                if let Some(updated_cursor) = updated_cursor {
                                    new_cursor = Some(updated_cursor);
                                }
                                Some(res)
                            } else {
                                is_backfill_complete = false;
                                None
                            },
                        )
                    })
                    .map_ok(|(doc, ts)| (ts, doc.id_with_table_id(), Some(doc)))
                    .boxed();
                (documents, previous_segments)
            },
        };

        let mut mutable_previous_segments =
            T::IndexType::download_previous_segments(storage.clone(), previous_segments).await?;

        let new_segment = T::IndexType::build_disk_index(
            &qdrant_schema,
            &index_path,
            documents,
            full_scan_threshold_kb,
            &mut mutable_previous_segments,
        )
        .await?;

        let updated_previous_segments =
            T::IndexType::upload_previous_segments(storage, mutable_previous_segments).await?;

        let index_backfill_result =
            if let MultipartBuildType::IncrementalComplete { .. } = build_type {
                Some(MultiSegmentBackfillResult {
                    new_cursor,
                    is_backfill_complete,
                })
            } else {
                None
            };

        Ok(MultiSegmentBuildResult {
            new_segment,
            updated_previous_segments,
            backfill_result: index_backfill_result,
        })
    }
}

pub struct IndexBuild<T: SearchIndex> {
    pub index_name: TabletIndexName,
    pub index_id: IndexId,
    pub by_id: IndexId,
    pub metadata_id: ResolvedDocumentId,
    pub index_config: SearchIndexConfig<T>,
    pub build_reason: BuildReason,
}

#[derive(Debug)]
pub struct IndexBuildResult<T: SearchIndex> {
    pub snapshot_ts: Timestamp,
    pub data: SnapshotData<T::Segment>,
    pub total_stats: T::Statistics,
    pub new_segment_stats: Option<T::Statistics>,
    pub new_segment_id: Option<String>,
    // If this is set, this iteration made progress on backfilling an index
    pub backfill_result: Option<MultiSegmentBackfillResult>,
}

#[derive(Debug)]
pub struct MultiSegmentBuildResult<T: SearchIndex> {
    // This is None only when no new segment was built because all changes were deletes
    new_segment: Option<T::NewSegment>,
    updated_previous_segments: Vec<T::Segment>,
    // This is set only if the build iteration created a segment for a backfilling index
    backfill_result: Option<MultiSegmentBackfillResult>,
}

/// Specifies how documents should be fetched to construct this segment
#[derive(Clone, Copy)]
pub enum MultipartBuildType {
    Partial(RepeatableTimestamp),
    IncrementalComplete {
        cursor: Option<ResolvedDocumentId>,
        backfill_snapshot_ts: RepeatableTimestamp,
    },
}
