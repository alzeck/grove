use std::collections::VecDeque;

use bytes::{Bytes, BytesMut};

/// The most recent `cap` bytes of output, so a new viewer can catch up.
pub(crate) struct Replay {
    buf: VecDeque<u8>,
    cap: usize,
}

impl Replay {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        if data.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
            return;
        }
        let overflow = (self.buf.len() + data.len()).saturating_sub(self.cap);
        self.buf.drain(..overflow);
        self.buf.extend(data);
    }

    pub fn snapshot(&self) -> Bytes {
        let (a, b) = self.buf.as_slices();
        let mut out = BytesMut::with_capacity(a.len() + b.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        out.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_tail() {
        let mut r = Replay::new(5);
        r.push(b"abc");
        assert_eq!(&r.snapshot()[..], b"abc");
        r.push(b"def");
        assert_eq!(&r.snapshot()[..], b"bcdef");
        r.push(b"0123456789");
        assert_eq!(&r.snapshot()[..], b"56789");
        r.push(b"x");
        assert_eq!(&r.snapshot()[..], b"6789x");
    }

    #[test]
    fn zero_capacity_keeps_nothing() {
        let mut r = Replay::new(0);
        r.push(b"abc");
        assert!(r.snapshot().is_empty());
    }
}
