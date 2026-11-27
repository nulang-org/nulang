//! Always-on RESP integer and TTL regression tests.
use nulang::runtime::{cache::CacheStore, resp_cache::execute_frame};

fn frame(args: &[&[u8]]) -> Vec<u8> {
    let mut wire = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        wire.extend_from_slice(format!("{}{}\r\n", '$', arg.len()).as_bytes());
        wire.extend_from_slice(arg);
        wire.extend_from_slice(b"\r\n");
    }
    wire
}
fn candidate(store: &mut CacheStore, args: &[&[u8]], now: u64) -> Vec<u8> {
    let wire = frame(args);
    let mut out = Vec::new();
    assert_eq!(
        execute_frame(store, &wire, now, &mut out).unwrap(),
        Some(wire.len())
    );
    out
}
#[test]
fn canonical_integer_and_ttl_regressions_without_external_server() {
    let mut store = CacheStore::new();
    for raw in [b"+1".as_slice(), b"01", b"-0", b"-01", b" 1"] {
        assert_eq!(candidate(&mut store, &[b"SET", b"k", raw], 0), b"+OK\r\n");
        assert_eq!(
            candidate(&mut store, &[b"INCR", b"k"], 0),
            b"-ERR value is not an integer or out of range\r\n"
        );
        let mut expected = format!("{}{}\r\n", '$', raw.len()).into_bytes();
        expected.extend(raw);
        expected.extend(b"\r\n");
        assert_eq!(candidate(&mut store, &[b"GET", b"k"], 0), expected);
    }
    assert_eq!(
        candidate(&mut store, &[b"SET", b"k", b"9223372036854775807"], 0),
        b"+OK\r\n"
    );
    assert_eq!(
        candidate(&mut store, &[b"INCR", b"k"], 0),
        b"-ERR increment or decrement would overflow\r\n"
    );
    assert_eq!(
        candidate(&mut store, &[b"GET", b"k"], 0),
        b"$19\r\n9223372036854775807\r\n"
    );
    assert_eq!(
        candidate(&mut store, &[b"SET", b"k", b"1", b"PX", b"1900"], 0),
        b"+OK\r\n"
    );
    for (time, expected) in [(0, 2), (400, 2), (401, 1), (1400, 1), (1401, 0), (1900, -2)] {
        assert_eq!(
            candidate(&mut store, &[b"TTL", b"k"], time),
            format!(":{expected}\r\n").as_bytes()
        );
    }
}
