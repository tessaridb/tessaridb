//! Round-trips and refusals of the codec, in both directions.
//!
//! Every `expect` here carries the fixture invariant: the bytes under test are
//! written by the matching encoder of this same file, so a failure is the
//! test's own bug, not a case to handle.

use super::write as out;
use super::{First, Frontend, PROTOCOL_VERSION, read};

fn len_of(frame: &[u8]) -> usize {
    let word: [u8; 4] = frame[..4].try_into().expect("a frame has its length word");
    usize::try_from(u32::from_be_bytes(word)).expect("u32 fits usize on this host")
}

fn framed(body: Vec<u8>) -> Vec<u8> {
    let mut frame = Vec::new();
    let len = u32::try_from(body.len().saturating_add(4)).expect("a test frame is far below u32");
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

fn startup(user: &str, database: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    for (key, value) in [
        ("user", user),
        ("database", database),
        ("application_name", "psql"),
    ] {
        body.extend_from_slice(key.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);
    framed(body)
}

#[test]
fn a_startup_packet_reads_its_version_and_pairs() {
    let frame = startup("app", "shop.main");
    let len = len_of(&frame);
    let decoded = read::first(&frame[4..len]).expect("the fixture startup decodes");
    let (version, pairs) = match decoded {
        First::Startup { version, pairs } => (version, pairs),
        First::Ssl => {
            assert_eq!(0, 1, "the fixture is a startup packet, not an SSLRequest");
            (0, Vec::new())
        }
        First::Cancel { .. } => {
            assert_eq!(0, 1, "the fixture is a startup packet, not a cancel");
            (0, Vec::new())
        }
    };
    assert_eq!(version, PROTOCOL_VERSION);
    assert_eq!(
        pairs,
        vec![
            ("user".to_owned(), "app".to_owned()),
            ("database".to_owned(), "shop.main".to_owned()),
            ("application_name".to_owned(), "psql".to_owned()),
        ]
    );
}

#[test]
fn an_ssl_request_is_recognized_by_its_code() {
    let frame = framed(super::SSL_REQUEST.to_be_bytes().to_vec());
    assert!(matches!(
        read::first(&frame[4..]).expect("the fixture ssl request decodes"),
        First::Ssl
    ));
}

#[test]
fn a_cancel_request_carries_pid_and_key() {
    let mut body = Vec::new();
    body.extend_from_slice(&super::CANCEL_REQUEST.to_be_bytes());
    body.extend_from_slice(&7_u32.to_be_bytes());
    body.extend_from_slice(&99_u32.to_be_bytes());
    let frame = framed(body);
    assert!(matches!(
        read::first(&frame[4..]).expect("the fixture cancel decodes"),
        First::Cancel { pid: 7, key: 99 }
    ));
}

#[test]
fn a_query_round_trips_as_text() {
    let frame = out::command_complete("SELECT 2");
    assert_eq!(frame[0], b'C');
    let decoded = read::message(b'Q', b"SELECT 1;\0").expect("the fixture query decodes");
    assert!(matches!(decoded, Frontend::Query(text) if text == "SELECT 1;"));
}

#[test]
fn an_unspoken_message_keeps_its_kind_byte() {
    let decoded = read::message(b'P', b"\0stmt\0\0\0\0").expect("the fixture parse decodes");
    assert!(matches!(decoded, Frontend::Unspoken(b'P')));
}

#[test]
fn truncation_refuses_every_decoder() {
    let frame = startup("app", "shop.main");
    let len = len_of(&frame);
    assert!(read::first(&frame[4..len - 3]).is_err());
    assert!(read::message(b'Q', b"unterminated").is_err());
}

#[test]
fn trailing_bytes_refuse() {
    assert!(read::message(b'S', b"extra").is_err());
}

#[test]
fn error_fields_carry_code_message_and_detail_in_order() {
    let frame = out::error("42601", "syntax error", Some("at or near \"fro\""));
    assert_eq!(frame[0], b'E');
    let body = &frame[5..];
    let text = String::from_utf8_lossy(body);
    let s = text.find('S').expect("the severity field is present");
    let v = text
        .find('V')
        .expect("the unlocalized severity field is present");
    let c = text.find('C').expect("the code field is present");
    let m = text.find('M').expect("the message field is present");
    let d = text.find('D').expect("the detail field is present");
    assert!(s < v && v < c && c < m && m < d);
    assert!(text.contains("42601\0"), "{text:?}");
    assert!(text.contains("syntax error\0"), "{text:?}");
}

#[test]
fn ready_for_query_spells_the_three_states() {
    assert_eq!(
        out::ready_for_query(out::Tx::Idle),
        [b'Z', 0, 0, 0, 5, b'I']
    );
    let in_tx = out::ready_for_query(out::Tx::InTx);
    assert_eq!(*in_tx.last().expect("ready_for_query has a body"), b'T');
    let failed = out::ready_for_query(out::Tx::Failed);
    assert_eq!(*failed.last().expect("ready_for_query has a body"), b'E');
}

#[test]
fn data_row_marks_null_with_minus_one() {
    let frame = out::data_row(&[Some("42".to_owned()), None]);
    let body = &frame[5..];
    let count: [u8; 2] = body[..2].try_into().expect("two count bytes");
    assert_eq!(u16::from_be_bytes(count), 2);
    let first_len = i32::from_be_bytes(body[2..6].try_into().expect("four length bytes"));
    assert_eq!(first_len, 2);
    let at = 6 + usize::try_from(first_len).expect("a positive length fits usize");
    let second_len = i32::from_be_bytes(body[at..at + 4].try_into().expect("four length bytes"));
    assert_eq!(second_len, -1);
}

#[test]
fn row_description_answers_text_format_columns() {
    let frame = out::row_description(&[out::Column {
        name: "id".to_owned(),
        type_oid: 20,
        type_len: 8,
    }]);
    assert_eq!(frame[0], b'T');
    let body = &frame[5..];
    let count: [u8; 2] = body[..2].try_into().expect("two count bytes");
    assert_eq!(u16::from_be_bytes(count), 1);
    let text = String::from_utf8_lossy(&body[2..]);
    assert!(text.starts_with("id\0"));
    // format code 0 (text) is the last word of the column.
    assert!(body.ends_with(&0_u16.to_be_bytes()));
}
