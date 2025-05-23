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

//! For single-thread flow worker

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use common_telemetry::info;
use dfir_rs::scheduled::graph::Dfir;
use enum_as_inner::EnumAsInner;
use snafu::ensure;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

use crate::adapter::FlowId;
use crate::compute::{Context, DataflowState, ErrCollector};
use crate::error::{Error, FlowAlreadyExistSnafu, InternalSnafu, UnexpectedSnafu};
use crate::expr::{Batch, GlobalId};
use crate::plan::TypedPlan;
use crate::repr::{self, DiffRow};

pub type SharedBuf = Arc<Mutex<VecDeque<DiffRow>>>;

type ReqId = usize;

/// Create both worker(`!Send`) and worker handle(`Send + Sync`)
pub fn create_worker<'a>() -> (WorkerHandle, Worker<'a>) {
    let (itc_client, itc_server) = create_inter_thread_call();
    let worker_handle = WorkerHandle {
        itc_client,
        shutdown: AtomicBool::new(false),
    };
    let worker = Worker {
        task_states: BTreeMap::new(),
        itc_server: Arc::new(Mutex::new(itc_server)),
    };
    (worker_handle, worker)
}

/// ActiveDataflowState is a wrapper around `Dfir` and `DataflowState`
pub(crate) struct ActiveDataflowState<'subgraph> {
    df: Dfir<'subgraph>,
    state: DataflowState,
    err_collector: ErrCollector,
}

impl std::fmt::Debug for ActiveDataflowState<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveDataflowState")
            .field("df", &"<Dfir>")
            .field("state", &self.state)
            .field("err_collector", &self.err_collector)
            .finish()
    }
}

impl Default for ActiveDataflowState<'_> {
    fn default() -> Self {
        ActiveDataflowState {
            df: Dfir::new(),
            state: DataflowState::default(),
            err_collector: ErrCollector::default(),
        }
    }
}

impl<'subgraph> ActiveDataflowState<'subgraph> {
    /// Create a new render context, assigned with given global id
    pub fn new_ctx<'ctx>(&'ctx mut self, global_id: GlobalId) -> Context<'ctx, 'subgraph>
    where
        'subgraph: 'ctx,
    {
        Context {
            id: global_id,
            df: &mut self.df,
            compute_state: &mut self.state,
            err_collector: self.err_collector.clone(),
            input_collection: Default::default(),
            local_scope: Default::default(),
            input_collection_batch: Default::default(),
            local_scope_batch: Default::default(),
        }
    }

    pub fn set_current_ts(&mut self, ts: repr::Timestamp) {
        self.state.set_current_ts(ts);
    }

    pub fn set_last_exec_time(&mut self, ts: repr::Timestamp) {
        self.state.set_last_exec_time(ts);
    }

    /// Run all available subgraph
    ///
    /// return true if any subgraph actually executed
    pub fn run_available(&mut self) -> bool {
        self.state.run_available_with_schedule(&mut self.df)
    }
}

#[derive(Debug)]
pub struct WorkerHandle {
    itc_client: InterThreadCallClient,
    shutdown: AtomicBool,
}

impl WorkerHandle {
    /// create task, return task id
    pub async fn create_flow(&self, create_reqs: Request) -> Result<Option<FlowId>, Error> {
        ensure!(
            matches!(create_reqs, Request::Create { .. }),
            InternalSnafu {
                reason: format!(
                    "Flow Node/Worker itc failed, expect Request::Create, found {create_reqs:?}"
                ),
            }
        );

        let ret = self.itc_client.call_with_resp(create_reqs).await?;
        ret.into_create().map_err(|ret| {
            InternalSnafu {
                reason: format!(
                    "Flow Node/Worker itc failed, expect Response::Create, found {ret:?}"
                ),
            }
            .build()
        })?
    }

    /// remove task, return task id
    pub async fn remove_flow(&self, flow_id: FlowId) -> Result<bool, Error> {
        let req = Request::Remove { flow_id };

        let ret = self.itc_client.call_with_resp(req).await?;

        ret.into_remove().map_err(|ret| {
            InternalSnafu {
                reason: format!("Flow Node/Worker failed, expect Response::Remove, found {ret:?}"),
            }
            .build()
        })
    }

