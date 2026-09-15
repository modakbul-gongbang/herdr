#[derive(Clone, Default)]
pub struct EventHub {
    inner: std::sync::Arc<std::sync::Mutex<EventHubState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventSequenceWindow {
    pub oldest_available: u64,
    pub latest: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventGap {
    pub requested_after: u64,
    pub oldest_available: u64,
    pub latest: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventReadError {
    Gap(EventGap),
    Unavailable,
}

#[derive(Default)]
struct EventHubState {
    next_sequence: u64,
    events: Vec<(u64, crate::api::schema::EventEnvelope)>,
}

impl EventHub {
    const MAX_EVENTS: usize = 512;

    pub fn push(&self, event: crate::api::schema::EventEnvelope) {
        let Ok(mut state) = self.inner.lock() else {
            tracing::error!("event journal lock poisoned; domain event was not recorded");
            return;
        };
        state.next_sequence += 1;
        let sequence = state.next_sequence;
        state.events.push((sequence, event));
        let overflow = state.events.len().saturating_sub(Self::MAX_EVENTS);
        if overflow > 0 {
            state.events.drain(0..overflow);
        }
    }

    pub fn events_after(&self, sequence: u64) -> Vec<(u64, crate::api::schema::EventEnvelope)> {
        let Ok(state) = self.inner.lock() else {
            return Vec::new();
        };
        state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect()
    }

    pub fn current_sequence(&self) -> u64 {
        match self.current_sequence_result() {
            Ok(sequence) => sequence,
            Err(EventReadError::Unavailable) => {
                tracing::error!("event journal lock poisoned; current sequence unavailable");
                0
            }
            Err(EventReadError::Gap(_)) => unreachable!("reading the current sequence cannot gap"),
        }
    }

    pub fn current_sequence_result(&self) -> Result<u64, EventReadError> {
        self.inner
            .lock()
            .map(|state| state.next_sequence)
            .map_err(|_| EventReadError::Unavailable)
    }

    pub fn sequence_window(&self) -> Result<EventSequenceWindow, EventReadError> {
        let state = self.inner.lock().map_err(|_| EventReadError::Unavailable)?;
        let oldest_available = state
            .events
            .first()
            .map(|(sequence, _)| *sequence)
            .unwrap_or_else(|| state.next_sequence.saturating_add(1));
        Ok(EventSequenceWindow {
            oldest_available,
            latest: state.next_sequence,
        })
    }

    pub fn read_after(
        &self,
        requested_after: u64,
    ) -> Result<Vec<crate::api::schema::SequencedEventEnvelope>, EventReadError> {
        let state = self.inner.lock().map_err(|_| EventReadError::Unavailable)?;
        let oldest_available = state
            .events
            .first()
            .map(|(sequence, _)| *sequence)
            .unwrap_or_else(|| state.next_sequence.saturating_add(1));
        let earliest_valid_cursor = oldest_available.saturating_sub(1);
        if requested_after < earliest_valid_cursor || requested_after > state.next_sequence {
            return Err(EventReadError::Gap(EventGap {
                requested_after,
                oldest_available,
                latest: state.next_sequence,
            }));
        }

        let host = crate::api::host_scope();
        Ok(state
            .events
            .iter()
            .filter(|(sequence, _)| *sequence > requested_after)
            .map(
                |(sequence, event)| crate::api::schema::SequencedEventEnvelope {
                    protocol: crate::protocol::PROTOCOL_VERSION,
                    host: host.clone(),
                    sequence: *sequence,
                    event: event.event,
                    data: event.data.clone(),
                },
            )
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EventData, EventEnvelope, EventKind};

    fn event(index: u64) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::PaneClosed,
            data: EventData::PaneClosed {
                pane_id: format!("pane_{index}"),
                workspace_id: "workspace_1".into(),
            },
        }
    }

    #[test]
    fn read_after_returns_monotonic_sequenced_events() {
        let hub = EventHub::default();
        hub.push(event(1));
        hub.push(event(2));

        let events = hub.read_after(0).expect("ordered events");
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(events[0].data, event(1).data);
        assert_eq!(events[1].data, event(2).data);
    }

    #[test]
    fn read_after_reports_gap_after_retention_overflow() {
        let hub = EventHub::default();
        for index in 0..=(EventHub::MAX_EVENTS as u64) {
            hub.push(event(index));
        }

        let Err(EventReadError::Gap(gap)) = hub.read_after(0) else {
            panic!("expected explicit event gap");
        };
        assert_eq!(gap.requested_after, 0);
        assert_eq!(gap.oldest_available, 2);
        assert_eq!(gap.latest, EventHub::MAX_EVENTS as u64 + 1);
    }

    #[test]
    fn read_after_rejects_future_cursor() {
        let hub = EventHub::default();
        hub.push(event(1));

        assert!(matches!(
            hub.read_after(2),
            Err(EventReadError::Gap(EventGap {
                requested_after: 2,
                oldest_available: 1,
                latest: 1,
            }))
        ));
    }
}
