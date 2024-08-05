// Copyright 2024 RisingWave Labs
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

use risingwave_pb::common::WorkerNode;
use risingwave_pb::meta::PausedReason;

use crate::barrier::info::{CommandActorChanges, CommandFragmentChanges, InflightActorInfo};
use crate::barrier::{Command, TracedEpoch};
use crate::manager::InflightFragmentInfo;

/// `BarrierManagerState` defines the necessary state of `GlobalBarrierManager`.
pub struct BarrierManagerState {
    /// The last sent `prev_epoch`
    ///
    /// There's no need to persist this field. On recovery, we will restore this from the latest
    /// committed snapshot in `HummockManager`.
    in_flight_prev_epoch: TracedEpoch,

    /// Inflight running actors info.
    pub(crate) inflight_actor_infos: InflightActorInfo,

    /// Whether the cluster is paused and the reason.
    paused_reason: Option<PausedReason>,
}

impl BarrierManagerState {
    pub fn new(
        in_flight_prev_epoch: TracedEpoch,
        inflight_actor_infos: InflightActorInfo,
        paused_reason: Option<PausedReason>,
    ) -> Self {
        Self {
            in_flight_prev_epoch,
            inflight_actor_infos,
            paused_reason,
        }
    }

    pub fn paused_reason(&self) -> Option<PausedReason> {
        self.paused_reason
    }

    pub fn set_paused_reason(&mut self, paused_reason: Option<PausedReason>) {
        if self.paused_reason != paused_reason {
            tracing::info!(current = ?self.paused_reason, new = ?paused_reason, "update paused state");
            self.paused_reason = paused_reason;
        }
    }

    pub fn in_flight_prev_epoch(&self) -> &TracedEpoch {
        &self.in_flight_prev_epoch
    }

    /// Returns the epoch pair for the next barrier, and updates the state.
    pub fn next_epoch_pair(&mut self) -> (TracedEpoch, TracedEpoch) {
        let prev_epoch = self.in_flight_prev_epoch.clone();
        let next_epoch = prev_epoch.next();
        self.in_flight_prev_epoch = next_epoch.clone();
        (prev_epoch, next_epoch)
    }

    pub fn resolve_worker_nodes(&mut self, nodes: impl IntoIterator<Item = WorkerNode>) {
        self.inflight_actor_infos.resolve_worker_nodes(nodes);
    }

    /// Returns the inflight actor infos that have included the newly added actors in the given command. The dropped actors
    /// will be removed from the state after the info get resolved.
    pub fn apply_command(&mut self, command: &Command) -> InflightActorInfo {
        self.inflight_actor_infos.pre_apply(command);
        // update the fragment_infos outside pre_apply
        let fragment_infos = &mut self.inflight_actor_infos.fragment_infos;
        if !matches!(command, Command::CreateSnapshotBackfillStreamingJob { .. }) {
            if let Some(CommandActorChanges { fragment_changes }) = command.actor_changes() {
                for (fragment_id, change) in fragment_changes {
                    if let CommandFragmentChanges::NewFragment(InflightFragmentInfo {
                        actors: new_actors,
                        state_table_ids: table_ids,
                        is_injectable,
                    }) = change
                    {
                        assert!(fragment_infos
                            .insert(
                                fragment_id,
                                InflightFragmentInfo {
                                    actors: new_actors,
                                    state_table_ids: table_ids,
                                    is_injectable,
                                }
                            )
                            .is_none());
                    }
                }
            }
        }

        let info = self.inflight_actor_infos.clone();
        self.inflight_actor_infos.post_apply(command);
        if let Command::FinishCreateSnapshotBackfillStreamingJobs(jobs_to_finish) = command {
            for (_, fragment_info) in jobs_to_finish.values() {
                for (fragment_id, fragment_info) in fragment_info {
                    assert!(self
                        .inflight_actor_infos
                        .fragment_infos
                        .insert(*fragment_id, fragment_info.clone())
                        .is_none());
                }
            }
        }

        info
    }
}
