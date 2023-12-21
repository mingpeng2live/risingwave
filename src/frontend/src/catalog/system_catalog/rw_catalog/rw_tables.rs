// Copyright 2023 RisingWave Labs
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

use std::collections::HashMap;

use itertools::Itertools;
use risingwave_common::catalog::RW_CATALOG_SCHEMA_NAME;
use risingwave_common::error::Result;
use risingwave_common::row::OwnedRow;
use risingwave_common::types::{DataType, ScalarImpl};
use risingwave_pb::user::grant_privilege::Object;

use crate::catalog::system_catalog::{
    extract_parallelism_from_table_state, get_acl_items, BuiltinTable, SysCatalogReaderImpl,
};

pub const RW_TABLES: BuiltinTable = BuiltinTable {
    name: "rw_tables",
    schema: RW_CATALOG_SCHEMA_NAME,
    columns: &[
        (DataType::Int32, "id"),
        (DataType::Varchar, "name"),
        (DataType::Int32, "schema_id"),
        (DataType::Int32, "owner"),
        (DataType::Varchar, "definition"),
        (DataType::Varchar, "acl"),
        (DataType::Varchar, "parallelism"),
        (DataType::Timestamptz, "initialized_at"),
        (DataType::Timestamptz, "created_at"),
    ],
    pk: &[0],
};

impl SysCatalogReaderImpl {
    pub async fn read_rw_table_info(&self) -> Result<Vec<OwnedRow>> {
        let states = self.meta_client.list_table_fragment_states().await?;
        let states: HashMap<_, _> = states
            .into_iter()
            .map(|state| (state.table_id, state))
            .collect();
        let reader = self.catalog_reader.read_guard();
        let schemas = reader.iter_schemas(&self.auth_context.database)?;
        let user_reader = self.user_info_reader.read_guard();
        let users = user_reader.get_all_users();
        let username_map = user_reader.get_user_name_map();

        Ok(schemas
            .flat_map(|schema| {
                schema.iter_table().map(|table| {
                    let table_state = states.get(&table.id.table_id);
                    let parallelism = table_state
                        .map(|state| extract_parallelism_from_table_state(state))
                        .unwrap_or("unknown".to_string());

                    OwnedRow::new(vec![
                        Some(ScalarImpl::Int32(table.id.table_id as i32)),
                        Some(ScalarImpl::Utf8(table.name().into())),
                        Some(ScalarImpl::Int32(schema.id() as i32)),
                        Some(ScalarImpl::Int32(table.owner as i32)),
                        Some(ScalarImpl::Utf8(table.create_sql().into())),
                        Some(ScalarImpl::Utf8(
                            get_acl_items(
                                &Object::TableId(table.id.table_id),
                                true,
                                &users,
                                username_map,
                            )
                            .into(),
                        )),
                        Some(ScalarImpl::Utf8(parallelism.to_uppercase().into())),
                        table.initialized_at_epoch.map(|e| e.as_scalar()),
                        table.created_at_epoch.map(|e| e.as_scalar()),
                    ])
                })
            })
            .collect_vec())
    }
}
