//! Bounded recovery metadata included atomically in a v2 manifest.
//! Nonzero floors restrict recovery visibility; immutable SSTs retain full history.

use super::tablet::{KeyRange, TabletDescriptor, TabletId, TransactionId, TransactionReceipt};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Cursor, Read};

const MAX_RANGE_BYTES: usize = 1024 * 1024;
const MAX_RECEIPTS: usize = 65_536;
pub(super) const MAX_META_BYTES: usize =
    8 + 4 + MAX_RANGE_BYTES + 1 + 4 + MAX_RANGE_BYTES + 8 + 56 * MAX_RECEIPTS;
fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecoveryMeta {
    pub(super) descriptor: TabletDescriptor,
    pub(super) floor: u64,
    pub(super) receipts: BTreeMap<TransactionId, TransactionReceipt>,
}

impl RecoveryMeta {
    pub(super) fn new(
        descriptor: TabletDescriptor,
        floor: u64,
        sequence: u64,
        receipts: impl ExactSizeIterator<Item = TransactionReceipt>,
    ) -> io::Result<Self> {
        if receipts.len() > MAX_RECEIPTS {
            return Err(invalid("recovery receipt limit exceeded"));
        }
        let mut result = Self {
            descriptor,
            floor,
            receipts: BTreeMap::new(),
        };
        for receipt in receipts {
            if result.receipts.insert(receipt.id(), receipt).is_some() {
                return Err(invalid("duplicate recovery receipt ID"));
            }
        }
        result.validate(sequence)?;
        Ok(result)
    }
    pub(super) fn validate(&self, sequence: u64) -> io::Result<()> {
        if self.floor > sequence
            || self.descriptor.range().start().len() > MAX_RANGE_BYTES
            || self
                .descriptor
                .range()
                .end()
                .is_some_and(|end| end.len() > MAX_RANGE_BYTES)
            || self.receipts.len() > MAX_RECEIPTS
        {
            return Err(invalid("invalid recovery floor or metadata limit"));
        }
        let mut seen = BTreeSet::new();
        for receipt in self.receipts.values() {
            if receipt.sequence() == 0
                || receipt.sequence() > sequence
                || !seen.insert(receipt.sequence())
            {
                return Err(invalid("invalid recovery receipt sequence"));
            }
        }
        Ok(())
    }
    pub(super) fn encode(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&self.floor.to_le_bytes());
        let range = self.descriptor.range();
        bytes.extend_from_slice(&(range.start().len() as u32).to_le_bytes());
        bytes.extend_from_slice(range.start());
        match range.end() {
            None => bytes.push(0),
            Some(end) => {
                bytes.push(1);
                bytes.extend_from_slice(&(end.len() as u32).to_le_bytes());
                bytes.extend_from_slice(end);
            }
        }
        bytes.extend_from_slice(&(self.receipts.len() as u64).to_le_bytes());
        for receipt in self.receipts.values() {
            bytes.extend_from_slice(&receipt.id().bytes());
            bytes.extend_from_slice(&receipt.sequence().to_le_bytes());
            bytes.extend_from_slice(&receipt.digest());
        }
    }
    pub(super) fn decode(bytes: &[u8], tablet: u64, epoch: u64, sequence: u64) -> io::Result<Self> {
        fn fixed<const N: usize>(reader: &mut Cursor<&[u8]>) -> io::Result<[u8; N]> {
            let mut b = [0; N];
            reader.read_exact(&mut b)?;
            Ok(b)
        }
        fn key(reader: &mut Cursor<&[u8]>) -> io::Result<Vec<u8>> {
            let len = u32::from_le_bytes(fixed(reader)?) as usize;
            if len > MAX_RANGE_BYTES
                || len as u64 > reader.get_ref().len() as u64 - reader.position()
            {
                return Err(invalid("invalid recovery range length"));
            }
            let mut result = vec![0; len];
            reader.read_exact(&mut result)?;
            Ok(result)
        }
        let mut reader = Cursor::new(bytes);
        let floor = u64::from_le_bytes(fixed(&mut reader)?);
        let start = key(&mut reader)?;
        let end = match fixed::<1>(&mut reader)?[0] {
            0 => None,
            1 => Some(key(&mut reader)?),
            _ => return Err(invalid("invalid recovery range end tag")),
        };
        let range = KeyRange::new(start, end).map_err(|_| invalid("invalid recovery key range"))?;
        let descriptor = TabletDescriptor::new(
            TabletId::new(tablet).map_err(|_| invalid("invalid recovery tablet ID"))?,
            range,
            epoch,
        )
        .map_err(|_| invalid("invalid recovery descriptor"))?;
        let count = u64::from_le_bytes(fixed(&mut reader)?);
        if count > MAX_RECEIPTS as u64 || count * 56 != bytes.len() as u64 - reader.position() {
            return Err(invalid("invalid recovery receipt count or length"));
        }
        let mut receipts = Vec::with_capacity(count as usize);
        for _ in 0..count {
            receipts.push(TransactionReceipt::from_parts(
                TransactionId::new(fixed(&mut reader)?),
                u64::from_le_bytes(fixed(&mut reader)?),
                fixed(&mut reader)?,
            ));
        }
        Self::new(descriptor, floor, sequence, receipts.into_iter())
    }
}
