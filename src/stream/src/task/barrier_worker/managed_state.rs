// Copyright 2025 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cell::LazyCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::{Debug, Display, Formatter};
use std::future::{Future, pending, poll_fn};
use std::mem::replace;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use futures::FutureExt;
use futures::stream::FuturesOrdered;
use prometheus::HistogramTimer;
use risingwave_common::catalog::{DatabaseId, TableId};
use risingwave_common::util::epoch::EpochPair;
use risingwave_pb::stream_plan::barrier::BarrierKind;
use risingwave_storage::StateStoreImpl;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::{StreamError, StreamResult};
use crate::executor::Barrier;
use crate::executor::monitor::StreamingMetrics;
use crate::task::progress::BackfillState;
use crate::task::{
    ActorId, LocalBarrierEvent, LocalBarrierManager, NewOutputRequest, PartialGraphId,
    StreamActorManager, UpDownActorIds,
};

struct IssuedState {
    /// Actor ids remaining to be collected.
    pub remaining_actors: BTreeSet<ActorId>,

    pub barrier_inflight_latency: HistogramTimer,
}

impl Debug for IssuedState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedState")
            .field("remaining_actors", &self.remaining_actors)
            .finish()
    }
}

/// The state machine of local barrier manager.
#[derive(Debug)]
enum ManagedBarrierStateInner {
    /// Meta service has issued a `send_barrier` request. We're collecting barriers now.
    Issued(IssuedState),

    /// The barrier has been collected by all remaining actors
    AllCollected(Vec<PbCreateMviewProgress>),
}

#[derive(Debug)]
struct BarrierState {
    barrier: Barrier,
    /// Only be `Some(_)` when `barrier.kind` is `Checkpoint`
    table_ids: Option<HashSet<TableId>>,
    inner: ManagedBarrierStateInner,
}

use risingwave_common::must_match;
use risingwave_pb::stream_plan::SubscriptionUpstreamInfo;
use risingwave_pb::stream_service::InjectBarrierRequest;
use risingwave_pb::stream_service::barrier_complete_response::PbCreateMviewProgress;
use risingwave_pb::stream_service::streaming_control_stream_request::{
    DatabaseInitialPartialGraph, InitialPartialGraph,
};

use crate::executor::exchange::permit;
use crate::executor::exchange::permit::channel_from_config;
use crate::task::barrier_worker::ScoredStreamError;
use crate::task::barrier_worker::await_epoch_completed_future::AwaitEpochCompletedFuture;

pub(super) struct ManagedBarrierStateDebugInfo<'a> {
    running_actors: BTreeSet<ActorId>,
    graph_states: &'a HashMap<PartialGraphId, PartialGraphManagedBarrierState>,
}

impl Display for ManagedBarrierStateDebugInfo<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "running_actors: ")?;
        for actor_id in &self.running_actors {
            write!(f, "{}, ", actor_id)?;
        }
        for (partial_graph_id, graph_states) in self.graph_states {
            writeln!(f, "--- Partial Group {}", partial_graph_id.0)?;
            write!(f, "{}", graph_states)?;
        }
        Ok(())
    }
}

impl Display for &'_ PartialGraphManagedBarrierState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut prev_epoch = 0u64;
        for (epoch, barrier_state) in &self.epoch_barrier_state_map {
            write!(f, "> Epoch {}: ", epoch)?;
            match &barrier_state.inner {
                ManagedBarrierStateInner::Issued(state) => {
                    write!(
                        f,
                        "Issued [{:?}]. Remaining actors: [",
                        barrier_state.barrier.kind
                    )?;
                    let mut is_prev_epoch_issued = false;
                    if prev_epoch != 0 {
                        let bs = &self.epoch_barrier_state_map[&prev_epoch];
                        if let ManagedBarrierStateInner::Issued(IssuedState {
                            remaining_actors: remaining_actors_prev,
                            ..
                        }) = &bs.inner
                        {
                            // Only show the actors that are not in the previous epoch.
                            is_prev_epoch_issued = true;
                            let mut duplicates = 0usize;
                            for actor_id in &state.remaining_actors {
                                if !remaining_actors_prev.contains(actor_id) {
                                    write!(f, "{}, ", actor_id)?;
                                } else {
                                    duplicates += 1;
                                }
                            }
                            if duplicates > 0 {
                                write!(f, "...and {} actors in prev epoch", duplicates)?;
                            }
                        }
                    }
                    if !is_prev_epoch_issued {
                        for actor_id in &state.remaining_actors {
                            write!(f, "{}, ", actor_id)?;
                        }
                    }
                    write!(f, "]")?;
                }
                ManagedBarrierStateInner::AllCollected(_) => {
                    write!(f, "AllCollected")?;
                }
            }
            prev_epoch = *epoch;
            writeln!(f)?;
        }

        if !self.create_mview_progress.is_empty() {
            writeln!(f, "Create MView Progress:")?;
            for (epoch, progress) in &self.create_mview_progress {
                write!(f, "> Epoch {}:", epoch)?;
                for (actor_id, state) in progress {
                    write!(f, ">> Actor {}: {}, ", actor_id, state)?;
                }
            }
        }

        Ok(())
    }
}

enum InflightActorStatus {
    /// The actor has been issued some barriers, but has not collected the first barrier
    IssuedFirst(Vec<Barrier>),
    /// The actor has been issued some barriers, and has collected the first barrier
    Running(u64),
}

impl InflightActorStatus {
    fn max_issued_epoch(&self) -> u64 {
        match self {
            InflightActorStatus::Running(epoch) => *epoch,
            InflightActorStatus::IssuedFirst(issued_barriers) => {
                issued_barriers.last().expect("non-empty").epoch.prev
            }
        }
    }
}