    /// trigger running the worker, will not block, and will run the worker parallelly
    ///
    /// will set the current timestamp to `now` for all dataflows before running them
    ///
    /// `blocking` indicate whether it will wait til all dataflows are finished computing if true or
    /// just start computing and return immediately if false
    ///
    /// the returned error is unrecoverable, and the worker should be shutdown/rebooted
    pub async fn run_available(&self, now: repr::Timestamp, blocking: bool) -> Result<(), Error> {
        common_telemetry::trace!("Running available with blocking={}", blocking);
        if blocking {
            let resp = self
                .itc_client
                .call_with_resp(Request::RunAvail { now, blocking })
                .await?;
            common_telemetry::trace!("Running available with response={:?}", resp);
            Ok(())
        } else {
            self.itc_client
                .call_no_resp(Request::RunAvail { now, blocking })
        }
    }

    pub async fn contains_flow(&self, flow_id: FlowId) -> Result<bool, Error> {
        let req = Request::ContainTask { flow_id };
        let ret = self.itc_client.call_with_resp(req).await?;

        ret.into_contain_task().map_err(|ret| {
            InternalSnafu {
                reason: format!(
                    "Flow Node/Worker itc failed, expect Response::ContainTask, found {ret:?}"
                ),
            }
            .build()
        })
    }

    /// shutdown the worker
    pub fn shutdown(&self) -> Result<(), Error> {
        if !self.shutdown.fetch_or(true, Ordering::SeqCst) {
            self.itc_client.call_no_resp(Request::Shutdown)
        } else {
            UnexpectedSnafu {
                reason: "Worker already shutdown",
            }
            .fail()
        }
    }

    pub async fn get_state_size(&self) -> Result<BTreeMap<FlowId, usize>, Error> {
        let ret = self
            .itc_client
            .call_with_resp(Request::QueryStateSize)
            .await?;
        ret.into_query_state_size().map_err(|ret| {
            InternalSnafu {
                reason: format!(
                    "Flow Node/Worker itc failed, expect Response::QueryStateSize, found {ret:?}"
                ),
            }
            .build()
        })
    }

    pub async fn get_last_exec_time_map(&self) -> Result<BTreeMap<FlowId, i64>, Error> {
        let ret = self
            .itc_client
            .call_with_resp(Request::QueryLastExecTimeMap)
            .await?;
        ret.into_query_last_exec_time_map().map_err(|ret| {
            InternalSnafu {
                reason: format!(
                    "Flow Node/Worker get_last_exec_time_map failed, expect Response::QueryLastExecTimeMap, found {ret:?}"
                ),
            }
            .build()
        })
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        if let Err(ret) = self.shutdown() {
            common_telemetry::error!(
                ret;
                "While dropping Worker Handle, failed to shutdown worker, worker might be in inconsistent state."
            );
        } else {
            info!("Flow Worker shutdown due to Worker Handle dropped.")
        }
    }
}

/// The actual worker that does the work and contain active state
#[derive(Debug)]
pub struct Worker<'subgraph> {
    /// Task states
    pub(crate) task_states: BTreeMap<FlowId, ActiveDataflowState<'subgraph>>,
    itc_server: Arc<Mutex<InterThreadCallServer>>,
}

impl<'s> Worker<'s> {
    #[allow(clippy::too_many_arguments)]
    pub fn create_flow(
        &mut self,
        flow_id: FlowId,
        plan: TypedPlan,
        sink_id: GlobalId,
        sink_sender: mpsc::UnboundedSender<Batch>,
        source_ids: &[GlobalId],
        src_recvs: Vec<broadcast::Receiver<Batch>>,
        // TODO(discord9): set expire duration for all arrangement and compare to sys timestamp instead
        expire_after: Option<repr::Duration>,
        or_replace: bool,
        create_if_not_exists: bool,
        err_collector: ErrCollector,
    ) -> Result<Option<FlowId>, Error> {
        let already_exist = self.task_states.contains_key(&flow_id);
        match (create_if_not_exists, or_replace, already_exist) {
            // if replace, ignore that old flow exists
            (_, true, true) => {
                info!("Replacing flow with id={}", flow_id);
            }
            (false, false, true) => FlowAlreadyExistSnafu { id: flow_id }.fail()?,
            // already exists, and not replace, return None
            (true, false, true) => {
                info!("Flow with id={} already exists, do nothing", flow_id);
                return Ok(None);
            }
            // continue as normal
            (_, _, false) => (),
        }

        let mut cur_task_state = ActiveDataflowState::<'s> {
            err_collector,
            ..Default::default()
        };
        cur_task_state.state.set_expire_after(expire_after);

