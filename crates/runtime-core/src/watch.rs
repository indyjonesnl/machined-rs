//! Change events emitted by the [`crate::State`] store and the broadcast
//! channel controllers subscribe to.

use machined_resources::{ResourceObject, ResourceType};
use tokio::sync::broadcast;

/// What happened to a resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Created,
    Updated,
    Destroyed,
}

/// A change notification carrying the affected object's post-change state
/// (for `Destroyed`, the object as it was immediately before removal).
#[derive(Clone, Debug)]
pub struct Event {
    pub kind: EventKind,
    pub object: ResourceObject,
}

impl Event {
    pub fn namespace(&self) -> &str {
        &self.object.metadata.namespace
    }

    pub fn resource_type(&self) -> ResourceType {
        self.object.metadata.typ
    }
}

/// Capacity of the per-store broadcast channel. Sized generously; controllers
/// that lag past this see a `RecvError::Lagged` and perform a full re-list,
/// which is the correct recovery for a reconcile loop.
pub(crate) const CHANNEL_CAPACITY: usize = 1024;

/// Create the store's broadcast sender.
pub(crate) fn channel() -> broadcast::Sender<Event> {
    broadcast::Sender::new(CHANNEL_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{Resource, ServiceState, ServiceStatusSpec};

    fn sample() -> ResourceObject {
        ResourceObject::new(
            "runtime",
            "etcd",
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: "etcd".into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: "ok".into(),
            }),
        )
    }

    #[tokio::test]
    async fn broadcast_delivers_event() {
        let tx = channel();
        let mut rx = tx.subscribe();
        tx.send(Event {
            kind: EventKind::Created,
            object: sample(),
        })
        .unwrap();

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.kind, EventKind::Created);
        assert_eq!(ev.resource_type(), ResourceType::ServiceStatus);
        assert_eq!(ev.namespace(), "runtime");
    }
}
