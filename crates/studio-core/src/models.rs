//! Discovery, registration, and explicit opt-in deletion of local model directories.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use crate::{paths::AppPaths, store::Store};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModelRecord {
    pub id: String,
    pub name: String,
    pub source: String,
    pub repo_id: Option<String>,
    pub revision: Option<String>,
    pub path: PathBuf,
    pub weights_bytes: u64,
    pub compatible: bool,
    pub architecture: Option<String>,
    pub quantization: Option<String>,
    pub context_length: Option<u64>,
}

pub fn list(store: &Store) -> Result<Vec<ModelRecord>> {
    let mut records = store.list::<ModelRecord>("models")?;
    records.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(records)
}

/// Register a complete or inspectable local safetensors model without copying it.
pub fn import(store: &Store, path: &Path) -> Result<ModelRecord> {
    let record = inspect(path, "import", None, None)?;
    store.delete("model-hidden", &record.id)?;
    store.set("models", &record.id, &record)?;
    Ok(record)
}

/// Unregister only. Keeping the hidden ID prevents the next automatic scan from re-adding it.
pub fn remove(store: &Store, id: &str) -> Result<()> {
    if store.get::<ModelRecord>("models", id)?.is_none() {
        bail!("Unknown model: {id}");
    }
    store.set("model-hidden", id, &true)?;
    store.delete("models", id)
}

/// Delete only an explicitly selected, registered model directory. Shared Hub snapshots
/// must be managed by a cache-aware tool; deleting their links would not free the weights.
pub(crate) fn purge_files(
    paths: &AppPaths,
    store: &Store,
    model: &ModelRecord,
    library_roots: &[PathBuf],
) -> Result<()> {
    let path = &model.path;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Cannot inspect model directory {}", path.display()))?;
    let parent = path.parent().context("Cannot delete a filesystem root")?;
    let parent_metadata = fs::metadata(parent)?;
    let canonical = path.canonicalize()?;
    if !path.is_absolute()
        || canonical != *path
        || !metadata.is_dir()
        || metadata.file_type().is_symlink()
    {
        bail!(
            "The model directory changed or is a symbolic link; import its current physical directory before deleting files"
        );
    }
    if model.id != model_id(path) {
        bail!(
            "The registered model ID does not match its physical directory; rescan or import the model before deleting files"
        );
    }
    if model.source == "huggingface" || hf_identity(path).0.is_some() {
        bail!(
            "This model is a shared Hugging Face snapshot. Use a Hugging Face cache manager to delete it safely; removing its links alone would not free the shared weights. You can remove the library registration without deleting files."
        );
    }
    let mut protected = vec![
        paths.data_dir.clone(),
        paths.models_dir.clone(),
        paths.locals_source.clone(),
    ];
    protected.extend(library_roots.iter().cloned());
    if let Some(parent) = paths.database_path.parent() {
        protected.push(parent.to_owned());
    }
    if let Some(parent) = paths.socket_path.parent() {
        protected.push(parent.to_owned());
    }
    if let Some(home) = dirs::home_dir() {
        protected.push(home.clone());
        for relative in [".lmstudio/models", ".cache/lm-studio/models"] {
            protected.push(home.join(relative));
        }
        for settings in [
            home.join(".lmstudio/settings.json"),
            home.join(".cache/lm-studio/settings.json"),
        ] {
            if let Some(folder) = lmstudio_models_folder(&settings, &home) {
                protected.push(folder);
            }
        }
    }
    protected.extend(huggingface_home());
    protected.extend(resolve_hf_cache(dirs::home_dir(), env_path));
    if parent.parent().is_none()
        || protected
            .iter()
            .any(|root| normalized_path(root).starts_with(path))
    {
        bail!(
            "Refusing to delete a home, application, cache or library root. Select a model subdirectory instead."
        );
    }
    for other in list(store)?
        .into_iter()
        .filter(|other| other.id != model.id)
    {
        if paths_overlap(path, &other.path) || references_directory(&other.path, path, &mut 0)? {
            bail!(
                "Another registered model ('{}') overlaps or links to this directory; remove that relationship before deleting files",
                other.name
            );
        }
    }
    validate_purge_tree(path, path, metadata.dev(), &mut 0)?;
    crate::model_files::ModelDirectory::open(path, &metadata, &parent_metadata)?
        .remove()
        .with_context(|| {
            format!(
                "Could not delete all local files in {}; the model and profiles remain registered",
                path.display()
            )
        })
}

