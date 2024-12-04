// Copyright 2023 Greptime Team
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

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use client::DEFAULT_CATALOG_NAME;
use common_catalog::consts::{is_readonly_schema, MITO_ENGINE};
use common_catalog::format_full_table_name;
use common_meta::key::catalog_name::CatalogManager;
use common_meta::key::schema_name::SchemaManager;
use common_meta::key::TableMetadataManager;
use common_meta::kv_backend::chroot::ChrootKvBackend;
use common_meta::kv_backend::etcd::EtcdStore;
use common_meta::kv_backend::KvBackendRef;
use common_telemetry::{error, info};
use etcd_client::Client;
use futures::future::join_all;
use futures::TryStreamExt;
use meta_srv::error::ConnectEtcdSnafu;
use meta_srv::Result as MetaResult;
use snafu::ResultExt;
use store_api::storage::RegionId;
use tokio::sync::Semaphore;
use tracing_appender::non_blocking::WorkerGuard;

use crate::cli::database::DatabaseClient;
use crate::cli::{Instance, Tool};
use crate::error::{self, Result};

#[derive(Debug, Default, Parser)]
pub struct FlushCommand {
    /// Server address to connect
    #[clap(long)]
    addr: String,

    /// The basic authentication for connecting to the server
    #[clap(long)]
    auth_basic: Option<String>,

    /// The timeout of invoking the database.
    ///
    /// It is used to override the server-side timeout setting.
    /// The default behavior will disable server-side default timeout(i.e. `0s`).
    #[clap(long, value_parser = humantime::parse_duration)]
    timeout: Option<Duration>,

    /// Store server address default to etcd store.
    #[clap(long, value_delimiter = ',', default_value="127.0.0.1:2379", num_args=1..)]
    store_addr: Vec<String>,

    /// If it's not empty, the metasrv will store all data with this key prefix.
    #[clap(long, default_value = "")]
    store_key_prefix: String,

    /// Maximum number of operations permitted in a transaction.
    #[clap(long, default_value = "128")]
    max_txn_ops: usize,

    /// The number of flush tasks to execute in parallel.
    #[clap(long, default_value = "128")]
    parallelism: usize,
}

impl FlushCommand {
    async fn create_etcd_client(&self) -> MetaResult<KvBackendRef> {
        let etcd_client = Client::connect(&self.store_addr, None)
            .await
            .context(ConnectEtcdSnafu)?;
        let kv_backend = {
            let etcd_backend = EtcdStore::with_etcd_client(etcd_client, self.max_txn_ops);
            if !self.store_key_prefix.is_empty() {
                Arc::new(ChrootKvBackend::new(
                    self.store_key_prefix.clone().into_bytes(),
                    etcd_backend,
                ))
            } else {
                etcd_backend
            }
        };

        Ok(kv_backend)
    }

    pub async fn build(&self, guard: Vec<WorkerGuard>) -> Result<Instance> {
        let database_client = DatabaseClient::new(
            self.addr.clone(),
            DEFAULT_CATALOG_NAME.to_string(),
            self.auth_basic.clone(),
            // Treats `None` as `0s` to disable server-side default timeout.
            self.timeout.unwrap_or_default(),
        );

        let kv_backend = self
            .create_etcd_client()
            .await
            .context(error::BuildKvBackendSnafu)?;

        Ok(Instance {
            tool: Box::new(Flush {
                database_client,
                kv_backend,
                parallelism: self.parallelism,
            }),
            _guard: guard,
        })
    }
}

pub struct Flush {
    database_client: DatabaseClient,
    kv_backend: KvBackendRef,
    parallelism: usize,
}

#[async_trait::async_trait]
impl Tool for Flush {
    async fn do_work(&self) -> Result<()> {
        let catalog_manager = CatalogManager::new(self.kv_backend.clone());
        let schema_manager = SchemaManager::new(self.kv_backend.clone());
        let table_metadata_manager = TableMetadataManager::new(self.kv_backend.clone());

        let semaphore = Arc::new(Semaphore::new(self.parallelism));
        let mut tasks = vec![];
        let catalogs = catalog_manager
            .catalog_names()
            .into_stream()
            .try_collect::<Vec<_>>()
            .await
            .context(error::FetchMetadataSnafu)?;

        for catalog in catalogs {
            let schemas = schema_manager
                .schema_names(&catalog)
                .into_stream()
                .try_collect::<Vec<_>>()
                .await
                .context(error::FetchMetadataSnafu)?;
            let schemas_total = schemas.len();

            for (idx, schema) in schemas.into_iter().enumerate() {
                if is_readonly_schema(&schema) {
                    info!(
                        "Ignored {catalog}.{schema}, processing: {}/{}",
                        idx + 1,
                        schemas_total
                    );
                    continue;
                }

                let tables = table_metadata_manager
                    .table_name_manager()
                    .tables(&catalog, &schema)
                    .into_stream()
                    .try_collect::<Vec<_>>()
                    .await
                    .context(error::FetchMetadataSnafu)?;
                let tables_total = tables.len();
                info!(
                    "Handling {catalog}.{schema}, found {} tables, processing: {}/{}",
                    tables_total,
                    idx + 1,
                    schemas_total
                );

                for (idx, (table_name, table_value)) in tables.into_iter().enumerate() {
                    let (table_info, table_route) = table_metadata_manager
                        .get_full_table_info(table_value.table_id())
                        .await
                        .context(error::FetchMetadataSnafu)?;
                    let table_id = table_value.table_id();

                    if let (Some(table_info), Some(table_route)) = (table_info, table_route) {
                        if table_info.table_info.meta.engine.as_str() == MITO_ENGINE {
                            for region_number in table_route.region_numbers() {
                                let moved_database_client = self.database_client.clone();
                                let moved_catalog = catalog.to_string();
                                let moved_schema = schema.to_string();
                                let moved_table_name = table_name.to_string();
                                let moved_semaphore = semaphore.clone();

                                tasks.push(async move {
                                    let _permit = moved_semaphore.acquire().await.unwrap();
                                    info!(
                                        "Flushing table: {} region_number: {}, processing: {}/{}",
                                        format_full_table_name(
                                            &moved_catalog,
                                            &moved_schema,
                                            &moved_table_name
                                        ),
                                        region_number,
                                        idx + 1,
                                        tables_total
                                    );
                                    moved_database_client
                                        .sql_in_public(
                                            &format!(
                                                "admin flush_region({});",
                                                RegionId::new(table_id, region_number).as_u64()
                                            ),
                                        )
                                        .await
                                        .map(|_| ())
                                        .inspect_err(
                                            |err| error!(err; "Failed to flush table: {}({})", format_full_table_name(&moved_catalog, &moved_schema, &moved_table_name), table_id),
                                        )
                                });
                            }
                        } else {
                            info!(
                                "Ignored table: {}({}), engine: {}",
                                format_full_table_name(&catalog, &schema, &table_name),
                                table_value.table_id(),
                                table_info.table_info.meta.engine,
                            )
                        }
                    } else {
                        info!(
                            "Ignored table: {}({}), table metadata not found!",
                            format_full_table_name(&catalog, &schema, &table_name),
                            table_value.table_id()
                        )
                    }
                }
            }
        }

        let _ = join_all(tasks)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        Ok(())
    }
}
