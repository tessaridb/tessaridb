//! A Kafka request as a client frames it: the header, then the body of every
//! api and version a connection would read it for, and the record batches a
//! produce carries.
#![no_main]

use tessari_kafka::api::{
    self, committed, coordinator, fetch, groups, init_producer, join_group, list_offsets,
    membership, metadata, produce, sasl, topics,
};
use tessari_kafka::codec::Reader;
use tessari_kafka::header::RequestHeader;
use tessari_kafka::records::decode_batches;

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let mut reader = Reader::new(data);
    let Ok(header) = RequestHeader::read(&mut reader) else {
        return;
    };
    // A connection reads a body only for an api and version it serves.
    if !api::served(header.key).is_some_and(|served| served.serves(header.version)) {
        return;
    }
    let (version, flexible) = (header.version, header.flexible);
    let reader = &mut reader;
    match header.key {
        0 => {
            if let Ok(request) = produce::Request::read(reader, flexible) {
                for partition in request.topics.iter().flat_map(|topic| &topic.partitions) {
                    if let Some(records) = partition.records {
                        drop(decode_batches(records));
                    }
                }
            }
        }
        1 => drop(fetch::Request::read(reader, version, flexible)),
        2 => drop(list_offsets::read_request(reader, version, flexible)),
        3 => drop(metadata::Request::read(reader, version, flexible)),
        8 => drop(committed::CommitRequest::read(reader, version, flexible)),
        9 => drop(committed::FetchRequest::read(reader, version, flexible)),
        10 => drop(coordinator::Request::read(reader, version, flexible)),
        11 => drop(join_group::Request::read(reader, version, flexible)),
        12 => drop(membership::read_heartbeat(reader, version, flexible)),
        13 => drop(membership::Leave::read(reader, version, flexible)),
        14 => drop(membership::Sync::read(reader, version, flexible)),
        15 => drop(groups::read_describe(reader, version, flexible)),
        16 => drop(groups::read_list(reader, version, flexible)),
        17 => drop(sasl::read_handshake(reader)),
        19 => drop(topics::CreateRequest::read(reader, flexible)),
        20 => drop(topics::read_delete(reader, flexible)),
        22 => drop(init_producer::read_request(reader, version, flexible)),
        36 => {
            if let Ok(token) = sasl::read_authenticate(reader, flexible) {
                drop(sasl::Plain::read(token));
            }
        }
        42 => drop(groups::read_delete(reader, flexible)),
        _ => {}
    }
});