/// Resolve an existing prefix as well as missing download destinations.
pub(crate) fn normalized_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let mut prefix = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        if let Ok(canonical) = prefix.canonicalize() {
            let mut result = canonical;
            for component in suffix.into_iter().rev() {
                result.push(component);
            }
            return result;
        }
        let Some(name) = prefix.file_name() else {
            return absolute;
        };
        suffix.push(name.to_owned());
        let Some(parent) = prefix.parent() else {
            return absolute;
        };
        prefix = parent;
    }
}

pub(crate) fn paths_overlap(first: &Path, second: &Path) -> bool {
    let first = normalized_path(first);
    let second = normalized_path(second);
    first.starts_with(&second) || second.starts_with(&first)
}

fn references_directory(path: &Path, target: &Path, entries: &mut usize) -> Result<bool> {
    if path.components().count() > 128 {
        bail!("Model directory nesting is too deep to verify safely");
    }
    let children = match fs::read_dir(path) {
        Ok(children) => children,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("Cannot verify shared-file references in {}", path.display())
            });
        }
    };
    for child in children {
        let child = child?;
        *entries += 1;
        if *entries > 100_000 {
            bail!("Model directory is too large to verify shared-file references safely");
        }
        let kind = child.file_type()?;
        if kind.is_symlink() {
            match child.path().canonicalize() {
                Ok(path) if path.starts_with(target) => return Ok(true),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Cannot verify shared-file link {}", child.path().display())
                    });
                }
            }
        } else if kind.is_dir() && references_directory(&child.path(), target, entries)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_purge_tree(path: &Path, root: &Path, device: u64, count: &mut usize) -> Result<()> {
    if path.components().count() > 128 {
        bail!("Model directory nesting is too deep to verify safely");
    }
    let entries: Vec<_> = fs::read_dir(path)?.collect::<std::io::Result<_>>()?;
    if path != root
        && path.join("config.json").is_file()
        && entries.iter().any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "safetensors")
        })
    {
        bail!(
            "This directory contains another model at {}; delete only a single model directory",
            path.display()
        );
    }
    if path == root && !path.join("config.json").is_file() {
        bail!(
            "The selected directory no longer contains a model config.json; refusing to delete unrelated files"
        );
    }
    for entry in entries {
        *count += 1;
        if *count > 100_000 {
            bail!("Model directory is too large to verify safely before deletion");
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            if metadata.dev() != device {
                bail!("Refusing to delete a mounted directory inside a model");
            }
            validate_purge_tree(&entry.path(), root, device, count)?;
        } else if metadata.file_type().is_symlink() {
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "safetensors")
                && !entry.path().canonicalize()?.starts_with(root)
            {
                bail!(
                    "This model links to shared weights outside its directory. Remove the library registration without deleting files, or use the owning cache manager."
                );
            }
        } else if !metadata.is_file() {
            bail!("Refusing to delete a special file inside the model directory");
        }
    }
    Ok(())
}

fn model_id(path: &Path) -> String {
    format!(
        "model-{:x}",
        Sha256::digest(path.to_string_lossy().as_bytes())
    )
}