        {
            let mut ctx = cur_task_state.new_ctx(sink_id);
            for (source_id, src_recv) in source_ids.iter().zip(src_recvs) {
                let bundle = ctx.render_source_batch(src_recv)?;
                ctx.insert_global_batch(*source_id, bundle);
            }

            let rendered = ctx.render_plan_batch(plan)?;
            ctx.render_unbounded_sink_batch(rendered, sink_sender);
        }
        self.task_states.insert(flow_id, cur_task_state);
        Ok(Some(flow_id))
    }

    /// remove task, return true if a task is removed
    pub fn remove_flow(&mut self, flow_id: FlowId) -> bool {
        self.task_states.remove(&flow_id).is_some()
    }

    /// Run the worker, blocking, until shutdown signal is received
    pub fn run(&mut self) {
        loop {
            let (req, ret_tx) = if let Some(ret) = self.itc_server.blocking_lock().blocking_recv() {
                ret
            } else {
                common_telemetry::error!(
                    "Worker's itc server has been closed unexpectedly, shutting down worker now."
                );
                break;
            };

            let ret = self.handle_req(req);
            match (ret, ret_tx) {
                (Ok(Some(resp)), Some(ret_tx)) => {
                    if let Err(err) = ret_tx.send(resp) {
                        common_telemetry::error!(
                            err;
                            "Result receiver is dropped, can't send result"
                        );
                    };
                }
                (Ok(None), None) => continue,
                (Ok(Some(resp)), None) => {
                    common_telemetry::error!(
                        "Expect no result for current request, but found {resp:?}"
                    )
                }
                (Ok(None), Some(_)) => {
                    common_telemetry::error!("Expect result for current request, but found nothing")
                }
                (Err(()), _) => {
                    break;
                }
            }
        }
    }

    /// run with tick acquired from tick manager(usually means system time)
    /// TODO(discord9): better tick management
    pub fn run_tick(&mut self, now: repr::Timestamp) {
        for (_flow_id, task_state) in self.task_states.iter_mut() {
            task_state.set_current_ts(now);
            task_state.set_last_exec_time(now);
            task_state.run_available();
        }
    }
    /// handle request, return response if any, Err if receive shutdown signal
    ///
    /// return `Err(())` if receive shutdown request
    fn handle_req(&mut self, req: Request) -> Result<Option<Response>, ()> {
        let ret = match req {
            Request::Create {
                flow_id,
                plan,
                sink_id,
                sink_sender,
                source_ids,
                src_recvs,
                expire_after,
                or_replace,
                create_if_not_exists,
                err_collector,
            } => {
                let task_create_result = self.create_flow(
                    flow_id,
                    plan,
                    sink_id,
                    sink_sender,
                    &source_ids,
                    src_recvs,
                    expire_after,
                    or_replace,
                    create_if_not_exists,
                    err_collector,
                );
                Some(Response::Create {
                    result: task_create_result,
                })
            }
            Request::Remove { flow_id } => {
                let ret = self.remove_flow(flow_id);
                Some(Response::Remove { result: ret })
            }
            Request::RunAvail { now, blocking } => {
                self.run_tick(now);
                if blocking {
                    Some(Response::RunAvail)
                } else {
                    None
                }
            }
            Request::ContainTask { flow_id } => {
                let ret = self.task_states.contains_key(&flow_id);
                Some(Response::ContainTask { result: ret })
            }
            Request::Shutdown => return Err(()),
            Request::QueryStateSize => {
                let mut ret = BTreeMap::new();
                for (flow_id, task_state) in self.task_states.iter() {
                    ret.insert(*flow_id, task_state.state.get_state_size());
                }
                Some(Response::QueryStateSize { result: ret })
            }
            Request::QueryLastExecTimeMap => {
                let mut ret = BTreeMap::new();
                for (flow_id, task_state) in self.task_states.iter() {
                    if let Some(last_exec_time) = task_state.state.last_exec_time() {
                        ret.insert(*flow_id, last_exec_time);
                    }
                }
                Some(Response::QueryLastExecTimeMap { result: ret })
            }
        };
        Ok(ret)
    }
}

#[derive(Debug, EnumAsInner)]
pub enum Request {
    Create {
        flow_id: FlowId,
        plan: TypedPlan,
        sink_id: GlobalId,
        sink_sender: mpsc::UnboundedSender<Batch>,
        source_ids: Vec<GlobalId>,
        src_recvs: Vec<broadcast::Receiver<Batch>>,
        expire_after: Option<repr::Duration>,
        or_replace: bool,
        create_if_not_exists: bool,
        err_collector: ErrCollector,
    },
    Remove {
        flow_id: FlowId,
    },
    /// Trigger the worker to run, useful after input buffer is full
    RunAvail {
        now: repr::Timestamp,
        blocking: bool,
    },
    ContainTask {
        flow_id: FlowId,
    },
    Shutdown,
    QueryStateSize,
    QueryLastExecTimeMap,
}