pub(crate) struct InflightActorState {
    actor_id: ActorId,
    barrier_senders: Vec<mpsc::UnboundedSender<Barrier>>,
    /// `prev_epoch` -> partial graph id
    pub(crate) inflight_barriers: BTreeMap<u64, PartialGraphId>,
    status: InflightActorStatus,
    /// Whether the actor has been issued a stop barrier
    is_stopping: bool,

    new_output_request_tx: UnboundedSender<(ActorId, NewOutputRequest)>,
    join_handle: JoinHandle<()>,
    monitor_task_handle: Option<JoinHandle<()>>,
}

impl InflightActorState {
    pub(super) fn start(
        actor_id: ActorId,
        initial_partial_graph_id: PartialGraphId,
        initial_barrier: &Barrier,
        new_output_request_tx: UnboundedSender<(ActorId, NewOutputRequest)>,
        join_handle: JoinHandle<()>,
        monitor_task_handle: Option<JoinHandle<()>>,
    ) -> Self {
        Self {
            actor_id,
            barrier_senders: vec![],
            inflight_barriers: BTreeMap::from_iter([(
                initial_barrier.epoch.prev,
                initial_partial_graph_id,
            )]),
            status: InflightActorStatus::IssuedFirst(vec![initial_barrier.clone()]),
            is_stopping: false,
            new_output_request_tx,
            join_handle,
            monitor_task_handle,
        }
    }

    pub(super) fn issue_barrier(
        &mut self,
        partial_graph_id: PartialGraphId,
        barrier: &Barrier,
        is_stop: bool,
    ) -> StreamResult<()> {
        assert!(barrier.epoch.prev > self.status.max_issued_epoch());

        for barrier_sender in &self.barrier_senders {
            barrier_sender.send(barrier.clone()).map_err(|_| {
                StreamError::barrier_send(
                    barrier.clone(),
                    self.actor_id,
                    "failed to send to registered sender",
                )
            })?;
        }

        assert!(
            self.inflight_barriers
                .insert(barrier.epoch.prev, partial_graph_id)
                .is_none()
        );

        match &mut self.status {
            InflightActorStatus::IssuedFirst(pending_barriers) => {
                pending_barriers.push(barrier.clone());
            }
            InflightActorStatus::Running(prev_epoch) => {
                *prev_epoch = barrier.epoch.prev;
            }
        };

        if is_stop {
            assert!(!self.is_stopping, "stopped actor should not issue barrier");
            self.is_stopping = true;
        }
        Ok(())
    }

    pub(super) fn collect(&mut self, epoch: EpochPair) -> (PartialGraphId, bool) {
        let (prev_epoch, prev_partial_graph_id) =
            self.inflight_barriers.pop_first().expect("should exist");
        assert_eq!(prev_epoch, epoch.prev);
        match &self.status {
            InflightActorStatus::IssuedFirst(pending_barriers) => {
                assert_eq!(
                    prev_epoch,
                    pending_barriers.first().expect("non-empty").epoch.prev
                );
                self.status = InflightActorStatus::Running(
                    pending_barriers.last().expect("non-empty").epoch.prev,
                );
            }
            InflightActorStatus::Running(_) => {}
        }
        (
            prev_partial_graph_id,
            self.inflight_barriers.is_empty() && self.is_stopping,
        )
    }
}

/// Part of [`DatabaseManagedBarrierState`]
pub(crate) struct PartialGraphManagedBarrierState {
    /// Record barrier state for each epoch of concurrent checkpoints.
    ///
    /// The key is `prev_epoch`, and the first value is `curr_epoch`
    epoch_barrier_state_map: BTreeMap<u64, BarrierState>,

    prev_barrier_table_ids: Option<(EpochPair, HashSet<TableId>)>,

    mv_depended_subscriptions: HashMap<TableId, HashSet<u32>>,

    /// Record the progress updates of creating mviews for each epoch of concurrent checkpoints.
    ///
    /// This is updated by [`crate::task::barrier_manager::CreateMviewProgressReporter::update`] and will be reported to meta
    /// in [`crate::task::barrier_worker::BarrierCompleteResult`].
    pub(crate) create_mview_progress: HashMap<u64, HashMap<ActorId, BackfillState>>,

    state_store: StateStoreImpl,

    streaming_metrics: Arc<StreamingMetrics>,
}

impl PartialGraphManagedBarrierState {
    pub(super) fn new(actor_manager: &StreamActorManager) -> Self {
        Self::new_inner(
            actor_manager.env.state_store(),
            actor_manager.streaming_metrics.clone(),
        )
    }

    fn new_inner(state_store: StateStoreImpl, streaming_metrics: Arc<StreamingMetrics>) -> Self {
        Self {
            epoch_barrier_state_map: Default::default(),
            prev_barrier_table_ids: None,
            mv_depended_subscriptions: Default::default(),
            create_mview_progress: Default::default(),
            state_store,
            streaming_metrics,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::new_inner(
            StateStoreImpl::for_test(),
            Arc::new(StreamingMetrics::unused()),
        )
    }

    pub(super) fn is_empty(&self) -> bool {
        self.epoch_barrier_state_map.is_empty()
    }
}

pub(crate) struct SuspendedDatabaseState {
    pub(super) suspend_time: Instant,
    inner: DatabaseManagedBarrierState,
    failure: Option<(Option<ActorId>, StreamError)>,
}

impl SuspendedDatabaseState {
    fn new(
        state: DatabaseManagedBarrierState,
        failure: Option<(Option<ActorId>, StreamError)>,
        _completing_futures: Option<FuturesOrdered<AwaitEpochCompletedFuture>>, /* discard the completing futures */
    ) -> Self {
        Self {
            suspend_time: Instant::now(),
            inner: state,
            failure,
        }
    }

