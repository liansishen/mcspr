use super::users;
use super::whitelist_sync::{SyncAllReport, SyncStatus};
use crate::auth::RetiredNameStatus;
use crate::state::AppState;
use std::collections::BTreeSet;

pub async fn release_confirmed(state: &AppState, report: &SyncAllReport) -> Result<usize, String> {
    if report.outcomes.iter().any(|outcome| {
        !matches!(
            outcome.status,
            SyncStatus::Applied | SyncStatus::Idle | SyncStatus::Unsupported
        )
    }) {
        return Ok(0);
    }
    let mut instances: Vec<_> = state.instances.read().await.values().cloned().collect();
    instances.sort_by(|a, b| a.dir.cmp(&b.dir));
    let mut guards = Vec::with_capacity(instances.len());
    for instance in &instances {
        guards.push(instance.whitelist_sync_lock.lock().await);
    }
    let mut retained = BTreeSet::new();
    for instance in &instances {
        let entries = users::read_array_strict(&instance.dir, "whitelist.json")?;
        for entry in entries {
            if let Some(name) = entry.get("name").and_then(serde_json::Value::as_str) {
                retained.insert(name.to_ascii_lowercase());
            }
        }
    }
    state
        .auth
        .release_retired_names_if_removed(|name| {
            if retained.contains(&name.to_ascii_lowercase()) {
                RetiredNameStatus::Retained
            } else {
                RetiredNameStatus::Removed
            }
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;
    use crate::config::PanelConfig;
    use crate::instance::whitelist_sync::{self, DesiredSnapshot};
    use crate::instance::{InstanceMeta, InstanceRuntime};

    #[tokio::test]
    async fn old_name_release_checks_manual_entries_on_all_instances() {
        let dir = std::env::temp_dir().join(format!("mcspr-name-release-{}", uuid::Uuid::new_v4()));
        let state = AppState::new(PanelConfig {
            data_dir: dir.to_string_lossy().into(),
            ..Default::default()
        })
        .await
        .unwrap();
        state
            .auth
            .create_user("root", "password123", Role::Admin, vec![])
            .await
            .unwrap();
        for id in ["granted", "previously-granted"] {
            let path = dir.join("instances").join(id);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("server.properties"), "online-mode=false\n").unwrap();
            let runtime = InstanceRuntime::new(
                InstanceMeta {
                    id: id.into(),
                    ..Default::default()
                },
                path,
            );
            state.instances.write().await.insert(id.into(), runtime);
        }
        let user = state
            .auth
            .register_pending_user("alice", "password123", "Alice", "test")
            .await
            .unwrap();
        state
            .auth
            .approve_application(&user.id, 1, vec!["granted".into()], "root")
            .await
            .unwrap();
        let (revision, grants) = state.auth.approved_grants_snapshot().await;
        whitelist_sync::sync_all(&state, &DesiredSnapshot::from_grants(revision, grants)).await;
        let request = state
            .auth
            .request_name_change(&user.id, "Neo", "rename")
            .await
            .unwrap();
        state
            .auth
            .approve_application(&request.id, request.revision, vec![], "root")
            .await
            .unwrap();
        let (revision, grants) = state.auth.approved_grants_snapshot().await;
        let report =
            whitelist_sync::sync_all(&state, &DesiredSnapshot::from_grants(revision, grants)).await;
        assert!(report.ok);
        let old_instance = state
            .instances
            .read()
            .await
            .get("previously-granted")
            .unwrap()
            .clone();
        users::write_array_atomic(
            &old_instance.dir,
            "whitelist.json",
            &[serde_json::json!({"name":"Alice","uuid":"manual"})],
        )
        .await
        .unwrap();
        assert_eq!(release_confirmed(&state, &report).await.unwrap(), 0);
        assert!(state
            .auth
            .register_pending_user("bob", "password123", "Alice", "test")
            .await
            .is_err());
        users::write_array_atomic(&old_instance.dir, "whitelist.json", &[]).await.unwrap();
        assert_eq!(release_confirmed(&state, &report).await.unwrap(), 1);
        assert!(state
            .auth
            .register_pending_user("bob", "password123", "Alice", "test")
            .await
            .is_ok());
        drop(old_instance);
        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
