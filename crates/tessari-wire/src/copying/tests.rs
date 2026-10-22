//! The frames a copy carries a topic's entries in, and the end that counts them.

use tessari_encoding::{TopicMark, TopicOffsetKey};
use tessari_storage::TopicEntry;
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

use super::{end, entries_body, entries_in};
use crate::frame;

fn entry(offset: u64, id: &str, appended: Option<u64>) -> TopicEntry {
    TopicEntry {
        key: TopicOffsetKey {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(2),
            table: TableId::new(3),
            offset,
            id: RecordId::Text(id.to_owned()),
        },
        mark: TopicMark { appended },
    }
}

#[test]
fn an_entries_frame_reads_back_what_was_written() {
    let page = vec![
        entry(7, "zulu", Some(1_792_645_972_731)),
        entry(8, "alpha", None),
    ];
    assert_eq!(
        entries_in(&entries_body(&page)).expect("the frame reads back"),
        page
    );
    assert!(entries_in(&entries_body(&page)[..9]).is_err());
}

#[test]
fn an_end_counts_the_entries_only_when_the_leader_sent_them() {
    let mut earlier = Vec::new();
    frame::put_u64(&mut earlier, 4);
    frame::put_u32(&mut earlier, 0);
    let (records, topics, entries) = end(&earlier).expect("the frame reads back");
    assert_eq!((records, topics.len(), entries), (4, 0, None));
    frame::put_u64(&mut earlier, 9);
    assert_eq!(end(&earlier).expect("the frame reads back").2, Some(9));
}
