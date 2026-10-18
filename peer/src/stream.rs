//! USB stream demultiplexing: replies and bounded pending observations.
use crate::{Packet, StateChange};
use lattice_peer_wire::host::Decoder;
use std::collections::VecDeque;
const CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LossSource {
    Companion,
    Client,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Changed(StateChange),
    /// Local buffering loss, not a count of frames lost over the radio.
    Lagged {
        source: LossSource,
        dropped: u32,
    },
}
pub(crate) struct Inbox {
    decoder: Decoder,
    changes: VecDeque<StateChange>,
    companion_dropped: u32,
    client_dropped: u32,
}
impl Inbox {
    pub(crate) fn new() -> Self {
        Self {
            decoder: Decoder::new(),
            changes: VecDeque::with_capacity(CAPACITY),
            companion_dropped: 0,
            client_dropped: 0,
        }
    }
    pub(crate) fn next(&mut self) -> Option<Event> {
        for (source, dropped) in [
            (LossSource::Companion, &mut self.companion_dropped),
            (LossSource::Client, &mut self.client_dropped),
        ] {
            if *dropped != 0 {
                return Some(Event::Lagged {
                    source,
                    dropped: std::mem::take(dropped),
                });
            }
        }
        self.changes.pop_front().map(Event::Changed)
    }
    pub(crate) fn feed(&mut self, bytes: &[u8], request: Option<Packet>) -> Option<Packet> {
        let mut response = None;
        for byte in bytes {
            let Some(packet) = self.decoder.push(*byte) else {
                continue;
            };
            match packet {
                Packet::Changed { change } => {
                    if self.changes.len() == CAPACITY {
                        self.changes.pop_front();
                        self.client_dropped = self.client_dropped.saturating_add(1);
                    }
                    self.changes.push_back(change);
                }
                Packet::ChangesLost { dropped } => {
                    self.companion_dropped = self.companion_dropped.saturating_add(dropped);
                }
                reply => {
                    if request.is_some_and(|request| matches_reply(request, reply))
                        && response.is_none()
                    {
                        response = Some(reply);
                    }
                }
            }
        }
        // Consume the entire OS read, including changes following this reply.
        response
    }
}
fn matches_reply(request: Packet, reply: Packet) -> bool {
    if request.token() != reply.token() {
        return false;
    }
    match (request, reply) {
        (_, Packet::Error { .. }) => true,
        (
            Packet::Update { owner, data, .. },
            Packet::Updated {
                owner: o, progress, ..
            },
        ) => {
            owner == o
                && lattice_peer_wire::update::Request::decode(data.as_slice())
                    .is_some_and(|request| request.package() == progress.package)
        }
        (Packet::UpdateControl { .. }, Packet::UpdateControlResult { .. }) => true,
        (Packet::Status { .. }, Packet::Ready { .. }) => true,
        (
            Packet::ReadLinks {
                owner, after, via, ..
            },
            Packet::Link {
                owner: o,
                after: a,
                via: v,
                ..
            },
        ) => (owner, after, via) == (o, a, v),
        (
            Packet::ReadSurface { owner, offset, .. },
            Packet::Surface {
                owner: o,
                offset: at,
                ..
            },
        ) => (owner, offset) == (o, at),
        (
            Packet::ReadState {
                owner, attribute, ..
            },
            Packet::State {
                owner: o,
                attribute: a,
                ..
            },
        ) => (owner, attribute) == (o, a),
        (
            Packet::Invoke {
                owner, transformer, ..
            },
            Packet::Invoked {
                owner: o,
                transformer: t,
                ..
            },
        ) => (owner, transformer) == (o, t),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lattice_peer_wire::host::MAX_PACKET;
    fn change(sequence: u32) -> StateChange {
        StateChange {
            owner: 2,
            attribute: 3,
            sequence,
            value: None,
        }
    }
    fn encode(packets: &[Packet]) -> Vec<u8> {
        let mut out = Vec::new();
        for packet in packets {
            let mut buffer = [0; MAX_PACKET];
            let n = packet.encode(&mut buffer);
            out.extend_from_slice(&buffer[..n]);
        }
        out
    }
    #[test]
    fn update_replies_require_token_owner_and_package_while_preserving_changes() {
        use lattice_peer_wire::{
            cost::TimePerByte,
            host::Bytes,
            update::{Phase, Progress, Request, Status},
        };
        let mut bytes = [0; lattice_peer_wire::update::REQUEST_BYTES];
        let n = Request::Finish { package: [7; 16] }.encode(&mut bytes);
        let request = Packet::Update {
            token: 12,
            owner: 9,
            data: Bytes::new(&bytes[..n]).unwrap(),
        };
        for (token, owner, package, accepted) in [
            (12, 9, 7, true),
            (13, 9, 7, false),
            (12, 8, 7, false),
            (12, 9, 8, false),
        ] {
            let reply = Packet::Updated {
                token,
                owner,
                progress: Progress {
                    package: [package; 16],
                    phase: Phase::Verifying,
                    status: Status::Ok,
                    accepted: 100,
                    committed: 100,
                    prepared: 1,
                },
                cost: TimePerByte::ZERO,
            };
            let mut inbox = Inbox::new();
            assert_eq!(
                inbox.feed(
                    &encode(&[reply, Packet::Changed { change: change(1) }]),
                    Some(request)
                ),
                accepted.then_some(reply)
            );
            assert_eq!(inbox.next(), Some(Event::Changed(change(1))));
        }
    }
    #[test]
    fn changes_survive_reply_coalescing_splits_and_buffer_pressure() {
        let request = Packet::Status { token: 0 };
        let reply = Packet::Ready {
            token: 0,
            address: 1,
            founder: 1,
            channel: 15,
        };
        let bytes = encode(&[
            Packet::Changed { change: change(1) },
            reply,
            Packet::Changed { change: change(2) },
        ]);
        for split in 0..=bytes.len() {
            let mut inbox = Inbox::new();
            let a = inbox.feed(&bytes[..split], Some(request));
            let b = inbox.feed(&bytes[split..], Some(request));
            assert_eq!(a.or(b), Some(reply));
            assert_eq!(inbox.next(), Some(Event::Changed(change(1))));
            assert_eq!(inbox.next(), Some(Event::Changed(change(2))));
            assert_eq!(inbox.next(), None);
        }
        let mut inbox = Inbox::new();
        for sequence in 0..35 {
            assert_eq!(
                inbox.feed(
                    &encode(&[Packet::Changed {
                        change: change(sequence)
                    }]),
                    Some(request)
                ),
                None
            );
        }
        inbox.feed(&encode(&[Packet::ChangesLost { dropped: 7 }]), None);
        assert_eq!(
            inbox.next(),
            Some(Event::Lagged {
                source: LossSource::Companion,
                dropped: 7
            })
        );
        assert_eq!(
            inbox.next(),
            Some(Event::Lagged {
                source: LossSource::Client,
                dropped: 3
            })
        );
        for sequence in 3..35 {
            assert_eq!(inbox.next(), Some(Event::Changed(change(sequence))));
        }
        assert_eq!(inbox.next(), None);
        assert_eq!(
            inbox.feed(
                &encode(&[Packet::Ready {
                    token: 1,
                    address: 1,
                    founder: 1,
                    channel: 15
                }]),
                Some(request)
            ),
            None
        );
    }
}
