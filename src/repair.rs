use crate::errors::NcsError::*;
use crate::local_listen::{get_localpath, LocalEvent};
use crate::meta::*;
use crate::nc_listen::*;
use crate::*;
use anyhow::Result;
use log::info;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::fs;
use std::path::Path;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

// パスの表現がモジュールごとに読まないとわからなくなっている -> リファクタリングしたい事項

pub trait ModifiedPath {
    type Item: AsRef<Path> + Debug;

    fn to_modified_path(&self) -> Option<Self::Item>;
}

pub trait ModifiedPathVec {
    type Item: AsRef<Path> + Debug;

    fn get_modified_path_vec(&self) -> Vec<Self::Item>;
}

impl<T: ModifiedPath> ModifiedPathVec for Vec<T> {
    type Item = T::Item;

    fn get_modified_path_vec(&self) -> Vec<Self::Item> {
        self.iter()
            .filter_map(|item| item.to_modified_path())
            .collect()
    }
}

// rough repair(don't use.): Get new tree and missing files and folders to avoid contradiction. Unnecessary folders and files will remain.
//  (like behavior at app's start. If there are already having files, they will be ignored.)
//  this ope will be used with local event stack under offline.
// soft repair: Get new events, operate current tree. Using Local events, get modified files list and using it to check modified file.
// first, nc tree's modified files will downloaded and unnecessary files and folders will be removed (with stash). second, local tree's modified files will uploaded if they are remained.
// normal repair: Get new tree, remove unnecessary folders and files and get new folders and files.
//  this operation using stash to protect files.
//  this operation don't use local tree's modified files.
//  this operation use local tree's etag to distinguish modified and not-modified. -> 実装する...?
// hard repair: equal reset. delete .ncs file and all files in root and restart all process.

// soft repair
// return have_to_rerun.
pub async fn soft_repair(
    local_info: &LocalInfo,
    nc_info: &NCInfo,
    resource: &ArcResource,
    local_events: Vec<LocalEvent>,
    tx: mpsc::Sender<Command>,
    nc2l_cancel_map: &mut HashMap<String, usize>,
    l2nc_cancel_set: &mut HashSet<NCEvent>,
) -> Result<bool> {
    let res;
    let events;
    {
        let mut pr_ref = resource.lock().map_err(|_| LockError)?;
        events = get_ncevents(nc_info, local_info, &mut pr_ref.nc_state).await?;

        res = update_and_download(
            events.clone(),
            &pr_ref.root,
            nc_info,
            local_info,
            nc2l_cancel_map,
            l2nc_cancel_set,
        )
        .await;
    }

    if let Err(e) = res {
        info!("{:?}\nI'll try normal repair.", e);
        *nc2l_cancel_map = HashMap::new();
        *l2nc_cancel_set = HashSet::new();
        normal_repair(local_info, nc_info, resource, events).await?;
        sleep(Duration::from_secs(20)).await;
        return Ok(true);
    }

    let pr_ref = resource.lock().map_err(|_| LockError)?;

    let mut download_list = Vec::new();
    check_exists_rec(
        "",
        &pr_ref.root,
        local_info,
        &mut download_list,
        nc2l_cancel_map,
    )?;

    for w in download_list.into_iter() {
        let entry = if let Some(v) = w.upgrade() {
            v
        } else {
            continue;
        };

        let _ = nc_listen::download_file_with_check_etag(nc_info, local_info, &entry).await?;
    }

    let local_modified_path_vec = local_events.get_modified_path_vec();

    info!("local_events: {:?}", local_modified_path_vec);

    for p in local_modified_path_vec.into_iter() {
        let local_p = get_localpath(&p, local_info);
        if !local_p.exists() {
            continue;
        }
        let p_str = path2str(&p);
        let com = if Entry::get(&pr_ref.root, &p_str)?.is_some() {
            Command::LocEvent(LocalEvent::Modify(p))
        } else {
            Command::LocEvent(LocalEvent::Create(p))
        };
        tx.send(com).await?;
    }

    Ok(false)
}

