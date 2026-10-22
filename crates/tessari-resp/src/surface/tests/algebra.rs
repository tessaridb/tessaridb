//! Sorted-set ranges by member, set algebra and its stores, container scans,
//! pops from the first of several keys, and `WATCH` (G079 C6).

use super::keys::{array, bytes_of};
use super::{Client, bulk, serving_with, status};
use crate::codec::Reply;

fn texts(items: &[&str]) -> Reply {
    Reply::Array(items.iter().map(|item| bulk(item)).collect())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ranking_of_equal_scores_reads_by_member() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client
        .ask(&["ZADD", "z", "0", "a", "0", "b", "0", "c", "0", "d"])
        .await;
    assert_eq!(
        client.ask(&["ZRANGEBYLEX", "z", "[b", "(d"]).await,
        texts(&["b", "c"])
    );
    assert_eq!(
        client
            .ask(&["ZREVRANGEBYLEX", "z", "+", "-", "LIMIT", "0", "2"])
            .await,
        texts(&["d", "c"])
    );
    assert_eq!(
        client.ask(&["ZLEXCOUNT", "z", "-", "+"]).await,
        Reply::Integer(4)
    );
    assert_eq!(
        client.ask(&["ZRANGE", "z", "[a", "[b", "BYLEX"]).await,
        texts(&["a", "b"])
    );
    assert!(matches!(
        client.ask(&["ZRANGEBYLEX", "z", "b", "d"]).await,
        Reply::Error(text) if text.contains("not valid string range item")
    ));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn sorted_sets_combine_and_store_their_combination() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client.ask(&["ZADD", "a", "1", "x", "2", "y"]).await;
    client.ask(&["ZADD", "b", "3", "y", "4", "z"]).await;
    client.ask(&["SADD", "s", "y"]).await;
    assert_eq!(
        client.ask(&["ZUNION", "2", "a", "b", "WITHSCORES"]).await,
        texts(&["x", "1", "z", "4", "y", "5"])
    );
    assert_eq!(
        client
            .ask(&["ZINTER", "2", "a", "b", "AGGREGATE", "MAX", "WITHSCORES"])
            .await,
        texts(&["y", "3"])
    );
    assert_eq!(client.ask(&["ZDIFF", "2", "a", "b"]).await, texts(&["x"]));
    // A plain set counts as a ranking whose members score 1.
    assert_eq!(
        client.ask(&["ZINTERCARD", "3", "a", "b", "s"]).await,
        Reply::Integer(1)
    );
    client.ask(&["SET", "d", "a string"]).await;
    client.ask(&["EXPIRE", "d", "100"]).await;
    assert_eq!(
        client
            .ask(&["ZUNIONSTORE", "d", "2", "a", "b", "WEIGHTS", "2", "1"])
            .await,
        Reply::Integer(3)
    );
    // The destination is written whole: another kind and an expiry both go.
    assert_eq!(
        client.ask(&["ZRANGE", "d", "0", "-1", "WITHSCORES"]).await,
        texts(&["x", "2", "z", "4", "y", "7"])
    );
    assert_eq!(client.ask(&["TTL", "d"]).await, Reply::Integer(-1));
    assert_eq!(
        client.ask(&["ZDIFFSTORE", "d", "2", "a", "a"]).await,
        Reply::Integer(0)
    );
    assert_eq!(client.ask(&["EXISTS", "d"]).await, Reply::Integer(0));
    assert_eq!(
        client
            .ask(&["ZRANGESTORE", "r", "b", "(3", "+inf", "BYSCORE"])
            .await,
        Reply::Integer(1)
    );
    assert_eq!(client.ask(&["ZRANGE", "r", "0", "-1"]).await, texts(&["z"]));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn sets_store_their_union_intersection_and_difference() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client.ask(&["SADD", "p", "1", "2", "3"]).await;
    client.ask(&["SADD", "q", "2", "3", "4"]).await;
    assert_eq!(
        client.ask(&["SINTERSTORE", "i", "p", "q"]).await,
        Reply::Integer(2)
    );
    assert_eq!(client.ask(&["SMEMBERS", "i"]).await, texts(&["2", "3"]));
    assert_eq!(
        client.ask(&["SUNIONSTORE", "u", "p", "q"]).await,
        Reply::Integer(4)
    );
    assert_eq!(
        client.ask(&["SDIFFSTORE", "f", "p", "q"]).await,
        Reply::Integer(1)
    );
    assert_eq!(client.ask(&["SMEMBERS", "f"]).await, texts(&["1"]));
    // A store may name one of its own sources as the destination.
    assert_eq!(
        client.ask(&["SDIFFSTORE", "p", "p", "p"]).await,
        Reply::Integer(0)
    );
    assert_eq!(client.ask(&["EXISTS", "p"]).await, Reply::Integer(0));
    client.ask(&["SET", "str", "v"]).await;
    assert!(matches!(
        client.ask(&["SUNIONSTORE", "u", "str"]).await,
        Reply::Error(text) if text.starts_with("WRONGTYPE")
    ));
    assert_eq!(client.ask(&["SCARD", "u"]).await, Reply::Integer(4));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_container_is_scanned_in_one_page() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client
        .ask(&["HSET", "h", "f1", "v1", "f2", "v2", "g", "v3"])
        .await;
    client
        .ask(&["SADD", "s", "apple", "avocado", "banana"])
        .await;
    client.ask(&["ZADD", "z", "1.5", "m"]).await;
    let scanned = |reply: Reply| {
        let mut parts = array(reply).into_iter();
        let cursor = bytes_of(parts.next().unwrap());
        (cursor, parts.next().unwrap())
    };
    assert_eq!(
        scanned(client.ask(&["HSCAN", "h", "0", "MATCH", "f*"]).await),
        (b"0".to_vec(), texts(&["f1", "v1", "f2", "v2"]))
    );
    assert_eq!(
        scanned(client.ask(&["HSCAN", "h", "0", "NOVALUES"]).await),
        (b"0".to_vec(), texts(&["f1", "f2", "g"]))
    );
    assert_eq!(
        scanned(
            client
                .ask(&["SSCAN", "s", "0", "MATCH", "a*", "COUNT", "1"])
                .await
        ),
        (b"0".to_vec(), texts(&["apple", "avocado"]))
    );
    assert_eq!(
        scanned(client.ask(&["ZSCAN", "z", "0"]).await),
        (b"0".to_vec(), texts(&["m", "1.5"]))
    );
    // Only 0 is ever handed out; any other cursor is a walk already over.
    assert_eq!(
        scanned(client.ask(&["SSCAN", "s", "7"]).await),
        (b"0".to_vec(), texts(&[]))
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_multi_pop_takes_from_the_first_key_holding_any() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client.ask(&["RPUSH", "l1", "a", "b", "c"]).await;
    client.ask(&["EXPIRE", "l1", "100"]).await;
    assert_eq!(
        client
            .ask(&["LMPOP", "2", "none", "l1", "RIGHT", "COUNT", "2"])
            .await,
        Reply::Array(vec![bulk("l1"), texts(&["c", "b"])])
    );
    assert!(matches!(
        client.ask(&["TTL", "l1"]).await,
        Reply::Integer(99 | 100)
    ));
    assert_eq!(
        client
            .ask(&["LMPOP", "1", "l1", "LEFT", "COUNT", "5"])
            .await,
        Reply::Array(vec![bulk("l1"), texts(&["a"])])
    );
    assert_eq!(client.ask(&["EXISTS", "l1"]).await, Reply::Integer(0));
    assert_eq!(
        client.ask(&["LMPOP", "1", "l1", "LEFT"]).await,
        Reply::NilArray
    );
    client.ask(&["ZADD", "z", "1", "a", "2", "b"]).await;
    assert_eq!(
        client.ask(&["ZMPOP", "1", "z", "MAX"]).await,
        Reply::Array(vec![
            bulk("z"),
            Reply::Array(vec![Reply::Array(vec![bulk("b"), bulk("2")])])
        ])
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_multi_pops_hand_out_each_element_once() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    let items: Vec<String> = (0..200).map(|item| item.to_string()).collect();
    let mut push = vec!["RPUSH", "jobs"];
    push.extend(items.iter().map(String::as_str));
    client.ask(&push).await;
    let mut workers = Vec::new();
    for _ in 0..8 {
        let address = address.clone();
        workers.push(tokio::spawn(async move {
            let mut client = Client::to(&address).await;
            let mut taken = Vec::new();
            loop {
                let reply = client.ask(&["LMPOP", "1", "jobs", "LEFT"]).await;
                if reply == Reply::NilArray {
                    return taken;
                }
                assert!(matches!(reply, Reply::Array(_)), "{reply:?}");
                let mut parts = array(reply).into_iter();
                parts.next();
                taken.extend(array(parts.next().unwrap()).into_iter().map(bytes_of));
            }
        }));
    }
    let mut every = Vec::new();
    for worker in workers {
        every.extend(worker.await.unwrap());
    }
    every.sort();
    let mut expected: Vec<Vec<u8>> = items.into_iter().map(String::into_bytes).collect();
    expected.sort();
    assert_eq!(every, expected);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_does_not_run_when_a_watched_key_changed() {
    let (address, stop, _) = serving_with("").await;
    let mut watcher = Client::to(&address).await;
    let mut other = Client::to(&address).await;
    watcher.ask(&["SET", "k", "1"]).await;
    assert_eq!(watcher.ask(&["WATCH", "k", "absent"]).await, Reply::OK);
    other.ask(&["SET", "k", "2"]).await;
    watcher.ask(&["MULTI"]).await;
    assert!(matches!(
        watcher.ask(&["WATCH", "k"]).await,
        Reply::Error(text) if text.contains("inside MULTI")
    ));
    watcher.ask(&["SET", "k", "3"]).await;
    assert_eq!(watcher.ask(&["EXEC"]).await, Reply::NilArray);
    assert_eq!(watcher.ask(&["GET", "k"]).await, bulk("2"));
    // Unchanged, the transaction runs; and EXEC forgot the watch.
    watcher.ask(&["WATCH", "k"]).await;
    watcher.ask(&["MULTI"]).await;
    watcher.ask(&["INCR", "k"]).await;
    assert_eq!(
        watcher.ask(&["EXEC"]).await,
        Reply::Array(vec![Reply::Integer(3)])
    );
    // A key absent when watched and created since refuses it too.
    watcher.ask(&["WATCH", "fresh"]).await;
    other.ask(&["SET", "fresh", "x"]).await;
    watcher.ask(&["MULTI"]).await;
    watcher.ask(&["DEL", "fresh"]).await;
    assert_eq!(watcher.ask(&["EXEC"]).await, Reply::NilArray);
    // UNWATCH forgets it.
    watcher.ask(&["WATCH", "k"]).await;
    other.ask(&["SET", "k", "9"]).await;
    assert_eq!(watcher.ask(&["UNWATCH"]).await, Reply::OK);
    watcher.ask(&["MULTI"]).await;
    watcher.ask(&["GET", "k"]).await;
    assert_eq!(watcher.ask(&["EXEC"]).await, Reply::Array(vec![bulk("9")]));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ranking_is_cut_by_rank_by_score_and_by_member() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    let fill = [
        "ZADD", "z", "1", "a", "2", "b", "3", "c", "4", "d", "5", "e",
    ];
    client.ask(&fill).await;
    assert_eq!(
        client.ask(&["ZREMRANGEBYRANK", "z", "-2", "-1"]).await,
        Reply::Integer(2)
    );
    assert_eq!(
        client.ask(&["ZREMRANGEBYSCORE", "z", "(1", "2"]).await,
        Reply::Integer(1)
    );
    assert_eq!(
        client.ask(&["ZREMRANGEBYSCORE", "z", "+inf", "+inf"]).await,
        Reply::Integer(0)
    );
    assert_eq!(
        client.ask(&["ZRANGE", "z", "0", "-1"]).await,
        texts(&["a", "c"])
    );
    assert_eq!(
        client.ask(&["ZREMRANGEBYSCORE", "z", "-inf", "+inf"]).await,
        Reply::Integer(2)
    );
    client
        .ask(&["ZADD", "z", "0", "a", "0", "b", "0", "c", "0", "d"])
        .await;
    assert_eq!(
        client.ask(&["ZREMRANGEBYLEX", "z", "(a", "[c"]).await,
        Reply::Integer(2)
    );
    assert_eq!(
        client.ask(&["ZREMRANGEBYLEX", "z", "+", "-"]).await,
        Reply::Integer(0)
    );
    assert_eq!(
        client.ask(&["ZRANGE", "z", "0", "-1"]).await,
        texts(&["a", "d"])
    );
    assert!(matches!(
        client.ask(&["ZREMRANGEBYSCORE", "z", "x", "1"]).await,
        Reply::Error(text) if text.contains("not a float")
    ));
    client.ask(&["SET", "s", "v"]).await;
    assert!(matches!(
        client.ask(&["ZREMRANGEBYRANK", "s", "0", "-1"]).await,
        Reply::Error(text) if text.starts_with("WRONGTYPE")
    ));
    // The sliding-window rate limiter: queued inside MULTI and run as one.
    client
        .ask(&["ZADD", "hits", "100", "h1", "200", "h2"])
        .await;
    assert_eq!(client.ask(&["MULTI"]).await, Reply::OK);
    for queued in [
        &["ZREMRANGEBYSCORE", "hits", "-inf", "150"][..],
        &["ZADD", "hits", "300", "h3"],
        &["ZCARD", "hits"],
    ] {
        assert_eq!(client.ask(queued).await, status("QUEUED"));
    }
    assert_eq!(
        client.ask(&["EXEC"]).await,
        Reply::Array(vec![
            Reply::Integer(1),
            Reply::Integer(1),
            Reply::Integer(2)
        ])
    );
    stop.cancel();
}
