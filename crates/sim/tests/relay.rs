//! The relay in the simulator, against a relay server written from the
//! standard rather than from the client's codec (docs/03-connectivity.md 7).
//!
//! A symmetric client reaches a host through the relay and not without it;
//! the host-box deployment with one forwarded port; full-size datagrams both
//! ways in both framings; 300 seconds crossed three times and a nonce rotation
//! with the path kept; a loopback candidate never relayed; and each typed
//! outcome from its cause.
//!
//! The host knows nothing of the relay. It is handed one more candidate and
//! runs exactly as it would against a peer it can reach directly.
//!
//! **And the same relay over a stream** (03 7.4), the client's messages and
//! the relay's found in each from their own lengths: full-size datagrams both
//! ways in both framings and each typed outcome, as over datagrams; a
//! half-second stop of the relay's side with a run dropped, the session
//! surviving; 300 seconds crossed twice and a nonce rotation with nothing
//! re-sent; a head that is neither, and the client's close; and the storm a
//! stopping stream sets off, counted.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::conn::{self, Conn, Credentials, Kind, PROBE_TTL, Ttl};
use lowlat_core::endpoint::Endpoint;
use lowlat_core::envelope::Envelope;
use lowlat_core::relay::{Carrier, Failure, Relay as ClientRelay, State as RelayState};
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::{Health, Session};
use lowlat_core::turn;
use lowlat_sim::relay::Relay;
use lowlat_sim::stream::{Segment, StreamId};
use lowlat_sim::{HostId, Link, Nat, Sim};

const KEY: [u8; 32] = [0x2B; 32];
const CHANNEL: u8 = 1;
/// A fragment per slot at the default datagram size, as the client and the host
/// run it: what makes a full-size datagram full-size.
const SLOT: usize = lowlat_core::DEFAULT_BODY;
const SLOTS: usize = 256;
const USER: &str = "user";
const PASS: &str = "password";

/// Where each side would ask for its own mapping. Nothing needs to answer:
/// the simulator performs the translation the question would.
const STUN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 99)), 3478);

fn public(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(203, 0, 113, last))
}

fn private(third: u8, last: u8, port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, third, last)), port)
}

/// The host's machine, where the relay also runs in the deployment of
/// docs/03-connectivity.md 7.1.
fn the_box(port: u16) -> SocketAddr {
    private(1, 20, port)
}

struct Arena {
    recv_bodies: Vec<u8>,
    recv_meta: Vec<SlotMeta>,
    send_bodies: Vec<u8>,
    send_meta: Vec<SendSlot>,
}

impl Arena {
    fn new() -> Self {
        Self {
            recv_bodies: vec![0u8; SLOT * SLOTS],
            recv_meta: vec![SlotMeta::default(); SLOTS],
            send_bodies: vec![0u8; SLOT * SLOTS],
            send_meta: vec![SendSlot::default(); SLOTS],
        }
    }

    fn session(&mut self) -> Session<'_> {
        let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
        session
            .attach_recv(
                CHANNEL,
                RecvRing::new(&mut self.recv_bodies, &mut self.recv_meta, SLOT).unwrap(),
            )
            .unwrap();
        session
            .attach_send(
                CHANNEL,
                SendRing::new(&mut self.send_bodies, &mut self.send_meta, SLOT, CHANNEL).unwrap(),
            )
            .unwrap();
        session
    }
}

fn conn(
    ours: (&'static str, &'static str),
    theirs: (&'static str, &'static str),
    seed: u8,
) -> Conn<'static> {
    Conn::new(
        Credentials {
            local_ufrag: ours.0,
            local_pwd: ours.1,
            remote_ufrag: theirs.0,
            remote_pwd: theirs.1,
        },
        [seed; 16],
        0.0,
    )
}

const CLIENT: (&str, &str) = ("aaaa", "passwordforaaaa");
const HOST: (&str, &str) = ("bbbb", "passwordforbbbb");

/// A stream attempt's end of the leg: its stream, `None` when the connect was
/// refused; what has arrived of a message not yet whole; and whether the
/// stream has ended.
struct ClientStream {
    id: Option<StreamId>,
    held: Vec<u8>,
    ended: bool,
}

/// The network, the relay on it, and the two ends.
struct World<'a> {
    sim: Sim,
    relay: Option<Relay>,
    client: Endpoint<'a>,
    client_id: HostId,
    /// The client's stream to the relay, in an attempt over one.
    stream: Option<ClientStream>,
    host: Endpoint<'a>,
    host_id: HostId,
    /// Every datagram the host sent.
    host_sent: u64,
    offered: bool,
    at_client: Vec<Vec<u8>>,
    at_host: Vec<Vec<u8>>,
    /// Every address the client sent anything to.
    client_sent_to: Vec<SocketAddr>,
    message: Vec<u8>,
}

