//! ApiVersions as a 3.9 client meets it.

use crate::api::{SERVED, versions};
use crate::codec::{Reader, Writer};
use crate::header::{RequestHeader, write_response_header};

#[test]
fn a_client_asking_a_newer_version_is_told_the_table_at_version_zero() {
    let frame = include_bytes!("../../fixtures/api-versions-v4-request.bin");
    let mut reader = Reader::new(frame);
    let header = RequestHeader::read(&mut reader).unwrap();
    assert_eq!((header.key, header.version), (18, 4));
    assert!(
        !header.flexible,
        "a version this node does not speak is read as classic"
    );
    let mut writer = Writer::new();
    write_response_header(&mut writer, &header);
    versions::answer(&mut writer, header.version).unwrap();
    let bytes = writer.into_bytes();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.i32("correlation").unwrap(), header.correlation);
    assert_eq!(reader.i16("error").unwrap(), versions::UNSUPPORTED_VERSION);
    let count = reader.array_len(false, "keys").unwrap().unwrap();
    assert_eq!(count, SERVED.len());
    for api in SERVED {
        assert_eq!(
            (
                reader.i16("k").unwrap(),
                reader.i16("min").unwrap(),
                reader.i16("max").unwrap()
            ),
            (api.key, api.min, api.max)
        );
    }
    assert_eq!(
        reader.remaining(),
        0,
        "version 0 carries no throttle and no tags"
    );
}

#[test]
fn version_three_is_flexible_and_carries_the_throttle() {
    let mut writer = Writer::new();
    versions::answer(&mut writer, 3).unwrap();
    let bytes = writer.into_bytes();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.i16("error").unwrap(), 0);
    let count = reader.array_len(true, "keys").unwrap().unwrap();
    for _ in 0..count {
        reader.take(6, "entry").unwrap();
        reader.tagged_fields("entry").unwrap();
    }
    assert_eq!(reader.i32("throttle").unwrap(), 0);
    reader.tagged_fields("body").unwrap();
    assert_eq!(reader.remaining(), 0);
}
