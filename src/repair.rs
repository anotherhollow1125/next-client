use crate::fileope;
use crate::meta::*;
use anyhow::Result;
use std::fs;
use std::path::Path;

// rough repair: Get new tree and missing files and folders to avoid contradiction. Unnecessary folders and files will remain.
//  (like behavior at app's start. If there are already having files, they will be ignored.)
//  this ope will be used with local event stack under offline.
// soft repair: Get new tree, remove unnecessary folders and files and get new folders and files.
//  this operation using stash to protect files.
// hard repair: equal reset. delete .ncs file and all files in root and restart all process.

// hard repair
pub fn all_delete(local_info: &LocalInfo) -> Result<()> {
    let root_entry = Path::new(&local_info.root_path);
    let entries = fs::read_dir(root_entry)?
        .into_iter()
        .filter_map(|e| e.map(|e| e.path()).ok())
        .collect::<Vec<_>>();

    fileope::remove_items(&entries, false)?;

    Ok(())
}
