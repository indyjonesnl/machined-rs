//! Owner-cascade helpers: a controller owns the resources it creates,
//! garbage-collects ones no longer desired, and (via `reconcile_finalized`)
//! holds a desired resource alive until its consumer reverts the real-world
//! state it produced.

use std::collections::HashSet;
use std::future::Future;

use machined_resources::{Phase, ResourceObject, ResourceType};

use crate::error::Result;
use crate::state::State;

/// Reconcile the full set of resources of one `(namespace, typ)` that `owner`
/// should have. Upserts each `desired` resource (stamping ownership on create),
/// and for each existing resource owned by `owner` whose id is not in `desired`,
/// tears it down — destroying it once no finalizers remain.
///
/// `desired` must all share `namespace` and `typ`; ids must be unique.
pub fn reconcile_owned(
    state: &State,
    owner: &str,
    namespace: &str,
    typ: ResourceType,
    desired: Vec<ResourceObject>,
) -> Result<()> {
    let desired_ids: HashSet<String> = desired.iter().map(|o| o.metadata.id.clone()).collect();

    // Upsert desired.
    for obj in desired {
        let key = obj.metadata.key();
        match state.get(&key) {
            Ok(existing) => {
                if existing.spec != obj.spec {
                    state.update(&key, existing.metadata.version, obj.spec)?;
                }
            }
            Err(crate::error::Error::NotFound(_)) => {
                let mut owned = obj;
                owned.metadata.owner = Some(owner.to_string());
                state.create(owned)?;
            }
            Err(e) => return Err(e),
        }
    }

    // GC owned resources no longer desired.
    for existing in state.list(namespace, typ) {
        let owned_by_us = existing.metadata.owner.as_deref() == Some(owner);
        if owned_by_us && !desired_ids.contains(&existing.metadata.id) {
            let key = existing.metadata.key();
            let ready = state.teardown(&key)?;
            if ready {
                // Re-read for the current version bumped by teardown.
                if let Ok(cur) = state.get(&key) {
                    state.destroy(&key, cur.metadata.version)?;
                }
            }
        }
    }

    Ok(())
}

/// Apply/revert a controller's strong inputs under a finalizer. For each input
/// in `Running`, ensures the finalizer is present then calls `apply`; for each
/// in `TearingDown`, calls `revert` then removes the finalizer (releasing the
/// resource for destruction).
pub async fn reconcile_finalized<A, R, AFut, RFut>(
    state: &State,
    finalizer: &str,
    inputs: &[ResourceObject],
    mut apply: A,
    mut revert: R,
) -> Result<()>
where
    A: FnMut(&ResourceObject) -> AFut,
    R: FnMut(&ResourceObject) -> RFut,
    AFut: Future<Output = Result<()>>,
    RFut: Future<Output = Result<()>>,
{
    for obj in inputs {
        let key = obj.metadata.key();
        match obj.metadata.phase {
            Phase::Running => {
                state.add_finalizer(&key, finalizer)?;
                apply(obj).await?;
            }
            Phase::TearingDown => {
                revert(obj).await?;
                state.remove_finalizer(&key, finalizer)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{Key, Resource, ResourceObject, ServiceState, ServiceStatusSpec};
    use std::sync::{Arc, Mutex};

    const NS: &str = "runtime";

    fn svc(id: &str) -> ResourceObject {
        ResourceObject::new(
            NS,
            id,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: id.into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: String::new(),
            }),
        )
    }

    fn key(id: &str) -> Key {
        Key::new(NS, ResourceType::ServiceStatus, id)
    }

    #[test]
    fn reconcile_owned_creates_and_stamps_owner() {
        let state = State::new();
        reconcile_owned(
            &state,
            "ctl",
            NS,
            ResourceType::ServiceStatus,
            vec![svc("a")],
        )
        .unwrap();
        let got = state.get(&key("a")).unwrap();
        assert_eq!(got.metadata.owner.as_deref(), Some("ctl"));
    }

    #[test]
    fn reconcile_owned_gcs_removed_without_finalizers() {
        let state = State::new();
        reconcile_owned(
            &state,
            "ctl",
            NS,
            ResourceType::ServiceStatus,
            vec![svc("a"), svc("b")],
        )
        .unwrap();
        // Second pass drops "b" from desired → it should be destroyed.
        reconcile_owned(
            &state,
            "ctl",
            NS,
            ResourceType::ServiceStatus,
            vec![svc("a")],
        )
        .unwrap();
        assert!(state.get(&key("a")).is_ok());
        assert!(matches!(
            state.get(&key("b")),
            Err(crate::error::Error::NotFound(_))
        ));
    }

    #[test]
    fn reconcile_owned_holds_removed_with_finalizer() {
        let state = State::new();
        reconcile_owned(
            &state,
            "ctl",
            NS,
            ResourceType::ServiceStatus,
            vec![svc("a")],
        )
        .unwrap();
        state.add_finalizer(&key("a"), "consumer").unwrap();
        // Drop "a" from desired → finalizer holds it in TearingDown, not destroyed.
        reconcile_owned(&state, "ctl", NS, ResourceType::ServiceStatus, vec![]).unwrap();
        let held = state.get(&key("a")).unwrap();
        assert_eq!(held.metadata.phase, Phase::TearingDown);
        // After the consumer clears its finalizer, a further pass destroys it.
        state.remove_finalizer(&key("a"), "consumer").unwrap();
        reconcile_owned(&state, "ctl", NS, ResourceType::ServiceStatus, vec![]).unwrap();
        assert!(matches!(
            state.get(&key("a")),
            Err(crate::error::Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn reconcile_finalized_applies_running_and_reverts_tearing_down() {
        let state = State::new();
        state.create(svc("running")).unwrap();
        // Build a TearingDown input: create, finalize, teardown.
        state.create(svc("dying")).unwrap();
        state.add_finalizer(&key("dying"), "net").unwrap();
        state.teardown(&key("dying")).unwrap();

        let inputs = vec![
            state.get(&key("running")).unwrap(),
            state.get(&key("dying")).unwrap(),
        ];

        let applied = Arc::new(Mutex::new(Vec::<String>::new()));
        let reverted = Arc::new(Mutex::new(Vec::<String>::new()));
        let a = applied.clone();
        let r = reverted.clone();

        reconcile_finalized(
            &state,
            "net",
            &inputs,
            move |obj| {
                let a = a.clone();
                let id = obj.metadata.id.clone();
                async move {
                    a.lock().unwrap().push(id);
                    Ok(())
                }
            },
            move |obj| {
                let r = r.clone();
                let id = obj.metadata.id.clone();
                async move {
                    r.lock().unwrap().push(id);
                    Ok(())
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(*applied.lock().unwrap(), vec!["running".to_string()]);
        assert_eq!(*reverted.lock().unwrap(), vec!["dying".to_string()]);
        // Running input got the finalizer added.
        assert!(state
            .get(&key("running"))
            .unwrap()
            .metadata
            .finalizers
            .contains(&"net".to_string()));
        // Dying input had its finalizer removed.
        assert!(!state
            .get(&key("dying"))
            .unwrap()
            .metadata
            .finalizers
            .contains(&"net".to_string()));
    }
}