/// Scan Studio's model store, the shared HF cache, LM Studio and user-supplied roots.
/// Only model metadata is opened; no inference runtime or LM Studio daemon is started.
pub fn scan(paths: &AppPaths, store: &Store, extra: &[PathBuf]) -> Result<Vec<ModelRecord>> {
    let mut roots = vec![(paths.models_dir.clone(), "studio")];
    if let Some(root) = resolve_hf_cache(dirs::home_dir(), env_path) {
        roots.push((root, "huggingface"));
    }
    if let Some(home) = dirs::home_dir() {
        for settings in [
            home.join(".lmstudio/settings.json"),
            home.join(".cache/lm-studio/settings.json"),
        ] {
            if let Some(configured) = lmstudio_models_folder(&settings, &home) {
                roots.push((configured, "lmstudio"));
            }
        }
        roots.push((home.join(".lmstudio/models"), "lmstudio"));
        roots.push((home.join(".cache/lm-studio/models"), "lmstudio"));
    }
    for root in extra {
        roots.push((root.clone(), "external"));
    }

    let mut visited = HashSet::new();
    for (root, source) in roots {
        discover(&root, source, 0, &mut visited, store)?;
    }
    // Preserve explicit imports and configured paths, while updating missing/incomplete states.
    for mut record in list(store)? {
        if let Ok(updated) = inspect(
            &record.path,
            &record.source,
            record.repo_id.clone(),
            record.revision.clone(),
        ) {
            record = updated;
        } else {
            record.compatible = false;
        }
        store.set("models", &record.id, &record)?;
    }
    list(store)
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub(crate) fn huggingface_home() -> Option<PathBuf> {
    resolve_hf_home(dirs::home_dir(), env_path)
}

fn resolve_hf_home(
    home: Option<PathBuf>,
    lookup: impl Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    lookup("HF_HOME")
        .or_else(|| lookup("XDG_CACHE_HOME").map(|path| path.join("huggingface")))
        .or_else(|| home.map(|path| path.join(".cache/huggingface")))
}

fn resolve_hf_cache(
    home: Option<PathBuf>,
    lookup: impl Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    lookup("HF_HUB_CACHE")
        .or_else(|| lookup("HUGGINGFACE_HUB_CACHE"))
        .or_else(|| resolve_hf_home(home, lookup).map(|path| path.join("hub")))
}

fn lmstudio_models_folder(settings: &Path, home: &Path) -> Option<PathBuf> {
    let value: Value = serde_json::from_slice(&fs::read(settings).ok()?).ok()?;
    let folder = value.get("downloadsFolder")?.as_str()?.trim();
    if folder.is_empty() {
        return None;
    }
    let path = if folder == "~" {
        home.to_path_buf()
    } else if let Some(relative) = folder.strip_prefix("~/") {
        home.join(relative)
    } else {
        PathBuf::from(folder)
    };
    // LM Studio saves an absolute directory. Do not interpret malformed relative settings as CWD.
    path.is_absolute().then_some(path)
}

fn discover(
    path: &Path,
    source: &str,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    store: &Store,
) -> Result<()> {
    if depth > 7 || !path.is_dir() {
        return Ok(());
    }
    let Ok(canonical) = path.canonicalize() else {
        return Ok(());
    };
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }
    // An interrupted Studio download lives outside models_dir, but guard conventional staging roots too.
    if matches!(
        path.file_name().and_then(|s| s.to_str()),
        Some(".staging" | ".cache" | "blobs" | ".locks")
    ) {
        return Ok(());
    }
    if path.join("config.json").is_file() {
        let (repo, revision) = hf_identity(path);
        if let Ok(record) = inspect(path, source, repo, revision) {
            if !store
                .get::<bool>("model-hidden", &record.id)?
                .unwrap_or(false)
            {
                // Explicit identity from an already registered Studio download wins over path heuristics.
                let record = match store.get::<ModelRecord>("models", &record.id)? {
                    Some(old) => ModelRecord {
                        repo_id: old.repo_id.or(record.repo_id),
                        revision: old.revision.or(record.revision),
                        source: old.source,
                        ..record
                    },
                    None => record,
                };
                store.set("models", &record.id, &record)?;
            }
            return Ok(());
        }
    }
    let Ok(entries) = fs::read_dir(path) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let child = entry.path();
        // Do not walk arbitrary directory symlinks. HF's individual file symlinks are handled by inspect.
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            discover(&child, source, depth + 1, visited, store)?;
        }
    }
    Ok(())
}

