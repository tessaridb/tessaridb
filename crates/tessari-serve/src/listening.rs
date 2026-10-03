//! Binding a serving socket a restarted node can take straight back.
//!
//! A node killed while connections were open closes them from its own side,
//! so each one waits out `TIME_WAIT` on the node's port. Linux refuses to bind
//! a port any such socket is still on unless the new socket says
//! `SO_REUSEADDR`, so a node restarted after a crash could not listen again for
//! about a minute — the first thing a supervisor does after a crash, refused.
//! On Unix the option admits only that: a second live listener on the port is
//! still refused. Windows gives the option another meaning (taking a port
//! another process is listening on), so it is set on Unix alone.

use std::io;
use std::net::{TcpListener, ToSocketAddrs};

use socket2::{Domain, Protocol, Socket, Type};

/// How many finished handshakes wait to be accepted — the value the standard
/// library's own bind uses, so only the reuse changes.
const BACKLOG: i32 = 128;

/// Listen on `address` — every address it resolves to, in order, until one
/// binds — as `TcpListener::bind` does, and able to take the port back from a
/// killed process's connections still in `TIME_WAIT`.
///
/// # Errors
///
/// The last address's failure, or `InvalidInput` when `address` resolves to
/// nothing.
pub fn listen(address: impl ToSocketAddrs) -> io::Result<TcpListener> {
    let mut last = io::Error::new(
        io::ErrorKind::InvalidInput,
        "the address resolves to nothing to listen on",
    );
    for candidate in address.to_socket_addrs()? {
        match bound(candidate) {
            Ok(listener) => return Ok(listener),
            Err(why) => last = why,
        }
    }
    Err(last)
}

/// One address, bound and listening.
fn bound(address: std::net::SocketAddr) -> io::Result<TcpListener> {
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.bind(&address.into())?;
    socket.listen(BACKLOG)?;
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpStream};

    use super::listen;

    #[test]
    fn a_port_left_in_time_wait_by_a_closed_server_is_bound_again() {
        // The node closes first, as a killed process does, so its side of the
        // connection waits out TIME_WAIT on the port.
        let first = listen("127.0.0.1:0").expect("a port");
        let address = first.local_addr().expect("its address");
        let mut client = TcpStream::connect(address).expect("a connection");
        let (mut served, _) = first.accept().expect("accepted");
        served.write_all(b"x").expect("written");
        served
            .shutdown(Shutdown::Both)
            .expect("closed by the server");
        drop(served);
        let mut byte = [0_u8; 1];
        let _ = client.read(&mut byte);
        let _ = client.read(&mut byte);
        drop(client);
        drop(first);
        let again = listen(address);
        assert!(
            again.is_ok(),
            "the port could not be bound again: {again:?}"
        );
    }

    #[test]
    fn a_port_a_live_listener_holds_is_still_refused() {
        let held = listen("127.0.0.1:0").expect("a port");
        let address = held.local_addr().expect("its address");
        assert!(listen(address).is_err(), "two live listeners on one port");
    }
}
