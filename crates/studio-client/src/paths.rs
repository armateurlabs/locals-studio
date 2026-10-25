use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{os::unix::fs::PermissionsExt, path::PathBuf};

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub database_path: PathBuf,
    pub socket_path: PathBuf,
    pub models_dir: PathBuf,
    pub locals_source: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let home = dirs::home_dir().context("Cannot determine the home directory")?;
        let data_dir = std::env::var_os("LOCALS_STUDIO_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::data_local_dir()
                    .unwrap_or_else(|| home.join(".local/share"))
                    .join("Locals Studio")
            });
        let hf_home = std::env::var_os("HF_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache/huggingface"));
        let models_dir = std::env::var_os("LOCALS_STUDIO_MODELS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| hf_home.join("locals-studio"));
        let mut socket_path = data_dir.join("studio.sock");
        if socket_path.as_os_str().len() > 98 {
            let digest = format!(
                "{:x}",
                Sha256::digest(data_dir.as_os_str().as_encoded_bytes())
            );
            socket_path = std::env::temp_dir()
                .join(format!("locals-studio-{}", &digest[..20]))
                .join("studio.sock");
        }
        let locals_source = std::env::var_os("LOCALS_STUDIO_LOCALS_SOURCE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../Locals"));
        Ok(Self {
            database_path: data_dir.join("studio.sqlite3"),
            data_dir,
            socket_path,
            models_dir,
            locals_source,
        })
    }

    pub fn prepare(&self) -> Result<()> {
        for dir in [
            &self.data_dir,
            self.socket_path.parent().context("Invalid socket path")?,
        ] {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::create_dir_all(&self.models_dir)?;
        Ok(())
    }
}
