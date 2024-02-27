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

use risingwave_common::types::Fields;
use risingwave_frontend_macro::system_catalog;

use crate::catalog::system_catalog::rw_catalog::rw_types::read_rw_types;
use crate::catalog::system_catalog::SysCatalogReaderImpl;
use crate::error::Result;

/// The catalog `pg_type` stores information about data types.
/// Ref: [`https://www.postgresql.org/docs/current/catalog-pg-type.html`]
#[derive(Fields)]
struct PgType {
    #[primary_key]
    oid: i32,
    typname: String,
    typelem: i32,
    typarray: i32,
    typinput: String,
    typnotnull: bool,
    typbasetype: i32,
    typtypmod: i32,
    typcollation: i32,
    typlen: i32,
    typnamespace: i32,
    typtype: &'static str,
    typrelid: i32,
    typdefault: Option<String>,
    typcategory: Option<String>,
    typreceive: Option<i32>,
}

#[system_catalog(table, "pg_catalog.pg_type")]
fn read_pg_type(reader: &SysCatalogReaderImpl) -> Result<Vec<PgType>> {
    let catalog_reader = reader.catalog_reader.read_guard();
    let pg_catalog_id = catalog_reader
        .get_schema_by_name(&reader.auth_context.database, "pg_catalog")?
        .id() as i32;

    let rw_types = read_rw_types(reader)?;

    let mut rows = Vec::with_capacity(rw_types.len());
    for rw_type in rw_types {
        rows.push(PgType {
            oid: rw_type.id as i32,
            typname: rw_type.name,
            typelem: rw_type.typelem,
            typarray: rw_type.typarray,
            typinput: rw_type.input_oid,
            typnotnull: false,
            typbasetype: 0,
            typtypmod: -1,
            typcollation: 0,
            typlen: 0,
            typnamespace: pg_catalog_id,
            typtype: "b",
            typrelid: 0,
            typdefault: None,
            typcategory: None,
            typreceive: None,
        });
    }
    Ok(rows)
}
