// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::{fs::File, pin::Pin};

use crate::error::{BallistaError, Result};
use crate::execution_plans::{ShuffleWriterExec, UnresolvedShuffleExec};
use crate::memory_stream::MemoryStream;
use crate::serde::scheduler::PartitionStats;

use crate::config::BallistaConfig;
use datafusion::arrow::error::Result as ArrowResult;
use datafusion::arrow::{
    array::{
        ArrayBuilder, ArrayRef, StructArray, StructBuilder, UInt64Array, UInt64Builder,
    },
    datatypes::{DataType, Field, SchemaRef},
    ipc::reader::FileReader,
    ipc::writer::FileWriter,
    record_batch::RecordBatch,
};
use datafusion::execution::context::{ExecutionConfig, ExecutionContext};
use datafusion::logical_plan::Operator;
use datafusion::physical_optimizer::coalesce_batches::CoalesceBatches;
use datafusion::physical_optimizer::merge_exec::AddCoalescePartitionsExec;
use datafusion::physical_optimizer::optimizer::PhysicalOptimizerRule;
use datafusion::physical_plan::coalesce_batches::CoalesceBatchesExec;
use datafusion::physical_plan::coalesce_partitions::CoalescePartitionsExec;
use datafusion::physical_plan::csv::CsvExec;
use datafusion::physical_plan::expressions::{BinaryExpr, Column, Literal};
use datafusion::physical_plan::filter::FilterExec;
use datafusion::physical_plan::hash_aggregate::HashAggregateExec;
use datafusion::physical_plan::hash_join::HashJoinExec;
use datafusion::physical_plan::parquet::ParquetExec;
use datafusion::physical_plan::projection::ProjectionExec;
use datafusion::physical_plan::sort::SortExec;
use datafusion::physical_plan::{
    AggregateExpr, ExecutionPlan, PhysicalExpr, RecordBatchStream, SQLMetric,
};
use futures::{future, Stream, StreamExt};
use std::time::Instant;

/// Stream data to disk in Arrow IPC format

pub async fn write_stream_to_disk(
    stream: &mut Pin<Box<dyn RecordBatchStream + Send + Sync>>,
    path: &str,
    disk_write_metric: Arc<SQLMetric>,
) -> Result<PartitionStats> {
    let file = File::create(&path).map_err(|e| {
        BallistaError::General(format!(
            "Failed to create partition file at {}: {:?}",
            path, e
        ))
    })?;

    let mut num_rows = 0;
    let mut num_batches = 0;
    let mut num_bytes = 0;
    let mut writer = FileWriter::try_new(file, stream.schema().as_ref())?;

    while let Some(result) = stream.next().await {
        let batch = result?;

        let batch_size_bytes: usize = batch
            .columns()
            .iter()
            .map(|array| array.get_array_memory_size())
            .sum();
        num_batches += 1;
        num_rows += batch.num_rows();
        num_bytes += batch_size_bytes;

        let start = Instant::now();
        writer.write(&batch)?;
        disk_write_metric.add_elapsed(start);
    }
    let start = Instant::now();
    writer.finish()?;
    disk_write_metric.add_elapsed(start);
    Ok(PartitionStats::new(
        Some(num_rows as u64),
        Some(num_batches),
        Some(num_bytes as u64),
    ))
}

pub async fn collect_stream(
    stream: &mut Pin<Box<dyn RecordBatchStream + Send + Sync>>,
) -> Result<Vec<RecordBatch>> {
    let mut batches = vec![];
    while let Some(batch) = stream.next().await {
        batches.push(batch?);
    }
    Ok(batches)
}

pub fn produce_diagram(filename: &str, stages: &[Arc<ShuffleWriterExec>]) -> Result<()> {
    let write_file = File::create(filename)?;
    let mut w = BufWriter::new(&write_file);
    writeln!(w, "digraph G {{")?;

    // draw stages and entities
    for stage in stages {
        writeln!(w, "\tsubgraph cluster{} {{", stage.stage_id())?;
        writeln!(w, "\t\tlabel = \"Stage {}\";", stage.stage_id())?;
        let mut id = AtomicUsize::new(0);
        build_exec_plan_diagram(
            &mut w,
            stage.children()[0].as_ref(),
            stage.stage_id(),
            &mut id,
            true,
        )?;
        writeln!(w, "\t}}")?;
    }

    // draw relationships
    for stage in stages {
        let mut id = AtomicUsize::new(0);
        build_exec_plan_diagram(
            &mut w,
            stage.children()[0].as_ref(),
            stage.stage_id(),
            &mut id,
            false,
        )?;
    }

    write!(w, "}}")?;
    Ok(())
}