impl World<'_> {
    fn step(&mut self) {
        let now = self.sim.now_ms();
        let mut buf = [0u8; 2100];
        let mut scratch = [0u8; 2100];
        let ttl = |ttl: Ttl| if ttl == Ttl::Probe { PROBE_TTL } else { 64 };
        let mut write = Vec::new();
        while let Some(result) = self.client.get_output(now, &mut buf) {
            let egress =
                result.unwrap_or_else(|error| panic!("the client could not emit: {error}"));
            if !self.client_sent_to.contains(&egress.to) {
                self.client_sent_to.push(egress.to);
            }
            if self.stream.is_some() {
                // A pass's messages go as one write, as the shell's staging
                // sends them.
                write.extend_from_slice(&buf[..egress.len]);
            } else {
                self.sim.send(
                    self.client_id,
                    egress.to,
                    ttl(egress.ttl),
                    &buf[..egress.len],
                );
            }
        }
        if let Some(id) = self.stream.as_ref().and_then(|stream| stream.id)
            && !write.is_empty()
        {
            self.sim.stream_send(id, self.client_id, &write);
        }
        while let Some(result) = self.host.get_output(now, &mut buf) {
            let egress = result.unwrap_or_else(|error| panic!("the host could not emit: {error}"));
            self.host_sent += 1;
            self.sim
                .send(self.host_id, egress.to, ttl(egress.ttl), &buf[..egress.len]);
        }
        while let Some(arrival) = self.sim.next_arrival() {
            // A datagram that is refused is dropped, not fatal.
            if arrival.host == self.client_id {
                let _ = self.client.process_input(
                    &arrival.bytes,
                    arrival.from,
                    None,
                    now,
                    &mut scratch,
                );
            } else if arrival.host == self.host_id {
                let _ =
                    self.host
                        .process_input(&arrival.bytes, arrival.from, None, now, &mut scratch);
            } else if let Some(relay) = self.relay.as_mut().filter(|relay| relay.owns(arrival.host))
            {
                relay.receive(&mut self.sim, &arrival);
            }
        }
        while let Some(arrival) = self.sim.next_stream_arrival() {
            if arrival.host == self.client_id {
                self.client_stream_received(arrival.data, now, &mut scratch);
            } else if let Some(relay) = self.relay.as_mut().filter(|relay| relay.owns(arrival.host))
            {
                relay.receive_stream(&mut self.sim, &arrival);
            }
        }
        // Signaling, at once: the relayed address once there is one, and the
        // readiness marker after it.
        if !self.offered
            && let Some(relayed) = self.client.relay().and_then(ClientRelay::relayed)
        {
            self.host
                .conn()
                .add_candidate(relayed, Kind::marked(false, true))
                .unwrap();
            self.host.conn().set_peer_ready();
            self.offered = true;
        }
        while let Some(Ok(len)) = self
            .client
            .session()
            .take_message(CHANNEL, &mut self.message)
        {
            self.at_client.push(self.message[..len].to_vec());
        }
        while let Some(Ok(len)) = self.host.session().take_message(CHANNEL, &mut self.message) {
            self.at_host.push(self.message[..len].to_vec());
        }

        let wait = self
            .client
            .next_timer_ms(now)
            .min(self.host.next_timer_ms(now))
            .min(self.sim.next_delivery_ms().unwrap_or(f64::INFINITY))
            .clamp(1.0, 50.0);
        self.sim.advance_ms(wait);
        let now = self.sim.now_ms();
        self.client.poll(now);
        self.host.poll(now);
    }

    /// What the client's stream delivered: every message now whole in it,
    /// found by the core's own framer as the shell finds them and handed in
    /// from the relay, padding gone; or its end.
    fn client_stream_received(&mut self, data: Segment, now: f64, scratch: &mut [u8]) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        let bytes = match data {
            Segment::Bytes(bytes) => bytes,
            Segment::End => {
                stream.ended = true;
                return;
            }
        };
        stream.held.extend_from_slice(&bytes);
        let server = self.client.relay().expect("a relay attempt").server();
        loop {
            let frame = match turn::frame(&stream.held) {
                Ok(Some(frame)) if stream.held.len() >= frame.wire => frame,
                Ok(_) => return,
                // As the shell does, the stream is over.
                Err(_) => {
                    stream.ended = true;
                    return;
                }
            };
            if !frame.reserved {
                let _ = self.client.process_input(
                    &stream.held[..frame.len],
                    server,
                    None,
                    now,
                    scratch,
                );
            }
            stream.held.drain(..frame.wire);
        }
    }

    /// The client's stream, in an attempt whose connect was taken.
    fn stream_id(&self) -> StreamId {
        self.stream
            .as_ref()
            .and_then(|stream| stream.id)
            .expect("a stream")
    }

    fn stream_ended(&self) -> bool {
        self.stream.as_ref().is_some_and(|stream| stream.ended)
    }

    /// Run until `done` or `until_ms`; whether `done` came.
    fn run(&mut self, until_ms: f64, done: impl Fn(&Self) -> bool) -> bool {
        while self.sim.now_ms() < until_ms {
            if done(self) {
                return true;
            }
            self.step();
        }
        done(self)
    }

    fn established(&self) -> bool {
        self.client_path().is_some() && self.host_path().is_some()
    }

    fn client_path(&self) -> Option<SocketAddr> {
        self.client.path()
    }

    fn host_path(&self) -> Option<SocketAddr> {
        self.host.path()
    }

    fn relay_state(&self) -> RelayState {
        self.client.relay().expect("a relay attempt").state()
    }

    fn counts(&self) -> lowlat_sim::relay::Counts {
        self.relay.as_ref().expect("a relay").counts.clone()
    }
}

