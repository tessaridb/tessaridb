//! The hash, list, set and sorted-set commands, each a structure statement
//! on the store's own space (G079 C6).

use super::keys::{array, bytes_of};
use super::{Client, bulk, error_starting, serving_with, status};
use crate::codec::Reply;

fn texts(items: &[&str]) -> Reply {
    Reply::Array(items.iter().map(|item| bulk(item)).collect())
}

fn wrong_type() -> Reply {
    Reply::error("WRONGTYPE Operation against a key holding the wrong kind of value")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hash_is_fields_on_one_key() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    assert_eq!(
        client.ask(&["HSET", "h", "a", "1", "b", "2"]).await,
        Reply::Integer(2)
    );
    assert_eq!(
        client.ask(&["HSET", "h", "a", "3"]).await,
        Reply::Integer(0)
    );
    assert_eq!(client.ask(&["HGET", "h", "a"]).await, bulk("3"));
    assert_eq!(client.ask(&["HGET", "h", "zz"]).await, Reply::Nil);
    assert_eq!(
        client.ask(&["HMGET", "h", "a", "zz"]).await,
        Reply::Array(vec![bulk("3"), Reply::Nil])
    );
    assert_eq!(
        client.ask(&["HSETNX", "h", "a", "9"]).await,
        Reply::Integer(0)
    );
    assert_eq!(
        client.ask(&["HSETNX", "h", "c", "9"]).await,
        Reply::Integer(1)
    );
    assert_eq!(
        client.ask(&["HGETALL", "h"]).await,
        texts(&["a", "3", "b", "2", "c", "9"])
    );
    assert_eq!(client.ask(&["HLEN", "h"]).await, Reply::Integer(3));
    assert_eq!(client.ask(&["HEXISTS", "h", "b"]).await, Reply::Integer(1));
    assert_eq!(
        client.ask(&["HDEL", "h", "a", "zz"]).await,
        Reply::Integer(1)
    );
    assert_eq!(client.ask(&["HKEYS", "h"]).await, texts(&["b", "c"]));
    assert_eq!(client.ask(&["HVALS", "h"]).await, texts(&["2", "9"]));
    assert_eq!(
        client.ask(&["HINCRBY", "h", "b", "40"]).await,
        Reply::Integer(42)
    );
    assert_eq!(
        client.ask(&["HINCRBY", "h", "n", "-5"]).await,
        Reply::Integer(-5)
    );
    assert_eq!(
        client.ask(&["HINCRBYFLOAT", "h", "f", "1.5"]).await,
        bulk("1.5")
    );
    assert_eq!(client.ask(&["HSTRLEN", "h", "b"]).await, Reply::Integer(2));
    assert_eq!(client.ask(&["TYPE", "h"]).await, status("hash"));
    // The last field gone takes the key with it.
    client.ask(&["HDEL", "h", "b", "c", "n", "f"]).await;
    assert_eq!(client.ask(&["EXISTS", "h"]).await, Reply::Integer(0));
    assert_eq!(
        client.ask(&["HGETALL", "h"]).await,
        Reply::Array(Vec::new())
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_list_pushes_pops_and_reads_by_position() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    assert_eq!(
        client.ask(&["RPUSH", "l", "b", "c"]).await,
        Reply::Integer(2)
    );
    assert_eq!(client.ask(&["LPUSH", "l", "a"]).await, Reply::Integer(3));
    assert_eq!(
        client.ask(&["LPUSHX", "none", "a"]).await,
        Reply::Integer(0)
    );
    assert_eq!(client.ask(&["EXISTS", "none"]).await, Reply::Integer(0));
    assert_eq!(
        client.ask(&["LRANGE", "l", "0", "-1"]).await,
        texts(&["a", "b", "c"])
    );
    assert_eq!(
        client.ask(&["LRANGE", "l", "-2", "99"]).await,
        texts(&["b", "c"])
    );
    assert_eq!(client.ask(&["LINDEX", "l", "-1"]).await, bulk("c"));
    assert_eq!(client.ask(&["LINDEX", "l", "9"]).await, Reply::Nil);
    assert_eq!(client.ask(&["LLEN", "l"]).await, Reply::Integer(3));
    assert_eq!(client.ask(&["LSET", "l", "1", "B"]).await, Reply::OK);
    assert_eq!(
        client.ask(&["LSET", "l", "9", "x"]).await,
        Reply::error("ERR index out of range")
    );
    assert_eq!(
        client.ask(&["LSET", "none", "0", "x"]).await,
        Reply::error("ERR no such key")
    );
    assert_eq!(
        client.ask(&["LINSERT", "l", "BEFORE", "c", "x"]).await,
        Reply::Integer(4)
    );
    assert_eq!(
        client.ask(&["RPUSH", "l", "x", "x"]).await,
        Reply::Integer(6)
    );
    // A negative count removes from the back.
    assert_eq!(
        client.ask(&["LREM", "l", "-2", "x"]).await,
        Reply::Integer(2)
    );
    assert_eq!(
        client.ask(&["LRANGE", "l", "0", "-1"]).await,
        texts(&["a", "B", "x", "c"])
    );
    assert_eq!(client.ask(&["LPOS", "l", "c"]).await, Reply::Integer(3));
    assert_eq!(client.ask(&["LPOP", "l"]).await, bulk("a"));
    assert_eq!(client.ask(&["RPOP", "l", "2"]).await, texts(&["c", "x"]));
    assert_eq!(
        client.ask(&["LMOVE", "l", "m", "LEFT", "RIGHT"]).await,
        bulk("B")
    );
    assert_eq!(client.ask(&["EXISTS", "l"]).await, Reply::Integer(0));
    assert_eq!(client.ask(&["LRANGE", "m", "0", "-1"]).await, texts(&["B"]));
    assert_eq!(client.ask(&["LPOP", "l"]).await, Reply::Nil);
    assert_eq!(client.ask(&["TYPE", "m"]).await, status("list"));
    client.ask(&["RPUSH", "m", "1", "2"]).await;
    assert_eq!(client.ask(&["LTRIM", "m", "1", "-1"]).await, Reply::OK);
    assert_eq!(
        client.ask(&["LRANGE", "m", "0", "-1"]).await,
        texts(&["1", "2"])
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_set_holds_each_member_once_and_combines_with_others() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    assert_eq!(
        client.ask(&["SADD", "s", "a", "b", "a", "c"]).await,
        Reply::Integer(3)
    );
    assert_eq!(client.ask(&["SADD", "s", "a"]).await, Reply::Integer(0));
    assert_eq!(client.ask(&["SCARD", "s"]).await, Reply::Integer(3));
    assert_eq!(
        client.ask(&["SISMEMBER", "s", "b"]).await,
        Reply::Integer(1)
    );
    assert_eq!(
        client.ask(&["SMISMEMBER", "s", "b", "z"]).await,
        Reply::Array(vec![Reply::Integer(1), Reply::Integer(0)])
    );
    client.ask(&["SADD", "t", "b", "c", "d"]).await;
    assert_eq!(client.ask(&["SINTER", "s", "t"]).await, texts(&["b", "c"]));
    assert_eq!(
        client.ask(&["SUNION", "s", "t"]).await,
        texts(&["a", "b", "c", "d"])
    );
    assert_eq!(client.ask(&["SDIFF", "s", "t"]).await, texts(&["a"]));
    assert_eq!(
        client
            .ask(&["SINTERCARD", "2", "s", "t", "LIMIT", "1"])
            .await,
        Reply::Integer(1)
    );
    assert_eq!(
        client.ask(&["SMOVE", "s", "t", "a"]).await,
        Reply::Integer(1)
    );
    assert_eq!(
        client.ask(&["SMOVE", "s", "t", "a"]).await,
        Reply::Integer(0)
    );
    assert_eq!(client.ask(&["SMEMBERS", "s"]).await, texts(&["b", "c"]));
    assert_eq!(
        client.ask(&["SREM", "s", "b", "z"]).await,
        Reply::Integer(1)
    );
    let popped = client.ask(&["SPOP", "s"]).await;
    assert_eq!(popped, bulk("c"));
    assert_eq!(client.ask(&["EXISTS", "s"]).await, Reply::Integer(0));
    assert_eq!(client.ask(&["TYPE", "t"]).await, status("set"));
    let drawn = client.ask(&["SRANDMEMBER", "t", "-5"]).await;
    assert!(
        matches!(&drawn, Reply::Array(items) if items.len() == 5),
        "{drawn:?}"
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sorted_set_orders_members_by_score() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    assert_eq!(
        client
            .ask(&["ZADD", "z", "1", "a", "2", "b", "3", "c"])
            .await,
        Reply::Integer(3)
    );
    assert_eq!(
        client.ask(&["ZADD", "z", "NX", "9", "a"]).await,
        Reply::Integer(0)
    );
    assert_eq!(client.ask(&["ZSCORE", "z", "a"]).await, bulk("1"));
    assert_eq!(
        client
            .ask(&["ZADD", "z", "GT", "CH", "0", "a", "5", "b"])
            .await,
        Reply::Integer(1)
    );
    assert_eq!(client.ask(&["ZINCRBY", "z", "1.5", "a"]).await, bulk("2.5"));
    assert_eq!(
        client.ask(&["ZRANGE", "z", "0", "-1", "WITHSCORES"]).await,
        texts(&["a", "2.5", "c", "3", "b", "5"])
    );
    assert_eq!(
        client
            .ask(&[
                "ZRANGE", "z", "+inf", "(2.5", "BYSCORE", "REV", "LIMIT", "0", "1"
            ])
            .await,
        texts(&["b"])
    );
    assert_eq!(
        client.ask(&["ZRANGEBYSCORE", "z", "-inf", "3"]).await,
        texts(&["a", "c"])
    );
    assert_eq!(
        client.ask(&["ZCOUNT", "z", "(2.5", "5"]).await,
        Reply::Integer(2)
    );
    assert_eq!(client.ask(&["ZRANK", "z", "c"]).await, Reply::Integer(1));
    assert_eq!(client.ask(&["ZREVRANK", "z", "c"]).await, Reply::Integer(1));
    assert_eq!(client.ask(&["ZRANK", "z", "zz"]).await, Reply::Nil);
    assert_eq!(client.ask(&["ZCARD", "z"]).await, Reply::Integer(3));
    assert_eq!(client.ask(&["ZPOPMIN", "z"]).await, texts(&["a", "2.5"]));
    assert_eq!(
        client.ask(&["ZPOPMAX", "z", "5"]).await,
        texts(&["b", "5", "c", "3"])
    );
    assert_eq!(client.ask(&["EXISTS", "z"]).await, Reply::Integer(0));
    client.ask(&["ZADD", "z", "1", "x"]).await;
    assert_eq!(client.ask(&["TYPE", "z"]).await, status("zset"));
    assert!(error_starting(
        &client.ask(&["ZADD", "z", "high", "x"]).await,
        "ERR value is not a valid float"
    ));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_against_another_kind_is_wrongtype() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client.ask(&["SET", "str", "v"]).await;
    client.ask(&["RPUSH", "list", "v"]).await;
    client.ask(&["SADD", "set", "v"]).await;
    client.ask(&["HSET", "hash", "f", "v"]).await;
    client.ask(&["ZADD", "zset", "1", "v"]).await;
    assert_eq!(client.ask(&["LPUSH", "str", "x"]).await, wrong_type());
    assert_eq!(client.ask(&["GET", "list"]).await, wrong_type());
    assert_eq!(client.ask(&["HGET", "list", "f"]).await, wrong_type());
    assert_eq!(client.ask(&["SADD", "hash", "x"]).await, wrong_type());
    assert_eq!(client.ask(&["SMEMBERS", "zset"]).await, wrong_type());
    assert_eq!(client.ask(&["ZADD", "set", "1", "x"]).await, wrong_type());
    assert_eq!(client.ask(&["ZSCORE", "set", "v"]).await, wrong_type());
    assert_eq!(
        client.ask(&["LRANGE", "hash", "0", "-1"]).await,
        wrong_type()
    );
    // Nothing was changed by a refused write.
    assert_eq!(client.ask(&["GET", "str"]).await, bulk("v"));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_by_type_lists_only_keys_of_that_kind() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client.ask(&["SET", "s1", "v"]).await;
    client.ask(&["RPUSH", "l1", "v"]).await;
    client.ask(&["RPUSH", "l2", "v"]).await;
    client.ask(&["HSET", "h1", "f", "v"]).await;
    client.ask(&["ZADD", "z1", "1", "v"]).await;
    for (kind, expected) in [
        ("list", vec!["l1", "l2"]),
        ("string", vec!["s1"]),
        ("hash", vec!["h1"]),
        ("zset", vec!["z1"]),
        ("set", vec![]),
    ] {
        let mut seen = Vec::new();
        let mut cursor = "0".to_owned();
        loop {
            let mut parts = array(
                client
                    .ask(&["SCAN", &cursor, "TYPE", kind, "COUNT", "2"])
                    .await,
            )
            .into_iter();
            cursor = String::from_utf8(bytes_of(parts.next().unwrap())).unwrap();
            for key in array(parts.next().unwrap()) {
                seen.push(String::from_utf8(bytes_of(key)).unwrap());
            }
            if cursor == "0" {
                break;
            }
        }
        seen.sort();
        assert_eq!(seen, expected, "{kind}");
    }
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writers_to_one_hash_and_one_list_lose_nothing() {
    let (address, stop, _) = serving_with("").await;
    let mut writers = Vec::new();
    for writer in 0..8 {
        let address = address.clone();
        writers.push(tokio::spawn(async move {
            let mut client = Client::to(&address).await;
            for _ in 0..25 {
                let counted = client.ask(&["HINCRBY", "h", "n", "1"]).await;
                assert!(
                    matches!(counted, Reply::Integer(_)),
                    "{writer}: {counted:?}"
                );
                let pushed = client.ask(&["RPUSH", "l", &writer.to_string()]).await;
                assert!(matches!(pushed, Reply::Integer(_)), "{writer}: {pushed:?}");
                let added = client.ask(&["SADD", "s", &format!("{writer}")]).await;
                assert!(matches!(added, Reply::Integer(_)), "{writer}: {added:?}");
            }
        }));
    }
    for writer in writers {
        writer.await.unwrap();
    }
    let mut client = Client::to(&address).await;
    assert_eq!(client.ask(&["HGET", "h", "n"]).await, bulk("200"));
    assert_eq!(client.ask(&["LLEN", "l"]).await, Reply::Integer(200));
    assert_eq!(client.ask(&["SCARD", "s"]).await, Reply::Integer(8));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn in_resp3_a_score_is_a_double_and_scored_members_are_pairs() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client.ask(&["HELLO", "3"]).await;
    client
        .ask(&["ZADD", "z", "1", "a", "2.5", "b", "3", "c"])
        .await;
    let pair =
        |member: &str, score: &str| Reply::Array(vec![bulk(member), Reply::Double(score.into())]);
    assert_eq!(
        client.ask(&["ZSCORE", "z", "b"]).await,
        Reply::Double("2.5".into())
    );
    assert_eq!(
        client.ask(&["ZRANGE", "z", "0", "1", "WITHSCORES"]).await,
        Reply::Array(vec![pair("a", "1"), pair("b", "2.5")])
    );
    assert_eq!(
        client.ask(&["ZRANK", "z", "b", "WITHSCORE"]).await,
        Reply::Array(vec![Reply::Integer(1), Reply::Double("2.5".into())])
    );
    // A pop with no count answers one pair, flat; with a count, pairs.
    assert_eq!(
        client.ask(&["ZPOPMIN", "z"]).await,
        Reply::Array(vec![bulk("a"), Reply::Double("1".into())])
    );
    assert_eq!(
        client.ask(&["ZPOPMAX", "z", "1"]).await,
        Reply::Array(vec![pair("c", "3")])
    );
    assert_eq!(
        client.ask(&["ZRANDMEMBER", "z", "1", "WITHSCORES"]).await,
        Reply::Array(vec![pair("b", "2.5")])
    );
    client.ask(&["HSET", "h", "f", "v"]).await;
    assert_eq!(
        client.ask(&["HRANDFIELD", "h", "1", "WITHVALUES"]).await,
        Reply::Array(vec![Reply::Array(vec![bulk("f"), bulk("v")])])
    );
    stop.cancel();
}