fn build_exec_plan_diagram(
    w: &mut BufWriter<&File>,
    plan: &dyn ExecutionPlan,
    stage_id: usize,
    id: &mut AtomicUsize,
    draw_entity: bool,
) -> Result<usize> {
    let operator_str = if plan.as_any().downcast_ref::<HashAggregateExec>().is_some() {
        "HashAggregateExec"
    } else if plan.as_any().downcast_ref::<SortExec>().is_some() {
        "SortExec"
    } else if plan.as_any().downcast_ref::<ProjectionExec>().is_some() {
        "ProjectionExec"
    } else if plan.as_any().downcast_ref::<HashJoinExec>().is_some() {
        "HashJoinExec"
    } else if plan.as_any().downcast_ref::<ParquetExec>().is_some() {
        "ParquetExec"
    } else if plan.as_any().downcast_ref::<CsvExec>().is_some() {
        "CsvExec"
    } else if plan.as_any().downcast_ref::<FilterExec>().is_some() {
        "FilterExec"
    } else if plan.as_any().downcast_ref::<ShuffleWriterExec>().is_some() {
        "ShuffleWriterExec"
    } else if plan
        .as_any()
        .downcast_ref::<UnresolvedShuffleExec>()
        .is_some()
    {
        "UnresolvedShuffleExec"
    } else if plan
        .as_any()
        .downcast_ref::<CoalesceBatchesExec>()
        .is_some()
    {
        "CoalesceBatchesExec"
    } else if plan
        .as_any()
        .downcast_ref::<CoalescePartitionsExec>()
        .is_some()
    {
        "CoalescePartitionsExec"
    } else {
        println!("Unknown: {:?}", plan);
        "Unknown"
    };

    let node_id = id.load(Ordering::SeqCst);
    id.store(node_id + 1, Ordering::SeqCst);

    if draw_entity {
        writeln!(
            w,
            "\t\tstage_{}_exec_{} [shape=box, label=\"{}\"];",
            stage_id, node_id, operator_str
        )?;
    }
    for child in plan.children() {
        if let Some(shuffle) = child.as_any().downcast_ref::<UnresolvedShuffleExec>() {
            if !draw_entity {
                writeln!(
                    w,
                    "\tstage_{}_exec_1 -> stage_{}_exec_{};",
                    shuffle.stage_id, stage_id, node_id
                )?;
            }
        } else {
            // relationships within same entity
            let child_id =
                build_exec_plan_diagram(w, child.as_ref(), stage_id, id, draw_entity)?;
            if draw_entity {
                writeln!(
                    w,
                    "\t\tstage_{}_exec_{} -> stage_{}_exec_{};",
                    stage_id, child_id, stage_id, node_id
                )?;
            }
        }
    }
    Ok(node_id)
}

/// Create a DataFusion context that is compatible with Ballista
pub fn create_datafusion_context(config: &BallistaConfig) -> ExecutionContext {
    let config =
        ExecutionConfig::new().with_concurrency(config.default_shuffle_partitions());
    ExecutionContext::with_config(config)
}

pub struct WrappedStream {
    stream: Pin<Box<dyn Stream<Item = ArrowResult<RecordBatch>> + Send + Sync>>,
    schema: SchemaRef,
}

impl WrappedStream {
    pub fn new(
        stream: Pin<Box<dyn Stream<Item = ArrowResult<RecordBatch>> + Send + Sync>>,
        schema: SchemaRef,
    ) -> Self {
        Self { stream, schema }
    }
}

impl RecordBatchStream for WrappedStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

impl Stream for WrappedStream {
    type Item = ArrowResult<RecordBatch>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.stream.poll_next_unpin(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}
