//! Shared launch-option templates. Applying one copies its options into a profile;
//! profiles never keep a live reference to the template.
use crate::{store::Store, supervisor::validate_server_options, types::ConfigurationPreset};
use anyhow::{Context, Result, bail};
use serde_json::json;

const PRESETS: &str = "configuration-presets";

fn builtins() -> Vec<ConfigurationPreset> {
    vec![
        ConfigurationPreset {
            id: "builtin:locals".to_owned(),
            name: "Locals defaults".to_owned(),
            options: Default::default(),
        },
        ConfigurationPreset {
            id: "builtin:qwen-non-thinking".to_owned(),
            name: "Qwen — non-thinking".to_owned(),
            options: json!({
                "temp":0.7,"top_p":0.8,"top_k":20,"max_tokens":16384,"max_context":131072
            })
            .as_object()
            .unwrap()
            .clone(),
        },
    ]
}

pub fn list(store: &Store) -> Result<Vec<ConfigurationPreset>> {
    let mut saved = store.list::<ConfigurationPreset>(PRESETS)?;
    saved.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    let mut presets = builtins();
    presets.extend(saved);
    Ok(presets)
}

pub fn save(store: &Store, mut preset: ConfigurationPreset) -> Result<ConfigurationPreset> {
    ensure_custom(&preset.id)?;
    preset.name = preset.name.trim().to_owned();
    if preset.name.is_empty()
        || preset.name.chars().count() > 128
        || preset.name.chars().any(char::is_control)
    {
        bail!("Use a preset name with 1 to 128 printable characters");
    }
    if list(store)?.iter().any(|other| {
        other.id != preset.id && other.name.to_lowercase() == preset.name.to_lowercase()
    }) {
        bail!("Another configuration preset already uses this name");
    }
    validate_server_options(&preset.options)?;
    store.set(PRESETS, &preset.id, &preset)?;
    Ok(preset)
}

pub fn remove(store: &Store, id: &str) -> Result<()> {
    ensure_custom(id)?;
    store
        .get::<ConfigurationPreset>(PRESETS, id)?
        .context("Configuration preset not found")?;
    store.delete(PRESETS, id)
}

fn ensure_custom(id: &str) -> Result<()> {
    if id.starts_with("builtin:") {
        bail!("Built-in configuration presets are read-only; save a custom copy instead");
    }
    uuid::Uuid::parse_str(id)
        .context("A custom configuration preset must have a UUID identifier")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ServerProfile;

    #[test]
    fn builtins_are_available_without_stored_defaults_and_are_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("presets.db")).unwrap();
        let presets = list(&store).unwrap();
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0].id, "builtin:locals");
        assert!(presets[0].options.is_empty());
        assert_eq!(presets[1].id, "builtin:qwen-non-thinking");
        assert_eq!(
            presets[1].options,
            json!({"temp":0.7,"top_p":0.8,"top_k":20,"max_tokens":16384,"max_context":131072})
                .as_object()
                .unwrap()
                .clone()
        );
        for mut preset in presets {
            preset.name = "Changed builtin".to_owned();
            assert!(
                save(&store, preset.clone())
                    .unwrap_err()
                    .to_string()
                    .contains("read-only")
            );
            assert!(
                remove(&store, &preset.id)
                    .unwrap_err()
                    .to_string()
                    .contains("read-only")
            );
        }
        assert!(
            store
                .list::<ConfigurationPreset>(PRESETS)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn custom_presets_persist_rename_and_remove_without_changing_profile_copies() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presets.db");
        let store = Store::open(&path).unwrap();
        let preset: ConfigurationPreset = serde_json::from_value(json!({
            "name":"  My mixedCase preset  ","options":{"--temp":0.35,"top_p":null,"thinking":false,"engines":["locals","mlx"]}
        })).unwrap();
        let saved = save(&store, preset).unwrap();
        assert!(uuid::Uuid::parse_str(&saved.id).is_ok());
        assert_eq!(saved.name, "My mixedCase preset");
        let profile = ServerProfile {
            options: saved.options.clone(),
            ..Default::default()
        };
        store.set("profiles", &profile.id, &profile).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let persisted = list(&store)
            .unwrap()
            .into_iter()
            .find(|preset| preset.id == saved.id)
            .unwrap();
        assert_eq!(persisted.options, saved.options);
        let changed = ConfigurationPreset {
            name: "Renamed shared preset".to_owned(),
            options: json!({"temp":0.9}).as_object().unwrap().clone(),
            ..persisted
        };
        save(&store, changed).unwrap();
        assert_eq!(
            store
                .get::<ServerProfile>("profiles", &profile.id)
                .unwrap()
                .unwrap()
                .options,
            profile.options
        );
        remove(&store, &saved.id).unwrap();
        assert_eq!(list(&store).unwrap().len(), 2);
        assert_eq!(
            store
                .get::<ServerProfile>("profiles", &profile.id)
                .unwrap()
                .unwrap()
                .options,
            profile.options
        );
        assert!(remove(&store, &saved.id).is_err());
    }

    #[test]
    fn presets_reject_invalid_names_managed_options_and_profile_identity_fields() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("presets.db")).unwrap();
        for name in [
            "".to_owned(),
            "   ".to_owned(),
            "bad\nname".to_owned(),
            "x".repeat(129),
            "Locals defaults".to_owned(),
        ] {
            assert!(
                save(
                    &store,
                    ConfigurationPreset {
                        name,
                        ..Default::default()
                    }
                )
                .is_err()
            );
        }
        let saved = save(
            &store,
            ConfigurationPreset {
                name: "Duplicate".to_owned(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            save(
                &store,
                ConfigurationPreset {
                    name: "duplicate".to_owned(),
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(
            save(
                &store,
                ConfigurationPreset {
                    id: "not-a-uuid".to_owned(),
                    name: "Invalid identity".to_owned(),
                    ..Default::default()
                }
            )
            .is_err()
        );
        for key in [
            "host",
            "--host",
            "model",
            "port",
            "--ollama-port",
            "unknown_option",
        ] {
            let mut preset = saved.clone();
            preset
                .options
                .insert(key.to_owned(), serde_json::Value::Null);
            assert!(save(&store, preset).is_err(), "{key}");
        }
        for options in [
            json!({"temp":{}}),
            json!({"temp":-1}),
            json!({"top_p":2}),
            json!({"top_k":1.5}),
            json!({"slots":"abc"}),
            json!({"engines":42}),
            json!({"top_k":true}),
            json!({"thinking":"true"}),
            json!({"cache_gb":0}),
        ] {
            let mut preset = saved.clone();
            preset.options = options.as_object().unwrap().clone();
            assert!(save(&store, preset).is_err());
        }
        for key in ["env", "alias", "model_id", "selected"] {
            let mut document = json!({"name":"Identity-free preset","options":{}});
            document[key] = json!("must not be copied");
            assert!(
                serde_json::from_value::<ConfigurationPreset>(document).is_err(),
                "{key}"
            );
        }
    }
}
