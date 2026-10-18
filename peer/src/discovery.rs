//! A bounded, on-demand walk of resident references. No retained global inventory.
use crate::{Client, Packet};
use std::{collections::VecDeque, io};

#[derive(Clone, Debug)]
pub struct Contact {
    pub address: u64,
    pub via: u64,
    pub responded: bool,
}
#[derive(Debug)]
pub struct Discovery {
    pub local: u64,
    pub founder: u64,
    pub channel: u8,
    pub contacts: Vec<Contact>,
    pub incomplete: Vec<String>,
}
impl Client {
    pub fn discover(&mut self, limit: usize) -> io::Result<Discovery> {
        if !(1..=128).contains(&limit) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "discovery limit must be 1..128",
            ));
        }
        let Packet::Ready {
            address: local,
            founder,
            channel,
            ..
        } = self.status()?
        else {
            return Err(io::Error::other("radio peer is not ready"));
        };
        let mut result = Discovery {
            local,
            founder,
            channel,
            contacts: Vec::new(),
            incomplete: Vec::new(),
        };
        let mut pending = VecDeque::from([(local, 0)]);
        while let Some((owner, via)) = pending.pop_front() {
            let mut after = 0;
            let mut revision = None;
            for index in 0..128 {
                let response = self.read_links(owner, after, via);
                let (current, peer) = match response {
                    Ok(Packet::Link { revision, peer, .. }) => (revision, peer),
                    other => {
                        result.incomplete.push(format!("{owner:016x}: {other:?}"));
                        break;
                    }
                };
                if let Some(contact) = result.contacts.iter_mut().find(|c| c.address == owner) {
                    contact.responded = true;
                }
                if revision.is_some_and(|r| r != current) || (peer != 0 && peer <= after) {
                    result
                        .incomplete
                        .push(format!("{owner:016x}: links changed during discovery"));
                    break;
                }
                revision = Some(current);
                if peer == 0 {
                    break;
                }
                after = peer;
                if peer != local && !result.contacts.iter().any(|c| c.address == peer) {
                    if result.contacts.len() == limit {
                        result
                            .incomplete
                            .push(format!("discovery stopped at {limit} vertices"));
                        return Ok(result);
                    }
                    result.contacts.push(Contact {
                        address: peer,
                        via: owner,
                        responded: false,
                    });
                    // A directly adjacent peer needs no traversal intermediary.
                    // Passing our own identity as `via` is not a valid RF hop.
                    pending.push_back((peer, if owner == local { 0 } else { owner }));
                }
                if index == 127 {
                    result
                        .incomplete
                        .push(format!("{owner:016x}: link limit reached"));
                }
            }
        }
        Ok(result)
    }
}