    async fn reset(mut self) -> ResetDatabaseOutput {
        let root_err = self.inner.try_find_root_actor_failure(self.failure).await;
        self.inner.abort_and_wait_actors().await;
        if let Some(hummock) = self.inner.actor_manager.env.state_store().as_hummock() {
            hummock.clear_tables(self.inner.table_ids).await;
        }
        ResetDatabaseOutput { root_err }
    }
}

pub(crate) struct ResettingDatabaseState {
    join_handle: JoinHandle<ResetDatabaseOutput>,
    reset_request_id: u32,
}

pub(crate) struct ResetDatabaseOutput {
    pub(crate) root_err: Option<ScoredStreamError>,
}

pub(crate) enum DatabaseStatus {
    ReceivedExchangeRequest(
        Vec<(
            UpDownActorIds,
            oneshot::Sender<StreamResult<permit::Receiver>>,
        )>,
    ),
    Running(DatabaseManagedBarrierState),
    Suspended(SuspendedDatabaseState),
    Resetting(ResettingDatabaseState),
    /// temporary place holder
    Unspecified,
}

impl DatabaseStatus {
    pub(crate) async fn abort(&mut self) {
        match self {
            DatabaseStatus::ReceivedExchangeRequest(pending_requests) => {
                for (_, sender) in pending_requests.drain(..) {
                    let _ = sender.send(Err(anyhow!("database aborted").into()));
                }
            }
            DatabaseStatus::Running(state) => {
                state.abort_and_wait_actors().await;
            }
            DatabaseStatus::Suspended(SuspendedDatabaseState { inner: state, .. }) => {
                state.abort_and_wait_actors().await;
            }
            DatabaseStatus::Resetting(state) => {
                (&mut state.join_handle)
                    .await
                    .expect("failed to join reset database join handle");
            }
            DatabaseStatus::Unspecified => {
                unreachable!()
            }
        }
    }

    pub(crate) fn state_for_request(&mut self) -> Option<&mut DatabaseManagedBarrierState> {
        match self {
            DatabaseStatus::ReceivedExchangeRequest(_) => {
                unreachable!("should not handle request")
            }
            DatabaseStatus::Running(state) => Some(state),
            DatabaseStatus::Suspended(_) => None,
            DatabaseStatus::Resetting(_) => {
                unreachable!("should not receive further request during cleaning")
            }
            DatabaseStatus::Unspecified => {
                unreachable!()
            }
        }
    }

    pub(super) fn poll_next_event(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ManagedBarrierStateEvent> {
        match self {
            DatabaseStatus::ReceivedExchangeRequest(_) => Poll::Pending,
            DatabaseStatus::Running(state) => state.poll_next_event(cx),
            DatabaseStatus::Suspended(_) => Poll::Pending,
            DatabaseStatus::Resetting(state) => state.join_handle.poll_unpin(cx).map(|result| {
                let output = result.expect("should be able to join");
                ManagedBarrierStateEvent::DatabaseReset(output, state.reset_request_id)
            }),
            DatabaseStatus::Unspecified => {
                unreachable!()
            }
        }
    }

    pub(super) fn suspend(
        &mut self,
        failed_actor: Option<ActorId>,
        err: StreamError,
        completing_futures: Option<FuturesOrdered<AwaitEpochCompletedFuture>>,
    ) {
        let state = must_match!(replace(self, DatabaseStatus::Unspecified), DatabaseStatus::Running(state) => state);
        *self = DatabaseStatus::Suspended(SuspendedDatabaseState::new(
            state,
            Some((failed_actor, err)),
            completing_futures,
        ));
    }

    pub(super) fn start_reset(
        &mut self,
        database_id: DatabaseId,
        completing_futures: Option<FuturesOrdered<AwaitEpochCompletedFuture>>,
        reset_request_id: u32,
    ) {
        let join_handle = match replace(self, DatabaseStatus::Unspecified) {
            DatabaseStatus::ReceivedExchangeRequest(pending_requests) => {
                for (_, sender) in pending_requests {
                    let _ = sender.send(Err(anyhow!("database reset").into()));
                }
                tokio::spawn(async move { ResetDatabaseOutput { root_err: None } })
            }
            DatabaseStatus::Running(state) => {
                assert_eq!(database_id, state.database_id);
                info!(
                    database_id = database_id.database_id,
                    reset_request_id, "start database reset from Running"
                );
                tokio::spawn(SuspendedDatabaseState::new(state, None, completing_futures).reset())
            }
            DatabaseStatus::Suspended(state) => {
                assert!(
                    completing_futures.is_none(),
                    "should have been clear when suspended"
                );
                assert_eq!(database_id, state.inner.database_id);
                info!(
                    database_id = database_id.database_id,
                    reset_request_id,
                    suspend_elapsed = ?state.suspend_time.elapsed(),
                    "start database reset after suspended"
                );
                tokio::spawn(state.reset())
            }
            DatabaseStatus::Resetting(state) => {
                let prev_request_id = state.reset_request_id;
                info!(
                    database_id = database_id.database_id,
                    reset_request_id, prev_request_id, "receive duplicate reset request"
                );
                assert!(reset_request_id > prev_request_id);
                state.join_handle
            }
            DatabaseStatus::Unspecified => {
                unreachable!()
            }
        };
        *self = DatabaseStatus::Resetting(ResettingDatabaseState {
            join_handle,
            reset_request_id,
        });
    }
}

pub(crate) struct ManagedBarrierState {
    pub(crate) databases: HashMap<DatabaseId, DatabaseStatus>,
}

pub(super) enum ManagedBarrierStateEvent {
    BarrierCollected {
        partial_graph_id: PartialGraphId,
        barrier: Barrier,
    },
    ActorError {
        actor_id: ActorId,
        err: StreamError,
    },
    DatabaseReset(ResetDatabaseOutput, u32),
}

impl ManagedBarrierState {
    pub(super) fn new(
        actor_manager: Arc<StreamActorManager>,
        initial_partial_graphs: Vec<DatabaseInitialPartialGraph>,
        term_id: String,
    ) -> Self {
        let mut databases = HashMap::new();
        for database in initial_partial_graphs {
            let database_id = DatabaseId::new(database.database_id);
            assert!(!databases.contains_key(&database_id));
            let state = DatabaseManagedBarrierState::new(
                database_id,
                term_id.clone(),
                actor_manager.clone(),
                database.graphs,
            );
            databases.insert(database_id, DatabaseStatus::Running(state));
        }

        Self { databases }
    }