fn check_exists_rec(
    ancestor: &str,
    arc_entry: &ArcEntry,
    local_info: &LocalInfo,
    download_list: &mut Vec<WeakEntry>,
    nc2l_cancel_map: &mut HashMap<String, usize>,
) -> Result<()> {
    let weak_entry = Arc::downgrade(arc_entry);
    let mut entry = arc_entry.lock().map_err(|_| LockError)?;
    let name = entry.get_raw_name();
    if !local_info.exc_checker.judge(&name) {
        return Ok(());
    }

    let path_s = format!("{}/{}", ancestor, name);
    let local_path = get_localpath(Path::new(&path_s), local_info);

    entry.status = EntryStatus::UpToDate;

    let mut have_to_cancel = false;

    match entry.type_.clone() {
        EntryType::Directory => {
            if !local_path.exists() {
                fileope::create_dir_all(&local_path)?;
                have_to_cancel = true;
            }

            if local_path.is_file() {
                fileope::remove_entry(&local_path, Some(local_info))?;
                fileope::create_dir_all(&local_path)?;
                have_to_cancel = true;
            }

            let children = entry
                .get_all_children()
                .into_iter()
                .filter_map(|c| c.upgrade());

            for child in children {
                check_exists_rec(&path_s, &child, local_info, download_list, nc2l_cancel_map)?;
            }
        }
        EntryType::File { etag: _ } => {
            if !local_path.exists() {
                entry.type_ = EntryType::File { etag: None };
                entry.status = EntryStatus::NeedUpdate;
                download_list.push(weak_entry);
                have_to_cancel = true;
            }
        }
    }

    if have_to_cancel {
        let counter = nc2l_cancel_map.entry(path_s).or_insert(0);
        *counter += 1;
    }

    Ok(())
}

// normal repair
pub async fn normal_repair(
    local_info: &LocalInfo,
    nc_info: &NCInfo,
    resource: &ArcResource,
    events: Vec<NCEvent>,
) -> Result<()> {
    let root_entry = from_nc_all(nc_info, local_info, "/").await?;
    let latest_activity_id = get_latest_activity_id(nc_info).await?;

    let modified_path_vec = events.get_modified_path_vec();

    let mut download_list = modified_path_vec
        .into_iter()
        .filter_map(|p| Entry::get(&root_entry, &p).ok().and_then(|w| w))
        .collect::<Vec<_>>();

    choose_leave_dirfile_rec("", &root_entry, local_info, &mut download_list)?;

    for w in download_list.into_iter() {
        let entry = if let Some(v) = w.upgrade() {
            v
        } else {
            continue;
        };

        let _ = nc_listen::download_file_with_check_etag(nc_info, local_info, &entry).await?;
    }

    {
        let mut resource_ref = resource.lock().map_err(|_| LockError)?;
        resource_ref.root = root_entry;
        resource_ref.nc_state = nc_listen::NCState { latest_activity_id };
    }

    Ok(())
}

fn choose_leave_dirfile_rec(
    ancestor: &str,
    arc_entry: &ArcEntry,
    local_info: &LocalInfo,
    download_list: &mut Vec<WeakEntry>,
) -> Result<()> {
    let weak_entry = Arc::downgrade(arc_entry);
    let mut entry = arc_entry.lock().map_err(|_| LockError)?;
    let name = entry.get_raw_name();
    if !local_info.exc_checker.judge(&name) {
        return Ok(());
    }

    let path_s = format!("{}/{}", ancestor, name);
    let local_path = get_localpath(Path::new(&path_s), local_info);

    entry.status = EntryStatus::UpToDate;

    match entry.type_.clone() {
        EntryType::Directory => {
            if !local_path.exists() {
                fileope::create_dir_all(&local_path)?;
            }

            if local_path.is_file() {
                fileope::remove_entry(&local_path, Some(local_info))?;
                fileope::create_dir_all(&local_path)?;
            }

            // check unnecessary files and dirs
            for child in fs::read_dir(&local_path)? {
                let child = if let Ok(c) = child {
                    c
                } else {
                    continue;
                };

                let child_path = child.path();
                let c_name = if let Some(v) = child_path.file_name().map(|n| n.to_string_lossy()) {
                    v.to_string()
                } else {
                    continue;
                };

                if !local_info.exc_checker.judge(&c_name) {
                    continue;
                }

                if entry.get_child(&c_name).is_none() {
                    fileope::remove_entry(&child_path, Some(local_info))?;
                }
            }

            let children = entry
                .get_all_children()
                .into_iter()
                .filter_map(|c| c.upgrade());

            for child in children {
                choose_leave_dirfile_rec(&path_s, &child, local_info, download_list)?;
            }
        }
        EntryType::File { etag: _ } => {
            if !local_path.exists() {
                entry.type_ = EntryType::File { etag: None };
                entry.status = EntryStatus::NeedUpdate;
                download_list.push(weak_entry);
            }
        }
    }

    Ok(())
}

// hard repair
pub fn all_delete(local_info: &LocalInfo) -> Result<()> {
    let root_entry = Path::new(&local_info.root_path);
    let entries = fs::read_dir(root_entry)?
        .into_iter()
        .filter_map(|e| e.map(|e| e.path()).ok())
        .collect::<Vec<_>>();

    fileope::remove_items(&entries, None)?;

    Ok(())
}
