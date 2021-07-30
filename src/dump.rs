use crate::errors::NcsError::*;
use crate::*;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{fs, io::Write};

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