    pub(super) fn next_event(
        &mut self,
    ) -> impl Future<Output = (DatabaseId, ManagedBarrierStateEvent)> + '_ {
        poll_fn(|cx| {
            for (database_id, database) in &mut self.databases {
                if let Poll::Ready(event) = database.poll_next_event(cx) {
                    return Poll::Ready((*database_id, event));
                }
            }
            Poll::Pending
        })
    }
}

/// Per-database barrier state manager. Handles barriers for one specific database.
/// Part of [`ManagedBarrierState`] in [`super::LocalBarrierWorker`].
///
/// See [`crate::task`] for architecture overview.
pub(crate) struct DatabaseManagedBarrierState {
    database_id: DatabaseId,
    pub(crate) actor_states: HashMap<ActorId, InflightActorState>,
    pub(super) actor_pending_new_output_requests:
        HashMap<ActorId, Vec<(ActorId, NewOutputRequest)>>,

    pub(crate) graph_states: HashMap<PartialGraphId, PartialGraphManagedBarrierState>,

    table_ids: HashSet<TableId>,

    actor_manager: Arc<StreamActorManager>,

    pub(super) local_barrier_manager: LocalBarrierManager,

    barrier_event_rx: UnboundedReceiver<LocalBarrierEvent>,
    pub(super) actor_failure_rx: UnboundedReceiver<(ActorId, StreamError)>,
}

impl DatabaseManagedBarrierState {
    /// Create a barrier manager state. This will be called only once.
    pub(super) fn new(
        database_id: DatabaseId,
        term_id: String,
        actor_manager: Arc<StreamActorManager>,
        initial_partial_graphs: Vec<InitialPartialGraph>,
    ) -> Self {
        let (local_barrier_manager, barrier_event_rx, actor_failure_rx) =
            LocalBarrierManager::new(database_id, term_id, actor_manager.env.clone());
        Self {
            database_id,
            actor_states: Default::default(),
            actor_pending_new_output_requests: Default::default(),
            graph_states: initial_partial_graphs
                .into_iter()
                .map(|graph| {
                    let mut state = PartialGraphManagedBarrierState::new(&actor_manager);
                    state.add_subscriptions(graph.subscriptions);
                    (PartialGraphId::new(graph.partial_graph_id), state)
                })
                .collect(),
            table_ids: Default::default(),
            actor_manager,
            local_barrier_manager,
            barrier_event_rx,
            actor_failure_rx,
        }
    }

    pub(super) fn to_debug_info(&self) -> ManagedBarrierStateDebugInfo<'_> {
        ManagedBarrierStateDebugInfo {
            running_actors: self.actor_states.keys().cloned().collect(),
            graph_states: &self.graph_states,
        }
    }

    async fn abort_and_wait_actors(&mut self) {
        for (actor_id, state) in &self.actor_states {
            tracing::debug!("force stopping actor {}", actor_id);
            state.join_handle.abort();
            if let Some(monitor_task_handle) = &state.monitor_task_handle {
                monitor_task_handle.abort();
            }
        }

        for (actor_id, state) in self.actor_states.drain() {
            tracing::debug!("join actor {}", actor_id);
            let result = state.join_handle.await;
            assert!(result.is_ok() || result.unwrap_err().is_cancelled());
        }
    }
}

impl InflightActorState {
    pub(super) fn register_barrier_sender(
        &mut self,
        tx: mpsc::UnboundedSender<Barrier>,
    ) -> StreamResult<()> {
        match &self.status {
            InflightActorStatus::IssuedFirst(pending_barriers) => {
                for barrier in pending_barriers {
                    tx.send(barrier.clone()).map_err(|_| {
                        StreamError::barrier_send(
                            barrier.clone(),
                            self.actor_id,
                            "failed to send pending barriers to newly registered sender",
                        )
                    })?;
                }
                self.barrier_senders.push(tx);
            }
            InflightActorStatus::Running(_) => {
                unreachable!("should not register barrier sender when entering Running status")
            }
        }
        Ok(())
    }
}

impl DatabaseManagedBarrierState {
    pub(super) fn register_barrier_sender(
        &mut self,
        actor_id: ActorId,
        tx: mpsc::UnboundedSender<Barrier>,
    ) -> StreamResult<()> {
        self.actor_states
            .get_mut(&actor_id)
            .expect("should exist")
            .register_barrier_sender(tx)
    }
}

