//! A stream leg: one connection between a host and a listening address,
//! ordered and lossless as TCP makes it, for a relay's client whose network
//! carries no datagrams (docs/03-connectivity.md 7.4).
//!
//! **What the transport hides is not modelled; what it cannot hide is.** The
//! link's loss, duplication, reordering and policer apply to datagrams alone:
//! a stream delivers every write, once, in the order written, after the
//! link's one-way delay and its jitter. What a stream cannot hide is a sender
//! that stops -- a relay's side was measured stopping for half a second --
//! and that sender's queue behind the stop, bounded and dropping past its
//! depth. A test scripts the stop; a sender asks for the bound.
//!
//! Writes arrive whole. Finding messages across arbitrary splits is the
//! receivers' work and is tested where the receivers are. The connect's
//! handshake is not modelled: the first write leaves at once.

use std::net::SocketAddr;

use crate::{HostId, Sim, us_from_ms};

/// Handle to a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamId(usize);

/// What a stream delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// One write, whole.
    Bytes(Vec<u8>),
    /// The other end closed the stream; nothing follows.
    End,
}

/// One delivery on a stream.
#[derive(Debug, Clone)]
pub struct StreamArrival {
    pub stream: StreamId,
    /// Which end received it.
    pub host: HostId,
    /// The other end as this one sees it: the address the connecting end
    /// reached, or the address it came from, translated.
    pub from: SocketAddr,
    pub data: Segment,
}

/// One direction of a connection: when its last delivery is due, so that
/// nothing written later passes it, and until when it is stopped.
#[derive(Debug, Default)]
struct Direction {
    last_us: u64,
    stopped_until_us: u64,
}

/// One connection, both ways.
#[derive(Debug)]
pub(crate) struct Leg {
    client: HostId,
    client_addr: SocketAddr,
    listener: HostId,
    server: SocketAddr,
    toward_listener: Direction,
    toward_client: Direction,
    open: bool,
}

impl Leg {
    fn direction(&self, toward_listener: bool) -> &Direction {
        if toward_listener {
            &self.toward_listener
        } else {
            &self.toward_client
        }
    }

    fn direction_mut(&mut self, toward_listener: bool) -> &mut Direction {
        if toward_listener {
            &mut self.toward_listener
        } else {
            &mut self.toward_client
        }
    }
}

#[derive(Debug)]
pub(crate) struct Delivery {
    pub(crate) at_us: u64,
    seq: u64,
    stream: StreamId,
    toward_listener: bool,
    data: Segment,
}

impl Sim {
    /// Open a stream from `from` to whatever listens at `to`, reached as a
    /// datagram would reach it. `None` when nothing is there: the connect is
    /// refused.
    pub fn connect(&mut self, from: HostId, to: SocketAddr) -> Option<StreamId> {
        let (listener, client_addr) = match self.on_network(from, to) {
            Some(found) => found,
            None => {
                let source = self.translate_out(from, to);
                self.resolve(source, to)?
            }
        };
        self.legs.push(Leg {
            client: from,
            client_addr,
            listener,
            server: to,
            toward_listener: Direction::default(),
            toward_client: Direction::default(),
            open: true,
        });
        Some(StreamId(self.legs.len() - 1))
    }

    /// Write `bytes` on `stream` from its end `from`.
    pub fn stream_send(&mut self, stream: StreamId, from: HostId, bytes: &[u8]) {
        self.stream_send_within(stream, from, bytes, usize::MAX);
    }

    /// The same, from a sender that holds at most `depth` writes behind a
    /// stopped stream and drops past it, as a relay behind a stalled client
    /// does. **What is in flight is not a queue**: a stream that moves takes
    /// every write, a burst included. Whether the write was taken.
    pub fn stream_send_within(
        &mut self,
        stream: StreamId,
        from: HostId,
        bytes: &[u8],
        depth: usize,
    ) -> bool {
        let Some(toward_listener) = self.direction_from(stream, from) else {
            return false;
        };
        let stopped = self
            .legs
            .get(stream.0)
            .is_some_and(|leg| self.now_us < leg.direction(toward_listener).stopped_until_us);
        if stopped {
            let held = self
                .stream_queue
                .iter()
                .filter(|d| d.stream == stream && d.toward_listener == toward_listener)
                .count();
            if held >= depth {
                return false;
            }
        }
        self.put(stream, toward_listener, Segment::Bytes(bytes.to_vec()));
        true
    }

    /// Close `stream` from its end `by`: the other end is told after what was
    /// sent before, nothing still on its way to `by` reaches it, and nothing
    /// more is carried either way.
    pub fn close(&mut self, stream: StreamId, by: HostId) {
        let Some(toward_listener) = self.direction_from(stream, by) else {
            return;
        };
        self.stream_queue
            .retain(|d| d.stream != stream || d.toward_listener == toward_listener);
        self.put(stream, toward_listener, Segment::End);
        if let Some(leg) = self.legs.get_mut(stream.0) {
            leg.open = false;
        }
    }

