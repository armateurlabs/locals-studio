use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; CREATE TABLE IF NOT EXISTS records(namespace TEXT NOT NULL, key TEXT NOT NULL, document TEXT NOT NULL, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(namespace,key)); PRAGMA user_version=1;")?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn get<T: DeserializeOwned>(&self, namespace: &str, key: &str) -> Result<Option<T>> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Database lock poisoned"))?;
        let value: Option<String> = conn
            .query_row(
                "SELECT document FROM records WHERE namespace=?1 AND key=?2",
                params![namespace, key],
                |r| r.get(0),
            )
            .optional()?;
        value
            .map(|v| serde_json::from_str(&v).context("Invalid saved record"))
            .transpose()
    }

    pub fn set<T: Serialize>(&self, namespace: &str, key: &str, value: &T) -> Result<()> {
        let document = serde_json::to_string(value)?;
        self.connection.lock().map_err(|_| anyhow::anyhow!("Database lock poisoned"))?.execute("INSERT INTO records(namespace,key,document) VALUES(?1,?2,?3) ON CONFLICT(namespace,key) DO UPDATE SET document=excluded.document,updated_at=CURRENT_TIMESTAMP", params![namespace,key,document])?;
        Ok(())
    }

    /// Persist related documents together so readers cannot observe a partial rename.
    pub fn set_records(&self, records: &[(&str, &str, serde_json::Value)]) -> Result<()> {
        let documents: Vec<_> = records
            .iter()
            .map(|(namespace, key, value)| Ok((*namespace, *key, serde_json::to_string(value)?)))
            .collect::<Result<_>>()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Database lock poisoned"))?;
        let transaction = connection.transaction()?;
        for (namespace, key, document) in documents {
            transaction.execute("INSERT INTO records(namespace,key,document) VALUES(?1,?2,?3) ON CONFLICT(namespace,key) DO UPDATE SET document=excluded.document,updated_at=CURRENT_TIMESTAMP", params![namespace,key,document])?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn list<T: DeserializeOwned>(&self, namespace: &str) -> Result<Vec<T>> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Database lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT document FROM records WHERE namespace=?1 ORDER BY updated_at DESC,key",
        )?;
        let rows = stmt.query_map([namespace], |r| r.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn delete_records(&self, records: &[(&str, &str)]) -> Result<()> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Database lock poisoned"))?;
        let transaction = connection.transaction()?;
        for (namespace, key) in records {
            transaction.execute(
                "DELETE FROM records WHERE namespace=?1 AND key=?2",
                params![namespace, key],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn delete(&self, namespace: &str, key: &str) -> Result<()> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Database lock poisoned"))?
            .execute(
                "DELETE FROM records WHERE namespace=?1 AND key=?2",
                params![namespace, key],
            )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn records_survive_reopening_and_namespaces_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        {
            let db = Store::open(&path).unwrap();
            db.set("models", "one", &json!({"value":1})).unwrap();
            db.set("profiles", "one", &json!({"value":2})).unwrap();
        }
        let db = Store::open(&path).unwrap();
        assert_eq!(
            db.get::<serde_json::Value>("models", "one")
                .unwrap()
                .unwrap()["value"],
            1
        );
        db.delete("profiles", "one").unwrap();
        assert!(db.list::<serde_json::Value>("profiles").unwrap().is_empty());
        assert_eq!(db.list::<serde_json::Value>("models").unwrap().len(), 1);
    }

    #[test]
    fn related_record_updates_roll_back_if_any_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(&dir.path().join("data.db")).unwrap();
        db.set("model-aliases", "pool", &json!({"name":"old"}))
            .unwrap();
        db.set("conversations", "chat", &json!({"model":"old"}))
            .unwrap();
        db.connection.lock().unwrap().execute_batch(
            "CREATE TRIGGER fail_conversation_write BEFORE INSERT ON records WHEN NEW.namespace='conversations' BEGIN SELECT RAISE(ABORT, 'synthetic persistence failure'); END;"
        ).unwrap();
        assert!(
            db.set_records(&[
                ("model-aliases", "pool", json!({"name":"new"})),
                ("conversations", "chat", json!({"model":"new"})),
            ])
            .is_err()
        );
        assert_eq!(
            db.get::<serde_json::Value>("model-aliases", "pool")
                .unwrap()
                .unwrap()["name"],
            "old"
        );
        assert_eq!(
            db.get::<serde_json::Value>("conversations", "chat")
                .unwrap()
                .unwrap()["model"],
            "old"
        );
    }
}
