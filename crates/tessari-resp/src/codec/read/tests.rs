use super::*;

fn all(decoder: &mut Decoder) -> Vec<Command> {
    let mut read = Vec::new();
    while let Some(command) = decoder.command().unwrap() {
        read.push(command);
    }
    read
}

fn words(text: &[&str]) -> Command {
    text.iter().map(|word| word.as_bytes().to_vec()).collect()
}

#[test]
fn a_pipeline_of_arrays_and_inline_lines_reads_in_order() {
    let mut decoder = Decoder::new();
    decoder.extend(b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\nPING\r\n\r\n*0\r\nSET  a\tb\n");
    assert_eq!(
        all(&mut decoder),
        vec![
            words(&["GET", "k"]),
            words(&["PING"]),
            words(&["SET", "a", "b"])
        ]
    );
    assert!(!decoder.holds_a_part());
}

#[test]
fn a_command_split_anywhere_reads_once_it_is_whole() {
    let whole = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\nv\r\n\0z\r\n";
    for cut in 1..whole.len() {
        let mut decoder = Decoder::new();
        decoder.extend(&whole[..cut]);
        assert_eq!(decoder.command().unwrap(), None, "cut at {cut}");
        assert!(decoder.holds_a_part());
        decoder.extend(&whole[cut..]);
        assert_eq!(
            decoder.command().unwrap(),
            Some(vec![b"SET".to_vec(), b"k".to_vec(), b"v\r\n\0z".to_vec()]),
            "cut at {cut}"
        );
    }
}

#[test]
fn lengths_past_their_ceilings_are_refused_before_anything_is_held() {
    let refused = |bytes: &[u8]| {
        let mut decoder = Decoder::new();
        decoder.extend(bytes);
        decoder.command().unwrap_err().to_string()
    };
    assert!(refused(b"*1048577\r\n").contains("argument count is 1048577"));
    assert!(refused(b"*1\r\n$16777217\r\n").contains("bulk length is 16777217"));
    assert!(refused(b"*1\r\n:5\r\n").contains("bulk strings"));
    assert!(refused(b"*1\r\n$-1\r\n").contains("cannot be null"));
    assert!(refused(b"*1\r\n$1\r\nab\r\n").contains("does not end where its length says"));
    assert!(refused(b"*x\r\n").contains("not a number"));
    assert!(refused(&[b'P'; 70_000]).contains("inline command"));
}

#[test]
fn many_arguments_together_are_held_to_the_command_ceiling() {
    let mut decoder = Decoder::new();
    decoder.extend(b"*5\r\n");
    let bulk = vec![b'x'; RESP_MAX_BULK_BYTES];
    let mut refused = None;
    for _ in 0..5 {
        decoder.extend(format!("${}\r\n", bulk.len()).as_bytes());
        decoder.extend(&bulk);
        decoder.extend(b"\r\n");
        match decoder.command() {
            Ok(_) => {}
            Err(why) => {
                refused = Some(why.to_string());
                break;
            }
        }
    }
    assert!(
        refused.unwrap().contains("a command's arguments"),
        "the fifth bulk takes the command past its ceiling"
    );
}
