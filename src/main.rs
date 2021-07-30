use anyhow::Result;
use dotenv::dotenv;
use log::debug;
use ncs::dump::*;
use ncs::errors::NcsError::*;
use ncs::nc_listen::*;
use ncs::*;
use std::{env, fs};
#[allow(unused)]
use tokio::time::{sleep, Duration};
#[macro_use]
extern crate anyhow;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    env_logger::init();
    let username = env::var("NC_USERNAME").expect("NC_USERNAME not found");
    let password = env::var("NC_PASSWORD").expect("NC_PASSWORD not found");
    let host = env::var("NC_HOST").expect("NC_HOST not found");
    // let root_path = env::var("NC_ROOT").expect("NC_ROOT not found");

    let host = fix_host(&host);
    // let root_path = fix_root(&root_path);

    let nc_info = NCInfo::new(username, password, host);

    let local_root_path = env::var("LOCAL_ROOT").expect("LOCAL_ROOT not found");
    let local_info = LocalInfo::new(local_root_path);

    let mut args = env::args();
    match args.nth(1) {
        Some(s) if s == "init" => {
            let (root, latest_activity_id) = init(&nc_info, &local_info).await?;
            let json_entry = {
                let root_ref = root.lock().map_err(|_| LockError)?;
                root2json_entry(&root_ref)?
            };
            save_cache(latest_activity_id, json_entry, &local_info)?;
        }
        Some(s) if s == "update" => {
            let j = fs::read_to_string(local_info.get_cachefile_name().as_str())
                .map_err(|e| anyhow!("{}\nPlease init before update.", e))?;
            let ncs_cache: NCSCache = serde_json::from_str(&j)?;
            let mut nc_state = NCState {
                latest_activity_id: ncs_cache.latest_activity_id,
            };
            debug!("{}", nc_state.latest_activity_id);
            let root_entry = json_entry2entry(ncs_cache.root_entry)?;
            {
                let r = root_entry.lock().map_err(|_| anyhow!("Failed lock"))?;
                debug!("\n{}", r.get_tree());
            }
            let events = get_ncevents(&nc_info, &mut nc_state).await?;
            for e in events.iter() {
                debug!("{:?}", e);
            }

            let download_targets =
                update_tree(&nc_info, &local_info, events, &root_entry, false).await?;
            {
                let r = root_entry.lock().map_err(|_| anyhow!("Failed lock"))?;
                debug!("\n{}", r.get_tree());
            }
            debug!("{:?}", download_targets);
            for target in download_targets.into_iter() {
                if let Some(e) = target.upgrade() {
                    {
                        let e_ref = e.lock().map_err(|_| LockError)?;
                        if e_ref.type_.is_dir() {
                            continue;
                        }
                    }
                    download_file_with_check_etag(&nc_info, &local_info, &e).await?;
                    {
                        let mut e_ref = e.lock().map_err(|_| LockError)?;
                        e_ref.status = EntryStatus::UpToDate;
                    }
                }
            }
            let json_entry = {
                let r = root_entry.lock().map_err(|_| anyhow!("Failed lock"))?;
                debug!("\n{}", r.get_tree());
                root2json_entry(&r)?
            };
            save_cache(nc_state.latest_activity_id, json_entry, &local_info)?;
        }
        _ => (),
    }

    Ok(())
}

async fn init(nc_info: &NCInfo, local_info: &LocalInfo) -> Result<(ArcEntry, String)> {
    let root_entry = from_nc_all(&nc_info, "/").await?;
    let latest_activity_id = get_latest_activity_id(&nc_info).await?;
    debug!("{}", latest_activity_id);

    {
        let r = root_entry.lock().map_err(|_| anyhow!("Failed lock"))?;
        debug!("\n{}", r.get_tree());
    }

    init_local_entries(&nc_info, &local_info, &root_entry, "").await?;

    {
        let r = root_entry.lock().map_err(|_| anyhow!("Failed lock"))?;
        println!("\n{}", r.get_tree());
    }

    Ok((root_entry, latest_activity_id))
}
