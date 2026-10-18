//! Immutable package transfer through an ordinary peer. Activation remains local
//! to the receiving kernel. Native calls never use this retry policy.
use crate::{Client, Failure, Packet};
use lattice_peer_wire::{
    cost::TimePerByte,
    host::Bytes,
    update::{Control, DATA_BYTES, Phase, Progress, REQUEST_BYTES, Request, Status},
};
use sha2::{Digest, Sha256};
use std::{
    io, thread,
    time::{Duration, Instant},
};

pub const HEADER_BYTES: usize = 192;
pub const MAX_PACKAGE_BYTES: usize = 16 * 1024 * 1024 + HEADER_BYTES;

#[derive(Debug)]
pub struct Package {
    bytes: Vec<u8>,
    pub id: [u8; 16],
    pub platform: u16,
    pub layout: u32,
    pub appliance: [u8; 16],
    pub release: u64,
    pub measured: bool,
}
impl Package {
    /// Check transport integrity before touching a receiver. The receiver owns
    /// trusted-key signature verification and appliance compatibility decisions.
    pub fn parse(bytes: Vec<u8>) -> io::Result<Self> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid firmware package, length or image digest",
            )
        };
        if !(HEADER_BYTES + 1..=MAX_PACKAGE_BYTES).contains(&bytes.len())
            || &bytes[..6] != b"LVUP\x01\0"
        {
            return Err(invalid());
        }
        let image = &bytes[HEADER_BYTES..];
        if u32::from_le_bytes(bytes[68..72].try_into().unwrap()) as usize != image.len()
            || bytes[72..104] != Sha256::digest(image)[..]
        {
            return Err(invalid());
        }
        Ok(Self {
            id: Sha256::digest(&bytes[..HEADER_BYTES])[..16]
                .try_into()
                .unwrap(),
            platform: u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
            layout: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            appliance: bytes[12..28].try_into().unwrap(),
            release: u64::from_le_bytes(bytes[60..68].try_into().unwrap()),
            measured: bytes[104..128].chunks_exact(4).all(|c| c != [255; 4]),
            bytes,
        })
    }
    pub fn image_bytes(&self) -> u32 {
        (self.bytes.len() - HEADER_BYTES) as u32
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TransferProgress {
    pub receiver: Progress,
    pub path_cost: TimePerByte,
}

#[derive(Clone, Copy, Debug)]
pub enum UploadEvent {
    Progress(TransferProgress),
    Observed(crate::Event),
}

impl Client {
    /// Explicit maintenance on the attached MCU only; never routed or retried.
    pub fn update_control(&mut self, action: Control) -> io::Result<Packet> {
        self.token = self.token.wrapping_add(1);
        self.exchange(Packet::UpdateControl {
            token: self.token,
            action,
        })
    }

    fn update_request(
        &mut self,
        owner: u64,
        request: Request<'_>,
        report: &mut impl FnMut(UploadEvent),
    ) -> io::Result<TransferProgress> {
        let mut bytes = [0; REQUEST_BYTES];
        let n = request.encode(&mut bytes);
        let data = Bytes::new(&bytes[..n]).unwrap();
        let end = Instant::now() + Duration::from_secs(30);
        let mut failures = 0;
        loop {
            self.token = self.token.wrapping_add(1);
            let response = self.exchange(Packet::Update {
                token: self.token,
                owner,
                data,
            });
            while let Some(event) = self.inbox.next() {
                report(UploadEvent::Observed(event));
            }
            match response {
                Ok(Packet::Updated { progress, cost, .. }) => {
                    let result = TransferProgress {
                        receiver: progress,
                        path_cost: cost,
                    };
                    report(UploadEvent::Progress(result));
                    match progress.status {
                        Status::Ok => return Ok(result),
                        Status::Busy if Instant::now() < end => {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Status::Busy => {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "receiver deferred staging for 30 seconds; re-offer the same package to resume",
                            ));
                        }
                        status => {
                            return Err(io::Error::other(format!(
                                "receiver rejected update: {status:?} ({:?})",
                                progress.phase
                            )));
                        }
                    }
                }
                Ok(Packet::Error {
                    failure: Failure::Busy | Failure::Transmit | Failure::Timeout,
                    ..
                }) => {
                    failures += 1;
                    if failures >= 4 || Instant::now() >= end {
                        return Err(io::Error::other(
                            "update delivery exhausted bounded retries; re-offer the same package to resume",
                        ));
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                    failures += 1;
                    if failures >= 4 || Instant::now() >= end {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
                Ok(reply) => {
                    return Err(io::Error::other(format!(
                        "unexpected update reply: {reply:?}"
                    )));
                }
            }
        }
    }

    /// Re-offering the same package resumes the receiver's reported prefix.
    /// Ready means verified/staged, not booted, confirmed, or physically healthy.
    pub fn upload(
        &mut self,
        owner: u64,
        package: &Package,
        mut report: impl FnMut(UploadEvent),
    ) -> io::Result<TransferProgress> {
        let id = package.id;
        let mut result = None;
        for offset in (0..HEADER_BYTES).step_by(DATA_BYTES) {
            result = Some(self.update_request(
                owner,
                Request::Header {
                    package: id,
                    offset: offset as u8,
                    data: &package.bytes[offset..offset + DATA_BYTES],
                },
                &mut report,
            )?);
        }
        let mut result = result.unwrap();
        while result.receiver.phase == Phase::Preparing {
            result = self.update_request(owner, Request::Prepare { package: id }, &mut report)?;
        }
        let length = package.image_bytes();
        check_progress(result.receiver, length)?;
        if result.receiver.phase == Phase::Ready {
            return Ok(result);
        }
        if !matches!(result.receiver.phase, Phase::Receiving | Phase::Verifying) {
            return Err(io::Error::other(format!(
                "receiver cannot accept image: {:?}",
                result.receiver.phase
            )));
        }
        let mut offset = result.receiver.accepted;
        while offset < length {
            let end = (offset + DATA_BYTES as u32).min(length);
            result = self.update_request(
                owner,
                Request::Data {
                    package: id,
                    offset,
                    data: &package.bytes
                        [HEADER_BYTES + offset as usize..HEADER_BYTES + end as usize],
                },
                &mut report,
            )?;
            check_progress(result.receiver, length)?;
            if result.receiver.phase != Phase::Receiving || result.receiver.accepted < end {
                return Err(io::Error::other("receiver did not accept the image range"));
            }
            offset = result.receiver.accepted;
        }
        loop {
            result = self.update_request(owner, Request::Finish { package: id }, &mut report)?;
            check_progress(result.receiver, length)?;
            match result.receiver.phase {
                Phase::Ready => return Ok(result),
                Phase::Verifying => {}
                phase => {
                    return Err(io::Error::other(format!(
                        "verification stopped in {phase:?}"
                    )));
                }
            }
        }
    }
}
fn check_progress(p: Progress, length: u32) -> io::Result<()> {
    if p.accepted > length
        || p.committed > p.accepted
        || (p.accepted < length && !p.accepted.is_multiple_of(4))
        || (matches!(p.phase, Phase::Ready | Phase::Verifying)
            && (p.accepted != length || p.committed != length))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid receiver offset",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resume_offsets_and_verified_readiness_are_distinct() {
        for (phase, accepted, committed, valid) in [
            (Phase::Receiving, 48, 0, true),
            (Phase::Receiving, 101, 101, true),
            (Phase::Receiving, 47, 0, false),
            (Phase::Receiving, 104, 0, false),
            (Phase::Receiving, 48, 49, false),
            (Phase::Verifying, 101, 101, true),
            (Phase::Verifying, 100, 100, false),
            (Phase::Ready, 101, 101, true),
            (Phase::Ready, 101, 100, false),
        ] {
            let p = Progress {
                package: [0; 16],
                phase,
                status: Status::Ok,
                accepted,
                committed,
                prepared: 1,
            };
            assert_eq!(check_progress(p, 101).is_ok(), valid, "{p:?}");
        }
    }
    #[test]
    fn package_integrity_and_target_metadata() {
        let mut bytes = vec![0u8; HEADER_BYTES + 13];
        bytes[..6].copy_from_slice(b"LVUP\x01\0");
        bytes[6..8].copy_from_slice(&1u16.to_le_bytes());
        bytes[60..68].copy_from_slice(&3u64.to_le_bytes());
        bytes[68..72].copy_from_slice(&13u32.to_le_bytes());
        let digest = Sha256::digest(&bytes[HEADER_BYTES..]);
        bytes[72..104].copy_from_slice(&digest);
        let valid = Package::parse(bytes.clone()).unwrap();
        assert_eq!(
            (valid.platform, valid.release, valid.image_bytes()),
            (1, 3, 13)
        );
        for at in [0, 4, 68, 72, HEADER_BYTES] {
            let mut corrupt = bytes.clone();
            corrupt[at] ^= 1;
            assert!(Package::parse(corrupt).is_err());
        }
        assert!(Package::parse(bytes[..HEADER_BYTES].to_vec()).is_err());
        bytes[128] ^= 1; // Signature trust remains the receiver's decision.
        assert_ne!(Package::parse(bytes).unwrap().id, valid.id);
    }
}
