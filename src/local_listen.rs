use crate::errors::NcsError::*;
use crate::meta::*;
use crate::nc_listen::NCEvent;
use crate::repair::ModifiedPath;
use crate::*;
use anyhow::Result;
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use notify::DebouncedEvent as DebEvent;
use reqwest::{Client, Method, Url};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;
use tokio::sync::mpsc::Sender as TokioSender;
// use tokio::time::sleep;
// use tokio::time::Duration;
// use notify::{watcher, RecursiveMode, Watcher};
// use tokio::sync::mpsc::Receiver;
// use tokio::sync::mpsc as tokio_mpsc;
// use std::sync::mpsc as std_mpsc;
// use crate::nc_listen::{self, NCInfo};
// use crate::{ArcResource, LocalInfo};
// use anyhow::{Context, Result};

#[derive(Debug)]
pub enum LocalEvent {
    Create(PathBuf),
    Delete(PathBuf),
    Modify(PathBuf),
    Move(PathBuf, PathBuf),
}

impl ModifiedPath for LocalEvent {
    type Item = PathBuf;

    fn to_modified_path(&self) -> Option<PathBuf> {
        match self {
            Self::Create(p) => Some(p.clone()),
            Self::Delete(_) => None,
            Self::Modify(p) => Some(p.clone()),
            Self::Move(_, p) => Some(p.clone()),
        }
    }
}

impl LocalEvent {
    #[allow(dead_code)]
    fn strip_root(&mut self, root_path: &str) {
        *self = match self {
            Self::Create(p) => Self::Create(p.strip_prefix(root_path).unwrap_or(p).to_path_buf()),
            Self::Delete(p) => Self::Delete(p.strip_prefix(root_path).unwrap_or(p).to_path_buf()),
            Self::Modify(p) => Self::Modify(p.strip_prefix(root_path).unwrap_or(p).to_path_buf()),
            Self::Move(p, q) => Self::Move(
                p.strip_prefix(root_path).unwrap_or(p).to_path_buf(),
                q.strip_prefix(root_path).unwrap_or(q).to_path_buf(),
            ),
        };
    }

    #[allow(dead_code)]
    fn reformat_path(&mut self) {
        *self = match self {
            Self::Create(p) => Self::Create(fix_path(p)),
            Self::Delete(p) => Self::Delete(fix_path(p)),
            Self::Modify(p) => Self::Modify(fix_path(p)),
            Self::Move(p, q) => Self::Move(fix_path(p), fix_path(q)),
        };
    }
}

#[allow(dead_code)]
fn fix_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy().replace("\\", "/");
    let s = drop_slash(&s, &RE_HAS_LAST_SLASH);
    Path::new(&s).to_path_buf()
}

pub fn get_localpath(path: &Path, local_info: &LocalInfo) -> PathBuf {
    let path = if path.starts_with("/") {
        path.strip_prefix("/").unwrap_or(path)
    } else {
        path
    };
    Path::new(&local_info.root_path).join(path)
}

pub async fn watching(
    com_tx: TokioSender<Command>,
    rx: Mutex<std_mpsc::Receiver<DebEvent>>,
    local_info: &LocalInfo,
    _nc_info: &NCInfo,
) -> Result<()> {
    loop {
        if com_tx.is_closed() {
            return Ok(());
        }

        let mut items = Vec::new();
        {
            let rx_ref = rx.lock().map_err(|_| LockError)?;
            let mut stack = vec![rx_ref.recv()];
            while let Some(rcv) = stack.pop() {
                match rcv {
                    Ok(ev) => match ev {
                        DebEvent::Create(p) => items.push(LocalEvent::Create(p)),
                        DebEvent::Write(p) if p.is_file() => items.push(LocalEvent::Modify(p)),
                        DebEvent::Remove(p) => {
                            let d = StdDuration::from_millis(10);
                            match rx_ref.recv_timeout(d) {
                                // Ok(DebEvent::Create(q)) if p.file_name() == q.file_name() => {
                                Ok(DebEvent::Create(q)) => {
                                    items.push(LocalEvent::Move(p, q));
                                }
                                Err(_) => items.push(LocalEvent::Delete(p)),
                                Ok(q) => {
                                    items.push(LocalEvent::Delete(p));
                                    stack.push(Ok(q));
                                }
                            }
                        }
                        DebEvent::Rename(p, q) => items.push(LocalEvent::Move(p, q)),
                        _ => (),
                    },
                    Err(e) => {
                        error!("{:?}", e);
                        return Ok(());
                    }
                }
            }
        }

        // let _ = network::check(&com_tx, nc_info).await?;

        for mut item in items {
            item.strip_root(&local_info.root_path);
            // item.reformat_path();
            com_tx.send(Command::LocEvent(item)).await?;
        }
    }
}