impl PartialGraphManagedBarrierState {
    pub(super) fn add_subscriptions(&mut self, subscriptions: Vec<SubscriptionUpstreamInfo>) {
        for subscription_to_add in subscriptions {
            if !self
                .mv_depended_subscriptions
                .entry(TableId::new(subscription_to_add.upstream_mv_table_id))
                .or_default()
                .insert(subscription_to_add.subscriber_id)
            {
                if cfg!(debug_assertions) {
                    panic!("add an existing subscription: {:?}", subscription_to_add);
                }
                warn!(?subscription_to_add, "add an existing subscription");
            }
        }
    }

    pub(super) fn remove_subscriptions(&mut self, subscriptions: Vec<SubscriptionUpstreamInfo>) {
        for subscription_to_remove in subscriptions {
            let upstream_table_id = TableId::new(subscription_to_remove.upstream_mv_table_id);
            let Some(subscribers) = self.mv_depended_subscriptions.get_mut(&upstream_table_id)
            else {
                if cfg!(debug_assertions) {
                    panic!(
                        "unable to find upstream mv table to remove: {:?}",
                        subscription_to_remove
                    );
                }
                warn!(
                    ?subscription_to_remove,
                    "unable to find upstream mv table to remove"
                );
                continue;
            };
            if !subscribers.remove(&subscription_to_remove.subscriber_id) {
                if cfg!(debug_assertions) {
                    panic!(
                        "unable to find subscriber to remove: {:?}",
                        subscription_to_remove
                    );
                }
                warn!(
                    ?subscription_to_remove,
                    "unable to find subscriber to remove"
                );
            }
            if subscribers.is_empty() {
                self.mv_depended_subscriptions.remove(&upstream_table_id);
            }
        }
    }
}

impl DatabaseManagedBarrierState {
    pub(super) fn transform_to_issued(
        &mut self,
        barrier: &Barrier,
        request: InjectBarrierRequest,
    ) -> StreamResult<()> {
        let partial_graph_id = PartialGraphId::new(request.partial_graph_id);
        let actor_to_stop = barrier.all_stop_actors();
        let is_stop_actor = |actor_id| {
            actor_to_stop
                .map(|actors| actors.contains(&actor_id))
                .unwrap_or(false)
        };
        let graph_state = self
            .graph_states
            .get_mut(&partial_graph_id)
            .expect("should exist");

        graph_state.add_subscriptions(request.subscriptions_to_add);
        graph_state.remove_subscriptions(request.subscriptions_to_remove);

        let table_ids =
            HashSet::from_iter(request.table_ids_to_sync.iter().cloned().map(TableId::new));
        self.table_ids.extend(table_ids.iter().cloned());

        graph_state.transform_to_issued(
            barrier,
            request.actor_ids_to_collect.iter().cloned(),
            table_ids,
        );

        let mut new_actors = HashSet::new();
        let subscriptions =
            LazyCell::new(|| Arc::new(graph_state.mv_depended_subscriptions.clone()));
        for (node, fragment_id, actor) in
            request
                .actors_to_build
                .into_iter()
                .flat_map(|fragment_actors| {
                    let node = Arc::new(fragment_actors.node.unwrap());
                    fragment_actors
                        .actors
                        .into_iter()
                        .map(move |actor| (node.clone(), fragment_actors.fragment_id, actor))
                })
        {
            let actor_id = actor.actor_id;
            assert!(!is_stop_actor(actor_id));
            assert!(new_actors.insert(actor_id));
            assert!(request.actor_ids_to_collect.contains(&actor_id));
            let (new_output_request_tx, new_output_request_rx) = unbounded_channel();
            if let Some(pending_requests) = self.actor_pending_new_output_requests.remove(&actor_id)
            {
                for request in pending_requests {
                    let _ = new_output_request_tx.send(request);
                }
            }
            let (join_handle, monitor_join_handle) = self.actor_manager.spawn_actor(
                actor,
                fragment_id,
                node,
                (*subscriptions).clone(),
                self.local_barrier_manager.clone(),
                new_output_request_rx,
            );
            assert!(
                self.actor_states
                    .try_insert(
                        actor_id,
                        InflightActorState::start(
                            actor_id,
                            partial_graph_id,
                            barrier,
                            new_output_request_tx,
                            join_handle,
                            monitor_join_handle
                        )
                    )
                    .is_ok()
            );
        }

        // Spawn a trivial join handle to be compatible with the unit test. In the unit tests that involve local barrier manager,
        // actors are spawned in the local test logic, but we assume that there is an entry for each spawned actor in ·actor_states`,
        // so under cfg!(test) we add a dummy entry for each new actor.
        if cfg!(test) {
            for actor_id in &request.actor_ids_to_collect {
                if !self.actor_states.contains_key(actor_id) {
                    let (tx, rx) = unbounded_channel();
                    let join_handle = self.actor_manager.runtime.spawn(async move {
                        // The rx is spawned so that tx.send() will not fail.
                        let _ = rx;
                        pending().await
                    });
                    assert!(
                        self.actor_states
                            .try_insert(
                                *actor_id,
                                InflightActorState::start(
                                    *actor_id,
                                    partial_graph_id,
                                    barrier,
                                    tx,
                                    join_handle,
                                    None,
                                )
                            )
                            .is_ok()
                    );
                    new_actors.insert(*actor_id);
                }
            }
        }

        // Note: it's important to issue barrier to actor after issuing to graph to ensure that
        // we call `start_epoch` on the graph before the actors receive the barrier
        for actor_id in &request.actor_ids_to_collect {
            if new_actors.contains(actor_id) {
                continue;
            }
            self.actor_states
                .get_mut(actor_id)
                .unwrap_or_else(|| {
                    panic!(
                        "should exist: {} {:?}",
                        actor_id, request.actor_ids_to_collect
                    );
                })
                .issue_barrier(partial_graph_id, barrier, is_stop_actor(*actor_id))?;
        }

        Ok(())
    }

