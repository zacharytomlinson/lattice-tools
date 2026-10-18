//! Inspect a complete immutable LSF description without evaluating its IR.
//! Records retain opaque remote references. The original bytes remain the source
//! of the full mathematics; this summary exposes types/bindings for a consumer.
use crate::Type;
use std::io;

#[derive(Debug)]
pub struct Attribute {
    pub reference: u64,
    pub ty: Type,
}
#[derive(Debug)]
pub struct Transformer {
    pub reference: u64,
    pub arguments: Vec<Type>,
    pub inputs: Vec<Attribute>,
    pub outputs: Vec<Attribute>,
}
#[derive(Debug)]
pub struct SurfaceSummary {
    pub pointer_bits: u8,
    pub states: Vec<Attribute>,
    pub transformers: Vec<Transformer>,
    pub axioms: u16,
    pub invariants: u16,
}
impl SurfaceSummary {
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        let mut r = Reader { bytes, at: 0 };
        if r.take(4)? != b"LSF\x01" {
            return Err(invalid());
        }
        let pointer_bits = r.byte()?;
        if !matches!(pointer_bits, 32 | 64) {
            return Err(invalid());
        }
        let ns = r.count()?;
        let nt = r.count()?;
        let axioms = r.count()?;
        let invariants = r.count()?;
        let mut states = Vec::new();
        for _ in 0..ns {
            states.push(r.attribute()?);
        }
        let mut transformers = Vec::new();
        for _ in 0..nt {
            let reference = r.reference()?;
            r.predicate()?;
            let arguments = r.types()?;
            let inputs = r.attributes()?;
            let outputs = r.attributes()?;
            let results = r.count()?;
            if results as usize != outputs.len() {
                return Err(invalid());
            }
            for _ in 0..results {
                r.expression(0)?;
            }
            transformers.push(Transformer {
                reference,
                arguments,
                inputs,
                outputs,
            });
        }
        for _ in 0..u32::from(axioms) + u32::from(invariants) {
            r.reference()?;
            r.predicate()?;
        }
        if r.at != bytes.len() {
            return Err(invalid());
        }
        Ok(Self {
            pointer_bits,
            states,
            transformers,
            axioms,
            invariants,
        })
    }
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid or incomplete LSF surface",
    )
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or_else(invalid)?;
        let bytes = self.bytes.get(self.at..end).ok_or_else(invalid)?;
        self.at = end;
        Ok(bytes)
    }
    fn byte(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn count(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn reference(&mut self) -> io::Result<u64> {
        let r = u64::from_le_bytes(self.take(8)?.try_into().unwrap());
        if r == 0 || r == u64::MAX {
            return Err(invalid());
        }
        Ok(r)
    }
    fn ty(&mut self) -> io::Result<Type> {
        lattice_peer_wire::scalar::decode_type(self.byte()?).ok_or_else(invalid)
    }
    fn types(&mut self) -> io::Result<Vec<Type>> {
        let n = self.count()?;
        (0..n).map(|_| self.ty()).collect()
    }
    fn attribute(&mut self) -> io::Result<Attribute> {
        Ok(Attribute {
            reference: self.reference()?,
            ty: self.ty()?,
        })
    }
    fn attributes(&mut self) -> io::Result<Vec<Attribute>> {
        let n = self.count()?;
        (0..n).map(|_| self.attribute()).collect()
    }
    fn definition(&mut self, depth: u8) -> io::Result<()> {
        self.reference()?;
        self.types()?;
        self.types()?;
        self.expression(depth)
    }
    fn predicate(&mut self) -> io::Result<()> {
        self.definition(0)?;
        self.attributes()?;
        Ok(())
    }
    fn expression(&mut self, depth: u8) -> io::Result<()> {
        if depth >= 32 {
            return Err(invalid());
        }
        match self.byte()? {
            0 => {
                let (_, n) = lattice_peer_wire::scalar::decode_value(&self.bytes[self.at..])
                    .ok_or_else(invalid)?;
                self.take(n)?;
            }
            1 => {
                self.attribute()?;
            }
            2 | 3 => {
                self.count()?;
            }
            4 => self.expression(depth + 1)?,
            5 | 6 => {
                self.expression(depth + 1)?;
                self.expression(depth + 1)?;
            }
            7 => {
                match self.byte()? {
                    0 | 1 => {}
                    2..=5 => {
                        if !matches!(self.ty()?, Type::Integer(_)) {
                            return Err(invalid());
                        }
                    }
                    _ => return Err(invalid()),
                }
                self.expression(depth + 1)?;
            }
            8 => {
                if self.byte()? > 16 {
                    return Err(invalid());
                }
                self.expression(depth + 1)?;
                self.expression(depth + 1)?;
            }
            9 => {
                for _ in 0..3 {
                    self.expression(depth + 1)?;
                }
            }
            10 => self.definition(depth + 1)?,
            _ => return Err(invalid()),
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_partial_or_trailing_records_without_reading_or_executing_state() {
        let mut bytes = b"LSF\x01\x20\x01\x00\x00\x00\x00\x00\x00\x00".to_vec();
        bytes.extend_from_slice(&42u64.to_le_bytes());
        bytes.push(0);
        let surface = SurfaceSummary::parse(&bytes).unwrap();
        assert_eq!(surface.states[0].reference, 42);
        assert_eq!(surface.states[0].ty, Type::Bool);
        for end in 0..bytes.len() {
            assert!(SurfaceSummary::parse(&bytes[..end]).is_err());
        }
        bytes.push(0);
        assert!(SurfaceSummary::parse(&bytes).is_err());
    }
}