#[async_recursion]
pub async fn deal_local_event(
    ev: LocalEvent,
    root: &ArcEntry,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    nc2l_cancel_map: &mut HashMap<String, usize>,
    l2nc_cancel_set: &mut HashSet<NCEvent>,
) -> Result<()> {
    match ev {
        LocalEvent::Create(p) => {
            if !local_info.exc_checker.judge(&p) {
                debug!("Create {:?} : Excluded File.", p);
                return Ok(());
            }

            if haveto_cancel_target(&p, nc2l_cancel_map) {
                debug!("Create {:?} : canceled.", p);
                return Ok(());
            }

            let local_p = get_localpath(&p, local_info);

            debug!("local_p: {:?}", local_p);

            if !local_p.exists() {
                debug!("LocEvent::Create({:?}) : but not found.", p);
                return Ok(());
            }

            let p_parent = p
                .parent()
                .ok_or_else(|| InvalidPathError("Something wrong.".to_string()))?;
            let p_parent_str = path2str(p_parent);

            if Entry::get(root, &p_parent_str)?.is_none() {
                debug!("Its parent is not registered yet.");
                return Ok(());
            }

            info!("LocEvent::Create({:?})", p);

            let p_str = path2str(&p);
            if Entry::get(root, &p_str)?.is_some() {
                info!("Already Exists.");

                deal_local_event(
                    LocalEvent::Modify(p),
                    root,
                    nc_info,
                    local_info,
                    nc2l_cancel_map,
                    l2nc_cancel_set,
                )
                .await?;

                return Ok(());
            }

            let method = if local_p.is_file() {
                NCMethod::Put(p_str.clone(), local_p.clone())
            } else {
                NCMethod::Mkcol(p_str.clone())
            };

            let etag_w = comm_nc(nc_info, method).await?;
            let name = p
                .file_name()
                .ok_or_else(|| InvalidPathError("Something wrong in notify path.".to_string()))?
                .to_string_lossy()
                .to_string();
            let type_ = if local_p.is_file() {
                EntryType::File { etag: etag_w }
            } else {
                EntryType::Directory
            };
            let new_entry = Arc::new(Mutex::new(Entry::new(name, type_)));
            {
                let mut e_ref = new_entry.lock().map_err(|_| LockError)?;
                e_ref.status = EntryStatus::UpToDate;
            }
            let _ = Entry::append(root, &p_str, new_entry, AppendMode::Create, false)?;

            l2nc_cancel_set.insert(NCEvent::Create(p_str.clone()));

            if_chain! {
                if local_p.is_dir();
                if let Ok(readdir) = fs::read_dir(&local_p);
                then {
                    for item in readdir {
                        if let Ok(path) = item {
                            let s = path.file_name();
                            let s = s.to_string_lossy();
                            let c = format!("{}/{}", p_str, s);
                            deal_local_event(
                                LocalEvent::Create(Path::new(&c).to_path_buf()),
                                root,
                                nc_info,
                                local_info,
                                nc2l_cancel_map,
                                l2nc_cancel_set,
                            ).await?;
                        }
                    }
                }
            }
        }
        LocalEvent::Delete(p) => {
            if !local_info.exc_checker.judge(&p) {
                debug!("Delete {:?} : Excluded File.", p);
                return Ok(());
            }

            let local_p = get_localpath(&p, local_info);

            if local_p.exists() {
                debug!("LocEvent::Delete({:?}) : but still exists.", p);
                return Ok(());
            }

            info!("LocEvent::Delete({:?})", p);

            let p_str = path2str(&p);
            if Entry::get(root, &p_str)?.is_none() {
                info!("Already Deleted.");
                return Ok(());
            }

            let _ = comm_nc(nc_info, NCMethod::Delete(p_str.clone())).await?;
            {
                let mut root_ref = root.lock().map_err(|_| LockError)?;
                let _ = root_ref.pop(&p_str)?;
            }

            l2nc_cancel_set.insert(NCEvent::Delete(p_str));
        }
        LocalEvent::Modify(p) => {
            if !local_info.exc_checker.judge(&p) {
                debug!("Modify {:?} : Excluded File.", p);
                return Ok(());
            }

            if haveto_cancel_target(&p, nc2l_cancel_map) {
                debug!("Modify {:?} : canceled.", p);
                return Ok(());
            }

            let local_p = get_localpath(&p, local_info);

            if !local_p.exists() {
                debug!("LocEvent::Modify({:?}) : but not found.", p);
                return Ok(());
            }

            if local_p.is_dir() {
                debug!("LocEvent::Modify({:?}) : but this is dir.", p);
                return Ok(());
            }

            info!("LocEvent::Modify({:?})", p);

            let p_str = path2str(&p);
            if Entry::get(root, &p_str)?.is_none() {
                info!("But not found.");
                return Ok(());
            }

            let etag_w = comm_nc(nc_info, NCMethod::Put(p_str.clone(), local_p)).await?;
            let entry_w = Entry::get(root, &p_str)?;
            if_chain! {
                if let Some(w) = entry_w;
                if let Some(a) = w.upgrade();
                then {
                    let mut e_ref = a.lock().map_err(|_| LockError)?;
                    e_ref.type_ = EntryType::File { etag: etag_w };
                    e_ref.status = EntryStatus::UpToDate;
                }
            }

            l2nc_cancel_set.insert(NCEvent::Modify(p_str));
        }
        LocalEvent::Move(p, q) => {
            let local_p = get_localpath(&p, local_info);

            if local_p.exists() {
                debug!("LocEvent::Move({:?}, _) : but still exists.", p);
                return Ok(());
            }

            let p_is_exc = !local_info.exc_checker.judge(&p);
            let q_is_exc = !local_info.exc_checker.judge(&q);

            if p_is_exc && q_is_exc {
                debug!("Move({:?}, {:?}) : they are Excluded Files.", p, q);
                return Ok(());
            } else if p_is_exc {
                debug!(
                    "Move({:?}, {:?}) : {:?} is Excluded File. == Create({:?})",
                    p, q, p, q
                );
                deal_local_event(
                    LocalEvent::Create(q.clone()),
                    root,
                    nc_info,
                    local_info,
                    nc2l_cancel_map,
                    l2nc_cancel_set,
                )
                .await?;
                return Ok(());
            } else if q_is_exc {
                debug!(
                    "Move({:?}, {:?}) : {:?} is Excluded File. == Delete({:?})",
                    p, q, q, p
                );
                deal_local_event(
                    LocalEvent::Delete(p.clone()),
                    root,
                    nc_info,
                    local_info,
                    nc2l_cancel_map,
                    l2nc_cancel_set,
                )
                .await?;
                return Ok(());
            }

            info!("LocEvent::Move({:?}, {:?})", p, q);

            let p_str = path2str(&p);
            if Entry::get(root, &p_str)?.is_none() {
                info!("But not found from_item.");
                return Ok(());
            }

            let q_str = path2str(&q);

            let _ = comm_nc(nc_info, NCMethod::Move(p_str.clone(), q_str.clone())).await?;
            let entry = {
                let mut root_ref = root.lock().map_err(|_| LockError)?;
                root_ref
                    .pop(&p_str)?
                    .ok_or_else(|| InvalidPathError("Something wrong.".to_string()))?
            };

            let _ = Entry::append(root, &q_str, entry, AppendMode::Move, true)?;

            let cancel_target = if p.file_name() == q.file_name() {
                let q_parent = q
                    .parent()
                    .ok_or_else(|| InvalidPathError("Something wrong.".to_string()))?;
                let q_parent_str = path2str(q_parent);
                NCEvent::Move(p_str, q_parent_str)
            } else {
                NCEvent::Move(p_str, q_str)
            };

            l2nc_cancel_set.insert(cancel_target);
        }
    }

    Ok(())
}