    pub(super) fn new_actor_remote_output_request(
        &mut self,
        actor_id: ActorId,
        upstream_actor_id: ActorId,
        result_sender: oneshot::Sender<StreamResult<permit::Receiver>>,
    ) {
        let (tx, rx) = channel_from_config(self.local_barrier_manager.env.config());
        self.new_actor_output_request(actor_id, upstream_actor_id, NewOutputRequest::Remote(tx));
        let _ = result_sender.send(Ok(rx));
    }

    pub(super) fn new_actor_output_request(
        &mut self,
        actor_id: ActorId,
        upstream_actor_id: ActorId,
        request: NewOutputRequest,
    ) {
        if let Some(actor) = self.actor_states.get_mut(&upstream_actor_id) {
            let _ = actor.new_output_request_tx.send((actor_id, request));
        } else {
            self.actor_pending_new_output_requests
                .entry(upstream_actor_id)
                .or_default()
                .push((actor_id, request));
        }
    }

    /// Handles [`LocalBarrierEvent`] from [`crate::task::barrier_manager::LocalBarrierManager`].
    pub(super) fn poll_next_event(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ManagedBarrierStateEvent> {
        if let Poll::Ready(option) = self.actor_failure_rx.poll_recv(cx) {
            let (actor_id, err) = option.expect("non-empty when tx in local_barrier_manager");
            return Poll::Ready(ManagedBarrierStateEvent::ActorError { actor_id, err });
        }
        // yield some pending collected epochs
        for (partial_graph_id, graph_state) in &mut self.graph_states {
            if let Some(barrier) = graph_state.may_have_collected_all() {
                return Poll::Ready(ManagedBarrierStateEvent::BarrierCollected {
                    partial_graph_id: *partial_graph_id,
                    barrier,
                });
            }
        }
        while let Poll::Ready(event) = self.barrier_event_rx.poll_recv(cx) {
            match event.expect("non-empty when tx in local_barrier_manager") {
                LocalBarrierEvent::ReportActorCollected { actor_id, epoch } => {
                    if let Some((partial_graph_id, barrier)) = self.collect(actor_id, epoch) {
                        return Poll::Ready(ManagedBarrierStateEvent::BarrierCollected {
                            partial_graph_id,
                            barrier,
                        });
                    }
                }
                LocalBarrierEvent::ReportCreateProgress {
                    epoch,
                    actor,
                    state,
                } => {
                    self.update_create_mview_progress(epoch, actor, state);
                }
                LocalBarrierEvent::RegisterBarrierSender {
                    actor_id,
                    barrier_sender,
                } => {
                    if let Err(err) = self.register_barrier_sender(actor_id, barrier_sender) {
                        return Poll::Ready(ManagedBarrierStateEvent::ActorError { actor_id, err });
                    }
                }
                LocalBarrierEvent::RegisterLocalUpstreamOutput {
                    actor_id,
                    upstream_actor_id,
                    tx,
                } => {
                    self.new_actor_output_request(
                        actor_id,
                        upstream_actor_id,
                        NewOutputRequest::Local(tx),
                    );
                }
            }
        }

        debug_assert!(
            self.graph_states
                .values_mut()
                .all(|graph_state| graph_state.may_have_collected_all().is_none())
        );
        Poll::Pending
    }
}

impl DatabaseManagedBarrierState {
    #[must_use]
    pub(super) fn collect(
        &mut self,
        actor_id: ActorId,
        epoch: EpochPair,
    ) -> Option<(PartialGraphId, Barrier)> {
        let (prev_partial_graph_id, is_finished) = self
            .actor_states
            .get_mut(&actor_id)
            .expect("should exist")
            .collect(epoch);
        if is_finished {
            let state = self.actor_states.remove(&actor_id).expect("should exist");
            if let Some(monitor_task_handle) = state.monitor_task_handle {
                monitor_task_handle.abort();
            }
        }
        let prev_graph_state = self
            .graph_states
            .get_mut(&prev_partial_graph_id)
            .expect("should exist");
        prev_graph_state.collect(actor_id, epoch);
        prev_graph_state
            .may_have_collected_all()
            .map(|barrier| (prev_partial_graph_id, barrier))
    }

    pub(super) fn pop_barrier_to_complete(
        &mut self,
        partial_graph_id: PartialGraphId,
        prev_epoch: u64,
    ) -> (
        Barrier,
        Option<HashSet<TableId>>,
        Vec<PbCreateMviewProgress>,
    ) {
        self.graph_states
            .get_mut(&partial_graph_id)
            .expect("should exist")
            .pop_barrier_to_complete(prev_epoch)
    }

    /// Collect actor errors for a while and find the one that might be the root cause.
    ///
    /// Returns `None` if there's no actor error received.
    async fn try_find_root_actor_failure(
        &mut self,
        first_failure: Option<(Option<ActorId>, StreamError)>,
    ) -> Option<ScoredStreamError> {
        let mut later_errs = vec![];
        // fetch more actor errors within a timeout
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            let mut uncollected_actors: HashSet<_> = self.actor_states.keys().cloned().collect();
            if let Some((Some(failed_actor), _)) = &first_failure {
                uncollected_actors.remove(failed_actor);
            }
            while !uncollected_actors.is_empty()
                && let Some((actor_id, error)) = self.actor_failure_rx.recv().await
            {
                uncollected_actors.remove(&actor_id);
                later_errs.push(error);
            }
        })
        .await;

        first_failure
            .into_iter()
            .map(|(_, err)| err)
            .chain(later_errs.into_iter())
            .map(|e| e.with_score())
            .max_by_key(|e| e.score)
    }
}

