use crate::errors::NcsError::*;
use crate::*;
use anyhow::Result;
use chrono::prelude::*;
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use notify::DebouncedEvent;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::sync::Mutex;
use std::{fs, io::Write};
use tokio::sync::mpsc::Sender as TokioSender;
// use std::time::Duration as StdDuration;
// use notify::{watcher, DebouncedEvent, RecursiveMode, Watcher};

const NC_ROOT_PREFIX: &str = "/remote.php/dav/files/";
pub const OCS_ROOT: &str = "/ocs/v2.php/apps/activity/api/v2/activity/all";

#[derive(Serialize, Deserialize, Debug)]
pub struct NCSCache {
    pub latest_activity_id: String,
    pub root_entry: JsonEntry,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum JsonEntry {
    Dir {
        name: String,
        children: Vec<JsonEntry>,
    },
    File {
        name: String,
        etag: String,
    },
}

pub fn root2json_entry(root_entry: &Entry) -> Result<JsonEntry> {
    if !root_entry.is_root() {
        return Err(anyhow!("This function can be called by root entry."));
    }

    entry2json_entry_rec(root_entry)
}

fn entry2json_entry_rec(entry: &Entry) -> Result<JsonEntry> {
    if entry.type_.is_file() {
        Ok(JsonEntry::File {
            name: entry.get_name(),
            etag: entry.type_.get_etag(),
        })
    } else {
        let children = entry
            .children
            .values()
            .map(|c| -> Result<JsonEntry> {
                let c_ref = c.lock().map_err(|_| LockError)?;
                entry2json_entry_rec(&c_ref)
            })
            .collect::<Result<Vec<JsonEntry>>>()?;
        Ok(JsonEntry::Dir {
            name: entry.get_name(),
            children,
        })
    }
}

pub fn json_entry2entry(json_entry: JsonEntry) -> Result<ArcEntry> {
    match json_entry {
        JsonEntry::Dir { name, children } => {
            let mut entry = Entry::new(name, EntryType::Directory);
            entry.status = EntryStatus::UpToDate;
            let dir = Arc::new(Mutex::new(entry));
            for child in children.into_iter() {
                let child = json_entry2entry(child)?;
                Entry::append_child(&dir, child)?;
            }
            Ok(dir)
        }
        JsonEntry::File { name, etag } => {
            let type_ = EntryType::File { etag: Some(etag) };
            let mut entry = Entry::new(name, type_);
            entry.status = EntryStatus::UpToDate;
            Ok(Arc::new(Mutex::new(entry)))
        }
    }
}

pub fn load_cache(local_info: &LocalInfo) -> Result<NCSCache> {
    let j = fs::read_to_string(local_info.get_cachefile_name().as_str())?;
    Ok(serde_json::from_str(&j)?)
}

pub fn save_cache(
    latest_activity_id: String,
    root_entry: JsonEntry,
    local_info: &LocalInfo,
) -> Result<()> {
    fs::create_dir_all(local_info.get_metadir_name().as_str())?;

    let ncs_cache = NCSCache {
        latest_activity_id,
        root_entry,
    };
    let j = serde_json::to_string(&ncs_cache)?;
    let mut cache_file = fs::File::create(local_info.get_cachefile_name().as_str())?;
    writeln!(cache_file, "{}", j)?;

    Ok(())
}

#[derive(Clone)]
pub struct LocalInfo {
    pub root_path: String,
    pub exc_checker: meta::ExcludeChecker,
    log_file_name: String,
    pub req_client: reqwest::Client,
}

impl LocalInfo {
    pub fn new(root_path: String, req_client: reqwest::Client) -> Result<Self> {
        let root_path = drop_slash(&root_path, &RE_HAS_LAST_SLASH);
        let exc_checker = meta::ExcludeChecker::new(&root_path)?;
        let dt = Local::now();
        let log_file_name = format!("{}.log", dt.format("%Y%m%d"));
        Ok(Self {
            root_path,
            exc_checker,
            log_file_name,
            req_client,
        })
    }

    pub fn get_metadir_name(&self) -> String {
        format!("{}/.ncs/", self.root_path)
    }

    pub fn get_cachefile_name(&self) -> String {
        format!("{}cache.json", self.get_metadir_name())
    }

    pub fn get_excludefile_name(&self) -> String {
        format!("{}excludes.json", self.get_metadir_name())
    }

    pub fn get_stashpath_name(&self) -> String {
        format!("{}stash", self.get_metadir_name())
    }

    pub fn get_logfile_name(&self) -> String {
        format!("{}log/{}", self.get_metadir_name(), self.log_file_name)
    }

    pub fn get_keepalive_filename(&self) -> String {
        format!("{}.keepalive.txt", self.get_metadir_name())
    }

