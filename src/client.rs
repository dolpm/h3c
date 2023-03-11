use std::{net::ToSocketAddrs, str::FromStr};

use mio::net::UdpSocket;
use quiche::{h3, Connection, Error, RecvInfo};
use ring::rand::SecureRandom;

const MAX_DATAGRAM_SIZE: usize = 1472;
const INITIAL_RESPONSE_SIZE: usize = 14720;

pub struct Client {
    conn: Connection,
    recv_buf: [u8; MAX_DATAGRAM_SIZE],
    ret_buf: Vec<u8>,
    recv_info: RecvInfo,
    dgram: [u8; MAX_DATAGRAM_SIZE],
    socket: UdpSocket,
    events: mio::Events,
    poll: mio::Poll,
    pub h3_conn: h3::Connection,
}

impl<'a> Client {
    pub fn connect(peer: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let mut config = {
            let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION)?;

            config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)?;
            config.set_max_idle_timeout(5000);
            config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
            config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
            config.set_initial_max_data(10_000_000);
            config.set_initial_max_stream_data_bidi_local(1_000_000);
            config.set_initial_max_stream_data_bidi_remote(1_000_000);
            config.set_initial_max_stream_data_uni(1_000_000);
            config.set_initial_max_streams_bidi(100);
            config.set_initial_max_streams_uni(100);
            config.set_disable_active_migration(true);

            config
        };

        let scid = {
            let mut scid = [0; quiche::MAX_CONN_ID_LEN];
            ring::rand::SystemRandom::new().fill(&mut scid[..]).unwrap();
            scid
        };
        let scid = quiche::ConnectionId::from_ref(&scid);

        let peer_addr = format!("{peer}:443").to_socket_addrs()?.next().unwrap();
        let local_addr = match peer_addr {
            std::net::SocketAddr::V4(_) => std::net::SocketAddr::from_str("0.0.0.0:0"),
            std::net::SocketAddr::V6(_) => std::net::SocketAddr::from_str("[::]:0"),
        }?;

        let socket = {
            let socket = std::net::UdpSocket::bind(local_addr)?;
            socket.set_nonblocking(true)?;
            socket.connect(peer_addr)?;
            mio::net::UdpSocket::from_std(socket)
        };

        Self::establish(
            quiche::connect(Some(peer), &scid, local_addr, peer_addr, &mut config)?,
            socket,
        )
    }

    fn establish(
        mut conn: Connection,
        mut socket: UdpSocket,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut dgram = [0; MAX_DATAGRAM_SIZE];

        let mut poll = mio::Poll::new()?;
        poll.registry()
            .register(&mut socket, mio::Token(0), mio::Interest::READABLE)?;
        let mut events = mio::Events::with_capacity(1024);

        // send initial datagram
        let recv_info = {
            let (bw, send_info) = match conn.send(&mut dgram) {
                Ok(v) => v,
                Err(e) => return Err(Box::new(e)),
            };

            while let Err(e) = socket.send(&dgram[..bw]) {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    continue;
                }
                return Err(Box::new(e));
            }

            RecvInfo {
                from: send_info.to,
                to: send_info.from,
            }
        };

        let mut recv_buf = [0; MAX_DATAGRAM_SIZE];
        let mut h3_conn = None;
        let h3_config = quiche::h3::Config::new()?;

        // poll response and send second datagram after read complete
        // to complete the handshake
        // TODO: optimize?
        loop {
            poll.poll(&mut events, conn.timeout())?;

            'inner: loop {
                if events.is_empty() {
                    conn.on_timeout();
                    break 'inner;
                }

                let len = match socket.recv(&mut recv_buf) {
                    Ok(v) => v,
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            break 'inner;
                        }
                        return Err(Box::new(e));
                    }
                };

                let _ = conn.recv(&mut recv_buf[..len], recv_info);
            }

            if conn.is_closed() {
                // todo make this more verbose but it'll do for now
                return Err(Box::new(Error::StreamStopped(0)));
            }

            if conn.is_established() && h3_conn.is_none() {
                // println!("connection established with peer!");
                h3_conn = Some(quiche::h3::Connection::with_transport(
                    &mut conn, &h3_config,
                )?);
                break;
            } else {
                let (bw, _) = match conn.send(&mut dgram) {
                    Ok(v) => v,
                    Err(e) => return Err(Box::new(e)),
                };

                while let Err(e) = socket.send(&dgram[..bw]) {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        continue;
                    }
                    return Err(Box::new(e));
                }
            }
        }

        Ok(Self {
            conn,
            h3_conn: h3_conn.unwrap(),
            recv_buf,
            ret_buf: vec![0; INITIAL_RESPONSE_SIZE],
            socket,
            recv_info,
            dgram: [0; MAX_DATAGRAM_SIZE],
            poll,
            events,
        })
    }

    pub fn send(
        &'a mut self,
        url: url::Url,
        method: &str,
        user_headers: Option<Vec<(&str, &str)>>,
        body: Option<&[u8]>,
    ) -> Result<&'a [u8], Box<dyn std::error::Error>> {
        let mut sent = false;

        let mut headers = vec![
            quiche::h3::Header::new(b":method", method.as_bytes()),
            quiche::h3::Header::new(b":scheme", url.scheme().as_bytes()),
            quiche::h3::Header::new(b":authority", url.host_str().unwrap().as_bytes()),
            quiche::h3::Header::new(b":path", url.path().as_bytes()),
            quiche::h3::Header::new(b"user-agent", b"quiche"),
        ];

        if let Some(user_headers) = user_headers {
            headers.append(
                &mut user_headers
                    .iter()
                    .map(|(k, v)| quiche::h3::Header::new(k.as_bytes(), v.as_bytes()))
                    .collect::<Vec<_>>(),
            )
        }

        let mut read_upto = 0;

        loop {
            self.poll.poll(&mut self.events, self.conn.timeout())?;

            if !sent {
                let sid = self
                    .h3_conn
                    .send_request(&mut self.conn, &headers, body.is_none())?;
                if let Some(body) = body {
                    self.h3_conn.send_body(&mut self.conn, sid, body, true)?;
                }
                sent = true;
            }

            'inner: loop {
                if self.events.is_empty() {
                    self.conn.on_timeout();
                    break 'inner;
                }

                let len = match self.socket.recv(&mut self.recv_buf) {
                    Ok(v) => v,
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            break 'inner;
                        }
                        return Err(Box::new(e));
                    }
                };

                let _ = self.conn.recv(&mut self.recv_buf[..len], self.recv_info);
            }

            loop {
                match self.h3_conn.poll(&mut self.conn) {
                    // todo: do something with headers
                    Ok((_stream_id, quiche::h3::Event::Headers { list: _, .. })) => {}
                    Ok((stream_id, quiche::h3::Event::Data)) => {
                        while let Ok(upto) =
                            self.h3_conn
                                .recv_body(&mut self.conn, stream_id, &mut self.recv_buf)
                        {
                            // expand buffer?
                            let bl = self.ret_buf.len();
                            if read_upto + upto > bl {
                                self.ret_buf.resize((bl + upto).max(bl * 2), 0);
                            }

                            self.recv_buf[..upto]
                                .iter()
                                .enumerate()
                                .for_each(|(i, v)| self.ret_buf[read_upto + i] = *v);

                            read_upto += upto;
                        }
                    }
                    Ok((_stream_id, quiche::h3::Event::Finished)) => {
                        return Ok(&self.ret_buf[..read_upto]);
                    }

                    // todo: handle these
                    Ok((_flow_id, quiche::h3::Event::Datagram)) => (),
                    Ok((_goaway_id, quiche::h3::Event::GoAway)) => (),

                    Err(quiche::h3::Error::Done) => {
                        break;
                    }
                    Err(e) => {
                        return Err(Box::new(e));
                    }
                    // all other codes --> keep polling
                    _ => {
                        break;
                    }
                }
            }

            loop {
                let (bw, _) = match self.conn.send(&mut self.dgram) {
                    Ok(v) => v,
                    Err(quiche::Error::Done) => {
                        break;
                    }
                    Err(e) => {
                        self.conn.close(false, 0x1, b"fail").ok();
                        return Err(Box::new(e));
                    }
                };

                if let Err(e) = self.socket.send(&self.dgram[..bw]) {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        break;
                    }
                    return Err(Box::new(e));
                }
            }
        }
    }
}
