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

use risingwave_pb::stream_plan::LookupUnionNode;

use super::*;
use crate::executor::LookupUnionExecutor;

pub struct LookupUnionExecutorBuilder;

impl ExecutorBuilder for LookupUnionExecutorBuilder {
    type Node = LookupUnionNode;

    async fn new_boxed_executor(
        params: ExecutorParams,
        node: &Self::Node,
        _store: impl StateStore,
    ) -> StreamResult<Executor> {
        let exec = LookupUnionExecutor::new(params.input, node.order.clone());
        Ok((params.info, exec).into())
    }
}
