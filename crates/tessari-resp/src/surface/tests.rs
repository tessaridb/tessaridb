//! The surface served on an in-memory store and spoken to over a socket:
//! commands as a client sends them, replies read back as a client reads them.

use std::sync::Arc;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use crate::codec::Reply;
use crate::surface::{Node, Space};

mod algebra;
mod carrying;
mod containers;
mod keys;
mod session;
mod strings;

/// A node over a fresh store holding the space `app.main.cache`, after
/// `setup` ran on it; connections start in that space.
async fn serving_with(setup: &str) -> (String, CancellationToken, Arc<tessaridb::Db>) {
    let db = Arc::new(tessaridb::Db::in_memory().await.unwrap());
    let mut session = db.session();
    session
        .run(
            "DEFINE NAMESPACE app; USE NAMESPACE app; DEFINE DATABASE main; \
             USE DATABASE main; DEFINE SPACE cache;",
        )
        .await
        .unwrap();
    if !setup.is_empty() {
        session.run(setup).await.unwrap();
    }
    drop(session);
    let node = Node::bind(Arc::clone(&db), "127.0.0.1:0")
        .unwrap()
        .serving_space(Space::parse("app.main.cache").unwrap());
    let address = node.address().unwrap();
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    tokio::spawn(async move { node.serve(stopping).await });
    (address, stop, db)
}

/// A connected client.
struct Client {
    stream: TcpStream,
    read: Vec<u8>,
}

impl Client {
    async fn to(address: &str) -> Self {
        Self {
            stream: TcpStream::connect(address).await.unwrap(),
            read: Vec::new(),
        }
    }

    /// Send `words` as one command, without reading its reply.
    async fn send(&mut self, words: &[&[u8]]) {
        let mut frame = format!("*{}\r\n", words.len()).into_bytes();
        for word in words {
            frame.extend_from_slice(format!("${}\r\n", word.len()).as_bytes());
            frame.extend_from_slice(word);
            frame.extend_from_slice(b"\r\n");
        }
        self.stream.write_all(&frame).await.unwrap();
    }

    /// Send a command and read its reply.
    async fn ask(&mut self, words: &[&str]) -> Reply {
        let words: Vec<&[u8]> = words.iter().map(|word| word.as_bytes()).collect();
        self.send(&words).await;
        self.reply().await
    }

    /// Read one reply.
    async fn reply(&mut self) -> Reply {
        loop {
            if let Some((reply, used)) = parse(&self.read) {
                self.read.drain(..used);
                return reply;
            }
            let mut chunk = [0_u8; 4096];
            let read = self.stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "the connection closed before a reply");
            self.read.extend_from_slice(&chunk[..read]);
        }
    }
}

/// One reply at the start of `bytes`, and how many bytes it took.
fn parse(bytes: &[u8]) -> Option<(Reply, usize)> {
    let end = bytes.windows(2).position(|pair| pair == b"\r\n")?;
    let line = std::str::from_utf8(&bytes[1..end]).unwrap();
    let after = end.saturating_add(2);
    let count = || line.parse::<i64>().unwrap();
    Some(match bytes[0] {
        b'+' => (Reply::Status(line.to_owned().into()), after),
        b'-' => (Reply::Error(line.to_owned().into()), after),
        b':' => (Reply::Integer(count()), after),
        b'_' => (Reply::Nil, after),
        b',' => (Reply::Double(line.as_bytes().to_vec()), after),
        b'$' if count() < 0 => (Reply::Nil, after),
        b'$' => {
            let length = usize::try_from(count()).unwrap();
            let end = after.saturating_add(length);
            let data = bytes.get(after..end)?.to_vec();
            bytes.get(end..end.saturating_add(2))?;
            (Reply::Bulk(data), end.saturating_add(2))
        }
        b'*' | b'%' => {
            let pairs = bytes[0] == b'%';
            let wanted =
                usize::try_from(count())
                    .unwrap()
                    .saturating_mul(if pairs { 2 } else { 1 });
            let mut items = Vec::with_capacity(wanted);
            let mut at = after;
            for _ in 0..wanted {
                let (item, used) = parse(&bytes[at..])?;
                items.push(item);
                at = at.saturating_add(used);
            }
            if pairs {
                let mut paired = Vec::new();
                let mut items = items.into_iter();
                while let (Some(key), Some(value)) = (items.next(), items.next()) {
                    paired.push((key, value));
                }
                (Reply::Map(paired), at)
            } else {
                (Reply::Array(items), at)
            }
        }
        other => (Reply::Error(format!("not a reply: {other}").into()), after),
    })
}

fn bulk(text: &str) -> Reply {
    Reply::text(text)
}

fn status(text: &'static str) -> Reply {
    Reply::Status(text.into())
}

fn error_starting(reply: &Reply, class: &str) -> bool {
    matches!(reply, Reply::Error(text) if text.starts_with(class))
}