    /// Stop what `from` sends on `stream` for `ms`: what it sent before and
    /// what it sends meanwhile arrive together when the stop ends, in order.
    pub fn stall(&mut self, stream: StreamId, from: HostId, ms: f64) {
        let Some(toward_listener) = self.direction_from(stream, from) else {
            return;
        };
        let until = self.now_us.saturating_add(us_from_ms(ms));
        for delivery in &mut self.stream_queue {
            if delivery.stream == stream && delivery.toward_listener == toward_listener {
                delivery.at_us = delivery.at_us.max(until);
            }
        }
        if let Some(leg) = self.legs.get_mut(stream.0) {
            let direction = leg.direction_mut(toward_listener);
            direction.last_us = direction.last_us.max(until);
            direction.stopped_until_us = direction.stopped_until_us.max(until);
        }
    }

    /// Take the next stream delivery whose time has come. Drive until
    /// `None`, as with datagrams.
    pub fn next_stream_arrival(&mut self) -> Option<StreamArrival> {
        let now = self.now_us;
        let (index, _) = self
            .stream_queue
            .iter()
            .enumerate()
            .filter(|(_, d)| d.at_us <= now)
            .min_by_key(|(_, d)| (d.at_us, d.seq))?;
        let delivery = self.stream_queue.remove(index);
        let leg = self.legs.get(delivery.stream.0)?;
        let (host, from) = if delivery.toward_listener {
            (leg.listener, leg.client_addr)
        } else {
            (leg.client, leg.server)
        };
        Some(StreamArrival {
            stream: delivery.stream,
            host,
            from,
            data: delivery.data,
        })
    }

    /// Which way `from` sends on `stream`, while it is open: toward the
    /// listener, or toward the end that connected.
    fn direction_from(&self, stream: StreamId, from: HostId) -> Option<bool> {
        let leg = self.legs.get(stream.0).filter(|leg| leg.open)?;
        if from == leg.client {
            Some(true)
        } else if from == leg.listener {
            Some(false)
        } else {
            None
        }
    }

    fn put(&mut self, stream: StreamId, toward_listener: bool, data: Segment) {
        let jitter = if self.link.jitter_ms > 0.0 {
            self.rng.unit() * self.link.jitter_ms
        } else {
            0.0
        };
        let due = self
            .now_us
            .saturating_add(us_from_ms(self.link.one_way_ms + jitter));
        let Some(leg) = self.legs.get_mut(stream.0) else {
            return;
        };
        let direction = leg.direction_mut(toward_listener);
        let at_us = due.max(direction.last_us);
        direction.last_us = at_us;
        self.seq = self.seq.wrapping_add(1);
        self.stream_queue.push(Delivery {
            at_us,
            seq: self.seq,
            stream,
            toward_listener,
            data,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::{Link, Nat};

    fn public(last: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)), port)
    }