#[derive(Debug, EnumAsInner)]
enum Response {
    Create {
        result: Result<Option<FlowId>, Error>,
        // TODO(discord9): add flow err_collector
    },
    Remove {
        result: bool,
    },
    ContainTask {
        result: bool,
    },
    RunAvail,
    QueryStateSize {
        /// each flow tasks' state size
        result: BTreeMap<FlowId, usize>,
    },
    QueryLastExecTimeMap {
        /// each flow tasks' last execution time
        result: BTreeMap<FlowId, i64>,
    },
}

fn create_inter_thread_call() -> (InterThreadCallClient, InterThreadCallServer) {
    let (arg_send, arg_recv) = mpsc::unbounded_channel();
    let client = InterThreadCallClient {
        arg_sender: arg_send,
    };
    let server = InterThreadCallServer { arg_recv };
    (client, server)
}

#[derive(Debug)]
struct InterThreadCallClient {
    arg_sender: mpsc::UnboundedSender<(Request, Option<oneshot::Sender<Response>>)>,
}

impl InterThreadCallClient {
    /// call without response
    fn call_no_resp(&self, req: Request) -> Result<(), Error> {
        self.arg_sender.send((req, None)).map_err(from_send_error)
    }

    /// call with response
    async fn call_with_resp(&self, req: Request) -> Result<Response, Error> {
        let (tx, rx) = oneshot::channel();
        self.arg_sender
            .send((req, Some(tx)))
            .map_err(from_send_error)?;
        rx.await.map_err(|_| {
            InternalSnafu {
                reason: "Sender is dropped",
            }
            .build()
        })
    }
}

#[derive(Debug)]
struct InterThreadCallServer {
    pub arg_recv: mpsc::UnboundedReceiver<(Request, Option<oneshot::Sender<Response>>)>,
}

impl InterThreadCallServer {
    pub async fn recv(&mut self) -> Option<(Request, Option<oneshot::Sender<Response>>)> {
        self.arg_recv.recv().await
    }

    pub fn blocking_recv(&mut self) -> Option<(Request, Option<oneshot::Sender<Response>>)> {
        self.arg_recv.blocking_recv()
    }
}

fn from_send_error<T>(err: mpsc::error::SendError<T>) -> Error {
    InternalSnafu {
        // this `err` will simply display `channel closed`
        reason: format!(
            "Worker's receiver channel have been closed unexpected: {}",
            err
        ),
    }
    .build()
}

#[cfg(test)]
mod test {
    use tokio::sync::oneshot;

    use super::*;
    use crate::expr::Id;
    use crate::plan::Plan;
    use crate::repr::RelationType;

    #[test]
    fn drop_handle() {
        let (tx, rx) = oneshot::channel();
        let worker_thread_handle = std::thread::spawn(move || {
            let (handle, mut worker) = create_worker();
            tx.send(handle).unwrap();
            worker.run();
        });
        let handle = rx.blocking_recv().unwrap();
        drop(handle);
        worker_thread_handle.join().unwrap();
    }

    #[tokio::test]
    pub async fn test_simple_get_with_worker_and_handle() {
        let (tx, rx) = oneshot::channel();
        let worker_thread_handle = std::thread::spawn(move || {
            let (handle, mut worker) = create_worker();
            tx.send(handle).unwrap();
            worker.run();
        });
        let handle = rx.await.unwrap();
        let src_ids = vec![GlobalId::User(1)];
        let (tx, rx) = broadcast::channel::<Batch>(1024);
        let (sink_tx, mut sink_rx) = mpsc::unbounded_channel::<Batch>();
        let (flow_id, plan) = (
            1,
            TypedPlan {
                plan: Plan::Get {
                    id: Id::Global(GlobalId::User(1)),
                },
                schema: RelationType::new(vec![]).into_unnamed(),
            },
        );
        let create_reqs = Request::Create {
            flow_id,
            plan,
            sink_id: GlobalId::User(1),
            sink_sender: sink_tx,
            source_ids: src_ids,
            src_recvs: vec![rx],
            expire_after: None,
            or_replace: false,
            create_if_not_exists: true,
            err_collector: ErrCollector::default(),
        };
        assert_eq!(
            handle.create_flow(create_reqs).await.unwrap(),
            Some(flow_id)
        );
        tx.send(Batch::empty()).unwrap();
        handle.run_available(0, true).await.unwrap();
        assert_eq!(handle.get_state_size().await.unwrap().len(), 1);
        assert_eq!(sink_rx.recv().await.unwrap(), Batch::empty());
        drop(handle);
        worker_thread_handle.join().unwrap();
    }
}
