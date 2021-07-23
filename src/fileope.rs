use std::{fs, io, path};

pub fn save_file<R: io::Read>(r: &mut R, filename: &str) -> anyhow::Result<()> {
    let mut out = fs::File::create(filename)?;
    io::copy(r, &mut out)?;

    Ok(())
}

pub fn create_dir_all(dir_path: impl AsRef<path::Path>) -> anyhow::Result<()> {
    fs::create_dir_all(dir_path)?;

    Ok(())
}