/// A client behind a symmetric translator, which no direct punch gets
/// through: its checks to a peer leave from a mapping the peer was never
/// told about.
fn symmetric_client(sim: &mut Sim) -> HostId {
    let nat = sim.add_nat(Nat::symmetric(public(1)));
    sim.add_host(private(9, 10, 5000), &[nat])
}

/// Everything about one run but the arenas, which the endpoints borrow.
struct Setup {
    sim: Sim,
    relay: Option<Relay>,
    client_id: HostId,
    host_id: HostId,
    /// The relay as the client is configured with it; `None` for a direct
    /// attempt.
    server: Option<SocketAddr>,
    /// What the host's answer carries.
    host_candidates: Vec<(SocketAddr, Kind)>,
    /// What the client would offer in a direct attempt.
    client_candidates: Vec<(SocketAddr, Kind)>,
    /// The client reaches the relay over a stream.
    over_stream: bool,
}

fn world<'a>(
    setup: Setup,
    client_arena: &'a mut Arena,
    host_arena: &'a mut Arena,
    password: &'static str,
) -> World<'a> {
    let Setup {
        mut sim,
        relay,
        client_id,
        host_id,
        server,
        host_candidates,
        client_candidates,
        over_stream,
    } = setup;
    let client_conn = conn(CLIENT, HOST, 0xA1);
    let mut client = match server {
        Some(server) => {
            let relay = ClientRelay::new(server, USER, password, [0x5E; 16], 0.0);
            let relay = if over_stream {
                relay.over(Carrier::Stream)
            } else {
                relay
            };
            Endpoint::relayed(client_conn, client_arena.session(), relay)
        }
        None => Endpoint::new(client_conn, client_arena.session()),
    };
    let stream = over_stream.then(|| ClientStream {
        id: server.and_then(|server| sim.connect(client_id, server)),
        held: Vec::new(),
        ended: false,
    });
    let mut host = Endpoint::new(conn(HOST, CLIENT, 0xB2), host_arena.session());
    for (addr, kind) in host_candidates {
        client.add_candidate(addr, kind).unwrap();
    }
    client.conn().set_peer_ready();
    let offered = server.is_none();
    for (addr, kind) in client_candidates {
        host.conn().add_candidate(addr, kind).unwrap();
    }
    if offered {
        host.conn().set_peer_ready();
    }
    World {
        sim,
        relay,
        client,
        client_id,
        stream,
        host,
        host_id,
        host_sent: 0,
        offered,
        at_client: Vec::new(),
        at_host: Vec::new(),
        client_sent_to: Vec::new(),
        message: vec![0u8; 16_384],
    }
}

/// A relay on a public server; the host behind a port-restricted translator.
fn public_relay(with_relay: bool) -> Setup {
    let mut sim = Sim::new(0x5EED_0001);
    let client_id = symmetric_client(&mut sim);
    let host_nat = sim.add_nat(Nat::port_restricted(public(2)));
    let host_id = sim.add_host(private(2, 20, 6000), &[host_nat]);
    let server = SocketAddr::new(public(50), 3478);
    let relay = with_relay.then(|| Relay::new(&mut sim, server, public(50), &[], USER, PASS));
    let host_reflexive = sim.reflexive(host_id, STUN);
    let client_reflexive = sim.reflexive(client_id, STUN);
    Setup {
        sim,
        relay,
        client_id,
        host_id,
        server: with_relay.then_some(server),
        host_candidates: vec![(host_reflexive, Kind::Reflexive)],
        client_candidates: if with_relay {
            Vec::new()
        } else {
            vec![(client_reflexive, Kind::Reflexive)]
        },
        over_stream: false,
    }
}

