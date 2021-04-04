use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::db::UserId;
use crate::port::{ReadPort, WritePort};
use crate::stats;
use crate::util::SimpleResult;
use crate::{
    ConnectServerData, ConnectedServer, PermuterData, PermuterId, PermuterResult, PermuterWork,
    ServerUpdate, State,
};

const SERVER_WORK_QUEUE_SIZE: usize = 100;
const TIME_COST_MS_GUESS: f64 = 100.0;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    NeedWork,
    Update {
        permuter_id: PermuterId,
        time_cost_ms: f64,
        update: ServerUpdate,
    },
}

enum JobState {
    Loading,
    Loaded,
    Failed,
}

struct Job {
    state: JobState,
    energy: f64,
}

struct ServerState {
    min_priority: f64,
    jobs: HashMap<PermuterId, Job>,
}

async fn server_read(
    port: &mut ReadPort<'_>,
    who_id: &UserId,
    who_name: &str,
    server_state: &Mutex<ServerState>,
    state: &State,
    more_work_tx: mpsc::Sender<()>,
) -> SimpleResult<()> {
    loop {
        let msg = port.recv().await?;
        let msg: ServerMessage = serde_json::from_slice(&msg)?;
        let mut log_new = false;
        if let ServerMessage::Update {
            permuter_id: perm_id,
            mut update,
            time_cost_ms,
        } = msg
        {
            if let ServerUpdate::Result {
                ref mut compressed_source,
                has_source: true,
                ..
            } = &mut update
            {
                *compressed_source = Some(port.recv().await?);
            }
            let mut m = state.m.lock().unwrap();
            let mut server_state = server_state.lock().unwrap();

            // If we get back a message referring to a since-removed permuter,
            // no need to do anything.
            if let Some(job) = server_state.jobs.get_mut(&perm_id) {
                if let Some(perm) = m.permuters.get_mut(&perm_id) {
                    job.energy -= perm.energy_add * TIME_COST_MS_GUESS;
                    job.energy += perm.energy_add * time_cost_ms;

                    match update {
                        ServerUpdate::InitDone { .. } => {
                            job.state = JobState::Loaded;
                            log_new = true;
                        }
                        ServerUpdate::InitFailed { .. } | ServerUpdate::Disconnect => {
                            job.state = JobState::Failed;
                        }
                        ServerUpdate::Result { .. } => {}
                    }
                    perm.send_result(PermuterResult::Result(
                        who_id.clone(),
                        who_name.to_string(),
                        update,
                    ));
                }
            }
        }

        if log_new {
            state
                .log_stats(stats::Record::ServerNewFunction {
                    server: who_id.clone(),
                })
                .await?;
        }

        // Try requesting more work by sending a message to the writer thread.
        // If the queue is full (because the writer thread is blocked on a
        // send), drop the request to avoid an unbounded backlog.
        if let Err(TrySendError::Closed(_)) = more_work_tx.try_send(()) {
            break;
        }
    }
    Ok(())
}

enum ToSend {
    Work(PermuterWork),
    Add(UserId, String, Arc<PermuterData>),
    Remove,
}

