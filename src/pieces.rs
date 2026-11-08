//! Piece/block bookkeeping: which pieces we have, which are being assembled, and which
//! block to request next from a given peer.
//!
//! Memory policy: at most `max_active` pieces are buffered at once (the *memory budget*
//! divided by the piece length). Finished buffers are recycled through a small pool so a
//! long download does not churn the allocator.
//!
//! Selection policy: continue pieces already in progress first (finish what we started),
//! then start the *rarest* missing piece the peer has, breaking ties by lowest index so
//! disk writes stay mostly sequential. Since we never upload, swarm-health arguments for
//! randomisation do not apply. When every missing piece is already in progress we enter
//! *endgame* and duplicate outstanding blocks across peers so one slow peer cannot hold
//! up the last pieces.

use std::collections::BTreeMap;

use crate::bitfield::Bitfield;
use crate::wire::BLOCK_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRequest {
    pub index: u32,
    pub begin: u32,
    pub length: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockOutcome {
    /// Unsolicited, duplicate or malformed block; nothing stored.
    Ignored,
    Stored,
    /// All blocks of the piece arrived; verify with [`PieceTable::piece_data`] and then
    /// call [`PieceTable::finish_piece`].
    PieceComplete,
}

struct Active {
    buf: Vec<u8>,
    len: u32,
    received: Vec<bool>,
    requested: Vec<bool>,
    received_count: u32,
    /// Lowest block index that might still be unrequested (scan start).
    cursor: u32,
    complete: bool,
}

impl Active {
    fn blocks(&self) -> u32 {
        self.received.len() as u32
    }

    fn block_len(&self, b: u32) -> u32 {
        (self.len - b * BLOCK_SIZE).min(BLOCK_SIZE)
    }

    fn claim_unrequested(&mut self) -> Option<u32> {
        let from = self.cursor as usize;
        let found = (from..self.received.len())
            .find(|&b| !self.received[b] && !self.requested[b])?;
        self.requested[found] = true;
        self.cursor = found as u32 + 1;
        Some(found as u32)
    }
}

pub struct PieceTable {
    num_pieces: u32,
    piece_length: u32,
    total_length: u64,
    have: Bitfield,
    availability: Vec<u16>,
    active: BTreeMap<u32, Active>,
    max_active: usize,
    pool: Vec<Vec<u8>>,
    endgame_rotor: usize,
}

impl PieceTable {
    pub fn new(num_pieces: u32, piece_length: u32, total_length: u64, max_active: usize) -> Self {
        PieceTable {
            num_pieces,
            piece_length,
            total_length,
            have: Bitfield::new(num_pieces as usize),
            availability: vec![0; num_pieces as usize],
            active: BTreeMap::new(),
            max_active: max_active.max(1),
            pool: Vec::new(),
            endgame_rotor: 0,
        }
    }

    /// Number of pieces that fit in `memory_budget` bytes (always at least 1).
    pub fn max_active_for(memory_budget: usize, piece_length: u32) -> usize {
        (memory_budget / piece_length.max(1) as usize).max(1)
    }

    pub fn num_pieces(&self) -> u32 {
        self.num_pieces
    }

    pub fn piece_len(&self, index: u32) -> u32 {
        let start = index as u64 * self.piece_length as u64;
        (self.total_length - start).min(self.piece_length as u64) as u32
    }

    pub fn have(&self) -> &Bitfield {
        &self.have
    }

    pub fn is_complete(&self) -> bool {
        self.have.is_full()
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Record a piece found on disk during resume.
    pub fn mark_have(&mut self, index: u32) {
        self.have.set(index as usize);
    }

    pub fn peer_has_bitfield(&mut self, bf: &Bitfield) {
        for i in bf.iter_ones() {
            self.availability[i] = self.availability[i].saturating_add(1);
        }
    }

    pub fn peer_lost(&mut self, bf: &Bitfield) {
        for i in bf.iter_ones() {
            self.availability[i] = self.availability[i].saturating_sub(1);
        }
    }

    pub fn peer_have(&mut self, index: u32) {
        if let Some(a) = self.availability.get_mut(index as usize) {
            *a = a.saturating_add(1);
        }
    }

    /// Bytes verified plus bytes buffered for in-progress pieces.
    pub fn downloaded_bytes(&self) -> u64 {
        let mut total = 0u64;
        let n = self.have.count_ones() as u64;
        if n > 0 {
            total = n * self.piece_length as u64;
            if self.have.get(self.num_pieces as usize - 1) {
                total = total - self.piece_length as u64 + self.piece_len(self.num_pieces - 1) as u64;
            }
        }
        for a in self.active.values() {
            for (b, &r) in a.received.iter().enumerate() {
                if r {
                    total += a.block_len(b as u32) as u64;
                }
            }
        }
        total
    }

    /// Choose the next block for a peer with the given bitfield and mark it requested.
    pub fn next_request(&mut self, peer: &Bitfield) -> Option<BlockRequest> {
        // 1. Finish pieces that are already in progress.
        for (&index, a) in self.active.iter_mut() {
            if a.complete || !peer.get(index as usize) {
                continue;
            }
            if let Some(b) = a.claim_unrequested() {
                return Some(BlockRequest {
                    index,
                    begin: b * BLOCK_SIZE,
                    length: a.block_len(b),
                });
            }
        }

        // 2. Start a new piece (rarest first) if the memory budget allows.
        if self.active.len() < self.max_active {
            if let Some(index) = self.pick_new(peer) {
                let len = self.piece_len(index);
                let blocks = len.div_ceil(BLOCK_SIZE) as usize;
                let mut buf = self.pool.pop().unwrap_or_default();
                buf.resize(len as usize, 0);
                let mut a = Active {
                    buf,
                    len,
                    received: vec![false; blocks],
                    requested: vec![false; blocks],
                    received_count: 0,
                    cursor: 0,
                    complete: false,
                };
                let b = a.claim_unrequested().expect("new piece has blocks");
                let req = BlockRequest {
                    index,
                    begin: b * BLOCK_SIZE,
                    length: a.block_len(b),
                };
                self.active.insert(index, a);
                return Some(req);
            }
        }

        // 3. Endgame: every missing piece is in progress -> duplicate outstanding blocks.
        if self.have.count_ones() + self.active.len() == self.num_pieces as usize {
            let mut candidates: Vec<(u32, u32)> = Vec::new();
            for (&index, a) in self.active.iter() {
                if a.complete || !peer.get(index as usize) {
                    continue;
                }
                if let Some(b) = a.received.iter().position(|&r| !r) {
                    candidates.push((index, b as u32));
                }
            }
            if !candidates.is_empty() {
                self.endgame_rotor = self.endgame_rotor.wrapping_add(1);
                let (index, b) = candidates[self.endgame_rotor % candidates.len()];
                let a = &self.active[&index];
                return Some(BlockRequest {
                    index,
                    begin: b * BLOCK_SIZE,
                    length: a.block_len(b),
                });
            }
        }
        None
    }

    fn pick_new(&self, peer: &Bitfield) -> Option<u32> {
        let mut best: Option<u32> = None;
        let mut best_av = u16::MAX;
        for i in 0..self.num_pieces {
            let idx = i as usize;
            if self.have.get(idx) || !peer.get(idx) || self.active.contains_key(&i) {
                continue;
            }
            let av = self.availability[idx];
            if best.is_none() || av < best_av {
                best = Some(i);
                best_av = av;
                if av <= 1 {
                    break; // cannot do better than "only this peer has it"
                }
            }
        }
        best
    }

    /// Give back a request that will never be answered (peer choked/disconnected).
    pub fn release(&mut self, req: &BlockRequest) {
        if let Some(a) = self.active.get_mut(&req.index) {
            let b = req.begin / BLOCK_SIZE;
            if let Some(slot) = a.requested.get_mut(b as usize) {
                if !a.received[b as usize] {
                    *slot = false;
                    a.cursor = a.cursor.min(b);
                }
            }
        }
    }

    /// Store a received block.
    pub fn on_block(&mut self, index: u32, begin: u32, data: &[u8]) -> BlockOutcome {
        let Some(a) = self.active.get_mut(&index) else {
            return BlockOutcome::Ignored;
        };
        if a.complete || !begin.is_multiple_of(BLOCK_SIZE) {
            return BlockOutcome::Ignored;
        }
        let b = begin / BLOCK_SIZE;
        if b >= a.blocks() || data.len() != a.block_len(b) as usize || a.received[b as usize] {
            return BlockOutcome::Ignored;
        }
        let start = begin as usize;
        a.buf[start..start + data.len()].copy_from_slice(data);
        a.received[b as usize] = true;
        a.requested[b as usize] = true;
        a.received_count += 1;
        if a.received_count == a.blocks() {
            a.complete = true;
            BlockOutcome::PieceComplete
        } else {
            BlockOutcome::Stored
        }
    }

    /// Data of a completed (not yet finished) piece, for hashing and writing.
    pub fn piece_data(&self, index: u32) -> Option<&[u8]> {
        self.active
            .get(&index)
            .filter(|a| a.complete)
            .map(|a| a.buf.as_slice())
    }

    /// Conclude a completed piece: on success it becomes "have"; on failure it is dropped
    /// and will be downloaded again.
    pub fn finish_piece(&mut self, index: u32, verified: bool) {
        if let Some(a) = self.active.remove(&index) {
            if verified {
                self.have.set(index as usize);
            }
            if self.pool.len() < self.max_active {
                self.pool.push(a.buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BS: u32 = BLOCK_SIZE;

    fn full(n: usize) -> Bitfield {
        let mut b = Bitfield::new(n);
        (0..n).for_each(|i| b.set(i));
        b
    }

    fn table(pieces: u32, piece_len: u32, total: u64, max_active: usize) -> PieceTable {
        PieceTable::new(pieces, piece_len, total, max_active)
    }

    #[test]
    fn requests_blocks_in_order_within_a_piece() {
        let mut t = table(2, BS * 3, (BS * 5) as u64, 2);
        let peer = full(2);
        let reqs: Vec<_> = (0..3).map(|_| t.next_request(&peer).unwrap()).collect();
        assert_eq!(
            reqs,
            vec![
                BlockRequest { index: 0, begin: 0, length: BS },
                BlockRequest { index: 0, begin: BS, length: BS },
                BlockRequest { index: 0, begin: 2 * BS, length: BS },
            ]
        );
        // piece 0 fully requested -> starts piece 1 (2 blocks, as it is the last piece)
        let r = t.next_request(&peer).unwrap();
        assert_eq!(r, BlockRequest { index: 1, begin: 0, length: BS });
        let r = t.next_request(&peer).unwrap();
        assert_eq!(r, BlockRequest { index: 1, begin: BS, length: BS });
    }

    #[test]
    fn last_block_may_be_short() {
        let total = (BS + 100) as u64;
        let mut t = table(1, BS * 2, total, 1);
        let peer = full(1);
        assert_eq!(t.next_request(&peer).unwrap().length, BS);
        assert_eq!(t.next_request(&peer).unwrap().length, 100);
    }

    #[test]
    fn honours_peer_bitfield() {
        let mut t = table(3, BS, (3 * BS) as u64, 3);
        let mut peer = Bitfield::new(3);
        peer.set(2);
        assert_eq!(t.next_request(&peer).unwrap().index, 2);
        assert!(t.next_request(&peer).is_none());
        assert!(t.next_request(&Bitfield::new(3)).is_none());
    }

    #[test]
    fn prefers_rarest_piece_then_lowest_index() {
        let mut t = table(4, BS, (4 * BS) as u64, 1);
        let peer = full(4);
        // availability: piece0=3, piece1=2, piece2=1 (rarest), piece3=2
        for _ in 0..3 {
            let mut b = Bitfield::new(4);
            b.set(0);
            t.peer_has_bitfield(&b);
        }
        for i in [1usize, 3] {
            for _ in 0..2 {
                let mut b = Bitfield::new(4);
                b.set(i);
                t.peer_has_bitfield(&b);
            }
        }
        let mut b = Bitfield::new(4);
        b.set(2);
        t.peer_has_bitfield(&b);
        assert_eq!(t.next_request(&peer).unwrap().index, 2);
    }

    #[test]
    fn equal_availability_picks_lowest_index() {
        let mut t = table(5, BS, (5 * BS) as u64, 1);
        let peer = full(5);
        assert_eq!(t.next_request(&peer).unwrap().index, 0);
    }

    #[test]
    fn respects_memory_budget() {
        assert_eq!(PieceTable::max_active_for(16 << 20, 1 << 20), 16);
        assert_eq!(PieceTable::max_active_for(1 << 20, 4 << 20), 1);
        let mut t = table(10, BS, (10 * BS) as u64, 2);
        let peer = full(10);
        assert_eq!(t.next_request(&peer).unwrap().index, 0);
        assert_eq!(t.next_request(&peer).unwrap().index, 1);
        // budget exhausted and not in endgame -> nothing to do
        assert!(t.next_request(&peer).is_none());
        assert_eq!(t.active_count(), 2);
    }

    #[test]
    fn stores_blocks_and_completes_piece() {
        let mut t = table(1, BS * 2, (BS * 2) as u64, 1);
        let peer = full(1);
        let r0 = t.next_request(&peer).unwrap();
        let r1 = t.next_request(&peer).unwrap();
        assert_eq!(t.on_block(0, r1.begin, &vec![2u8; BS as usize]), BlockOutcome::Stored);
        assert_eq!(
            t.on_block(0, r0.begin, &vec![1u8; BS as usize]),
            BlockOutcome::PieceComplete
        );
        let data = t.piece_data(0).unwrap();
        assert!(data[..BS as usize].iter().all(|&b| b == 1));
        assert!(data[BS as usize..].iter().all(|&b| b == 2));
        t.finish_piece(0, true);
        assert!(t.is_complete());
        assert!(t.piece_data(0).is_none());
    }

    #[test]
    fn ignores_bad_blocks() {
        let mut t = table(1, BS * 2, (BS * 2) as u64, 1);
        let peer = full(1);
        let r0 = t.next_request(&peer).unwrap();
        let ok = vec![0u8; BS as usize];
        assert_eq!(t.on_block(9, 0, &ok), BlockOutcome::Ignored); // unknown piece
        assert_eq!(t.on_block(0, 5, &ok), BlockOutcome::Ignored); // misaligned
        assert_eq!(t.on_block(0, 2 * BS, &ok), BlockOutcome::Ignored); // out of range
        assert_eq!(t.on_block(0, 0, &ok[..10]), BlockOutcome::Ignored); // wrong length
        assert_eq!(t.on_block(0, r0.begin, &ok), BlockOutcome::Stored);
        assert_eq!(t.on_block(0, r0.begin, &ok), BlockOutcome::Ignored); // duplicate
    }

    #[test]
    fn failed_piece_is_requested_again() {
        let mut t = table(1, BS, BS as u64, 1);
        let peer = full(1);
        let r = t.next_request(&peer).unwrap();
        assert_eq!(t.on_block(0, r.begin, &vec![0u8; BS as usize]), BlockOutcome::PieceComplete);
        // completed piece is not re-requested while awaiting verification
        assert!(t.next_request(&peer).is_none());
        t.finish_piece(0, false);
        assert!(!t.is_complete());
        let r2 = t.next_request(&peer).unwrap();
        assert_eq!(r2, r);
    }

    #[test]
    fn release_makes_block_requestable_again() {
        let mut t = table(1, BS * 2, (BS * 2) as u64, 1);
        let peer = full(1);
        let r0 = t.next_request(&peer).unwrap();
        let _r1 = t.next_request(&peer).unwrap();
        t.release(&r0);
        assert_eq!(t.next_request(&peer).unwrap(), r0);
    }

    #[test]
    fn release_does_not_unrequest_received_blocks() {
        let mut t = table(1, BS * 2, (BS * 2) as u64, 1);
        let peer = full(1);
        let r0 = t.next_request(&peer).unwrap();
        t.on_block(0, r0.begin, &vec![0u8; BS as usize]);
        t.release(&r0);
        // next request must be block 1, not block 0
        assert_eq!(t.next_request(&peer).unwrap().begin, BS);
    }

    #[test]
    fn endgame_duplicates_outstanding_blocks() {
        let mut t = table(1, BS * 2, (BS * 2) as u64, 1);
        let peer = full(1);
        let a = t.next_request(&peer).unwrap();
        let b = t.next_request(&peer).unwrap();
        assert_ne!(a, b);
        // everything requested, piece active, nothing left to start -> endgame
        let dup = t.next_request(&peer).unwrap();
        assert!(dup == a || dup == b);
        // after block 0 arrives, only block 1 can still be duplicated
        t.on_block(0, 0, &vec![0u8; BS as usize]);
        for _ in 0..4 {
            assert_eq!(t.next_request(&peer).unwrap().begin, BS);
        }
    }

    #[test]
    fn no_endgame_while_unstarted_pieces_exist() {
        let mut t = table(3, BS, (3 * BS) as u64, 1);
        let peer = full(3);
        assert_eq!(t.next_request(&peer).unwrap().index, 0);
        // piece 0 outstanding, pieces 1/2 unstarted but budget exhausted: no duplicates
        assert!(t.next_request(&peer).is_none());
    }

    #[test]
    fn mark_have_skips_pieces() {
        let mut t = table(3, BS, (3 * BS) as u64, 3);
        t.mark_have(0);
        t.mark_have(1);
        let peer = full(3);
        assert_eq!(t.next_request(&peer).unwrap().index, 2);
        assert!(!t.is_complete());
    }

    #[test]
    fn availability_tracking_never_underflows() {
        let mut t = table(2, BS, (2 * BS) as u64, 2);
        let bf = full(2);
        t.peer_lost(&bf); // nothing registered yet
        t.peer_has_bitfield(&bf);
        t.peer_have(1);
        t.peer_have(99); // out of range is ignored
        t.peer_lost(&bf);
        t.peer_lost(&bf);
    }

    #[test]
    fn downloaded_bytes_counts_pieces_and_partial_blocks() {
        let total = (BS * 2 + 100) as u64; // piece_len 2*BS -> 2 pieces: 2*BS and 100
        let mut t = table(2, BS * 2, total, 2);
        assert_eq!(t.downloaded_bytes(), 0);
        let peer = full(2);
        let r = t.next_request(&peer).unwrap();
        t.on_block(r.index, r.begin, &vec![0u8; BS as usize]);
        assert_eq!(t.downloaded_bytes(), BS as u64);
        t.mark_have(1);
        assert_eq!(t.downloaded_bytes(), BS as u64 + 100);
        t.mark_have(0);
        assert_eq!(t.downloaded_bytes(), total + BS as u64); // active block still counted
    }

    #[test]
    fn buffers_are_recycled() {
        let mut t = table(3, BS, (3 * BS) as u64, 1);
        let peer = full(3);
        for i in 0..3u32 {
            let r = t.next_request(&peer).unwrap();
            assert_eq!(r.index, i);
            assert_eq!(
                t.on_block(i, 0, &vec![i as u8; BS as usize]),
                BlockOutcome::PieceComplete
            );
            assert!(t.piece_data(i).unwrap().iter().all(|&b| b == i as u8));
            t.finish_piece(i, true);
        }
        assert!(t.is_complete());
    }

    #[test]
    fn buffered_memory_never_exceeds_the_budget() {
        // Invariant: live buffers (active + pooled) never exceed max_active, because a new
        // buffer is only allocated when the pool is empty and fewer than max_active are active.
        let max_active = 4;
        let mut t = table(40, BS, (40 * BS) as u64, max_active);
        let peer = full(40);
        let mut in_flight: Vec<BlockRequest> = Vec::new();
        let mut done = 0;
        while done < 40 {
            while let Some(r) = t.next_request(&peer) {
                if in_flight.contains(&r) {
                    break;
                }
                in_flight.push(r);
            }
            assert!(t.active_count() + t.pool.len() <= max_active, "over budget before delivery");
            let r = in_flight.remove(0);
            if t.on_block(r.index, r.begin, &vec![0u8; BS as usize]) == BlockOutcome::PieceComplete {
                t.finish_piece(r.index, true);
                done += 1;
            }
            assert!(t.active_count() + t.pool.len() <= max_active, "over budget after finish");
        }
        assert!(t.is_complete());
    }

    #[test]
    fn recycled_buffer_resizes_for_shorter_last_piece() {
        // piece 0 is BS*2, last piece is BS
        let mut t = table(2, BS * 2, (BS * 3) as u64, 1);
        let peer = full(2);
        for _ in 0..2 {
            let r = t.next_request(&peer).unwrap();
            t.on_block(0, r.begin, &vec![1u8; BS as usize]);
        }
        t.finish_piece(0, true);
        let r = t.next_request(&peer).unwrap();
        assert_eq!(r.index, 1);
        assert_eq!(t.on_block(1, 0, &vec![9u8; BS as usize]), BlockOutcome::PieceComplete);
        assert_eq!(t.piece_data(1).unwrap().len(), BS as usize);
    }
}
