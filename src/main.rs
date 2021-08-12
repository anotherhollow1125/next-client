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
use tokio::sync::mpsc as tokio_mpsc;
#[allow(unused)]
use tokio::time::{sleep, Duration};
// #[macro_use]
// extern crate anyhow;

async fn run(nc_info: &NCInfo, local_info: &LocalInfo) -> Result<()> {
    let public_resource: PublicResource;
    if Path::new(local_info.get_cachefile_name().as_str()).exists() {
        // load cache
        let ncs_cache = load_cache(local_info)?;
        let nc_state = NCState {
            latest_activity_id: ncs_cache.latest_activity_id,
        };
        let root_entry = json_entry2entry(ncs_cache.root_entry)?;
        public_resource = PublicResource::new(root_entry, nc_state);
    } else {
        // init
        let (root, latest_activity_id) = init(nc_info, local_info).await?;
        let json_entry = {
            let root_ref = root.lock().map_err(|_| LockError)?;
            root2json_entry(&root_ref)?
        };
        save_cache(latest_activity_id.clone(), json_entry, local_info)?;
        let nc_state = NCState {
            latest_activity_id: latest_activity_id,
        };
        public_resource = PublicResource::new(root, nc_state);
    }

    let public_resource = Arc::new(Mutex::new(public_resource));

    let (com_tx, mut com_rx) = tokio_mpsc::channel(32);

    let (tx, rx) = std_mpsc::channel();
    let mut watcher = watcher(tx, Duration::from_secs(5)).unwrap();
    watcher.watch(&local_info.root_path, RecursiveMode::Recursive)?;
    let loceve_rx = Mutex::new(rx);

    let tx = com_tx.clone();
    let lci = local_info.clone();
    let watching_handle = tokio::spawn(async move {
        let res = watching(tx.clone(), loceve_rx, &lci).await;
        if let Err(e) = res {
            info!("{:?}", e);
            let _ = tx.send(Command::Terminate).await;
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
            let _ = tx.send(Command::Terminate).await;
        }
    });

    let tx = com_tx.clone();
    let control_handle = tokio::spawn(async move {
        let mut ln = String::new();
        let _ = std::io::stdin().read_line(&mut ln);
        let com = match ln.trim() {
            "RESET" => Command::HardRepair,
            _ => Command::Terminate,
        };
        let res = tx.send(com).await;
        if let Err(e) = res {
            info!("{:?}", e);
        }
    });

    let mut nc2l_cancel_map = HashMap::new();
    let mut l2nc_cancel_set = HashSet::new();
    while let Some(e) = com_rx.recv().await {
        match e {
            Command::LocEvent(ev) => {
                let pr_ref = public_resource.lock().map_err(|_| LockError)?;
                let res = deal_local_event(
                    ev,
                    &pr_ref.root,
                    nc_info,
                    local_info,
                    &mut nc2l_cancel_map,
                    &mut l2nc_cancel_set,
                )
                .await;
                if let Err(e) = res {
                    info!("{:?}", e);
                    break;
                }
            }
            Command::NCEvents(ev_vec, new_state) => {
                debug!("NCEvents({:?})", new_state);
                let mut pr_ref = public_resource.lock().map_err(|_| LockError)?;
                pr_ref.nc_state = new_state;
                let res = update_and_download(
                    ev_vec,
                    &pr_ref.root,
                    nc_info,
                    local_info,
                    &mut nc2l_cancel_map,
                    &mut l2nc_cancel_set,
                )
                .await;
                if let Err(e) = res {
                    info!("{:?}", e);
                    break;
                }
            }
            Command::HardRepair => {
                watching_handle.abort();
                nclisten_handle.abort();
                control_handle.abort();
                repair::all_delete(local_info)?;
                return Ok(());
            }
            Command::Terminate => break,
        }
    }

    drop(watcher);
    watching_handle.abort();
    nclisten_handle.abort();
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
        local_info,
    )?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    env_logger::init();
    let username = env::var("NC_USERNAME").expect("NC_USERNAME not found");
    let password = env::var("NC_PASSWORD").expect("NC_PASSWORD not found");
    let host = env::var("NC_HOST").expect("NC_HOST not found");
    let host = fix_host(&host);

    let nc_info = NCInfo::new(username, password, host);

    let local_root_path = env::var("LOCAL_ROOT").expect("LOCAL_ROOT not found");
    let local_info = LocalInfo::new(local_root_path)?;

    run(&nc_info, &local_info).await?;

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