    fn private(last: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, last)), port)
    }

    /// Everything delivered by `until_ms`, in delivery order.
    fn delivered(sim: &mut Sim, until_ms: f64) -> Vec<StreamArrival> {
        let mut out = Vec::new();
        while sim.now_ms() < until_ms {
            while let Some(arrival) = sim.next_stream_arrival() {
                out.push(arrival);
            }
            sim.advance_ms(1.0);
        }
        while let Some(arrival) = sim.next_stream_arrival() {
            out.push(arrival);
        }
        out
    }

    fn bytes(arrivals: &[StreamArrival]) -> Vec<Vec<u8>> {
        arrivals
            .iter()
            .filter_map(|arrival| match &arrival.data {
                Segment::Bytes(bytes) => Some(bytes.clone()),
                Segment::End => None,
            })
            .collect()
    }

    /// A link that loses, duplicates, reorders and jitters datagrams: a
    /// stream over it still delivers every write once, in order.
    #[test]
    fn a_stream_delivers_every_write_once_in_order_on_a_lossy_link() {
        let mut sim = Sim::new(9).with_link(Link {
            loss: 0.5,
            duplicate: 0.5,
            reorder: 0.5,
            reorder_ms: 30.0,
            jitter_ms: 20.0,
            ..Link::default()
        });
        let client = sim.add_host(public(10, 5000), &[]);
        let server = sim.add_host(public(20, 3478), &[]);
        let stream = sim.connect(client, public(20, 3478)).unwrap();
        for index in 0..200u8 {
            sim.stream_send(stream, client, &[index]);
            sim.advance_ms(0.5);
        }
        let arrived = delivered(&mut sim, 200.0);
        assert!(arrived.iter().all(|arrival| arrival.host == server));
        assert!(
            arrived
                .iter()
                .all(|arrival| arrival.from == public(10, 5000))
        );
        assert_eq!(
            bytes(&arrived),
            (0..200u8).map(|i| vec![i]).collect::<Vec<_>>()
        );
        assert!(sim.take_drops().is_empty(), "a stream write was dropped");
    }

    /// A stop holds one direction -- what was sent before it and what is
    /// sent during it -- and then delivers it all at once, in order; the other
    /// direction flows meanwhile.
    #[test]
    fn a_stall_holds_one_direction_and_then_releases_it_in_order() {
        let mut sim = Sim::new(1);
        let client = sim.add_host(public(10, 5000), &[]);
        let server = sim.add_host(public(20, 3478), &[]);
        let stream = sim.connect(client, public(20, 3478)).unwrap();
        sim.stream_send(stream, server, b"before");
        sim.stall(stream, server, 500.0);
        sim.advance_ms(100.0);
        sim.stream_send(stream, server, b"during");
        sim.stream_send(stream, client, b"the other way");

        let early = delivered(&mut sim, 499.0);
        assert_eq!(bytes(&early), [b"the other way".to_vec()]);
        assert_eq!(early[0].host, server);

        let late = delivered(&mut sim, 501.0);
        assert_eq!(bytes(&late), [b"before".to_vec(), b"during".to_vec()]);
        assert!(late.iter().all(|arrival| arrival.host == client));
        assert!(
            late.iter().all(|arrival| arrival.from == public(20, 3478)),
            "the connecting end sees the address it reached"
        );
    }

    /// A sender that holds a bounded queue per stream takes every write while
    /// the stream moves -- a burst in flight is not a queue -- and behind a
    /// stop takes writes until that many are held and drops the rest; once
    /// the stop ends and they are delivered, it takes writes again.
    #[test]
    fn a_bounded_sender_drops_past_its_depth_behind_a_stall() {
        let mut sim = Sim::new(1);
        let client = sim.add_host(public(10, 5000), &[]);
        let server = sim.add_host(public(20, 3478), &[]);
        let stream = sim.connect(client, public(20, 3478)).unwrap();
        let burst: Vec<bool> = (0..8u8)
            .map(|index| sim.stream_send_within(stream, server, &[index], 5))
            .collect();
        assert!(burst.iter().all(|taken| *taken), "a moving stream refused");
        assert_eq!(bytes(&delivered(&mut sim, 50.0)).len(), 8);

        sim.stall(stream, server, 500.0);
        let taken: Vec<bool> = (0..8u8)
            .map(|index| sim.stream_send_within(stream, server, &[index], 5))
            .collect();
        assert_eq!(taken, [true, true, true, true, true, false, false, false]);
        assert_eq!(
            bytes(&delivered(&mut sim, 600.0)),
            (0..5u8).map(|i| vec![i]).collect::<Vec<_>>()
        );
        assert!(sim.stream_send_within(stream, server, &[9], 5));
    }

    /// The end follows what was sent before it; what was on its way to the
    /// end that closed never reaches it; and nothing is carried after it
    /// either way.
    #[test]
    fn the_end_follows_what_was_sent_before_it_and_nothing_after() {
        let mut sim = Sim::new(1);
        let client = sim.add_host(public(10, 5000), &[]);
        let server = sim.add_host(public(20, 3478), &[]);
        let stream = sim.connect(client, public(20, 3478)).unwrap();
        sim.stream_send(stream, client, b"on its way to the closer");
        sim.stream_send(stream, server, b"last words");
        sim.close(stream, server);
        sim.stream_send(stream, server, b"after the end");
        sim.stream_send(stream, client, b"to a closed stream");
        let arrived = delivered(&mut sim, 100.0);
        let data: Vec<Segment> = arrived.into_iter().map(|arrival| arrival.data).collect();
        assert_eq!(data, [Segment::Bytes(b"last words".to_vec()), Segment::End]);
    }

    /// A connect is refused where nothing listens, and reaches a listener
    /// behind a router through its forwarded port, which then sees the
    /// client's translated address.
    #[test]
    fn a_connect_to_nothing_is_refused_and_a_forwarded_port_is_reached() {
        let mut sim = Sim::new(1);
        let translator = sim.add_nat(Nat::symmetric(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1))));
        let client = sim.add_host(private(10, 5000), &[translator]);
        let router = sim.add_nat(
            Nat::port_restricted(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 2)))
                .with_forward(50_085, private(20, 3478)),
        );
        let relay = sim.add_host(private(20, 3478), &[router]);

        assert!(sim.connect(client, public(77, 3478)).is_none());

        let stream = sim.connect(client, public(2, 50_085)).unwrap();
        sim.stream_send(stream, client, b"hello");
        let arrived = delivered(&mut sim, 50.0);
        assert_eq!(arrived.len(), 1);
        assert_eq!(arrived[0].host, relay);
        assert_eq!(
            arrived[0].from.ip(),
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1))
        );
    }
}
