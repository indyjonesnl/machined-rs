//! In-memory resource store with COSI semantics: versioned CAS updates,
//! finalizers, owner refs, teardown, and change broadcasting.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use machined_resources::{Key, Phase, Resource, ResourceObject, ResourceType};
use tokio::sync::broadcast;

use crate::error::{Error, Result};
use crate::watch::{channel, Event, EventKind};

#[derive(Default)]
struct Inner {
    objects: HashMap<Key, ResourceObject>,
}

/// Cheap-to-clone shared handle to the resource store.
#[derive(Clone)]
pub struct State {
    inner: Arc<Mutex<Inner>>,
    tx: broadcast::Sender<Event>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            tx: channel(),
        }
    }

    /// Subscribe to all change events. Controllers filter by type/namespace.
    pub fn watch(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    fn emit(&self, kind: EventKind, object: ResourceObject) {
        // A send error means no subscribers; that is fine.
        let _ = self.tx.send(Event { kind, object });
    }

    /// Fetch a resource, or `Error::NotFound`.
    pub fn get(&self, key: &Key) -> Result<ResourceObject> {
        let inner = self.inner.lock().unwrap();
        inner
            .objects
            .get(key)
            .cloned()
            .ok_or_else(|| Error::NotFound(key.clone()))
    }

    /// List all resources of a type within a namespace.
    pub fn list(&self, namespace: &str, typ: ResourceType) -> Vec<ResourceObject> {
        let inner = self.inner.lock().unwrap();
        inner
            .objects
            .values()
            .filter(|o| o.metadata.namespace == namespace && o.metadata.typ == typ)
            .cloned()
            .collect()
    }

    /// Create a new resource. The object's version is reset to 1 and its phase
    /// is forced to `Running` regardless of the supplied metadata; any
    /// caller-set `finalizers`/`owner` are preserved. Errors with
    /// `AlreadyExists` if the key is taken.
    pub fn create(&self, mut object: ResourceObject) -> Result<()> {
        let key = object.metadata.key();
        let mut inner = self.inner.lock().unwrap();
        if inner.objects.contains_key(&key) {
            return Err(Error::AlreadyExists(key));
        }
        object.metadata.version = 1;
        object.metadata.phase = Phase::Running;
        inner.objects.insert(key, object.clone());
        drop(inner);
        self.emit(EventKind::Created, object);
        Ok(())
    }

    /// Replace a resource's spec, requiring `expected_version` to match
    /// (optimistic concurrency). On success the stored version is bumped.
    pub fn update(&self, key: &Key, expected_version: u64, spec: Resource) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let obj = inner
            .objects
            .get_mut(key)
            .ok_or_else(|| Error::NotFound(key.clone()))?;
        if obj.metadata.version != expected_version {
            return Err(Error::Conflict {
                key: key.clone(),
                expected: expected_version,
                found: obj.metadata.version,
            });
        }
        obj.spec = spec;
        obj.metadata.version += 1;
        let snapshot = obj.clone();
        drop(inner);
        self.emit(EventKind::Updated, snapshot);
        Ok(())
    }

    /// Add a finalizer to a resource. Idempotent.
    pub fn add_finalizer(&self, key: &Key, finalizer: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let obj = inner
            .objects
            .get_mut(key)
            .ok_or_else(|| Error::NotFound(key.clone()))?;
        if !obj.metadata.finalizers.iter().any(|f| f == finalizer) {
            obj.metadata.finalizers.push(finalizer.to_string());
            obj.metadata.version += 1;
            let snapshot = obj.clone();
            drop(inner);
            self.emit(EventKind::Updated, snapshot);
        }
        Ok(())
    }

    /// Remove a finalizer. Idempotent.
    pub fn remove_finalizer(&self, key: &Key, finalizer: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let obj = inner
            .objects
            .get_mut(key)
            .ok_or_else(|| Error::NotFound(key.clone()))?;
        let before = obj.metadata.finalizers.len();
        obj.metadata.finalizers.retain(|f| f != finalizer);
        if obj.metadata.finalizers.len() != before {
            obj.metadata.version += 1;
            let snapshot = obj.clone();
            drop(inner);
            self.emit(EventKind::Updated, snapshot);
        }
        Ok(())
    }

    /// Mark a resource for deletion. Sets `Phase::TearingDown` and returns
    /// `true` when it is ready to destroy (no finalizers remain). When
    /// finalizers are present, the resource stays in the store in
    /// `TearingDown` so its owner's strong-input controllers can clean up.
    pub fn teardown(&self, key: &Key) -> Result<bool> {
        let mut inner = self.inner.lock().unwrap();
        let obj = inner
            .objects
            .get_mut(key)
            .ok_or_else(|| Error::NotFound(key.clone()))?;
        // Compute the return value before any further use of the broadcast
        // sender, so the mutable borrow of `obj` ends here.
        let ready = obj.metadata.finalizers.is_empty();
        let mut event = None;
        if obj.metadata.phase != Phase::TearingDown {
            obj.metadata.phase = Phase::TearingDown;
            obj.metadata.version += 1;
            event = Some(obj.clone());
        }
        drop(inner);
        if let Some(object) = event {
            let _ = self.tx.send(Event {
                kind: EventKind::Updated,
                object,
            });
        }
        Ok(ready)
    }

    /// Permanently remove a resource. Requires `expected_version` to match and
    /// errors with `HasFinalizers` if any finalizer remains.
    pub fn destroy(&self, key: &Key, expected_version: u64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let obj = inner
            .objects
            .get(key)
            .ok_or_else(|| Error::NotFound(key.clone()))?;
        if obj.metadata.version != expected_version {
            return Err(Error::Conflict {
                key: key.clone(),
                expected: expected_version,
                found: obj.metadata.version,
            });
        }
        if !obj.metadata.finalizers.is_empty() {
            return Err(Error::HasFinalizers(key.clone()));
        }
        let removed = inner.objects.remove(key).unwrap();
        drop(inner);
        self.emit(EventKind::Destroyed, removed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{ServiceState, ServiceStatusSpec};

    fn svc(id: &str, state: ServiceState) -> ResourceObject {
        ResourceObject::new(
            "runtime",
            id,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: id.into(),
                state,
                healthy: true,
                last_message: String::new(),
            }),
        )
    }

    fn key(id: &str) -> Key {
        Key::new("runtime", ResourceType::ServiceStatus, id)
    }

    #[test]
    fn create_sets_version_one_and_rejects_duplicates() {
        let st = State::new();
        st.create(svc("etcd", ServiceState::Preparing)).unwrap();
        let got = st.get(&key("etcd")).unwrap();
        assert_eq!(got.metadata.version, 1);

        let err = st.create(svc("etcd", ServiceState::Preparing)).unwrap_err();
        assert!(matches!(err, Error::AlreadyExists(_)));
    }

    #[test]
    fn update_requires_matching_version() {
        let st = State::new();
        st.create(svc("etcd", ServiceState::Preparing)).unwrap();

        // Stale version is rejected.
        let stale = st.update(
            &key("etcd"),
            99,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: "etcd".into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: String::new(),
            }),
        );
        assert!(matches!(stale, Err(Error::Conflict { .. })));

        // Correct version succeeds and bumps to 2.
        st.update(
            &key("etcd"),
            1,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: "etcd".into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: String::new(),
            }),
        )
        .unwrap();
        assert_eq!(st.get(&key("etcd")).unwrap().metadata.version, 2);
    }

    #[test]
    fn finalizer_gated_teardown() {
        let st = State::new();
        st.create(svc("etcd", ServiceState::Running)).unwrap();
        st.add_finalizer(&key("etcd"), "controller-x").unwrap();

        // teardown holds the resource while a finalizer remains.
        assert!(!st.teardown(&key("etcd")).unwrap());
        let v = st.get(&key("etcd")).unwrap().metadata.version;
        let destroy_err = st.destroy(&key("etcd"), v).unwrap_err();
        assert!(matches!(destroy_err, Error::HasFinalizers(_)));

        // Once the finalizer is cleared, teardown reports ready and destroy works.
        st.remove_finalizer(&key("etcd"), "controller-x").unwrap();
        assert!(st.teardown(&key("etcd")).unwrap());
        let v = st.get(&key("etcd")).unwrap().metadata.version;
        st.destroy(&key("etcd"), v).unwrap();
        assert!(matches!(st.get(&key("etcd")), Err(Error::NotFound(_))));
    }

    #[test]
    fn list_filters_by_namespace_and_type() {
        let st = State::new();
        st.create(svc("etcd", ServiceState::Running)).unwrap();
        st.create(svc("kubelet", ServiceState::Running)).unwrap();
        let all = st.list("runtime", ResourceType::ServiceStatus);
        assert_eq!(all.len(), 2);
        assert!(st.list("other", ResourceType::ServiceStatus).is_empty());
    }
}
