//! Compact bitset using BitTorrent bit order (piece 0 = most significant bit of byte 0).

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitfield {
    bytes: Vec<u8>,
    len: usize,
    ones: usize,
}

impl Bitfield {
    pub fn new(len: usize) -> Self {
        Bitfield {
            bytes: vec![0; len.div_ceil(8)],
            len,
            ones: 0,
        }
    }

    /// Parse a wire bitfield. Returns `None` if the byte length is wrong or any
    /// spare (padding) bit is set, as required by BEP 3.
    pub fn from_bytes(bytes: &[u8], len: usize) -> Option<Self> {
        if bytes.len() != len.div_ceil(8) {
            return None;
        }
        let spare = bytes.len() * 8 - len;
        if spare > 0 {
            let mask = (1u8 << spare) - 1;
            if bytes[bytes.len() - 1] & mask != 0 {
                return None;
            }
        }
        let ones = bytes.iter().map(|b| b.count_ones() as usize).sum();
        Some(Bitfield {
            bytes: bytes.to_vec(),
            len,
            ones,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, i: usize) -> bool {
        i < self.len && self.bytes[i / 8] & (0x80 >> (i % 8)) != 0
    }

    /// Set bit `i`; out-of-range indexes are ignored.
    pub fn set(&mut self, i: usize) {
        if i < self.len && !self.get(i) {
            self.bytes[i / 8] |= 0x80 >> (i % 8);
            self.ones += 1;
        }
    }

    pub fn count_ones(&self) -> usize {
        self.ones
    }

    pub fn is_full(&self) -> bool {
        self.ones == self.len
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Iterate over indexes of set bits.
    pub fn iter_ones(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.len).filter(move |&i| self.get(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_use_msb_first() {
        let mut b = Bitfield::new(10);
        b.set(0);
        b.set(9);
        assert!(b.get(0) && b.get(9) && !b.get(1));
        assert_eq!(b.as_bytes(), &[0b1000_0000, 0b0100_0000]);
        assert_eq!(b.count_ones(), 2);
    }

    #[test]
    fn set_is_idempotent_and_bounds_checked() {
        let mut b = Bitfield::new(3);
        b.set(1);
        b.set(1);
        b.set(100);
        assert_eq!(b.count_ones(), 1);
        assert!(!b.get(100));
    }

    #[test]
    fn full_detection() {
        let mut b = Bitfield::new(3);
        assert!(!b.is_full());
        (0..3).for_each(|i| b.set(i));
        assert!(b.is_full());
        assert!(Bitfield::new(0).is_full());
    }

    #[test]
    fn from_bytes_validates_length() {
        assert!(Bitfield::from_bytes(&[0xff], 16).is_none());
        assert!(Bitfield::from_bytes(&[0xff, 0xff, 0xff], 16).is_none());
        assert!(Bitfield::from_bytes(&[0xff, 0xff], 16).is_some());
    }

    #[test]
    fn from_bytes_rejects_spare_bits() {
        // 10 bits -> 6 spare bits in 2nd byte must be zero
        assert!(Bitfield::from_bytes(&[0xff, 0b1100_0000], 10).is_some());
        assert!(Bitfield::from_bytes(&[0xff, 0b1100_0001], 10).is_none());
    }

    #[test]
    fn from_bytes_counts_ones() {
        let b = Bitfield::from_bytes(&[0b1010_0000], 4).unwrap();
        assert_eq!(b.count_ones(), 2);
        assert_eq!(b.iter_ones().collect::<Vec<_>>(), vec![0, 2]);
    }
}
