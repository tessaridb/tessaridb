//! A split topic served as its partitions (ADR-0131): Metadata names them, a
//! produce lands in the partition the client chose, a fetch, a list of offsets
//! and a commit each address one.

use tokio::net::TcpStream;

use super::admin::create;
use super::groups::commit_at;
use super::{ask, fetch_body_at, fetched, produce_body_at, produced, serving_with};
use crate::codec::{Reader, Writer};
use crate::records::{Outgoing, decode_batches, encode_batch};
use crate::surface::errors;

/// How many partitions Metadata v9 names for `topic`.
async fn partitions(stream: &mut TcpStream, topic: &str) -> usize {
    let mut body = Writer::new();
    body.array_len(true, Some(1)).unwrap();
    body.string(true, topic).unwrap();
    body.no_tagged_fields();
    body.bool(false);
    body.bool(false);
    body.bool(false);
    body.no_tagged_fields();
    let answer = ask(stream, 3, 9, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    let count = reader.array_len(true, "brokers").unwrap().unwrap();
    for _ in 0..count {
        reader.i32("node").unwrap();
        reader.string(true, "host").unwrap();
        reader.i32("port").unwrap();
        reader.nullable_string(true, "rack").unwrap();
        reader.tagged_fields("broker").unwrap();
    }
    reader.nullable_string(true, "cluster").unwrap();
    reader.i32("controller").unwrap();
    reader.array_len(true, "topics").unwrap();
    assert_eq!(reader.i16("error").unwrap(), 0);
    reader.string(true, "name").unwrap();
    reader.bool("internal").unwrap();
    let count = reader.array_len(true, "partitions").unwrap().unwrap();
    for at in 0..count {
        assert_eq!(reader.i16("error").unwrap(), 0);
        assert_eq!(reader.i32("index").unwrap(), i32::try_from(at).unwrap());
        reader.i32("leader").unwrap();
        reader.i32("epoch").unwrap();
        for list in ["replicas", "isr"] {
            let count = reader.array_len(true, list).unwrap().unwrap();
            for _ in 0..count {
                reader.i32(list).unwrap();
            }
        }
        reader.array_len(true, "offline").unwrap();
        reader.tagged_fields("partition").unwrap();
    }
    count
}

/// The committed offset and code OffsetFetch v7 answers for each partition.
async fn committed_at(
    stream: &mut TcpStream,
    group: &str,
    topic: &str,
    indexes: &[i32],
) -> Vec<(i32, i64, i16)> {
    let mut body = Writer::new();
    body.string(true, group).unwrap();
    body.array_len(true, Some(1)).unwrap();
    body.string(true, topic).unwrap();
    body.array_len(true, Some(indexes.len())).unwrap();
    for index in indexes {
        body.i32(*index);
    }
    body.no_tagged_fields();
    body.bool(false);
    body.no_tagged_fields();
    let answer = ask(stream, 9, 7, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    reader.array_len(true, "topics").unwrap();
    reader.string(true, "name").unwrap();
    let count = reader.array_len(true, "partitions").unwrap().unwrap();
    let mut found = Vec::new();
    for _ in 0..count {
        let index = reader.i32("index").unwrap();
        let offset = reader.i64("offset").unwrap();
        reader.i32("epoch").unwrap();
        reader.nullable_string(true, "metadata").unwrap();
        let code = reader.i16("error").unwrap();
        reader.tagged_fields("partition").unwrap();
        found.push((index, offset, code));
    }
    found
}

fn batch(values: &[&[u8]]) -> Vec<u8> {
    let outgoing: Vec<Outgoing<'_>> = values
        .iter()
        .map(|value| Outgoing {
            timestamp: 1_760_000_000_000,
            key: None,
            value: Some(value),
            headers: &[],
        })
        .collect();
    let mut batch = Writer::new();
    encode_batch(&mut batch, 0, &outgoing).unwrap();
    batch.into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_split_topic_is_served_as_its_partitions() {
    let (address, stop, _db) = serving_with("DEFINE TOPIC parts PARTITIONS 3;").await;
    let mut stream = TcpStream::connect(&address).await.unwrap();
    assert_eq!(partitions(&mut stream, "parts").await, 3);
    assert_eq!(partitions(&mut stream, "orders").await, 1);
    // Where the client chose, not where the store would have: two batches into
    // partition 2, one into partition 0, offsets counted per partition.
    let into = |partition, values: &[&[u8]]| produce_body_at("parts", partition, &batch(values));
    assert_eq!(
        produced(&ask(&mut stream, 0, 9, true, &into(2, &[b"a", b"b"])).await),
        (0, 0)
    );
    assert_eq!(
        produced(&ask(&mut stream, 0, 9, true, &into(0, &[b"c"])).await),
        (0, 0)
    );
    assert_eq!(
        produced(&ask(&mut stream, 0, 9, true, &into(2, &[b"d"])).await),
        (0, 2)
    );
    assert_eq!(
        produced(&ask(&mut stream, 0, 9, true, &into(3, &[b"e"])).await).0,
        errors::UNKNOWN_TOPIC_OR_PARTITION
    );
    let (error, high, records) =
        fetched(&ask(&mut stream, 1, 12, true, &fetch_body_at("parts", 2, 1)).await);
    assert_eq!((error, high), (0, 3));
    let values: Vec<Vec<u8>> = decode_batches(&records)
        .unwrap()
        .iter()
        .flat_map(|batch| {
            batch
                .records
                .iter()
                .filter_map(|record| record.value.clone())
        })
        .collect();
    assert_eq!(values, vec![b"b".to_vec(), b"d".to_vec()]);
    let (error, high, _) =
        fetched(&ask(&mut stream, 1, 12, true, &fetch_body_at("parts", 1, 0)).await);
    assert_eq!((error, high), (0, 0));
    assert_eq!(
        fetched(&ask(&mut stream, 1, 12, true, &fetch_body_at("parts", 3, 0)).await).0,
        errors::UNKNOWN_TOPIC_OR_PARTITION
    );
    // A client creating a topic of four partitions gets four.
    assert_eq!(create(&mut stream, "made", 4, &[], false).await, 0);
    assert_eq!(partitions(&mut stream, "made").await, 4);
    assert_eq!(
        create(&mut stream, "huge", 2_000, &[], false).await,
        errors::INVALID_PARTITIONS
    );
    // A commit is the partition's own.
    assert_eq!(
        commit_at(&mut stream, ("readers", -1, ""), ("parts", 2), 2).await,
        0
    );
    assert_eq!(
        committed_at(&mut stream, "readers", "parts", &[0, 2]).await,
        vec![(0, -1, 0), (2, 2, 0)]
    );
    stop.cancel();
}