async fn choose_work(server_state: &Mutex<ServerState>, state: &State) -> (PermuterId, ToSend) {
    let mut wait_for = None;
    loop {
        if let Some(waiter) = wait_for {
            waiter.await;
        }

        let mut m = state.m.lock().unwrap();
        let mut server_state = server_state.lock().unwrap();

        // If possible, send a new permuter.
        if let Some((&perm_id, perm)) = m
            .permuters
            .iter()
            .find(|(&perm_id, _)| !server_state.jobs.contains_key(&perm_id))
        {
            server_state.jobs.insert(
                perm_id,
                Job {
                    state: JobState::Loading,
                    energy: 0.0,
                },
            );
            return (
                perm_id,
                ToSend::Add(
                    perm.client_id.clone(),
                    perm.client_name.clone(),
                    perm.data.clone(),
                ),
            );
        }

        // If none, find an existing one to work on, or to remove.
        let mut best_cost = 0.0;
        let mut best: Option<(PermuterId, &mut Job)> = None;
        let min_priority = server_state.min_priority;
        for (&perm_id, job) in server_state.jobs.iter_mut() {
            if let Some(perm) = m.permuters.get(&perm_id) {
                if matches!(job.state, JobState::Loaded)
                    && !perm.stale
                    && perm.priority >= min_priority
                    && (best.is_none() || job.energy < best_cost)
                {
                    best_cost = job.energy;
                    best = Some((perm_id, job));
                }
            } else {
                server_state.jobs.remove(&perm_id);
                return (perm_id, ToSend::Remove);
            }
        }

        let (perm_id, job) = match best {
            None => {
                // Nothing to work on! Register to be notified when something
                // happens and go to sleep.
                wait_for = Some(state.new_work_notification.notified());
                continue;
            }
            Some(tup) => tup,
        };

        let perm = m.permuters.get_mut(&perm_id).unwrap();
        let work = match perm.work_queue.pop_front() {
            None => {
                // Chosen permuter is out of work. Ask it for more, and mark it
                // as stale. When it goes unstale all sleeping writers will be
                // notified.
                perm.send_result(PermuterResult::NeedWork);
                perm.stale = true;
                wait_for = None;
                continue;
            }
            Some(work) => work,
        };
        perm.semaphore.release();

        let min_energy = job.energy;
        job.energy += perm.energy_add * TIME_COST_MS_GUESS;

        // Adjust energies to be around zero, to avoid problems with float
        // imprecision, and to ensure that new permuters that come in with
        // energy zero will fit the schedule.
        for job in server_state.jobs.values_mut() {
            job.energy -= min_energy;
        }

        return (perm_id, ToSend::Work(work));
    }
}

async fn send_work(
    port: &mut WritePort<'_>,
    perm_id: PermuterId,
    to_send: ToSend,
) -> SimpleResult<()> {
    match to_send {
        ToSend::Work(PermuterWork { seed }) => {
            port.send_json(&json!({
                "type": "work",
                "permuter": perm_id,
                "seed": seed,
            }))
            .await?;
        }
        ToSend::Add(client_id, client_name, permuter) => {
            port.send_json(&json!({
                "type": "add",
                "permuter": perm_id,
                "client_id": client_id,
                "client_name": client_name,
                "data": &*permuter,
            }))
            .await?;
            port.send(&permuter.compressed_source).await?;
            port.send(&permuter.compressed_target_o_bin).await?;
        }
        ToSend::Remove => {
            port.send_json(&json!({
                "type": "remove",
                "permuter": perm_id,
            }))
            .await?;
        }
    };
    Ok(())
}

async fn server_write(
    port: &mut WritePort<'_>,
    server_state: &Mutex<ServerState>,
    state: &State,
    mut more_work_rx: mpsc::Receiver<()>,
) -> SimpleResult<()> {
    loop {
        let (perm_id, to_send) = choose_work(server_state, state).await;
        send_work(port, perm_id, to_send).await?;
        if matches!(more_work_rx.recv().await, None) {
            break;
        }
    }
    Ok(())
}

pub(crate) async fn handle_connect_server<'a>(
    mut read_port: ReadPort<'a>,
    mut write_port: WritePort<'a>,
    who_id: &UserId,
    who_name: &str,
    state: &State,
    data: ConnectServerData,
) -> SimpleResult<()> {
    write_port
        .send_json(&json!({
            "docker_image": &state.docker_image,
        }))
        .await?;

    let (more_work_tx, more_work_rx) = mpsc::channel(SERVER_WORK_QUEUE_SIZE);

    let mut server_state = Mutex::new(ServerState {
        min_priority: data.min_priority,
        jobs: HashMap::new(),
    });

    let id = state.m.lock().unwrap().servers.insert(ConnectedServer {
        min_priority: data.min_priority,
        num_cores: data.num_cores,
    });

    let r = tokio::try_join!(
        server_read(
            &mut read_port,
            who_id,
            who_name,
            &server_state,
            state,
            more_work_tx
        ),
        server_write(&mut write_port, &server_state, state, more_work_rx)
    );

    {
        let mut m = state.m.lock().unwrap();
        for (&perm_id, job) in &server_state.get_mut().unwrap().jobs {
            if let JobState::Loaded = job.state {
                if let Some(perm) = m.permuters.get_mut(&perm_id) {
                    perm.send_result(PermuterResult::Result(
                        who_id.clone(),
                        who_name.to_string(),
                        ServerUpdate::Disconnect,
                    ));
                }
            }
        }

        m.servers.remove(id);
    }
    r?;
    Ok(())
}
