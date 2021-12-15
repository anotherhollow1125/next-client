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

pub fn save_file<R: io::Read + ?Sized>(
    r: &mut R,
    filename: &str,
    use_stash: bool,
    local_info: &LocalInfo,
) -> Result<()> {
    debug!("save_file: {}", filename);

    let p = path::Path::new(filename);
    if p.exists() {
        autostash_item(p, local_info).map_err(|e| anyhow!("{:?} | {:?}", filename, e))?;
        if use_stash {
            stash_item(p, local_info).map_err(|e| anyhow!("{:?} | {:?}", filename, e))?;
        }
    }

    let mut out = fs::File::create(filename).map_err(|e| anyhow!("{:?} | {:?}", filename, e))?;
    io::copy(r, &mut out).map_err(|e| anyhow!("{:?} | {:?}", filename, e))?;

    Ok(())
}

pub fn create_dir_all<T>(dir_path: T) -> Result<()>
where
    T: AsRef<path::Path> + Debug,
{
    debug!("create_dir_all: {:?}", dir_path);

    fs::create_dir_all(&dir_path).map_err(|e| anyhow!("{:?} | {:?}", dir_path, e))?;

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
            .open(&path)
            .map(|_| ())
            .map_err(|e| anyhow!("{:?} | {:?}", path, e))?;
    } else {
        create_dir_all(&path)?;
    }

    Ok(())
}

pub fn move_entry<T, U>(
    from_path: T,
    to_path: U,
    use_stash: bool,
    local_info: &LocalInfo,
) -> Result<()>
where
    T: AsRef<path::Path> + Debug,
    U: AsRef<path::Path> + Debug,
{
    debug!("move_entry: {:?} => {:?}", from_path, to_path);

    if_chain! {
        if to_path.as_ref().exists();
        if to_path.as_ref().is_file();
        then {
            autostash_item(&to_path, local_info)
                .map_err(|e| anyhow!("{:?} | {:?}", to_path, e))?;
            if use_stash {
                stash_item(&to_path, local_info)
                    .map_err(|e| anyhow!("{:?} | {:?}", to_path, e))?;
            }
        }
    }

    let options = fs_extra::dir::CopyOptions {
        overwrite: true,
        copy_inside: true,
        ..Default::default()
    };
    fs_extra::move_items(&[&from_path], &to_path, &options)
        .map_err(|e| anyhow!("{:?}->{:?} | {:?}", from_path, to_path, e))?;

    Ok(())
}

pub fn remove_entry<T>(path: T, use_stash: bool, local_info: &LocalInfo) -> Result<()>
where
    T: AsRef<path::Path> + Debug,
{
    debug!(
        "remove_entry: {:?} (is_dir: {:?})",
        path,
        path.as_ref().is_dir()
    );
    if !path.as_ref().exists() {
        return Ok(());
    }

    autostash_item(&path, local_info).map_err(|e| anyhow!("{:?} | {:?}", path, e))?;
    if use_stash {
        stash_item(&path, local_info).map_err(|e| anyhow!("{:?} | {:?}", path, e))?;
    }

    fs_extra::remove_items(&[&path]).map_err(|e| anyhow!("{:?} | {:?}", path, e))?;

    Ok(())
}

pub fn remove_items<P>(paths: &[P], use_stash: bool, local_info: &LocalInfo) -> Result<()>
where
    P: AsRef<path::Path> + Debug,
{
    debug!("remove items: len(paths) = {:?}", paths.len());

    for path in paths.iter() {
        if path.as_ref().exists() {
            autostash_item(&path, local_info).map_err(|e| anyhow!("{:?} | {:?}", path, e))?;
            if use_stash {
                stash_item(&path, local_info).map_err(|e| anyhow!("{:?} | {:?}", path, e))?;
            }
        }
    }

    fs_extra::remove_items(paths).map_err(|e| anyhow!("{:?} | {:?}", paths, e))?;

    Ok(())
}

fn stash_item<P>(path: P, local_info: &LocalInfo) -> Result<()>
where
    P: AsRef<path::Path> + Debug,
{
    let stashpath_name = local_info.get_stashpath_name();
    let stash_folder = path::Path::new(&stashpath_name);

    stash_item_sub(path, stash_folder)?;

    Ok(())
}

fn autostash_item<P>(path: P, local_info: &LocalInfo) -> Result<()>
where
    P: AsRef<path::Path> + Debug,
{
    let stashpath_name = local_info.get_autostashpath_name_with_date();
    let stash_folder = path::Path::new(&stashpath_name);

    stash_item_sub(path, stash_folder)?;
    cleanup_autostash(local_info)?;

    debug!("autostash_item: {:?}", stash_folder);

    Ok(())
}

use crate::errors::NcsError::InvalidPathError;
use once_cell::sync::Lazy;
use regex::Regex;

fn cleanup_autostash(local_info: &LocalInfo) -> Result<()> {
    #[allow(non_upper_case_globals)]
    const re: Lazy<Regex> = Lazy::new(|| regex::Regex::new(r"^\d{8}+$").unwrap());

    let autostashpath_name = local_info.get_autostashpath_name();
    let stash_folder = path::Path::new(&autostashpath_name);

    for entry in stash_folder.read_dir()? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_dir() {
            return Ok(());
        }

        let f_name = path
            .file_name()
            .ok_or_else(|| InvalidPathError("Invalid file name.".to_string()))?
            .to_string_lossy();
        let d = match re.captures(&f_name) {
            Some(d) => d,
            None => continue,
        };

        let d = d.get(0).unwrap().as_str();
        let d_w = NaiveDate::parse_from_str(d, "%Y%m%d");
        let d = match d_w {
            Ok(d) => d,
            Err(_) => continue,
        };

        if Local::today().naive_local() - d
            > chrono::Duration::days(local_info.autostash_keep_span as i64)
        {
            fs::remove_dir_all(path)?;
        }
    }

    Ok(())
}

fn stash_item_sub<P, Q>(path: P, stash_folder: Q) -> Result<()>
where
    P: AsRef<path::Path> + Debug,
    Q: AsRef<path::Path> + Debug,
{
    debug!("stash item: {:?}", path);

    if !path.as_ref().exists() {
        debug!("{:?} : not found.", path);
        return Ok(());
    }

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
    let stash_folder = stash_folder.as_ref();

    if p_ref.is_file() {
        let target_path = stash_folder.join(name);
        fs::copy(path, target_path)?;
    } else {
        // let target_path = stash_folder.join(name);
        fs_extra::copy_items(
            &[&path],
            &stash_folder,
            &CopyOptions {
                overwrite: true,
                copy_inside: true,
                ..Default::default()
            },
        )?;
        let from_path = stash_folder.join(
            p_ref
                .file_name()
                .ok_or_else(|| anyhow!("{:?} | invalid dir", p_ref))?,
        );
        let target_path = stash_folder.join(name);
        fs::rename(&from_path, &target_path)?;
    }

    Ok(())
}