/// The deployment of docs/03-connectivity.md 7.1: the relay on the host's
/// own machine behind a router that forwards its one port, the relayed
/// addresses the machine's own, the host beside it.
fn host_box() -> (Setup, SocketAddr) {
    let mut sim = Sim::new(0x5EED_0002);
    let client_id = symmetric_client(&mut sim);
    let router = sim.add_nat(Nat::port_restricted(public(2)).with_forward(50_085, the_box(3478)));
    let relay = Relay::new(
        &mut sim,
        the_box(3478),
        the_box(0).ip(),
        &[router],
        USER,
        PASS,
    );
    let host_id = sim.add_host(the_box(22_974), &[router]);
    let host_reflexive = sim.reflexive(host_id, STUN);
    let forwarded = SocketAddr::new(public(2), 50_085);
    (
        Setup {
            sim,
            relay: Some(relay),
            client_id,
            host_id,
            server: Some(forwarded),
            host_candidates: vec![
                (the_box(22_974), Kind::Direct),
                (host_reflexive, Kind::Reflexive),
            ],
            client_candidates: Vec::new(),
            over_stream: false,
        },
        forwarded,
    )
}

/// The host-box deployment, the client reaching the relay over a stream.
fn host_box_over_stream() -> Setup {
    let (mut setup, _) = host_box();
    setup.over_stream = true;
    setup
}

/// Send a message each way and run until both have arrived.
fn exchange(world: &mut World<'_>, payload: &[u8], within_ms: f64) -> bool {
    let (client_had, host_had) = (world.at_client.len(), world.at_host.len());
    world
        .client
        .session()
        .send_message(CHANNEL, b"c", payload)
        .unwrap();
    world
        .host
        .session()
        .send_message(CHANNEL, b"h", payload)
        .unwrap();
    let until = world.sim.now_ms() + within_ms;
    world.run(until, |world| {
        world.at_client.len() > client_had && world.at_host.len() > host_had
    })
}

/// The case the relay exists for: a client no punch reaches, reached through
/// the relay; and the same pair without it, timing out, so this passes on the
/// relay rather than on a topology anything could cross.
#[test]
fn a_symmetric_client_reaches_the_host_through_the_relay_and_not_without_it() {
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut direct = world(
        public_relay(false),
        &mut client_arena,
        &mut host_arena,
        PASS,
    );
    direct.run(conn::PUNCH_WINDOW_MS + 100.0, World::established);
    assert!(
        !direct.established(),
        "a direct punch got through a symmetric translator"
    );
    assert!(matches!(
        direct.client.conn().state(),
        conn::State::Failed(_)
    ));

    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut relayed = world(public_relay(true), &mut client_arena, &mut host_arena, PASS);
    assert!(
        relayed.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path through the relay"
    );
    assert_eq!(
        relayed.host_path(),
        relayed.client.relay().and_then(ClientRelay::relayed)
    );
    assert!(exchange(&mut relayed, b"through the relay", 2_000.0));
    assert_eq!(relayed.client_sent_to, [SocketAddr::new(public(50), 3478)]);
}

/// The host-box deployment. The client talks to the router's one forwarded
/// port and nothing else; the relayed address is the machine's own, which
/// nothing outside can reach, and the host reaches it without leaving the
/// machine.
#[test]
fn the_host_box_deployment_with_one_forwarded_port() {
    let (setup, forwarded) = host_box();
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );

    let relayed = world.client.relay().and_then(ClientRelay::relayed).unwrap();
    assert_eq!(
        relayed.ip(),
        the_box(0).ip(),
        "the relayed address is not the machine's own"
    );
    assert_eq!(world.host_path(), Some(relayed));
    assert_eq!(world.client_path(), Some(the_box(22_974)));
    assert!(exchange(&mut world, b"over the one port", 2_000.0));
    assert_eq!(
        world.client_sent_to,
        [forwarded],
        "the client sent outside the relay"
    );
    let counts = world.counts();
    assert_eq!(counts.allocations, 1);
    assert_eq!(counts.unpermitted, 0, "a datagram met no permission");
}

/// A host on the relay's machine that offers only its public address. The
/// relay's own machine permitted before the relayed address is offered is all
/// that lets its first check through: its checks come from the machine's own
/// address, and nothing it offered names that address.
#[test]
fn the_relays_own_machine_is_permitted_before_anything_is_offered() {
    let (mut setup, _) = host_box();
    setup
        .host_candidates
        .retain(|(_, kind)| *kind == Kind::Reflexive);
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );
    assert_eq!(world.counts().unpermitted, 0, "a check met no permission");
    assert!(exchange(&mut world, b"from the box", 2_000.0));
}