pub(crate) fn register_download(
    store: &Store,
    path: &Path,
    repo: &str,
    revision: &str,
) -> Result<ModelRecord> {
    let record = inspect(
        path,
        "studio",
        Some(repo.to_owned()),
        Some(revision.to_owned()),
    )?;
    store.delete("model-hidden", &record.id)?;
    store.set("models", &record.id, &record)?;
    Ok(record)
}

fn inspect(
    path: &Path,
    source: &str,
    repo_id: Option<String>,
    revision: Option<String>,
) -> Result<ModelRecord> {
    let path = path
        .canonicalize()
        .with_context(|| format!("Model directory not found: {}", path.display()))?;
    if !path.is_dir() {
        bail!("The model path must be a directory");
    }
    let config: Value = serde_json::from_slice(&fs::read(path.join("config.json"))?)
        .context("Invalid config.json")?;
    let entries: Vec<_> = fs::read_dir(&path)?.filter_map(|e| e.ok()).collect();
    let mut weights_bytes = 0u64;
    let mut weight_count = 0usize;
    for entry in &entries {
        if entry.path().extension().and_then(|s| s.to_str()) == Some("safetensors") {
            if let Ok(metadata) = fs::metadata(entry.path()) {
                if metadata.is_file() && metadata.len() > 0 {
                    weights_bytes = weights_bytes.saturating_add(metadata.len());
                    weight_count += 1;
                }
            }
        }
    }
    if weight_count == 0 {
        bail!("No safetensors weights found in {}", path.display());
    }
    let mut complete = true;
    if path.join("model.safetensors.index.json").exists() {
        complete = fs::read(path.join("model.safetensors.index.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|index| index.get("weight_map").and_then(Value::as_object).cloned())
            .map(|map| {
                !map.is_empty()
                    && map.values().all(|v| {
                        v.as_str()
                            .filter(|name| safe_relative(name))
                            .map(|name| {
                                fs::metadata(path.join(name))
                                    .map(|m| m.is_file() && m.len() > 0)
                                    .unwrap_or(false)
                            })
                            .unwrap_or(false)
                    })
            })
            .unwrap_or(false);
    }
    let tokenizer = path.join("tokenizer.json").is_file() || path.join("tokenizer.model").is_file();
    let architecture = config
        .get("model_type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    // Locals unwraps the text decoder of supported multimodal wrappers (e.g. Qwen3.5).
    // A vision_config alone must therefore not exclude a model with a usable text_config.
    let is_text = config.get("text_config").is_some_and(Value::is_object)
        || (!config.get("vision_config").is_some_and(|v| !v.is_null())
            && !config.get("audio_config").is_some_and(|v| !v.is_null()));
    let quantization = config
        .get("quantization")
        .or_else(|| config.get("quantization_config"))
        .map(|q| {
            q.get("bits")
                .and_then(Value::as_u64)
                .map(|bits| format!("{bits}-bit"))
                .unwrap_or_else(|| {
                    q.get("quant_method")
                        .and_then(Value::as_str)
                        .unwrap_or("MLX")
                        .to_owned()
                })
        });
    let context_length = [
        "max_position_embeddings",
        "max_sequence_length",
        "seq_length",
        "context_length",
    ]
    .iter()
    .find_map(|key| config.get(key).and_then(Value::as_u64))
    .or_else(|| {
        config
            .pointer("/text_config/max_position_embeddings")
            .and_then(Value::as_u64)
    });
    let id = model_id(&path);
    let name = repo_id
        .as_deref()
        .and_then(|s| s.rsplit('/').next())
        .map(str::to_owned)
        .or_else(|| path.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| id.clone());
    Ok(ModelRecord {
        id,
        name,
        source: source.to_owned(),
        repo_id,
        revision,
        path,
        weights_bytes,
        // This is a structural eligibility check; the worker performs the actual runtime load validation.
        compatible: complete && tokenizer && architecture.is_some() && is_text,
        architecture,
        quantization,
        context_length,
    })
}

pub(crate) fn safe_relative(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\\')
        && Path::new(name)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn hf_identity(path: &Path) -> (Option<String>, Option<String>) {
    let Some(parent) = path.parent() else {
        return (None, None);
    };
    if parent.file_name().and_then(|s| s.to_str()) != Some("snapshots") {
        return (None, None);
    }
    let repo = parent
        .parent()
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("models--"))
        .map(|s| s.replace("--", "/"));
    let revision = path.file_name().and_then(|s| s.to_str()).map(str::to_owned);
    (repo, revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hf_cache_locations_follow_primary_legacy_home_and_xdg_precedence() {
        let resolve = |entries: &[(&str, &str)]| {
            resolve_hf_cache(Some(PathBuf::from("/home/user")), |key| {
                entries
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| PathBuf::from(value))
            })
            .unwrap()
        };
        assert_eq!(
            resolve(&[]),
            PathBuf::from("/home/user/.cache/huggingface/hub")
        );
        assert_eq!(
            resolve(&[("XDG_CACHE_HOME", "/xdg")]),
            PathBuf::from("/xdg/huggingface/hub")
        );
        assert_eq!(
            resolve(&[("HF_HOME", "/hf"), ("XDG_CACHE_HOME", "/xdg")]),
            PathBuf::from("/hf/hub")
        );
        assert_eq!(
            resolve(&[("HUGGINGFACE_HUB_CACHE", "/legacy"), ("HF_HOME", "/hf")]),
            PathBuf::from("/legacy")
        );
        assert_eq!(
            resolve(&[
                ("HF_HUB_CACHE", "/primary"),
                ("HUGGINGFACE_HUB_CACHE", "/legacy")
            ]),
            PathBuf::from("/primary")
        );
    }

    #[test]
    fn lmstudio_custom_downloads_folder_is_discovered_without_copying() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("external volume/models");
        let model = folder.join("publisher/model");
        fixture(&model);
        let settings = temp.path().join("settings.json");
        fs::write(
            &settings,
            serde_json::to_vec(&serde_json::json!({"downloadsFolder":folder})).unwrap(),
        )
        .unwrap();
        let configured = lmstudio_models_folder(&settings, temp.path()).unwrap();
        assert_eq!(configured, folder);
        let store = Store::open(&temp.path().join("store.db")).unwrap();
        discover(&configured, "lmstudio", 0, &mut HashSet::new(), &store).unwrap();
        let records = list(&store).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, model.canonicalize().unwrap());
        assert_eq!(records[0].source, "lmstudio");
        fs::write(&settings, br#"{"downloadsFolder":"~/models"}"#).unwrap();
        assert_eq!(
            lmstudio_models_folder(&settings, temp.path()).unwrap(),
            temp.path().join("models")
        );
        fs::write(&settings, br#"{"downloadsFolder":"relative-models"}"#).unwrap();
        assert!(lmstudio_models_folder(&settings, temp.path()).is_none());
        fs::write(&settings, b"invalid JSON").unwrap();
        assert!(lmstudio_models_folder(&settings, temp.path()).is_none());
    }

    fn fixture(root: &Path) {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("config.json"),
            br#"{"model_type":"qwen2","max_position_embeddings":32768,"quantization":{"bits":4}}"#,
        )
        .unwrap();
        fs::write(root.join("tokenizer.json"), b"{}").unwrap();
        fs::write(root.join("model.safetensors"), b"test weights").unwrap();
    }

    fn purge_paths(root: &Path) -> AppPaths {
        let paths = AppPaths {
            data_dir: root.join("data"),
            database_path: root.join("data/store.db"),
            socket_path: root.join("data/studio.sock"),
            models_dir: root.join("models"),
            locals_source: root.join("Locals"),
        };
        paths.prepare().unwrap();
        paths
    }

    #[test]
    fn explicit_purge_removes_physical_studio_imported_and_lmstudio_models_only() {
        let temp = tempfile::tempdir().unwrap();
        let paths = purge_paths(temp.path());
        let store = Store::open(&paths.database_path).unwrap();
        let keep = temp.path().join("keep");
        fs::write(&keep, b"unrelated").unwrap();
        for source in ["studio", "import", "lmstudio"] {
            let path = paths.models_dir.join(source);
            fixture(&path);
            fs::create_dir(path.join("nested")).unwrap();
            fs::write(path.join("nested/tokenizer-assets"), b"assets").unwrap();
            std::os::unix::fs::symlink(&keep, path.join("linked-asset")).unwrap();
            let record = inspect(&path, source, None, None).unwrap();
            store.set("models", &record.id, &record).unwrap();
            purge_files(&paths, &store, &record, &[]).unwrap();
            assert!(!path.exists());
            assert_eq!(fs::read(&keep).unwrap(), b"unrelated");
            // Purge and registration removal are deliberately separate operations.
            assert!(
                store
                    .get::<ModelRecord>("models", &record.id)
                    .unwrap()
                    .is_some()
            );
            remove(&store, &record.id).unwrap();
        }
    }

    #[test]
    fn purge_refuses_roots_overlap_nested_models_and_invalid_path_identity() {
        let temp = tempfile::tempdir().unwrap();
        let paths = purge_paths(temp.path());
        let store = Store::open(&paths.database_path).unwrap();
        fixture(&paths.models_dir);
        let root = import(&store, &paths.models_dir).unwrap();
        assert!(purge_files(&paths, &store, &root, &[]).is_err());
        remove(&store, &root.id).unwrap();
        let path = paths.models_dir.join("parent-model");
        fixture(&path);
        let parent = import(&store, &path).unwrap();
        let nested_path = path.join("nested-model");
        fixture(&nested_path);
        let nested = import(&store, &nested_path).unwrap();
        assert!(purge_files(&paths, &store, &parent, &[]).is_err());
        remove(&store, &nested.id).unwrap();
        assert!(
            purge_files(&paths, &store, &parent, &[])
                .unwrap_err()
                .to_string()
                .contains("another model")
        );
        assert!(nested_path.join("model.safetensors").is_file());
        let mut invalid = parent.clone();
        invalid.id = "wrong-id".to_owned();
        assert!(
            purge_files(&paths, &store, &invalid, &[])
                .unwrap_err()
                .to_string()
                .contains("ID")
        );
        assert!(path.join("model.safetensors").is_file());
        assert!(
            store
                .get::<ModelRecord>("models", &parent.id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn purge_refuses_hf_snapshots_and_shared_weights_without_deleting_blobs() {
        let temp = tempfile::tempdir().unwrap();
        let paths = purge_paths(temp.path());
        let store = Store::open(&paths.database_path).unwrap();
        let snapshot = temp
            .path()
            .join("hub/models--org--model/snapshots/revision");
        fixture(&snapshot);
        let record = import(&store, &snapshot).unwrap();
        assert!(
            purge_files(&paths, &store, &record, &[])
                .unwrap_err()
                .to_string()
                .contains("Hugging Face")
        );
        remove(&store, &record.id).unwrap();
        let path = paths.models_dir.join("linked-weights");
        fixture(&path);
        let blob = temp.path().join("shared-blob");
        fs::write(&blob, b"shared weights").unwrap();
        fs::remove_file(path.join("model.safetensors")).unwrap();
        std::os::unix::fs::symlink(&blob, path.join("model.safetensors")).unwrap();
        let record = import(&store, &path).unwrap();
        assert!(
            purge_files(&paths, &store, &record, &[])
                .unwrap_err()
                .to_string()
                .contains("shared weights")
        );
        assert_eq!(fs::read(blob).unwrap(), b"shared weights");
        assert!(path.exists());
        assert!(snapshot.exists());
    }

    #[test]
    fn purge_protects_weights_referenced_by_another_registered_model() {
        let temp = tempfile::tempdir().unwrap();
        let paths = purge_paths(temp.path());
        let store = Store::open(&paths.database_path).unwrap();
        let first = paths.models_dir.join("first");
        fixture(&first);
        let first = import(&store, &first).unwrap();
        let second = paths.models_dir.join("second");
        fixture(&second);
        fs::remove_file(second.join("model.safetensors")).unwrap();
        std::os::unix::fs::symlink(
            first.path.join("model.safetensors"),
            second.join("model.safetensors"),
        )
        .unwrap();
        import(&store, &second).unwrap();
        assert!(
            purge_files(&paths, &store, &first, &[])
                .unwrap_err()
                .to_string()
                .contains("links to")
        );
        assert!(first.path.join("model.safetensors").is_file());
    }

    #[test]
    fn model_inspection_checks_index_and_tokenizer() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path());
        let model = inspect(temp.path(), "external", None, None).unwrap();
        assert!(model.compatible);
        assert_eq!(model.quantization.as_deref(), Some("4-bit"));
        fs::write(
            temp.path().join("model.safetensors.index.json"),
            br#"{"weight_map":{"a":"missing.safetensors"}}"#,
        )
        .unwrap();
        assert!(
            !inspect(temp.path(), "external", None, None)
                .unwrap()
                .compatible
        );
        fs::remove_file(temp.path().join("model.safetensors.index.json")).unwrap();
        fs::remove_file(temp.path().join("tokenizer.json")).unwrap();
        assert!(
            !inspect(temp.path(), "external", None, None)
                .unwrap()
                .compatible
        );
    }

    #[test]
    fn external_identity_is_stable_and_hf_identity_is_retained() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = temp
            .path()
            .join("models--mlx-community--example/snapshots/0123456789");
        fixture(&snapshot);
        let (repo, revision) = hf_identity(&snapshot);
        let model = inspect(&snapshot, "huggingface", repo, revision).unwrap();
        assert_eq!(model.repo_id.as_deref(), Some("mlx-community/example"));
        assert_eq!(
            model.id,
            inspect(&snapshot, "import", None, None).unwrap().id
        );
    }

    #[test]
    fn paths_cannot_escape_a_model_root() {
        for path in ["../weights", "/weights", "a/../../b", "a\\b", ""] {
            assert!(!safe_relative(path));
        }
        assert!(safe_relative("nested/model-00001.safetensors"));
    }

    #[test]
    fn scanning_reuses_external_weights_and_unregistering_keeps_them() {
        let temp = tempfile::tempdir().unwrap();
        let external = temp.path().join("external/owner/model");
        fixture(&external);
        let paths = AppPaths {
            data_dir: temp.path().join("data"),
            database_path: temp.path().join("data/store.db"),
            socket_path: temp.path().join("studio.sock"),
            models_dir: temp.path().join("models"),
            locals_source: temp.path().join("Locals"),
        };
        paths.prepare().unwrap();
        let store = Store::open(&paths.database_path).unwrap();
        let record = import(&store, &external).unwrap();
        let records = scan(&paths, &store, &[temp.path().join("external")]).unwrap();
        assert_eq!(records.iter().filter(|r| r.id == record.id).count(), 1);
        assert_eq!(record.path, external.canonicalize().unwrap());
        assert!(fs::read_dir(&paths.models_dir).unwrap().next().is_none());
        remove(&store, &record.id).unwrap();
        assert!(external.join("model.safetensors").is_file());
        assert!(
            !scan(&paths, &store, &[temp.path().join("external")])
                .unwrap()
                .iter()
                .any(|r| r.id == record.id)
        );
        import(&store, &external).unwrap();
        assert!(list(&store).unwrap().iter().any(|r| r.id == record.id));
    }
}