impl PartialGraphManagedBarrierState {
    /// This method is called when barrier state is modified in either `Issued` or `Stashed`
    /// to transform the state to `AllCollected` and start state store `sync` when the barrier
    /// has been collected from all actors for an `Issued` barrier.
    fn may_have_collected_all(&mut self) -> Option<Barrier> {
        for barrier_state in self.epoch_barrier_state_map.values_mut() {
            match &barrier_state.inner {
                ManagedBarrierStateInner::Issued(IssuedState {
                    remaining_actors, ..
                }) if remaining_actors.is_empty() => {}
                ManagedBarrierStateInner::AllCollected(_) => {
                    continue;
                }
                ManagedBarrierStateInner::Issued(_) => {
                    break;
                }
            }

            self.streaming_metrics.barrier_manager_progress.inc();

            let create_mview_progress = self
                .create_mview_progress
                .remove(&barrier_state.barrier.epoch.curr)
                .unwrap_or_default()
                .into_iter()
                .map(|(actor, state)| state.to_pb(actor))
                .collect();

            let prev_state = replace(
                &mut barrier_state.inner,
                ManagedBarrierStateInner::AllCollected(create_mview_progress),
            );

            must_match!(prev_state, ManagedBarrierStateInner::Issued(IssuedState {
                barrier_inflight_latency: timer,
                ..
            }) => {
                timer.observe_duration();
            });

            return Some(barrier_state.barrier.clone());
        }
        None
    }

    fn pop_barrier_to_complete(
        &mut self,
        prev_epoch: u64,
    ) -> (
        Barrier,
        Option<HashSet<TableId>>,
        Vec<PbCreateMviewProgress>,
    ) {
        let (popped_prev_epoch, barrier_state) = self
            .epoch_barrier_state_map
            .pop_first()
            .expect("should exist");

        assert_eq!(prev_epoch, popped_prev_epoch);

        let create_mview_progress = must_match!(barrier_state.inner, ManagedBarrierStateInner::AllCollected(create_mview_progress) => {
            create_mview_progress
        });
        (
            barrier_state.barrier,
            barrier_state.table_ids,
            create_mview_progress,
        )
    }
}

impl PartialGraphManagedBarrierState {
    /// Collect a `barrier` from the actor with `actor_id`.
    pub(super) fn collect(&mut self, actor_id: ActorId, epoch: EpochPair) {
        tracing::debug!(
            target: "events::stream::barrier::manager::collect",
            ?epoch, actor_id, state = ?self.epoch_barrier_state_map,
            "collect_barrier",
        );

        match self.epoch_barrier_state_map.get_mut(&epoch.prev) {
            None => {
                // If the barrier's state is stashed, this occurs exclusively in scenarios where the barrier has not been
                // injected by the barrier manager, or the barrier message is blocked at the `RemoteInput` side waiting for injection.
                // Given these conditions, it's inconceivable for an actor to attempt collect at this point.
                panic!(
                    "cannot collect new actor barrier {:?} at current state: None",
                    epoch,
                )
            }
            Some(&mut BarrierState {
                ref barrier,
                inner:
                    ManagedBarrierStateInner::Issued(IssuedState {
                        ref mut remaining_actors,
                        ..
                    }),
                ..
            }) => {
                let exist = remaining_actors.remove(&actor_id);
                assert!(
                    exist,
                    "the actor doesn't exist. actor_id: {:?}, curr_epoch: {:?}",
                    actor_id, epoch.curr
                );
                assert_eq!(barrier.epoch.curr, epoch.curr);
            }
            Some(BarrierState { inner, .. }) => {
                panic!(
                    "cannot collect new actor barrier {:?} at current state: {:?}",
                    epoch, inner
                )
            }
        }
    }

    /// When the meta service issues a `send_barrier` request, call this function to transform to
    /// `Issued` and start to collect or to notify.
    pub(super) fn transform_to_issued(
        &mut self,
        barrier: &Barrier,
        actor_ids_to_collect: impl IntoIterator<Item = ActorId>,
        table_ids: HashSet<TableId>,
    ) {
        let timer = self
            .streaming_metrics
            .barrier_inflight_latency
            .start_timer();

        if let Some(hummock) = self.state_store.as_hummock() {
            hummock.start_epoch(barrier.epoch.curr, table_ids.clone());
        }

        let table_ids = match barrier.kind {
            BarrierKind::Unspecified => {
                unreachable!()
            }
            BarrierKind::Initial => {
                assert!(
                    self.prev_barrier_table_ids.is_none(),
                    "non empty table_ids at initial barrier: {:?}",
                    self.prev_barrier_table_ids
                );
                info!(epoch = ?barrier.epoch, "initialize at Initial barrier");
                self.prev_barrier_table_ids = Some((barrier.epoch, table_ids));
                None
            }
            BarrierKind::Barrier => {
                if let Some((prev_epoch, prev_table_ids)) = self.prev_barrier_table_ids.as_mut() {
                    assert_eq!(prev_epoch.curr, barrier.epoch.prev);
                    assert_eq!(prev_table_ids, &table_ids);
                    *prev_epoch = barrier.epoch;
                } else {
                    info!(epoch = ?barrier.epoch, "initialize at non-checkpoint barrier");
                    self.prev_barrier_table_ids = Some((barrier.epoch, table_ids));
                }
                None
            }
            BarrierKind::Checkpoint => Some(
                if let Some((prev_epoch, prev_table_ids)) = self
                    .prev_barrier_table_ids
                    .replace((barrier.epoch, table_ids))
                    && prev_epoch.curr == barrier.epoch.prev
                {
                    prev_table_ids
                } else {
                    debug!(epoch = ?barrier.epoch, "reinitialize at Checkpoint barrier");
                    HashSet::new()
                },
            ),
        };

        if let Some(&mut BarrierState { ref inner, .. }) =
            self.epoch_barrier_state_map.get_mut(&barrier.epoch.prev)
        {
            {
                panic!(
                    "barrier epochs{:?} state has already been `Issued`. Current state: {:?}",
                    barrier.epoch, inner
                );
            }
        };

        self.epoch_barrier_state_map.insert(
            barrier.epoch.prev,
            BarrierState {
                barrier: barrier.clone(),
                inner: ManagedBarrierStateInner::Issued(IssuedState {
                    remaining_actors: BTreeSet::from_iter(actor_ids_to_collect),
                    barrier_inflight_latency: timer,
                }),
                table_ids,
            },
        );
    }

