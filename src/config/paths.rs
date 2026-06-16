use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("could not resolve user config directory")?;
    Ok(base.join("fish-coding-agent"))
}

pub fn config_file_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}
