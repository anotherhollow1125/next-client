use crate::meta::LocalInfo;
use anyhow::Result;
use chrono::prelude::*;
use fs_extra::dir::CopyOptions;
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use std::ffi::OsStr;
use std::fmt::Debug;
use std::{fs, io, path};
// use crate::errors::NcsError::*;

pub fn save_file<R: io::Read>(r: &mut R, filename: &str) -> Result<()> {
    debug!("save_file: {}", filename);

    let mut out = fs::File::create(filename)?;
    io::copy(r, &mut out)?;

    Ok(())
}

pub fn create_dir_all<T>(dir_path: T) -> Result<()>
where
    T: AsRef<path::Path> + Debug,
{
    debug!("create_dir_all: {:?}", dir_path);

    fs::create_dir_all(dir_path)?;

    Ok(())
}

pub fn touch_entry<T>(path: T, is_file: bool) -> Result<()>
where
    T: AsRef<path::Path> + Debug,
{
    debug!("touch_entry: {:?}", path);

    if is_file {
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(path)
            .map(|_| ())?;
    } else {
        create_dir_all(path)?;
    }

    Ok(())
}

pub fn move_entry<T, U>(from_path: T, to_path: U, stash: Option<&LocalInfo>) -> Result<()>
where
    T: AsRef<path::Path> + Debug,
    U: AsRef<path::Path> + Debug,
{
    debug!("move_entry: {:?} => {:?}", from_path, to_path);

    if_chain! {
        if let Some(local_info) = stash;
        if to_path.as_ref().exists();
        if to_path.as_ref().is_file();
        then {
            stash_item(&to_path, local_info)?;
        }
    }

    let options = fs_extra::dir::CopyOptions {
        overwrite: true,
        copy_inside: true,
        ..Default::default()
    };
    fs_extra::move_items(&[from_path], to_path, &options)?;

    Ok(())
}

pub fn remove_entry<T>(path: T, stash: Option<&LocalInfo>) -> Result<()>
where
    T: AsRef<path::Path> + Debug,
{
    debug!("remove_entry: {:?}", path);
    if !path.as_ref().exists() {
        return Ok(());
    }

    if let Some(local_info) = stash {
        stash_item(&path, local_info)?;
    }

    fs_extra::remove_items(&[path])?;

    Ok(())
}

pub fn remove_items<P>(paths: &[P], stash: Option<&LocalInfo>) -> Result<()>
where
    P: AsRef<path::Path> + Debug,
{
    debug!("remove items: len(paths) = {:?}", paths.len());

    // todo
    if let Some(local_info) = stash {
        for path in paths.iter() {
            if path.as_ref().exists() {
                stash_item(&path, local_info)?;
            }
        }
    }

    fs_extra::remove_items(paths)?;

    Ok(())
}

fn stash_item<P>(path: P, local_info: &LocalInfo) -> Result<()>
where
    P: AsRef<path::Path> + Debug,
{
    debug!("stash item: {:?}", path);

    if !path.as_ref().exists() {
        debug!("{:?} : not found.", path);
        return Ok(());
    }

    let stashpath_name = local_info.get_stashpath_name();
    let stash_folder = path::Path::new(&stashpath_name);

    fs::create_dir_all(&stash_folder)?;

    let p_ref = path.as_ref();
    let name = p_ref.file_stem().map(OsStr::to_string_lossy);

    let original_name = match name {
        Some(v) => v,
        _ => return Ok(()),
    }
    .to_string();

    let ext = p_ref.extension().map(OsStr::to_string_lossy);
    let ext = match ext {
        Some(e) => format!(".{}", e),
        _ => "".to_string(),
    };
    let dt = Local::now();
    let name = format!("{}_{}{}", original_name, dt.format("%Y%m%d%H%M%S%3f"), ext);

    if p_ref.is_file() {
        let target_path = stash_folder.join(name);
        fs::copy(path, target_path)?;
    } else {
        fs_extra::copy_items(
            &[&path],
            &stash_folder,
            &CopyOptions {
                overwrite: true,
                copy_inside: true,
                ..Default::default()
            },
        )?;
        let from_path = stash_folder.join(original_name);
        let target_path = stash_folder.join(name);

        fs::rename(from_path, target_path)?;
    }

    Ok(())
}