    #[cfg(test)]
    async fn pop_next_completed_epoch(&mut self) -> u64 {
        if let Some(barrier) = self.may_have_collected_all() {
            self.pop_barrier_to_complete(barrier.epoch.prev);
            return barrier.epoch.prev;
        }
        pending().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use risingwave_common::util::epoch::test_epoch;

    use crate::executor::Barrier;
    use crate::task::barrier_worker::managed_state::PartialGraphManagedBarrierState;

    #[tokio::test]
    async fn test_managed_state_add_actor() {
        let mut managed_barrier_state = PartialGraphManagedBarrierState::for_test();
        let barrier1 = Barrier::new_test_barrier(test_epoch(1));
        let barrier2 = Barrier::new_test_barrier(test_epoch(2));
        let barrier3 = Barrier::new_test_barrier(test_epoch(3));
        let actor_ids_to_collect1 = HashSet::from([1, 2]);
        let actor_ids_to_collect2 = HashSet::from([1, 2]);
        let actor_ids_to_collect3 = HashSet::from([1, 2, 3]);
        managed_barrier_state.transform_to_issued(&barrier1, actor_ids_to_collect1, HashSet::new());
        managed_barrier_state.transform_to_issued(&barrier2, actor_ids_to_collect2, HashSet::new());
        managed_barrier_state.transform_to_issued(&barrier3, actor_ids_to_collect3, HashSet::new());
        managed_barrier_state.collect(1, barrier1.epoch);
        managed_barrier_state.collect(2, barrier1.epoch);
        assert_eq!(
            managed_barrier_state.pop_next_completed_epoch().await,
            test_epoch(0)
        );
        assert_eq!(
            managed_barrier_state
                .epoch_barrier_state_map
                .first_key_value()
                .unwrap()
                .0,
            &test_epoch(1)
        );
        managed_barrier_state.collect(1, barrier2.epoch);
        managed_barrier_state.collect(1, barrier3.epoch);
        managed_barrier_state.collect(2, barrier2.epoch);
        assert_eq!(
            managed_barrier_state.pop_next_completed_epoch().await,
            test_epoch(1)
        );
        assert_eq!(
            managed_barrier_state
                .epoch_barrier_state_map
                .first_key_value()
                .unwrap()
                .0,
            &test_epoch(2)
        );
        managed_barrier_state.collect(2, barrier3.epoch);
        managed_barrier_state.collect(3, barrier3.epoch);
        assert_eq!(
            managed_barrier_state.pop_next_completed_epoch().await,
            test_epoch(2)
        );
        assert!(managed_barrier_state.epoch_barrier_state_map.is_empty());
    }

    #[tokio::test]
    async fn test_managed_state_stop_actor() {
        let mut managed_barrier_state = PartialGraphManagedBarrierState::for_test();
        let barrier1 = Barrier::new_test_barrier(test_epoch(1));
        let barrier2 = Barrier::new_test_barrier(test_epoch(2));
        let barrier3 = Barrier::new_test_barrier(test_epoch(3));
        let actor_ids_to_collect1 = HashSet::from([1, 2, 3, 4]);
        let actor_ids_to_collect2 = HashSet::from([1, 2, 3]);
        let actor_ids_to_collect3 = HashSet::from([1, 2]);
        managed_barrier_state.transform_to_issued(&barrier1, actor_ids_to_collect1, HashSet::new());
        managed_barrier_state.transform_to_issued(&barrier2, actor_ids_to_collect2, HashSet::new());
        managed_barrier_state.transform_to_issued(&barrier3, actor_ids_to_collect3, HashSet::new());

        managed_barrier_state.collect(1, barrier1.epoch);
        managed_barrier_state.collect(1, barrier2.epoch);
        managed_barrier_state.collect(1, barrier3.epoch);
        managed_barrier_state.collect(2, barrier1.epoch);
        managed_barrier_state.collect(2, barrier2.epoch);
        managed_barrier_state.collect(2, barrier3.epoch);
        assert_eq!(
            managed_barrier_state
                .epoch_barrier_state_map
                .first_key_value()
                .unwrap()
                .0,
            &0
        );
        managed_barrier_state.collect(3, barrier1.epoch);
        managed_barrier_state.collect(3, barrier2.epoch);
        assert_eq!(
            managed_barrier_state
                .epoch_barrier_state_map
                .first_key_value()
                .unwrap()
                .0,
            &0
        );
        managed_barrier_state.collect(4, barrier1.epoch);
        assert_eq!(
            managed_barrier_state.pop_next_completed_epoch().await,
            test_epoch(0)
        );
        assert_eq!(
            managed_barrier_state.pop_next_completed_epoch().await,
            test_epoch(1)
        );
        assert_eq!(
            managed_barrier_state.pop_next_completed_epoch().await,
            test_epoch(2)
        );
        assert!(managed_barrier_state.epoch_barrier_state_map.is_empty());
    }
}
