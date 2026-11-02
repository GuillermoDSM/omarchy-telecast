use std::{collections::VecDeque, time::Duration};
use tokio::time::Instant;

// At most ~45 KiB of 1400-byte RTP datagrams, and never intentionally
// retransmit data older than 50 ms. This bounds only our userspace queue,
// not the kernel, driver, receiver or end-to-end latency.
const MAX_PACKETS: usize = 32;
const MAX_AGE: Duration = Duration::from_millis(50);

pub struct SendQueue {
    packets: VecDeque<(Instant, Vec<u8>)>,
}

impl SendQueue {
    pub fn new() -> Self {
        Self {
            packets: VecDeque::new(),
        }
    }
    pub fn len(&self) -> usize {
        self.packets.len()
    }
    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }
    pub fn front(&self) -> Option<&[u8]> {
        self.packets.front().map(|(_, p)| p.as_slice())
    }
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        self.packets.pop_front().map(|(_, p)| p)
    }
    pub fn push(&mut self, now: Instant, packet: Vec<u8>) -> bool {
        let dropped = if self.packets.len() == MAX_PACKETS {
            self.packets.pop_front();
            true
        } else {
            false
        };
        self.packets.push_back((now, packet));
        dropped
    }
    pub fn expire(&mut self, now: Instant) -> u64 {
        let mut count = 0;
        while self
            .packets
            .front()
            .is_some_and(|(time, _)| now.saturating_duration_since(*time) >= MAX_AGE)
        {
            self.packets.pop_front();
            count += 1;
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queue_is_short_and_discards_oldest() {
        let now = Instant::now();
        let mut q = SendQueue::new();
        for i in 0..MAX_PACKETS {
            assert!(!q.push(now, vec![i as u8]));
        }
        assert!(q.push(now, vec![99]));
        assert_eq!(q.len(), MAX_PACKETS);
        assert_eq!(q.front(), Some([1].as_slice()));
        assert_eq!(q.expire(now + Duration::from_millis(49)), 0);
        assert_eq!(
            q.expire(now + Duration::from_millis(50)),
            MAX_PACKETS as u64
        );
        assert!(q.is_empty());
    }
    #[test]
    fn recovery_does_not_send_stale_packets() {
        let now = Instant::now();
        let mut q = SendQueue::new();
        q.push(now, vec![1]);
        q.push(now + Duration::from_millis(40), vec![2]);
        assert_eq!(q.expire(now + Duration::from_millis(60)), 1);
        assert_eq!(q.pop(), Some(vec![2]));
        assert!(q.is_empty());
    }

    #[test]
    fn sustained_stall_stays_bounded_then_recovers_in_order() {
        let start = Instant::now();
        let mut q = SendQueue::new();
        for i in 0..1000u32 {
            let now = start + Duration::from_millis(u64::from(i));
            q.expire(now);
            q.push(now, i.to_be_bytes().to_vec());
            assert!(q.len() <= MAX_PACKETS);
        }
        q.expire(start + Duration::from_millis(1010));
        let mut previous = None;
        while let Some(packet) = q.pop() {
            let value = u32::from_be_bytes(packet.try_into().unwrap());
            assert!(value >= 961);
            assert!(previous.is_none_or(|p| value > p));
            previous = Some(value);
        }
        assert_eq!(previous, Some(999));
    }
}