    pub fn get_metadir_name_raw(root_path: &str) -> String {
        format!("{}/.ncs/", root_path)
    }

    pub fn get_cachefile_name_raw(root_path: &str) -> String {
        format!("{}cache.json", Self::get_metadir_name_raw(root_path))
    }

    pub fn get_excludefile_name_raw(root_path: &str) -> String {
        format!("{}excludes.json", Self::get_metadir_name_raw(root_path))
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct JsonExcludeList {
    pub blacks: Vec<String>,
    pub whites: Vec<String>,
}

impl JsonExcludeList {
    pub fn new() -> Self {
        Self {
            blacks: Vec::new(),
            whites: Vec::new(),
        }
    }

    pub fn from_json(root_path: &str) -> Result<Self> {
        let f_path = LocalInfo::get_excludefile_name_raw(root_path);
        if !Path::new(&f_path).exists() {
            let s = Self::new();
            s.save_excludelist(root_path)?;
        }
        let j = fs::read_to_string(&f_path)?;
        Ok(serde_json::from_str(&j)?)
    }

    pub fn save_excludelist(&self, root_path: &str) -> Result<()> {
        fs::create_dir_all(LocalInfo::get_metadir_name_raw(root_path).as_str())?;
        let f_path = LocalInfo::get_excludefile_name_raw(root_path);

        let j = serde_json::to_string_pretty(self)?;
        let mut exc_file = fs::File::create(&f_path)?;
        writeln!(exc_file, "{}", j)?;

        Ok(())
    }
}

pub async fn exc_list_update_watching(
    com_tx: TokioSender<Command>,
    rx: Mutex<std_mpsc::Receiver<DebouncedEvent>>,
    local_info: &LocalInfo,
) -> Result<()> {
    let exc_file_name = local_info.get_excludefile_name();
    let exc_file = Path::new(&exc_file_name)
        .file_name()
        .ok_or_else(|| InvalidPathError("Invalid exclude file name.".to_string()))?;

    loop {
        if com_tx.is_closed() {
            return Ok(());
        }

        let c = {
            let rx_ref = rx.lock().map_err(|_| LockError)?;
            match rx_ref.recv() {
                Ok(DebouncedEvent::Create(p)) | Ok(DebouncedEvent::Write(p)) => {
                    if p.file_name() == Some(exc_file) {
                        Some(Command::UpdateExcFile)
                    } else {
                        None
                    }
                }
                Ok(_) => None,
                Err(_e) => {
                    // error!("{:?}", e);
                    return Ok(());
                }
            }
        };
        if let Some(c) = c {
            com_tx.send(c).await?;
        }
    }
}

#[derive(Clone)]
pub struct ExcludeChecker {
    blacks: Vec<Regex>,
    whites: Vec<Regex>,
}

impl ExcludeChecker {
    pub fn new(root_path: &str) -> Result<Self> {
        let raw_list = JsonExcludeList::from_json(root_path)?;
        let mut blacks = raw_list
            .blacks
            .into_iter()
            .filter_map(|s| Regex::new(&s).ok())
            .collect::<Vec<_>>();
        blacks.push(Regex::new(r"^\.").unwrap());
        blacks.push(Regex::new(r"^~").unwrap());
        let whites = raw_list
            .whites
            .into_iter()
            .filter_map(|s| Regex::new(&s).ok())
            .collect::<Vec<_>>();
        Ok(Self { blacks, whites })
    }

    pub fn judge<P>(&self, p: P) -> bool
    where
        P: AsRef<Path>,
    {
        let path = p.as_ref();
        'compcheck: for c in path.components() {
            let s = c.as_os_str().to_string_lossy();
            for r in self.whites.iter() {
                if r.is_match(&s) {
                    continue 'compcheck;
                }
            }

            for r in self.blacks.iter() {
                if r.is_match(&s) {
                    return false;
                }
            }
        }

        /*
        // check its name.
        let s_w = path.file_name();
        let s = if let Some(v) = s_w {
            v.to_string_lossy()
        } else {
            return false;
        };

        for r in self.white_files.iter() {
            if r.is_match(&s) {
                return true;
            }
        }

        for r in self.black_files.iter() {
            if r.is_match(&s) {
                return false;
            }
        }
        */

        true
    }
}

#[derive(Clone, Debug)]
pub struct NCInfo {
    pub username: String,
    pub password: String,
    pub host: String,
    pub root_path: String,
}

impl NCInfo {
    pub fn new(username: String, password: String, host: String) -> Self {
        let host = fix_host(&host);
        let root_path = format!("{}{}", NC_ROOT_PREFIX, username);
        let root_path = fix_root(&root_path);
        Self {
            username,
            password,
            host,
            root_path,
        }
    }
}