/// Floor-sized datagrams both ways, as indications through a relay that binds
/// no channel and as channel data through one that does. A relay's framing
/// added to a full datagram is what a buffer sized for the datagram alone
/// drops, and a refused binding leaves media on indications, not stopped.
///
/// Over datagrams and over a stream. On a stream channel data is padded both
/// ways, or the next message is read from the wrong place; the sizes the
/// relay counts are the framings' own, padding aside.
#[test]
fn full_size_datagrams_cross_both_ways_in_both_framings() {
    let floor = lowlat_core::DEFAULT_DATAGRAM;
    for (over_stream, channels) in [(false, false), (false, true), (true, false), (true, true)] {
        let (mut setup, _) = host_box();
        setup.over_stream = over_stream;
        if !channels {
            setup.relay = setup.relay.map(Relay::refusing_channels);
        }
        let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
        let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
        assert!(
            world.run(conn::PUNCH_WINDOW_MS, World::established),
            "no path, over_stream={over_stream}"
        );

        let large: Vec<u8> = (0u8..=250).cycle().take(3_000).collect();
        assert!(exchange(&mut world, &large, 2_000.0));
        world.run(world.sim.now_ms() + 500.0, |_| false);
        assert!(exchange(&mut world, &large, 2_000.0));
        assert_eq!(world.at_client.len() + world.at_host.len(), 4);
        for arrived in world.at_client.iter().chain(world.at_host.iter()) {
            assert_eq!(&arrived[1..], &large[..], "a large message arrived damaged");
        }

        let counts = world.counts();
        if over_stream {
            assert!(counts.stream_in > 0, "nothing came over the stream");
            assert_eq!(
                counts.stream_errors, 0,
                "the relay lost the stream's framing"
            );
            assert!(
                !world.stream_ended(),
                "the client lost the stream's framing, channels={channels}"
            );
        }
        if channels {
            assert_eq!(
                counts.largest_channel_in,
                floor + 4,
                "client to relay, channel data"
            );
            assert_eq!(
                counts.largest_channel_out,
                floor + 4,
                "relay to client, channel data"
            );
        } else {
            assert_eq!(
                counts.channel_in + counts.channel_out,
                0,
                "a channel was used"
            );
            assert_eq!(
                counts.largest_indication_in,
                floor + 36 + 3,
                "client to relay, indication"
            );
            assert_eq!(
                counts.largest_indication_out,
                floor + 36 + 3,
                "relay to client, indication"
            );
        }
    }
}

/// Sixteen minutes: every permission crosses 300 seconds three times, the
/// allocation is refreshed at each half-life, and the relay's nonce rotates
/// twice. Media every 20 ms from the host at a wide-area round trip, a message
/// each second from the client, and every one arrives: a permission renewed
/// as it lapses rather than before would drop what crosses in between.
///
/// Through a relay that binds no channel as well as one that does: a binding
/// renews its address's permission too, and can hide a permission renewed
/// late. Without one the permission is all that keeps the path.
#[test]
fn the_path_is_kept_across_three_permission_lifetimes_and_two_nonce_rotations() {
    for channels in [false, true] {
        kept_for_sixteen_minutes(channels);
    }
}

/// Media every 20 ms from the host and a message each second from the
/// client until `until_ms`, then two quiet seconds; how many went each way.
/// A full ring is media that stopped leaving: the path lapsed.
fn media_until(world: &mut World<'_>, until_ms: f64, case: &str) -> (usize, usize) {
    let (mut to_client, mut to_host) = (0, 0);
    let (mut media_at, mut message_at) = (world.sim.now_ms(), world.sim.now_ms());
    while world.sim.now_ms() < until_ms {
        let now = world.sim.now_ms();
        if now >= media_at {
            let frame = format!("frame {to_client}");
            assert!(
                world
                    .host
                    .session()
                    .send_message(CHANNEL, &[], frame.as_bytes())
                    .is_ok(),
                "the host's media stalled at {now} ms, {case}"
            );
            to_client += 1;
            media_at += 20.0;
        }
        if now >= message_at {
            let tick = format!("tick {to_host}");
            assert!(
                world
                    .client
                    .session()
                    .send_message(CHANNEL, &[], tick.as_bytes())
                    .is_ok(),
                "the client's messages stalled at {now} ms, {case}"
            );
            to_host += 1;
            message_at += 1_000.0;
        }
        world.step();
    }
    world.run(world.sim.now_ms() + 2_000.0, |_| false);
    (to_client, to_host)
}

