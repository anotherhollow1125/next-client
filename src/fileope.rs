// use crate::errors::NcsError::*;
use anyhow::Result;
use log::debug;
use std::fmt::Display;
use std::{fs, io, path};

pub fn save_file<R: io::Read>(r: &mut R, filename: &str) -> Result<()> {
    debug!("save_file: {}", filename);

    let mut out = fs::File::create(filename)?;
    io::copy(r, &mut out)?;

    Ok(())
}

pub fn create_dir_all<T>(dir_path: T) -> Result<()>
where
    T: AsRef<path::Path> + Display,
{
    debug!("create_dir_all: {}", dir_path);

    fs::create_dir_all(dir_path)?;

    Ok(())
}

pub fn touch_entry<T>(path: T, is_file: bool) -> Result<()>
where
    T: AsRef<path::Path> + Display,
{
    debug!("touch_entry: {}", path);

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

pub fn move_entry<T, U>(from_path: T, to_path: U, _stash: bool) -> Result<()>
where
    T: AsRef<path::Path> + Display,
    U: AsRef<path::Path> + Display,
{
    debug!("move_entry: {} => {}", from_path, to_path);

    // todo
    // stash function it will stash from_path file or dir.

    let options = fs_extra::dir::CopyOptions {
        overwrite: true,
        copy_inside: true,
        ..Default::default()
    };
    fs_extra::move_items(&[from_path], to_path, &options)?;

    Ok(())
}

pub fn remove_entry<T>(path: T, _stash: bool) -> Result<()>
where
    T: AsRef<path::Path> + Display,
{
    debug!("remove_entry: {}", path);

    // todo
    // stash function it will stash path file or dir.

    fs_extra::remove_items(&[path])?;

    Ok(())
}
