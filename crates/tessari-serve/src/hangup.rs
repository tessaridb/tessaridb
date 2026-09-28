//! Whether the client at the far end of a feed has hung up.

use std::io::ErrorKind;
use std::net::TcpStream;

/// Whether the peer of a connection this side only writes to has closed it.
///
/// A feed learns of a hang-up at its next failed write, and on a quiet table
/// there is no next write — so a client that subscribed and left held its thread
/// and its place at the door until somebody else changed something. Asked once a
/// feed round, this is the question the socket can answer without a write.
///
/// End of stream, or a socket error other than "nothing yet", means gone. Bytes
/// waiting mean the peer is still there; what they mean is the protocol's call.
/// The probe borrows the descriptor's non-blocking mode for one `peek`, which
/// every duplicate of the socket shares — so it is asked only from the thread
/// that also writes, never while a write is under way.
#[must_use]
pub fn hung_up(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let mut probe = [0_u8; 1];
    let gone = match stream.peek(&mut probe) {
        Ok(0) => true,
        Ok(_) => false,
        Err(why) => !matches!(why.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted),
    };
    // A socket left non-blocking would fail the feed's next write with
    // `WouldBlock` — ending it anyway, but reported as a fault rather than a
    // hang-up. Failing to restore is therefore reported as gone.
    stream.set_nonblocking(false).is_err() || gone
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};

    use super::hung_up;

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let client =
            TcpStream::connect(listener.local_addr().expect("its address")).expect("a connection");
        let (served, _) = listener.accept().expect("the other end");
        (client, served)
    }

    #[test]
    fn a_peer_that_is_still_there_has_not_hung_up() {
        let (_client, served) = pair();
        assert!(!hung_up(&served));
        // And the socket is blocking again afterwards, or the feed's writes break.
        let mut writer = &served;
        writer
            .write_all(b"still writable")
            .expect("a blocking write");
    }

    #[test]
    fn a_peer_that_closed_its_side_has_hung_up() {
        let (client, served) = pair();
        drop(client);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !hung_up(&served) {
            assert!(
                std::time::Instant::now() < deadline,
                "the close never became visible"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn bytes_waiting_are_not_a_hang_up() {
        let (mut client, served) = pair();
        client.write_all(b"x").expect("a byte");
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!hung_up(&served));
    }
}
