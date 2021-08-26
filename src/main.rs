use crate::network::{self, NetworkStatus};
use anyhow::Result;
use dotenv::dotenv;
use log::{debug, info};
use ncs::errors::NcsError::*;
use ncs::local_listen::*;
use ncs::meta::*;
use ncs::nc_listen::*;
use ncs::*;
use notify::{watcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;
use tokio::sync::mpsc as tokio_mpsc;
#[allow(unused)]
use tokio::time::{sleep, Duration};
// #[macro_use]
// extern crate anyhow;

macro_rules! terminate_send {
    ($tx:expr) => {
        let mut counter: u32 = 0;
        while let Err(e) = $tx.send(Command::Terminate(true)).await {
            info!("{:?}", e);
            counter += 1;
            if counter > 3 {
                break;
            }
        }
    };
}

async fn run() -> Result<bool> {
    // Can't update these environment variables by rerun.
    let username = env::var("NC_USERNAME").expect("NC_USERNAME not found");
    let password = env::var("NC_PASSWORD").expect("NC_PASSWORD not found");
    let host = env::var("NC_HOST").expect("NC_HOST not found");
    let host = fix_host(&host);

    let nc_info = NCInfo::new(username, password, host);

    let local_root_path = env::var("LOCAL_ROOT").expect("LOCAL_ROOT not found");
    let local_info = LocalInfo::new(local_root_path)?;

    // debug!("log_file: {}", local_info.get_logfile_name());

    let public_resource: PublicResource;
    if Path::new(local_info.get_cachefile_name().as_str()).exists() {
        // load cache
        let ncs_cache = load_cache(&local_info)?;
        let nc_state = NCState {
            latest_activity_id: ncs_cache.latest_activity_id,
        };
        let root_entry = json_entry2entry(ncs_cache.root_entry)?;
        public_resource = PublicResource::new(root_entry, nc_state);
    } else {
        // init
        if !network::is_online(&nc_info).await {
            return Err(NetworkOfflineError.into());
        }

        let (root, latest_activity_id) = init(&nc_info, &local_info).await?;
        let json_entry = {
            let root_ref = root.lock().map_err(|_| LockError)?;
            root2json_entry(&root_ref)?
        };
        save_cache(latest_activity_id.clone(), json_entry, &local_info)?;
        let nc_state = NCState {
            latest_activity_id: latest_activity_id,
        };
        public_resource = PublicResource::new(root, nc_state);
    }

    let public_resource = Arc::new(Mutex::new(public_resource));

    // to end with successful completion, watchers must be managed here.

    let (tx, rx) = std_mpsc::channel();
    let mut root_watcher = watcher(tx, StdDuration::from_secs(5)).unwrap();
    root_watcher.watch(&local_info.root_path, RecursiveMode::Recursive)?;
    let loceve_rx = Mutex::new(rx);

    let (tx, rx) = std_mpsc::channel();
    let mut meta_watcher = watcher(tx, StdDuration::from_secs(5)).unwrap();
    meta_watcher.watch(
        local_info.get_metadir_name().as_str(),
        RecursiveMode::Recursive,
    )?;
    let metaeve_rx = Mutex::new(rx);

    let (com_tx, mut com_rx) = tokio_mpsc::channel(32);

    let tx = com_tx.clone();
    let lci = local_info.clone();
    let nci = nc_info.clone();
    let watching_handle = tokio::spawn(async move {
        let res = watching(tx.clone(), loceve_rx, &lci, &nci).await;
        if let Err(e) = res {
            info!("{:?}", e);
            terminate_send!(tx);
        }
    });

    let tx = com_tx.clone();
    let lci = local_info.clone();
    let updateexcfile_handle = tokio::spawn(async move {
        let res = exc_list_update_watching(tx.clone(), metaeve_rx, &lci).await;
        if let Err(e) = res {
            info!("{:?}", e);
            terminate_send!(tx);
        }
    });

    let nc_state = {
        let pr_ref = public_resource.lock().map_err(|_| LockError)?;
        pr_ref.nc_state.clone()
    };
    let tx = com_tx.clone();
    let nci = nc_info.clone();
    let lci = local_info.clone();
    let nclisten_handle = tokio::spawn(async move {
        let res = nclistening(tx.clone(), &nci, &lci, nc_state.clone()).await;
        if let Err(e) = res {
            info!("{:?}", e);
            terminate_send!(tx);
        }
    });

    let tx = com_tx.clone();
    let control_handle = tokio::spawn(async move {
        let mut ln = String::new();
        let _ = std::io::stdin().read_line(&mut ln);
        let com = match ln.trim() {
            "RESET" => Command::HardRepair,
            _ => Command::Terminate(false),
        };
        // まだRESETでsend errorの時を考慮してない
        let res = tx.send(com).await;
        if let Err(e) = res {
            info!("{:?}", e);
            terminate_send!(tx);
        }
    });

    let mut network_status = network::status(&nc_info).await?;
    let mut nc2l_cancel_map = HashMap::new();
    let mut l2nc_cancel_set = HashSet::new();
    let mut offline_locevent_que: Vec<local_listen::LocalEvent> = Vec::new();
    let mut retry = false;
    while let Some(e) = com_rx.recv().await {
        match e {
            Command::LocEvent(ev) => match network_status {
                NetworkStatus::Connect => {
                    let pr_ref = public_resource.lock().map_err(|_| LockError)?;
                    let res = deal_local_event(
                        ev,
                        &pr_ref.root,
                        &nc_info,
                        &local_info,
                        &mut nc2l_cancel_map,
                        &mut l2nc_cancel_set,
                    )
                    .await;
                    if let Err(e) = res {
                        info!("{:?}", e);
                        // break;
                    }
                }
                NetworkStatus::Disconnect | NetworkStatus::Err(_) => {
                    debug!("LocEvent({:?}) @ offline", ev);
                    offline_locevent_que.push(ev);
                }
            },
            Command::NCEvents(ev_vec, new_state) => match network_status {
                NetworkStatus::Connect => {
                    debug!("NCEvents({:?})", new_state);
                    let mut pr_ref = public_resource.lock().map_err(|_| LockError)?;

                    if pr_ref.nc_state.eq_or_newer_than(&new_state) {
                        continue;
                    }

                    pr_ref.nc_state = new_state;
                    let res = update_and_download(
                        ev_vec,
                        &pr_ref.root,
                        &nc_info,
                        &local_info,
                        &mut nc2l_cancel_map,
                        &mut l2nc_cancel_set,
                        false,
                    )
                    .await;
                    if let Err(e) = res {
                        info!("{:?}", e);
                        // break;
                    }
                }
                NetworkStatus::Disconnect | NetworkStatus::Err(_) => {
                    info!("It should be unreachable branch. something wrong.");
                }
            },
            Command::UpdateExcFile | Command::UpdateConfigFile => {
                retry = true;
                break;
                /*
                drop(root_watcher);
                drop(meta_watcher);
                com_rx.close();
                nclisten_handle.await?;
                watching_handle.await?;
                updateexcfile_handle.await?;
                control_handle.abort();
                return Ok(true);
                */
            }
            Command::HardRepair => {
                drop(root_watcher);
                drop(meta_watcher);
                com_rx.close();
                nclisten_handle.await?;
                watching_handle.await?;
                updateexcfile_handle.await?;
                control_handle.abort();
                repair::all_delete(&local_info)?;
                return Ok(true);
            }
            Command::NormalRepair => {
                let events = {
                    let mut pr_ref = public_resource.lock().map_err(|_| LockError)?;
                    get_ncevents(&nc_info, &local_info, &mut pr_ref.nc_state).await?
                };
                repair::normal_repair(&local_info, &nc_info, &public_resource, events).await?;
                sleep(Duration::from_secs(20)).await;
                retry = true;
                break;
            }
            Command::NetworkConnect => match network_status {
                NetworkStatus::Connect => (),
                _ => {
                    // Reconnect situation

                    /*
                    let nc_events;
                    {
                        let mut pr_ref = public_resource.lock().map_err(|_| LockError)?;
                        nc_events =
                            get_ncevents(&nc_info, &local_info, &mut pr_ref.nc_state).await?;
                        info!("nc_state: {:?}", pr_ref.nc_state);
                    }
                    repair::normal_repair(&local_info, &nc_info, &public_resource, nc_events)
                        .await?;
                    // network_status = NetworkStatus::Connect;
                    sleep(Duration::from_secs(20)).await;
                    */

                    let res = repair::soft_repair(
                        &local_info,
                        &nc_info,
                        &public_resource,
                        offline_locevent_que.drain(..).collect(),
                        com_tx.clone(),
                        &mut nc2l_cancel_map,
                        &mut l2nc_cancel_set,
                    )
                    .await?;
                    if res {
                        retry = true;
                        break;
                    } else {
                        network_status = NetworkStatus::Connect;
                        retry = false;
                    }
                }
            },
            Command::NetworkDisconnect => match network_status {
                NetworkStatus::Connect => {
                    // disconnect situation
                    nc2l_cancel_map = HashMap::new();
                    l2nc_cancel_set = HashSet::new();
                    network_status = NetworkStatus::Disconnect;
                }
                _ => (),
            },
            Command::Terminate(r) => {
                retry = r;
                break;
            }
        }
    }

    drop(root_watcher);
    drop(meta_watcher);

    com_rx.close();

    nclisten_handle.await?;
    watching_handle.await?;
    updateexcfile_handle.await?;
    control_handle.abort();

    let pr_ref = public_resource.lock().map_err(|_| LockError)?;
    let json_entry = {
        let r = pr_ref.root.lock().map_err(|_| LockError)?;
        debug!("\n{}", r.get_tree());
        root2json_entry(&r)?
    };
    save_cache(
        pr_ref.nc_state.latest_activity_id.clone(),
        json_entry,
        &local_info,
    )?;

    Ok(retry)
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    env_logger::init();

    while run().await? {}

    Ok(())
}

async fn init(nc_info: &NCInfo, local_info: &LocalInfo) -> Result<(ArcEntry, String)> {
    let root_entry = from_nc_all(nc_info, local_info, "/").await?;
    let latest_activity_id = get_latest_activity_id(nc_info).await?;
    debug!("{}", latest_activity_id);

    init_local_entries(nc_info, local_info, &root_entry, "").await?;

    {
        let r = root_entry.lock().map_err(|_| LockError)?;
        println!("\n{}", r.get_tree());
    }

    Ok((root_entry, latest_activity_id))
}
