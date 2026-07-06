//! In-memory [`InstanceRegistry`] — the zero-dependency default backend.
//!
//! Useful for single-host deployments and tests. For multi-host rolling
//! updates, back this with a shared store (Postgres table / shared JSON file).

use std::{collections::HashMap, sync::Mutex};

use async_trait::async_trait;

use crate::{
    InstanceInfo, InstanceRole,
    traits::{InstanceRegistry, RegistryError},
};

/// A simple in-memory registry keyed by `(group, instance_id)`.
pub struct InMemoryRegistry {
    by_group: Mutex<HashMap<String, HashMap<String, InstanceInfo>>>,
}

impl Default for InMemoryRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_group: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl InstanceRegistry for InMemoryRegistry {
    async fn register(&self, info: InstanceInfo) -> Result<(), RegistryError> {
        let mut g = self.by_group.lock().map_err(poison)?;
        g.entry(info.group.clone())
            .or_default()
            .insert(info.instance_id.clone(), info);
        Ok(())
    }

    async fn set_role(&self, instance_id: &str, role: InstanceRole) -> Result<(), RegistryError> {
        let mut g = self.by_group.lock().map_err(poison)?;
        for members in g.values_mut() {
            if let Some(info) = members.get_mut(instance_id) {
                info.role = role;
                return Ok(());
            }
        }
        Err(RegistryError::NotFound(instance_id.to_string()))
    }

    async fn deregister(&self, instance_id: &str) -> Result<(), RegistryError> {
        let mut g = self.by_group.lock().map_err(poison)?;
        for members in g.values_mut() {
            if members.remove(instance_id).is_some() {
                return Ok(());
            }
        }
        Err(RegistryError::NotFound(instance_id.to_string()))
    }

    async fn list(&self, group: &str) -> Result<Vec<InstanceInfo>, RegistryError> {
        let g = self.by_group.lock().map_err(poison)?;
        Ok(g.get(group)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default())
    }
}

fn poison<T>(_: T) -> RegistryError {
    RegistryError::Store("registry mutex poisoned".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, group: &str, role: InstanceRole) -> InstanceInfo {
        InstanceInfo {
            instance_id: id.into(),
            group: group.into(),
            role,
            generation: 1,
            started_at: "1970-01-01T00:00:00Z".into(),
            endpoint: None,
            backend: None,
        }
    }

    #[tokio::test]
    async fn register_list_setrole_deregister() {
        let r = InMemoryRegistry::new();
        r.register(info("a", "g", InstanceRole::Active))
            .await
            .unwrap();
        r.register(info("b", "g", InstanceRole::Active))
            .await
            .unwrap();
        assert_eq!(r.list("g").await.unwrap().len(), 2);
        r.set_role("a", InstanceRole::Draining).await.unwrap();
        r.deregister("b").await.unwrap();
        let v = r.list("g").await.unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].role, InstanceRole::Draining);
    }
}