fn kept_for_sixteen_minutes(channels: bool) {
    let (mut setup, _) = host_box();
    setup.sim = setup.sim.with_link(lowlat_sim::Link {
        one_way_ms: 25.0,
        ..lowlat_sim::Link::default()
    });
    setup.relay = setup.relay.map(|relay| {
        let relay = relay.rotating_nonce_every(400_000.0);
        if channels {
            relay
        } else {
            relay.refusing_channels()
        }
    });
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );

    let (to_client, to_host) = media_until(&mut world, 960_000.0, &format!("channels={channels}"));

    let now = world.sim.now_ms();
    assert_eq!(
        world.relay_state(),
        RelayState::Ready(world.host_path().unwrap())
    );
    assert_eq!(world.client.health(now), Health::Alive);
    assert_eq!(
        world.at_client.len(),
        to_client,
        "media to the client went missing, channels={channels}"
    );
    assert_eq!(
        world.at_host.len(),
        to_host,
        "messages to the host went missing"
    );

    let counts = world.counts();
    assert_eq!(
        counts.unpermitted, 0,
        "a permission lapsed, channels={channels}: {counts:?}"
    );
    assert!(
        counts.stale_nonces >= 2,
        "the nonce never rotated: {counts:?}"
    );
    assert!(
        counts.refreshes >= 3,
        "the allocation was not refreshed: {counts:?}"
    );
    assert!(
        counts.permissions >= 2 * 4,
        "permissions were not renewed: {counts:?}"
    );
}

/// A host that offers a loopback candidate, hostile or mistaken. Relayed
/// toward, it would destroy the allocation; it is never permitted or checked,
/// and the session carries on.
#[test]
fn a_loopback_candidate_does_not_end_the_session() {
    let (mut setup, _) = host_box();
    setup.host_candidates.insert(
        0,
        (
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 22_974),
            Kind::Direct,
        ),
    );
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );
    assert!(exchange(&mut world, b"still here", 2_000.0));
    assert_eq!(
        world.counts().destroyed,
        0,
        "an allocation was relayed toward loopback"
    );
    assert!(matches!(world.relay_state(), RelayState::Ready(_)));
}

/// Nothing answers at the configured address: unreachable, at the deadline.
/// Over a stream the connect is refused, and nothing re-sent stands in for
/// the deadline.
#[test]
fn a_relay_that_is_not_there_is_unreachable() {
    for over_stream in [false, true] {
        let (mut setup, _) = host_box();
        setup.over_stream = over_stream;
        setup.server = Some(SocketAddr::new(public(77), 3478));
        let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
        let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
        if over_stream {
            assert!(
                world.stream.as_ref().unwrap().id.is_none(),
                "a connect to nothing was taken"
            );
        }
        world.run(lowlat_core::relay::SETUP_DEADLINE_MS + 100.0, |world| {
            world.relay_state() != RelayState::Setup
        });
        assert_eq!(
            world.relay_state(),
            RelayState::Failed(Failure::Unreachable),
            "over_stream={over_stream}"
        );
        assert!(world.sim.now_ms() >= lowlat_core::relay::SETUP_DEADLINE_MS);
    }
}

/// The wrong password: the relay challenges the credentials it was sent, and
/// the attempt ends refused rather than asking again. Over datagrams and over
/// a stream.
#[test]
fn wrong_credentials_are_refused() {
    for over_stream in [false, true] {
        let (mut setup, _) = host_box();
        setup.over_stream = over_stream;
        let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
        let mut world = world(
            setup,
            &mut client_arena,
            &mut host_arena,
            "not the password",
        );
        world.run(1_000.0, |world| world.relay_state() != RelayState::Setup);
        assert_eq!(
            world.relay_state(),
            RelayState::Failed(Failure::Refused),
            "over_stream={over_stream}"
        );
        assert_eq!(world.counts().allocations, 0);
    }
}

/// A relay that restarted has let every allocation go; the next renewal is
/// refused, and the attempt ends lost. Over datagrams and over a stream.
#[test]
fn a_relay_that_forgets_the_allocation_is_lost() {
    for over_stream in [false, true] {
        let (mut setup, _) = host_box();
        setup.over_stream = over_stream;
        let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
        let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
        assert!(
            world.run(conn::PUNCH_WINDOW_MS, World::established),
            "no path, over_stream={over_stream}"
        );
        world.relay.as_mut().unwrap().forget_allocations();
        world.run(400_000.0, |world| {
            world.relay_state() != RelayState::Ready(world.host_path().unwrap())
        });
        assert_eq!(
            world.relay_state(),
            RelayState::Failed(Failure::Lost),
            "over_stream={over_stream}"
        );
    }
}

