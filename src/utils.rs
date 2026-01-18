use anyhow::{Context, Result};
use std::path::PathBuf;

fn get_runtime_dir() -> Result<PathBuf> {
    if let Some(dir) = dirs::runtime_dir()
        && dir.exists()
    {
        return Ok(dir);
    }

    if let Some(dir) = dirs::cache_dir()
        && dir.exists()
    {
        return Ok(dir);
    }

    if let Some(tmp) = std::env::temp_dir().to_str() {
        let buf = PathBuf::from(tmp);
        if buf.exists() {
            return Ok(buf);
        }
    }

    anyhow::bail!("Could not determine runtime directory (XDG_RUNTIME_DIR or /tmp)")
}

const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
pub fn get_socket_dir() -> Result<PathBuf> {
    let root = get_runtime_dir()?;
    let dir = root.join(CRATE_NAME);
    if !dir.exists() {
        std::fs::create_dir_all(&dir).context("Failed to create socket directory")?;
    }
    Ok(dir)
}