fn haveto_cancel_target(p: &Path, book: &mut HashMap<String, usize>) -> bool {
    let path_str = path2str(p);
    let count_w = book.remove(&path_str);
    match count_w {
        Some(mut count) => {
            count -= 1;
            if count > 0 {
                book.insert(path_str, count);
            }
            true
        }
        None => false,
    }
}

enum NCMethod {
    Put(String, PathBuf),
    Mkcol(String),
    Delete(String),
    Move(String, String),
}

async fn comm_nc(nc_info: &NCInfo, method: NCMethod) -> Result<Option<String>> {
    let target = match method {
        NCMethod::Put(ref target, _) => target.to_string(),
        NCMethod::Mkcol(ref target) => target.to_string(),
        NCMethod::Delete(ref target) => target.to_string(),
        NCMethod::Move(ref target, _) => target.to_string(),
    };

    let path = format!("{}{}", &nc_info.root_path, target)
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();

    let mut url = Url::parse(&nc_info.host)?;
    url.path_segments_mut().unwrap().extend(path);

    let reqbuil = match method {
        NCMethod::Put(_, file_path) => {
            let buf = if let Ok(v) = fs::read(&file_path) {
                v
            } else {
                vec![]
            };

            // debug!("{:?} 's content : {:?}", file_path, buf);

            Client::new().request(Method::PUT, url.as_str()).body(buf)
        }
        NCMethod::Mkcol(_) => {
            Client::new().request(Method::from_bytes(b"MKCOL").unwrap(), url.as_str())
        }
        NCMethod::Delete(_) => Client::new().request(Method::DELETE, url.as_str()),
        NCMethod::Move(_, to_target) => {
            let p = format!("{}{}", &nc_info.root_path, to_target)
                .split("/")
                .map(|v| v.to_string())
                .collect::<Vec<String>>();
            let mut to_url = Url::parse(&nc_info.host)?;
            to_url.path_segments_mut().unwrap().extend(p);

            Client::new()
                .request(Method::from_bytes(b"MOVE").unwrap(), url.as_str())
                .header("Destination", to_url.as_str())
        }
    };

    let res = reqbuil
        .basic_auth(&nc_info.username, Some(&nc_info.password))
        .send()
        .await?;

    if !res.status().is_success() {
        return Err(BadStatusError(res.status().as_u16()).into());
    }

    let headers = res.headers();
    Ok(headers
        .get("Etag")
        .and_then(|s| s.to_str().ok())
        .map(|s| s.replace("\"", "")))
}