/// Half a second with the relay's side of the stream stopped, as a deployed
/// relay's was measured stopping, while the host streams three fragments
/// every ten milliseconds: the relay holds a hundred of the host's datagrams
/// and drops the rest, and once the stream moves again the session repairs
/// what it can and carries on.
#[test]
fn a_half_second_stop_with_a_run_dropped_leaves_the_session_alive() {
    let mut setup = host_box_over_stream();
    setup.sim = setup.sim.with_link(Link {
        one_way_ms: 25.0,
        ..Link::default()
    });
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );
    let stream = world.stream_id();
    let relay_side = world.relay.as_ref().unwrap().listener();
    let started = world.sim.now_ms();
    let frame: Vec<u8> = (0u8..=250).cycle().take(3_000).collect();
    let (mut next, mut stopped) = (started, false);
    while world.sim.now_ms() < started + 3_000.0 {
        let now = world.sim.now_ms();
        if !stopped && now >= started + 1_000.0 {
            world.sim.stall(stream, relay_side, 500.0);
            stopped = true;
        }
        while now >= next && next < started + 2_000.0 {
            assert!(
                world
                    .host
                    .session()
                    .send_message(CHANNEL, &[], &frame)
                    .is_ok(),
                "the host's ring filled at {now} ms"
            );
            next += 10.0;
        }
        world.step();
    }
    world.run(world.sim.now_ms() + 3_000.0, |_| false);

    let counts = world.counts();
    assert!(
        counts.stream_drops > 0,
        "the stop dropped nothing: {counts:?}"
    );
    let now = world.sim.now_ms();
    assert_eq!(world.client.health(now), Health::Alive);
    assert!(matches!(world.relay_state(), RelayState::Ready(_)));
    assert!(!world.stream_ended());
    assert!(exchange(&mut world, b"after the stop", 2_000.0));
}

/// Eleven minutes over a stream: the path's permission crosses 300 seconds
/// twice and the relay's nonce rotates, with nothing ever re-sent -- the stale
/// nonce answered by a new request, every renewal landing before its lapse --
/// and every message arrives. Through a relay that binds no channel, where
/// the permission alone keeps the path.
#[test]
fn the_path_is_kept_over_a_stream_across_two_permission_lifetimes_and_a_nonce_rotation() {
    let mut setup = host_box_over_stream();
    setup.sim = setup.sim.with_link(Link {
        one_way_ms: 25.0,
        ..Link::default()
    });
    setup.relay = setup
        .relay
        .map(|relay| relay.rotating_nonce_every(400_000.0).refusing_channels());
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );
    let (to_client, to_host) = media_until(&mut world, 660_000.0, "over a stream");

    let now = world.sim.now_ms();
    assert_eq!(
        world.relay_state(),
        RelayState::Ready(world.host_path().unwrap())
    );
    assert_eq!(world.client.health(now), Health::Alive);
    assert_eq!(
        world.at_client.len(),
        to_client,
        "media to the client went missing"
    );
    assert_eq!(
        world.at_host.len(),
        to_host,
        "messages to the host went missing"
    );
    assert!(!world.stream_ended());
    let counts = world.counts();
    assert_eq!(counts.unpermitted, 0, "a permission lapsed: {counts:?}");
    assert_eq!(counts.stream_drops, 0, "a calm stream dropped: {counts:?}");
    assert!(
        counts.stale_nonces >= 1,
        "the nonce never rotated: {counts:?}"
    );
    assert!(
        counts.refreshes >= 2,
        "the allocation was not refreshed: {counts:?}"
    );
    assert!(
        counts.permissions >= 2 * 3,
        "permissions were not renewed: {counts:?}"
    );
}

/// A head on the client's stream that is neither a message nor channel data:
/// nothing after it can be found, so the relay closes the stream, and the
/// allocation on it goes with it.
#[test]
fn a_head_that_is_neither_closes_the_stream_and_its_allocation() {
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(
        host_box_over_stream(),
        &mut client_arena,
        &mut host_arena,
        PASS,
    );
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );
    let stream = world.stream_id();
    world
        .sim
        .stream_send(stream, world.client_id, &[0x80, 0, 0, 4, 1, 2, 3, 4]);
    world.run(world.sim.now_ms() + 200.0, World::stream_ended);
    assert!(
        world.stream_ended(),
        "the relay kept a stream it cannot read"
    );
    let counts = world.counts();
    assert_eq!(counts.stream_errors, 1);
    assert_eq!(counts.closed, 1, "the allocation outlived its stream");
}

/// The client closing its stream frees its allocation, as a deployed relay
/// does: nothing the host sends is relayed after it, nor tried on the stream
/// that is gone.
#[test]
fn the_clients_close_frees_its_allocation() {
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(
        host_box_over_stream(),
        &mut client_arena,
        &mut host_arena,
        PASS,
    );
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );
    assert!(exchange(&mut world, b"before the close", 2_000.0));
    let stream = world.stream_id();
    world.sim.close(stream, world.client_id);
    world.run(world.sim.now_ms() + 200.0, |_| false);
    // Counted from once the relay has seen the end: until then what the
    // host sends still finds the allocation and is tried on the stream.
    let tried = |counts: &lowlat_sim::relay::Counts| {
        counts.channel_out + counts.indications_out + counts.stream_drops
    };
    let before = tried(&world.counts());
    world
        .host
        .session()
        .send_message(CHANNEL, &[], b"after the close")
        .unwrap();
    world.run(world.sim.now_ms() + 500.0, |_| false);
    let counts = world.counts();
    assert_eq!(counts.closed, 1, "the close freed nothing");
    assert_eq!(tried(&counts), before, "an allocation outlived its stream");
}

/// What the session did over one minute, for the storm's record.
struct Storm {
    host_sent: u64,
    nacks: u64,
    late: u64,
    duplicate: u64,
    relay_drops: u32,
    delivered: usize,
    sent: usize,
    /// The host's smoothed round trip at its highest: the media's sender
    /// sees each stop as a longer trip.
    srtt_max_ms: f64,
}

/// A minute of the host's media over a stream -- three fragments every ten
/// milliseconds -- with the relay's side stopped as a deployed relay's was
/// measured stopping when `stops`: 300 ms at 12, 22, 32, 42 and 52 seconds,
/// under what the relay holds, and once at 3 seconds for 536 ms, past it.
fn storm(stops: bool) -> Storm {
    let mut setup = host_box_over_stream();
    setup.sim = setup.sim.with_link(Link {
        one_way_ms: 25.0,
        ..Link::default()
    });
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );
    let stream = world.stream_id();
    let relay_side = world.relay.as_ref().unwrap().listener();
    let started = world.sim.now_ms();
    let mut schedule = if stops {
        vec![
            (3_000.0, 536.0),
            (12_000.0, 300.0),
            (22_000.0, 300.0),
            (32_000.0, 300.0),
            (42_000.0, 300.0),
            (52_000.0, 300.0),
        ]
    } else {
        Vec::new()
    };
    let frame: Vec<u8> = (0u8..=250).cycle().take(3_000).collect();
    let (mut next, mut sent, mut srtt_max_ms) = (started, 0, 0.0f64);
    let (host_sent, delivered) = (world.host_sent, world.at_client.len());
    while world.sim.now_ms() < started + 60_000.0 {
        let now = world.sim.now_ms();
        if let Some(&(at, ms)) = schedule.first()
            && now >= started + at
        {
            world.sim.stall(stream, relay_side, ms);
            schedule.remove(0);
        }
        while now >= next {
            assert!(
                world
                    .host
                    .session()
                    .send_message(CHANNEL, &[], &frame)
                    .is_ok(),
                "the host's ring filled at {now} ms, stops={stops}"
            );
            sent += 1;
            next += 10.0;
        }
        world.step();
        srtt_max_ms = srtt_max_ms.max(world.host.session().srtt_ms());
    }
    world.run(world.sim.now_ms() + 3_000.0, |_| false);
    let now = world.sim.now_ms();
    assert_eq!(world.client.health(now), Health::Alive, "stops={stops}");
    assert!(matches!(world.relay_state(), RelayState::Ready(_)));
    let session = world.client.session();
    Storm {
        host_sent: world.host_sent - host_sent,
        nacks: session.nacks_sent(CHANNEL),
        late: session.recv_arrivals(CHANNEL).unwrap().late,
        duplicate: session.recv_drops(CHANNEL).unwrap().duplicate,
        relay_drops: world.counts().stream_drops,
        delivered: world.at_client.len() - delivered,
        sent,
        srtt_max_ms,
    }
}

/// The storm a stopping stream sets off: each stop holds the host's media
/// while its resend timer runs, the resends queue behind the stop, the drops
/// past the relay's depth come back as negative acknowledgements, and what
/// was never lost arrives twice. The counts are the stream's cost, recorded
/// beside a calm minute rather than asserted; what is asserted is that the
/// session survives and that the stops happened.
#[test]
fn the_storm_a_stopping_stream_sets_off_is_counted() {
    let calm = storm(false);
    let stormy = storm(true);
    for (case, run) in [("calm", &calm), ("storm", &stormy)] {
        println!(
            "{case}: sent {} delivered {} host datagrams {} nacks {} late {} duplicate {} \
             relay drops {} srtt max {:.1} ms",
            run.sent,
            run.delivered,
            run.host_sent,
            run.nacks,
            run.late,
            run.duplicate,
            run.relay_drops,
            run.srtt_max_ms
        );
    }
    assert_eq!(calm.relay_drops, 0, "a calm stream dropped");
    assert!(
        stormy.relay_drops > 0,
        "no stop went past the relay's depth"
    );
}
